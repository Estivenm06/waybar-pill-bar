//! Headless rendering helpers for view tests: run `tick`/`draw` against a synthetic `State` and save a PNG
//! composited over a wallpaper-like backdrop so translucency is visible.
use crate::render::{Canvas, TextSys};
use crate::state::State;
use crate::wayland::{Ctx, DrawCtx, Event, Poster, View};
use tiny_skia::{Color, Paint, Pixmap, PixmapPaint, Transform};

pub struct Rig {
    pub st: State,
    pub text: TextSys,
    pub poster: Poster,
    pub scale: f32,
}

impl Rig {
    pub fn new() -> Rig { Rig { st: State::new(), text: TextSys::new(), poster: Poster::detached(), scale: 1.6 } }

    /// A rig whose poster is drained by the returned `Pump`, so worker results reach `st`.
    pub fn pumped() -> (Rig, Pump) {
        let (poster, rx) = Poster::pumped();
        let lp = calloop::EventLoop::try_new().unwrap();
        lp.handle().insert_source(rx, |ev, _, st: &mut State| {
            if let calloop::channel::Event::Msg(f) = ev { f(st, &mut Vec::new()); }
        }).unwrap();
        (Rig { poster, ..Rig::new() }, Pump { lp })
    }

    /// Advance `view` by `secs` in 16 ms steps (returns commands it issued).
    pub fn run(&mut self, view: &mut dyn View, secs: f32) -> Vec<crate::wayland::Cmd> {
        let mut cmds = Vec::new();
        let mut t = 0.0;
        while t < secs {
            let mut ctx = Ctx { state: &mut self.st, cmds: &mut cmds, text: &mut self.text, poster: &self.poster, id: 1, output: None };
            view.tick(0.016, &mut ctx);
            t += 0.016;
        }
        cmds
    }

    pub fn event(&mut self, view: &mut dyn View, ev: Event) -> Vec<crate::wayland::Cmd> {
        let mut cmds = Vec::new();
        let mut ctx = Ctx { state: &mut self.st, cmds: &mut cmds, text: &mut self.text, poster: &self.poster, id: 1, output: None };
        view.event(ev, &mut ctx);
        cmds
    }

    /// Draw one frame sized by the view's spec; returns the transparent pixmap.
    pub fn draw(&mut self, view: &mut dyn View) -> Pixmap {
        let spec = view.spec(&self.st);
        let (w, h) = ((spec.size.0 as f32 * self.scale).ceil() as u32, (spec.size.1 as f32 * self.scale).ceil() as u32);
        let mut pm = Pixmap::new(w.max(1), h.max(1)).unwrap();
        let mut cmds = Vec::new();
        {
            let mut cv = Canvas::new(&mut pm, &mut self.text, self.scale);
            let mut dc = DrawCtx { state: &self.st, cmds: &mut cmds, poster: &self.poster, id: 1, output: None };
            view.draw(&mut cv, &mut dc);
        }
        pm
    }

    /// Tick to rest, draw and save to `/tmp/<name>.png` over a backdrop with `pad` px around.
    pub fn snap(&mut self, view: &mut dyn View, name: &str) -> Pixmap {
        self.run(view, 1.0);
        let pm = self.draw(view);
        save_over_backdrop(&pm, 20, &format!("/tmp/{name}.png"));
        pm
    }
}

/// Applies the posts workers send (`Poster::post`) to a state, on the calling thread.
pub struct Pump { lp: calloop::EventLoop<'static, State> }

impl Pump {
    /// Dispatch whatever arrives within `ms` milliseconds.
    pub fn dispatch(&mut self, st: &mut State, ms: u64) { self.lp.dispatch(Some(std::time::Duration::from_millis(ms)), st).unwrap(); }

    /// Keep dispatching until `cond` holds (panics after 10 s).
    pub fn until(&mut self, st: &mut State, what: &str, cond: impl Fn(&State) -> bool) {
        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !cond(st) {
            assert!(std::time::Instant::now() < end, "timed out waiting for {what}");
            self.dispatch(st, 20);
        }
    }
}

pub fn save_over_backdrop(pm: &Pixmap, pad: u32, path: &str) {
    let mut bg = Pixmap::new(pm.width() + pad * 2, pm.height() + pad * 2).unwrap();
    let mut paint = Paint::default();
    paint.set_color(Color::from_rgba8(0x3a, 0x4a, 0x6c, 255));
    bg.fill_rect(tiny_skia::Rect::from_xywh(0.0, 0.0, bg.width() as f32, bg.height() as f32).unwrap(), &paint, Transform::identity(), None);
    paint.set_color(Color::from_rgba8(0x90, 0x60, 0x70, 255));
    bg.fill_rect(tiny_skia::Rect::from_xywh(bg.width() as f32 / 2.0, 0.0, bg.width() as f32 / 2.0, bg.height() as f32).unwrap(), &paint, Transform::identity(), None);
    bg.draw_pixmap(pad as i32, pad as i32, pm.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
    bg.save_png(path).unwrap();
}
