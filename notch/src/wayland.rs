//! Wayland platform layer: SCTK layer-shell panels drawn with tiny-skia.
//!
//! One `App` owns the connection, the calloop loop and every `Panel`. A panel
//! pairs a layer surface with a `View` (the notch, a pill, a popup...). Views
//! draw in logical pixels; the surface is rendered at the compositor's
//! fractional scale (`wp_fractional_scale_v1`) and presented through
//! `wp_viewport`, so text and edges are sharp at 1.6x etc.
//!
//! Worker threads never touch UI state: they send a `Post` closure through the
//! calloop channel; it runs on the main thread against `State`.

use crate::layout::{Point, Rect, pt};
use crate::render::{Canvas, TextSys, blit_argb8888};
use crate::state::State;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_dispatch2, delegate_registry,
    output::{OutputHandler, OutputState},
    reexports::calloop_wayland_source::WaylandSource,
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
    },
    shell::{
        WaylandSurface,
        wlr_layer::{Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure},
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use calloop::{EventLoop, LoopHandle, LoopSignal, channel, timer::{TimeoutAction, Timer}};
use std::time::{Duration, Instant};
use tiny_skia::Pixmap;
use wayland_client::{
    Connection, Dispatch, QueueHandle,
    globals::{GlobalList, registry_queue_init},
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
};
use wayland_protocols::wp::{
    cursor_shape::v1::client::{
        wp_cursor_shape_device_v1::{self, WpCursorShapeDeviceV1},
        wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
    },
    fractional_scale::v1::client::{
        wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
        wp_fractional_scale_v1::{self, WpFractionalScaleV1},
    },
    viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
};

pub type PanelId = u32;

/// Deferred state mutation sent from worker threads.
pub type Post = Box<dyn FnOnce(&mut State, &mut Vec<Cmd>) + Send>;

#[derive(Clone)]
pub struct Poster(channel::Sender<Post>);

impl Poster {
    /// Queue `f` to run on the UI thread; every panel is redrawn afterwards.
    pub fn post(&self, f: impl FnOnce(&mut State, &mut Vec<Cmd>) + Send + 'static) {
        let _ = self.0.send(Box::new(f));
    }
}

impl Poster {
    /// A poster whose queue nobody drains (headless rendering/tests).
    #[cfg(test)]
    pub fn detached() -> Poster { Poster(channel::channel().0) }

    /// A poster whose queue a test drains through a calloop channel source (see `testkit::Pump`).
    #[cfg(test)]
    pub fn pumped() -> (Poster, channel::Channel<Post>) {
        let (tx, rx) = channel::channel();
        (Poster(tx), rx)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cursor { Default, Pointer, Text }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KbMode { None, OnDemand, Exclusive }

#[derive(Clone, Debug, PartialEq)]
pub struct PanelSpec {
    pub namespace: &'static str,
    pub layer: Layer,
    pub anchor: Anchor,
    /// Logical surface size. 0 on an axis stretches to the anchored span.
    pub size: (u32, u32),
    /// top, right, bottom, left
    pub margin: (i32, i32, i32, i32),
    pub exclusive: i32,
    pub kb: KbMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button { Left, Right, Middle, Other(u32) }

#[derive(Clone, Debug)]
pub struct KeyInput {
    pub sym: Keysym,
    pub text: Option<String>,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

#[derive(Clone, Debug)]
pub enum Event {
    PointerEnter(Point),
    PointerMove(Point),
    PointerLeave,
    Press(Button, Point),
    Release(Button, Point),
    /// Scroll in logical px (positive = content moves up / wheel down) and discrete wheel steps.
    Scroll { dx: f32, dy: f32, steps: i32, pos: Point },
    Key(KeyInput),
    KeyboardEnter,
    KeyboardLeave,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputInfo {
    pub name: String,
    pub pos: (i32, i32),
    pub size: (i32, i32),
}

pub enum Cmd {
    Open { view: Box<dyn View>, output: Option<String> },
    Close(PanelId),
    Redraw(PanelId),
    RedrawAll,
    Exit,
}

/// What a view receives while handling input or ticking.
pub struct Ctx<'a> {
    pub state: &'a mut State,
    pub cmds: &'a mut Vec<Cmd>,
    pub text: &'a mut TextSys,
    pub poster: &'a Poster,
    pub id: PanelId,
    /// Name of the output this panel is on, if known.
    pub output: Option<&'a str>,
}

/// What a view receives while drawing (the canvas already owns the text system).
pub struct DrawCtx<'a> {
    pub state: &'a State,
    pub cmds: &'a mut Vec<Cmd>,
    pub poster: &'a Poster,
    pub id: PanelId,
    pub output: Option<&'a str>,
}

pub trait View {
    fn spec(&self, state: &State) -> PanelSpec;
    /// Advance animations by `dt` seconds. Return true while more frames are needed.
    fn tick(&mut self, dt: f32, ctx: &mut Ctx) -> bool;
    /// Draw into the (cleared) canvas; logical size is `cv.size()`.
    fn draw(&mut self, cv: &mut Canvas, ctx: &mut DrawCtx);
    /// Region that receives pointer input, in logical px. `None` = entire surface.
    fn input_region(&self) -> Option<Vec<Rect>> { None }
    fn event(&mut self, ev: Event, ctx: &mut Ctx);
    fn cursor(&self) -> Cursor { Cursor::Default }
    /// Redraw once per second even when idle (clocks).
    fn ticks_seconds(&self) -> bool { false }
    /// Called once the panel exists, with its id.
    fn attached(&mut self, _id: PanelId) {}
}

pub trait Shell {
    /// Outputs were added/removed/changed; open/close per-output panels here.
    fn outputs_changed(&mut self, state: &mut State, cmds: &mut Vec<Cmd>);
}

struct Panel {
    id: PanelId,
    view: Box<dyn View>,
    layer: LayerSurface,
    viewport: Option<WpViewport>,
    _frac: Option<WpFractionalScaleV1>,
    pool: SlotPool,
    pixmap: Option<Pixmap>,
    spec: PanelSpec,
    scale120: u32,
    configured: bool,
    dirty: bool,
    animating: bool,
    frame_pending: bool,
    last_frame_ms: Option<u32>,
    last_draw: Instant,
    output: Option<String>,
    cursor: Cursor,
}

pub struct App {
    registry: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    viewporter: Option<WpViewporter>,
    frac_mgr: Option<WpFractionalScaleManagerV1>,
    cursor_mgr: Option<WpCursorShapeManagerV1>,
    cursor_dev: Option<WpCursorShapeDeviceV1>,
    pointer: Option<wl_pointer::WlPointer>,
    pointer_serial: u32,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    panels: Vec<Panel>,
    next_id: PanelId,
    pointer_panel: Option<PanelId>,
    keyboard_panel: Option<PanelId>,
    pub state: State,
    pub text: TextSys,
    poster: Poster,
    shell: Option<Box<dyn Shell>>,
    loop_signal: LoopSignal,
    exit: bool,
    pending: Vec<Cmd>,
    mods: Modifiers,
}

impl App {
    fn spawn_panel(&mut self, qh: &QueueHandle<App>, view: Box<dyn View>, output: Option<String>) -> PanelId {
        let id = self.next_id;
        self.next_id += 1;
        let spec = view.spec(&self.state);
        let surface = self.compositor.create_surface(qh);
        let wl_out = output.as_ref().and_then(|n| {
            self.output_state.outputs().find(|o| self.output_state.info(o).and_then(|i| i.name).as_deref() == Some(n.as_str()))
        });
        let layer = self.layer_shell.create_layer_surface(qh, surface.clone(), spec.layer, Some(spec.namespace), wl_out.as_ref());
        apply_spec(&layer, &spec);
        let viewport = self.viewporter.as_ref().map(|v| v.get_viewport(&surface, qh, ()));
        let frac = self.frac_mgr.as_ref().map(|m| m.get_fractional_scale(&surface, qh, surface.clone()));
        layer.commit();
        let pool = SlotPool::new(4096, &self.shm).expect("shm pool");
        let mut view = view;
        view.attached(id);
        self.panels.push(Panel {
            id, view, layer, viewport, _frac: frac, pool, pixmap: None, spec, scale120: 120, configured: false, dirty: true,
            animating: false, frame_pending: false, last_frame_ms: None, last_draw: Instant::now(), output, cursor: Cursor::Default,
        });
        id
    }

    fn panel_idx(&self, id: PanelId) -> Option<usize> { self.panels.iter().position(|p| p.id == id) }
    fn close_panel(&mut self, id: PanelId) {
        self.panels.retain(|p| p.id != id);
        if self.pointer_panel == Some(id) { self.pointer_panel = None; }
        if self.keyboard_panel == Some(id) { self.keyboard_panel = None; }
    }


    fn panel_by_surface(&self, s: &wl_surface::WlSurface) -> Option<usize> {
        self.panels.iter().position(|p| p.layer.wl_surface() == s)
    }

    fn apply_cmds(&mut self, qh: &QueueHandle<App>) {
        let mut guard = 0;
        while !self.pending.is_empty() && guard < 16 {
            guard += 1;
            for c in std::mem::take(&mut self.pending) {
                match c {
                    Cmd::Open { view, output } => {
                        // never let the compositor pick another output for a panel bound to one that is gone
                        if bound_to_missing(output.as_deref(), &self.state.outputs) { continue; }
                        self.spawn_panel(qh, view, output);
                    }
                    Cmd::Close(id) => self.close_panel(id),
                    Cmd::Redraw(id) => { if let Some(i) = self.panel_idx(id) { self.panels[i].dirty = true; } }
                    Cmd::RedrawAll => self.panels.iter_mut().for_each(|p| p.dirty = true),
                    Cmd::Exit => { self.exit = true; self.loop_signal.stop(); }
                }
            }
        }
        for i in 0..self.panels.len() {
            if self.panels[i].configured && (self.panels[i].dirty || self.panels[i].animating) && !self.panels[i].frame_pending {
                self.draw(i, qh);
            }
        }
    }

    fn with_ctx<R>(&mut self, idx: usize, f: impl FnOnce(&mut dyn View, &mut Ctx) -> R) -> R {
        let mut cmds = Vec::new();
        let id = self.panels[idx].id;
        let out = self.panels[idx].output.clone();
        let r = {
            let App { panels, state, text, poster, .. } = self;
            let mut ctx = Ctx { state, cmds: &mut cmds, text, poster, id, output: out.as_deref() };
            f(panels[idx].view.as_mut(), &mut ctx)
        };
        self.pending.extend(cmds);
        r
    }

    fn dispatch_event(&mut self, idx: usize, ev: Event, qh: &QueueHandle<App>) {
        self.with_ctx(idx, |v, c| v.event(ev, c));
        self.panels[idx].dirty = true;
        self.apply_cmds(qh);
    }

    fn draw(&mut self, idx: usize, qh: &QueueHandle<App>) {
        // Tick first so the frame shows the new state.
        let now = Instant::now();
        let dt = now.duration_since(self.panels[idx].last_draw).as_secs_f32().min(0.05);
        self.panels[idx].last_draw = now;
        let animating = self.with_ctx(idx, |v, c| v.tick(dt, c));
        let spec = self.panels[idx].view.spec(&self.state);
        if spec != self.panels[idx].spec {
            apply_spec(&self.panels[idx].layer, &spec);
            self.panels[idx].spec = spec.clone();
        }
        let scale = self.panels[idx].scale120 as f32 / 120.0;
        let (lw, lh) = (spec.size.0.max(1), spec.size.1.max(1));
        let (pw, ph) = ((lw as f32 * scale).ceil() as u32, (lh as f32 * scale).ceil() as u32);
        let mut pm = match self.panels[idx].pixmap.take() {
            Some(pm) if pm.width() == pw && pm.height() == ph => pm,
            _ => match Pixmap::new(pw, ph) { Some(p) => p, None => return },
        };
        {
            let mut cmds = Vec::new();
            let id = self.panels[idx].id;
            let out = self.panels[idx].output.clone();
            let App { panels, state, text, poster, .. } = self;
            let mut cv = Canvas::new(&mut pm, text, scale);
            cv.clear();
            let mut ctx = DrawCtx { state, cmds: &mut cmds, poster, id, output: out.as_deref() };
            panels[idx].view.draw(&mut cv, &mut ctx);
            self.pending.extend(cmds);
        }
        let panel = &mut self.panels[idx];
        let stride = pw as i32 * 4;
        match panel.pool.create_buffer(pw as i32, ph as i32, stride, wl_shm::Format::Argb8888) {
            Ok((buffer, canvas)) => {
                blit_argb8888(&pm, canvas);
                let surface = panel.layer.wl_surface();
                match &panel.viewport {
                    Some(vp) => vp.set_destination(lw as i32, lh as i32),
                    None => {}
                }
                surface.damage_buffer(0, 0, pw as i32, ph as i32);
                surface.frame(qh, smithay_client_toolkit::compositor::FrameCallbackData(surface.clone()));
                if buffer.attach_to(surface).is_err() { panel.pixmap = Some(pm); return; }
                // Input region.
                if let Some(rects) = panel.view.input_region() {
                    let region = self.compositor.wl_compositor().create_region(qh, ());
                    for r in rects { region.add(r.x.floor() as i32, r.y.floor() as i32, r.w.ceil() as i32, r.h.ceil() as i32); }
                    surface.set_input_region(Some(&region));
                    region.destroy();
                } else {
                    surface.set_input_region(None);
                }
                panel.layer.commit();
                panel.frame_pending = true;
            }
            Err(e) => eprintln!("waybar-notch: shm buffer failed: {e}"),
        }
        panel.pixmap = Some(pm);
        panel.dirty = false;
        panel.animating = animating;
        let cur = panel.view.cursor();
        if cur != panel.cursor { panel.cursor = cur; }
        if self.pointer_panel == Some(panel.id) { self.update_cursor(); }
    }

    fn update_cursor(&mut self) {
        let (Some(dev), Some(id)) = (&self.cursor_dev, self.pointer_panel) else { return };
        let Some(p) = self.panels.iter().find(|p| p.id == id) else { return };
        let shape = match p.cursor {
            Cursor::Default => wp_cursor_shape_device_v1::Shape::Default,
            Cursor::Pointer => wp_cursor_shape_device_v1::Shape::Pointer,
            Cursor::Text => wp_cursor_shape_device_v1::Shape::Text,
        };
        dev.set_shape(self.pointer_serial, shape);
    }

    fn rescan_outputs(&mut self) {
        let outs: Vec<OutputInfo> = self.output_state.outputs().filter_map(|o| {
            let i = self.output_state.info(&o)?;
            Some(OutputInfo { name: i.name?, pos: i.logical_position.unwrap_or(i.location), size: i.logical_size.unwrap_or((0, 0)) })
        }).collect();
        if outs == self.state.outputs { return; }
        self.state.outputs = outs;
        // panels bound to an output that vanished go first, so shell reconciliation sees the real set of panels
        let gone: Vec<PanelId> = self.panels.iter().filter(|p| bound_to_missing(p.output.as_deref(), &self.state.outputs)).map(|p| p.id).collect();
        for id in gone { self.close_panel(id); }
        let mut cmds = Vec::new();
        if let Some(shell) = self.shell.as_mut() { shell.outputs_changed(&mut self.state, &mut cmds); }
        self.pending.extend(cmds);
        self.pending.push(Cmd::RedrawAll);
    }
}

/// A panel explicitly bound to an output name that is not among the connected `live` outputs.
fn bound_to_missing(panel_output: Option<&str>, live: &[OutputInfo]) -> bool {
    panel_output.is_some_and(|n| !live.iter().any(|o| o.name == n))
}

fn apply_spec(layer: &LayerSurface, s: &PanelSpec) {
    layer.set_anchor(s.anchor);
    layer.set_size(s.size.0, s.size.1);
    layer.set_margin(s.margin.0, s.margin.1, s.margin.2, s.margin.3);
    layer.set_exclusive_zone(s.exclusive);
    layer.set_keyboard_interactivity(match s.kb {
        KbMode::None => KeyboardInteractivity::None,
        KbMode::OnDemand => KeyboardInteractivity::OnDemand,
        KbMode::Exclusive => KeyboardInteractivity::Exclusive,
    });
}

// ---- SCTK handlers -------------------------------------------------------

impl CompositorHandler for App {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, qh: &QueueHandle<Self>, surface: &wl_surface::WlSurface, time: u32) {
        let Some(i) = self.panel_by_surface(surface) else { return };
        self.panels[i].frame_pending = false;
        self.panels[i].last_frame_ms = Some(time);
        if self.panels[i].dirty || self.panels[i].animating {
            self.draw(i, qh);
        }
        self.apply_cmds(qh);
    }
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState { &mut self.output_state }
    fn new_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: wl_output::WlOutput) { self.rescan_outputs(); self.apply_cmds(qh); }
    fn update_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: wl_output::WlOutput) { self.rescan_outputs(); self.apply_cmds(qh); }
    fn output_destroyed(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: wl_output::WlOutput) { self.rescan_outputs(); self.apply_cmds(qh); }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, qh: &QueueHandle<Self>, layer: &LayerSurface) {
        // Output unplugged or compositor closed it: drop the panel; the shell re-creates per-output ones.
        if let Some(i) = self.panel_by_surface(layer.wl_surface()) {
            let id = self.panels[i].id;
            self.pending.push(Cmd::Close(id));
        }
        self.apply_cmds(qh);
    }
    fn configure(&mut self, _: &Connection, qh: &QueueHandle<Self>, layer: &LayerSurface, _: LayerSurfaceConfigure, _: u32) {
        let Some(i) = self.panel_by_surface(layer.wl_surface()) else { return };
        self.panels[i].configured = true;
        self.panels[i].dirty = true;
        if !self.panels[i].frame_pending { self.draw(i, qh); }
        self.apply_cmds(qh);
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState { &mut self.seat_state }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, cap: Capability) {
        if cap == Capability::Pointer && self.pointer.is_none() {
            if let Ok(p) = self.seat_state.get_pointer(qh, &seat) {
                self.cursor_dev = self.cursor_mgr.as_ref().map(|m| m.get_pointer(&p, qh, ()));
                self.pointer = Some(p);
            }
        }
        if cap == Capability::Keyboard && self.keyboard.is_none() {
            let h = self.loop_handle_for_repeat();
            if let Ok(k) = self.seat_state.get_keyboard_with_repeat(
                qh, &seat, None, h,
                Box::new(|app: &mut App, _kb: &wl_keyboard::WlKeyboard, ev: KeyEvent| app.key_event(ev)),
            ) {
                self.keyboard = Some(k);
            }
        }
    }
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, cap: Capability) {
        if cap == Capability::Pointer { if let Some(p) = self.pointer.take() { p.release(); } self.cursor_dev = None; }
        if cap == Capability::Keyboard { if let Some(k) = self.keyboard.take() { k.release(); } }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

thread_local! {
    static LOOP_HANDLE: std::cell::RefCell<Option<LoopHandle<'static, App>>> = const { std::cell::RefCell::new(None) };
    static QH: std::cell::RefCell<Option<QueueHandle<App>>> = const { std::cell::RefCell::new(None) };
}

impl App {
    fn loop_handle_for_repeat(&self) -> LoopHandle<'static, App> {
        LOOP_HANDLE.with(|h| h.borrow().clone().expect("loop handle"))
    }

    fn key_event(&mut self, ev: KeyEvent) {
        let Some(id) = self.keyboard_panel else { return };
        let Some(idx) = self.panel_idx(id) else { return };
        let m = self.mods;
        let input = KeyInput { sym: ev.keysym, text: ev.utf8.filter(|t| !t.chars().all(char::is_control)), ctrl: m.ctrl, alt: m.alt, shift: m.shift };
        let qh = QH.with(|q| q.borrow().clone()).expect("qh");
        self.dispatch_event(idx, Event::Key(input), &qh);
    }
}

impl KeyboardHandler for App {
    fn enter(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, surface: &wl_surface::WlSurface, _: u32, _: &[u32], _: &[Keysym]) {
        if let Some(i) = self.panel_by_surface(surface) {
            self.keyboard_panel = Some(self.panels[i].id);
            self.dispatch_event(i, Event::KeyboardEnter, qh);
        }
    }
    fn leave(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, surface: &wl_surface::WlSurface, _: u32) {
        if let Some(i) = self.panel_by_surface(surface) {
            if self.keyboard_panel == Some(self.panels[i].id) { self.keyboard_panel = None; }
            self.dispatch_event(i, Event::KeyboardLeave, qh);
        }
    }
    fn press_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, event: KeyEvent) { self.key_event(event); }
    fn repeat_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: KeyEvent) {}
    fn release_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: KeyEvent) {}
    fn update_modifiers(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, modifiers: Modifiers, _: RawModifiers, _: u32) {
        self.mods = modifiers;
    }
}

impl PointerHandler for App {
    fn pointer_frame(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: &wl_pointer::WlPointer, events: &[PointerEvent]) {
        for e in events {
            let Some(i) = self.panel_by_surface(&e.surface) else { continue };
            let p = pt(e.position.0 as f32, e.position.1 as f32);
            let ev = match e.kind {
                PointerEventKind::Enter { serial } => {
                    self.pointer_serial = serial;
                    self.pointer_panel = Some(self.panels[i].id);
                    self.update_cursor();
                    Event::PointerEnter(p)
                }
                PointerEventKind::Leave { .. } => {
                    if self.pointer_panel == Some(self.panels[i].id) { self.pointer_panel = None; }
                    Event::PointerLeave
                }
                PointerEventKind::Motion { .. } => Event::PointerMove(p),
                PointerEventKind::Press { button, .. } => Event::Press(map_button(button), p),
                PointerEventKind::Release { button, .. } => Event::Release(map_button(button), p),
                PointerEventKind::Axis { horizontal, vertical, .. } => {
                    let steps = if vertical.value120 != 0 { -(vertical.value120 / 120) } else { -vertical.discrete };
                    Event::Scroll { dx: horizontal.absolute as f32, dy: vertical.absolute as f32, steps: -steps, pos: p }
                }
            };
            self.dispatch_event(i, ev, qh);
        }
    }
}

fn map_button(b: u32) -> Button {
    match b {
        0x110 => Button::Left,
        0x111 => Button::Right,
        0x112 => Button::Middle,
        o => Button::Other(o),
    }
}

impl ShmHandler for App { fn shm_state(&mut self) -> &mut Shm { &mut self.shm } }

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState { &mut self.registry }
    registry_handlers![OutputState, SeatState];
}

impl Dispatch<WpViewporter, ()> for App {
    fn event(_: &mut Self, _: &WpViewporter, _: <WpViewporter as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<WpViewport, ()> for App {
    fn event(_: &mut Self, _: &WpViewport, _: <WpViewport as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<WpFractionalScaleManagerV1, ()> for App {
    fn event(_: &mut Self, _: &WpFractionalScaleManagerV1, _: <WpFractionalScaleManagerV1 as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<WpCursorShapeManagerV1, ()> for App {
    fn event(_: &mut Self, _: &WpCursorShapeManagerV1, _: <WpCursorShapeManagerV1 as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<WpCursorShapeDeviceV1, ()> for App {
    fn event(_: &mut Self, _: &WpCursorShapeDeviceV1, _: <WpCursorShapeDeviceV1 as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<wayland_client::protocol::wl_region::WlRegion, ()> for App {
    fn event(_: &mut Self, _: &wayland_client::protocol::wl_region::WlRegion, _: <wayland_client::protocol::wl_region::WlRegion as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<WpFractionalScaleV1, wl_surface::WlSurface> for App {
    fn event(app: &mut Self, _: &WpFractionalScaleV1, event: wp_fractional_scale_v1::Event, surface: &wl_surface::WlSurface, _: &Connection, qh: &QueueHandle<Self>) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            if let Some(i) = app.panel_by_surface(surface) {
                app.panels[i].scale120 = scale.max(120);
                app.panels[i].dirty = true;
                if app.panels[i].configured && !app.panels[i].frame_pending { app.draw(i, qh); }
            }
        }
    }
}

delegate_dispatch2!(App);
delegate_registry!(App);

fn bind_opt<I, U>(globals: &GlobalList, qh: &QueueHandle<App>, ver: std::ops::RangeInclusive<u32>, udata: U) -> Option<I>
where
    I: wayland_client::Proxy + 'static,
    App: Dispatch<I, U>,
    U: Send + Sync + 'static,
{
    globals.bind(qh, ver, udata).ok()
}

/// Connect, create the loop and run until `Cmd::Exit`.
/// `setup` runs once with the poster so the caller can start worker threads and
/// seed initial panels via the returned commands.
pub fn run(state: State, shell: Box<dyn Shell>, setup: impl FnOnce(&Poster, &mut State) -> Vec<Cmd>) -> Result<(), String> {
    let conn = Connection::connect_to_env().map_err(|e| format!("wayland connect: {e}"))?;
    let (globals, event_queue) = registry_queue_init::<App>(&conn).map_err(|e| format!("registry: {e}"))?;
    let qh = event_queue.handle();
    QH.with(|q| *q.borrow_mut() = Some(qh.clone()));
    let mut event_loop: EventLoop<App> = EventLoop::try_new().map_err(|e| e.to_string())?;
    let lh = event_loop.handle();
    LOOP_HANDLE.with(|h| *h.borrow_mut() = Some(lh.clone()));

    let (tx, rx) = channel::channel::<Post>();
    let poster = Poster(tx);

    // SIGUSR1 toggles bar visibility, SIGUSR2 reloads theme + settings. Created before any worker thread exists so the
    // block mask is inherited and the default (terminate) action can never fire.
    let signals = calloop::signals::Signals::new(&[calloop::signals::Signal::SIGUSR1, calloop::signals::Signal::SIGUSR2]).map_err(|e| e.to_string())?;
    lh.insert_source(signals, |ev, _, app: &mut App| {
        match ev.signal() {
            calloop::signals::Signal::SIGUSR1 => app.state.bar_hidden = !app.state.bar_hidden,
            _ => {
                crate::theme::reload();
                let (s, _) = crate::config::load_settings();
                app.state.settings = s;
                app.state.refresh_palette();
                app.text.set_ui_family(Some(crate::theme::current().font));
            }
        }
        app.pending.push(Cmd::RedrawAll);
        let qh = QH.with(|q| q.borrow().clone()).expect("qh");
        app.apply_cmds(&qh);
    }).map_err(|e| e.to_string())?;

    let compositor = CompositorState::bind(&globals, &qh).map_err(|e| format!("wl_compositor: {e}"))?;
    let layer_shell = LayerShell::bind(&globals, &qh).map_err(|e| format!("zwlr_layer_shell: {e}"))?;
    let shm = Shm::bind(&globals, &qh).map_err(|e| format!("wl_shm: {e}"))?;
    let mut app = App {
        registry: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        compositor, layer_shell, shm,
        viewporter: bind_opt(&globals, &qh, 1..=1, ()),
        frac_mgr: bind_opt(&globals, &qh, 1..=1, ()),
        cursor_mgr: bind_opt(&globals, &qh, 1..=2, ()),
        cursor_dev: None, pointer: None, pointer_serial: 0, keyboard: None,
        panels: Vec::new(), next_id: 1, pointer_panel: None, keyboard_panel: None,
        state, text: TextSys::new(), poster: poster.clone(), shell: Some(shell),
        loop_signal: event_loop.get_signal(), exit: false, pending: Vec::new(), mods: Modifiers::default(),
    };

    lh.insert_source(rx, |ev, _, app: &mut App| {
        if let channel::Event::Msg(post) = ev {
            let mut cmds = Vec::new();
            post(&mut app.state, &mut cmds);
            app.text.set_ui_family(Some(crate::theme::current().font));
            app.pending.extend(cmds);
            app.pending.push(Cmd::RedrawAll);
            let qh = QH.with(|q| q.borrow().clone()).expect("qh");
            app.apply_cmds(&qh);
        }
    }).map_err(|e| e.to_string())?;

    // 1 Hz tick for clocks.
    lh.insert_source(Timer::from_duration(Duration::from_secs(1)), |_, _, app: &mut App| {
        for p in app.panels.iter_mut() { if p.view.ticks_seconds() { p.dirty = true; } }
        let qh = QH.with(|q| q.borrow().clone()).expect("qh");
        app.apply_cmds(&qh);
        TimeoutAction::ToDuration(Duration::from_secs(1))
    }).map_err(|e| e.to_string())?;

    WaylandSource::new(conn, event_queue).insert(lh.clone()).map_err(|e| e.to_string())?;

    app.text.set_ui_family(Some(crate::theme::current().font));
    let initial = setup(&poster, &mut app.state);
    app.pending.extend(initial);
    app.rescan_outputs();
    app.apply_cmds(&qh);

    while !app.exit {
        event_loop.dispatch(None, &mut app).map_err(|e| e.to_string())?;
    }
    Ok(())
}
