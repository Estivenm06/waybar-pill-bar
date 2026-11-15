//! Wi-Fi joins through NetworkManager's D-Bus API (system bus), so a typed password only ever travels inside the
//! method call that creates the profile: never in argv, the environment, a file or a command string.
//! Profiles are identified by the object paths NetworkManager returns, never by SSID-as-name.
use gtk4::gio;
use gtk4::glib::{prelude::ToVariant, variant::ObjectPath, Variant, VariantDict, VariantTy};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const NM: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const NM_DEVICE: &str = "org.freedesktop.NetworkManager.Device";
const NM_WIRELESS: &str = "org.freedesktop.NetworkManager.Device.Wireless";
const NM_AP: &str = "org.freedesktop.NetworkManager.AccessPoint";
const NM_ACTIVE: &str = "org.freedesktop.NetworkManager.Connection.Active";
const NM_SETTINGS: &str = "org.freedesktop.NetworkManager.Settings";
const NM_SETTINGS_PATH: &str = "/org/freedesktop/NetworkManager/Settings";
const NM_SETTINGS_CONN: &str = "org.freedesktop.NetworkManager.Settings.Connection";
const PROPS: &str = "org.freedesktop.DBus.Properties";

const DEVICE_TYPE_WIFI: u32 = 2;
const AP_FLAG_PRIVACY: u32 = 0x1;
const KEY_MGMT_PSK: u32 = 0x100;
const KEY_MGMT_SAE: u32 = 0x400;
const ACTIVATED: u32 = 2;
const DEACTIVATING_OR_GONE: [u32; 2] = [3, 4];

pub const JOIN_TIMEOUT: Duration = Duration::from_secs(90);
const CALL_TIMEOUT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(250);

/// One D-Bus conversation with NetworkManager; every call is bounded by the remaining overall deadline.
struct Bus<'a> {
    conn: &'a gio::DBusConnection,
    deadline: Instant,
}

impl Bus<'_> {
    fn call(&self, path: &str, iface: &str, method: &str, params: Option<&Variant>) -> Result<Variant, String> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("timed out".into());
        }
        let ms = left.min(CALL_TIMEOUT).as_millis().max(1) as i32;
        self.conn
            .call_sync(Some(NM), path, iface, method, params, None, gio::DBusCallFlags::NO_AUTO_START, ms, None::<&gio::Cancellable>)
            .map_err(|e| e.message().to_string())
    }

    fn props(&self, path: &str, iface: &str) -> Result<VariantDict, String> {
        let v = self.call(path, PROPS, "GetAll", Some(&(iface,).to_variant()))?;
        Ok(VariantDict::new(Some(&v.child_value(0))))
    }

    fn paths(&self, path: &str, iface: &str, method: &str) -> Result<Vec<String>, String> {
        let v = self.call(path, iface, method, None)?.child_value(0);
        Ok((0..v.n_children()).filter_map(|i| v.child_value(i).str().map(str::to_owned)).collect())
    }

    fn wifi_devices(&self) -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        for dev in self.paths(NM_PATH, NM, "GetDevices")? {
            if u32_of(&self.props(&dev, NM_DEVICE)?, "DeviceType") == DEVICE_TYPE_WIFI {
                out.push(dev);
            }
        }
        Ok(out)
    }
}

fn u32_of(d: &VariantDict, key: &str) -> u32 {
    d.lookup::<u32>(key).ok().flatten().unwrap_or(0)
}

fn bytes_of(d: &VariantDict, key: &str) -> Vec<u8> {
    d.lookup::<Vec<u8>>(key).ok().flatten().unwrap_or_default()
}

/// An object-path property; "/" means "none" in NetworkManager.
fn path_of(d: &VariantDict, key: &str) -> Option<String> {
    d.lookup_value(key, None).and_then(|v| v.str().map(str::to_owned)).filter(|p| p != "/")
}

fn scrub(msg: &str, secret: &str) -> String {
    if secret.is_empty() { msg.to_string() } else { msg.replace(secret, "***") }
}

fn dict(entries: Vec<(&str, Variant)>) -> Variant {
    let d = VariantDict::new(None);
    for (k, v) in &entries {
        d.insert_value(k, v);
    }
    d.end()
}

fn settings(groups: Vec<(&str, Variant)>) -> Variant {
    Variant::array_from_iter_with_type(
        VariantTy::new("{sa{sv}}").expect("static type"),
        groups.iter().map(|(k, v)| Variant::from_dict_entry(&k.to_variant(), v)),
    )
}

fn object_path(p: &str) -> Result<Variant, String> {
    ObjectPath::try_from(p).map(|o| o.to_variant()).map_err(|_| "NetworkManager returned an invalid object path".to_string())
}

#[derive(Debug, PartialEq)]
enum Security { Psk, Sae, Wep }

/// Maps the AP's advertised capabilities onto the one-password UI; anything else cannot be represented.
fn classify(flags: u32, wpa: u32, rsn: u32) -> Result<Security, String> {
    if (wpa | rsn) & KEY_MGMT_PSK != 0 {
        Ok(Security::Psk)
    } else if rsn & KEY_MGMT_SAE != 0 {
        Ok(Security::Sae)
    } else if flags & AP_FLAG_PRIVACY != 0 && wpa == 0 && rsn == 0 {
        Ok(Security::Wep)
    } else {
        Err("this network's security mode needs more than a password".into())
    }
}

struct Candidate { device: String, ap: String, strength: u8, flags: u32, wpa: u32, rsn: u32 }

/// Strongest AP with exactly this SSID across all Wi-Fi devices; object-path order breaks ties.
fn find_ap(bus: &Bus, ssid: &str) -> Result<Candidate, String> {
    let mut best: Option<Candidate> = None;
    for dev in bus.wifi_devices().map_err(|e| format!("cannot list network devices: {e}"))? {
        let aps = bus.paths(&dev, NM_WIRELESS, "GetAllAccessPoints").map_err(|e| format!("cannot list access points: {e}"))?;
        for ap in aps {
            let Ok(p) = bus.props(&ap, NM_AP) else { continue }; // it may have vanished since the listing
            if bytes_of(&p, "Ssid") != ssid.as_bytes() { continue; }
            let c = Candidate {
                device: dev.clone(), ap: ap.clone(),
                strength: p.lookup::<u8>("Strength").ok().flatten().unwrap_or(0),
                flags: u32_of(&p, "Flags"), wpa: u32_of(&p, "WpaFlags"), rsn: u32_of(&p, "RsnFlags"),
            };
            let better = match &best {
                None => true,
                Some(b) => c.strength > b.strength || (c.strength == b.strength && c.ap < b.ap),
            };
            if better { best = Some(c); }
        }
    }
    best.ok_or_else(|| "network not found (it may be out of range)".to_string())
}

fn new_profile(ssid: &str, password: &str, sec: &Security) -> Variant {
    let security = match sec {
        Security::Psk => vec![("key-mgmt", "wpa-psk".to_variant()), ("psk", password.to_variant())],
        Security::Sae => vec![("key-mgmt", "sae".to_variant()), ("psk", password.to_variant())],
        Security::Wep => vec![
            ("key-mgmt", "none".to_variant()), ("wep-key0", password.to_variant()),
            ("wep-tx-keyidx", 0u32.to_variant()), ("wep-key-type", 1u32.to_variant()),
        ],
    };
    settings(vec![
        ("connection", dict(vec![("type", "802-11-wireless".to_variant()), ("id", ssid.to_variant())])),
        ("802-11-wireless", dict(vec![("ssid", ssid.as_bytes().to_variant())])),
        ("802-11-wireless-security", dict(security)),
    ])
}

/// Creates a profile for `ssid` with `password`, activates it and waits for NetworkManager to report ACTIVATED.
/// Only the profile and active connection created here are ever cleaned up on failure.
pub fn connect_on(conn: &gio::DBusConnection, ssid: &str, password: &str, timeout: Duration) -> Result<(), String> {
    let bus = Bus { conn, deadline: Instant::now() + timeout };
    let ap = find_ap(&bus, ssid)?;
    let sec = classify(ap.flags, ap.wpa, ap.rsn)?;
    let params = Variant::tuple_from_iter([
        new_profile(ssid, password, &sec),
        object_path(&ap.device)?,
        object_path(&ap.ap)?,
        dict(vec![("persist", "disk".to_variant())]),
    ]);
    let reply = bus
        .call(NM_PATH, NM, "AddAndActivateConnection2", Some(&params))
        .map_err(|e| format!("could not start the connection: {}", scrub(&e, password)))?;
    let (Some(profile), Some(active)) =
        (reply.child_value(0).str().map(str::to_owned), reply.child_value(1).str().map(str::to_owned))
    else {
        return Err("NetworkManager returned an unexpected reply".into());
    };

    let outcome = wait_activated(&bus, &active);
    if outcome.is_err() {
        // bounded best-effort cleanup of exactly what this call created
        let cleanup = Bus { conn, deadline: Instant::now() + CALL_TIMEOUT };
        if let Ok(p) = object_path(&active) {
            let _ = cleanup.call(NM_PATH, NM, "DeactivateConnection", Some(&Variant::tuple_from_iter([p])));
        }
        let _ = cleanup.call(&profile, NM_SETTINGS_CONN, "Delete", None);
    }
    outcome
}

fn wait_activated(bus: &Bus, active: &str) -> Result<(), String> {
    loop {
        let p = bus.props(active, NM_ACTIVE).map_err(|_| "the connection attempt ended before it activated".to_string())?;
        match u32_of(&p, "State") {
            ACTIVATED => return Ok(()),
            s if DEACTIVATING_OR_GONE.contains(&s) => return Err("the connection failed (wrong password or network refused it)".into()),
            _ => {}
        }
        if Instant::now() + POLL >= bus.deadline {
            return Err("timed out waiting for the connection".into());
        }
        std::thread::sleep(POLL);
    }
}

/// Deactivates the active Wi-Fi connection(s) whose access point has exactly this SSID; nothing else.
pub fn disconnect_on(conn: &gio::DBusConnection, ssid: &str, timeout: Duration) -> Result<(), String> {
    let bus = Bus { conn, deadline: Instant::now() + timeout };
    let mut done = false;
    for dev in bus.wifi_devices().map_err(|e| format!("cannot list network devices: {e}"))? {
        let Some(ap) = path_of(&bus.props(&dev, NM_WIRELESS)?, "ActiveAccessPoint") else { continue };
        let Ok(p) = bus.props(&ap, NM_AP) else { continue };
        if bytes_of(&p, "Ssid") != ssid.as_bytes() { continue; }
        let Some(active) = path_of(&bus.props(&dev, NM_DEVICE)?, "ActiveConnection") else { continue };
        bus.call(NM_PATH, NM, "DeactivateConnection", Some(&Variant::tuple_from_iter([object_path(&active)?])))
            .map_err(|e| format!("could not disconnect: {e}"))?;
        done = true;
    }
    if done { Ok(()) } else { Err("not connected to that network".into()) }
}

/// SSIDs of the saved Wi-Fi profiles (actual SSID bytes, never profile names). Secrets are never requested.
pub fn saved_ssids_on(conn: &gio::DBusConnection, timeout: Duration) -> Result<Vec<Vec<u8>>, String> {
    let bus = Bus { conn, deadline: Instant::now() + timeout };
    let mut out = Vec::new();
    for profile in bus.paths(NM_SETTINGS_PATH, NM_SETTINGS, "ListConnections")? {
        let Ok(reply) = bus.call(&profile, NM_SETTINGS_CONN, "GetSettings", None) else { continue };
        let all = reply.child_value(0);
        let (mut kind, mut ssid) = (None::<String>, None::<Vec<u8>>);
        for i in 0..all.n_children() {
            let entry = all.child_value(i);
            let group = VariantDict::new(Some(&entry.child_value(1)));
            match entry.child_value(0).str() {
                Some("connection") => kind = group.lookup::<String>("type").ok().flatten(),
                Some("802-11-wireless") => ssid = Some(bytes_of(&group, "ssid")),
                _ => {}
            }
        }
        if kind.as_deref() == Some("802-11-wireless") {
            out.extend(ssid.filter(|s| !s.is_empty()));
        }
    }
    Ok(out)
}

fn system_bus() -> Result<gio::DBusConnection, String> {
    gio::bus_get_sync(gio::BusType::System, None::<&gio::Cancellable>).map_err(|e| format!("cannot reach the system bus: {}", e.message()))
}

pub fn saved_ssids() -> Result<Vec<Vec<u8>>, String> {
    saved_ssids_on(&system_bus()?, CALL_TIMEOUT)
}

pub fn disconnect(ssid: &str) -> Result<(), String> {
    disconnect_on(&system_bus()?, ssid, CALL_TIMEOUT * 2)
}

/// Joins a saved or open network without any password: NetworkManager picks a compatible profile itself.
pub fn connect_saved(ssid: &str) -> Result<(), String> {
    let ok = Command::new("nmcli").args(["device", "wifi", "connect"]).arg(ssid)
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
        .status().map(|s| s.success()).unwrap_or(false);
    if ok { Ok(()) } else { Err("could not connect".into()) }
}

/// Joins a new secured network with a typed password. A profile for this SSID that appeared since the list was drawn
/// is activated through NetworkManager instead of creating (or later deleting) anything.
pub fn connect_with_password(ssid: &str, password: &str) -> Result<(), String> {
    let conn = system_bus()?;
    if saved_ssids_on(&conn, CALL_TIMEOUT).is_ok_and(|s| s.iter().any(|x| x == ssid.as_bytes())) {
        return connect_saved(ssid);
    }
    connect_on(&conn, ssid, password, JOIN_TIMEOUT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn security_classification() {
        assert_eq!(classify(1, 0, 0x188), Ok(Security::Psk)); // RSN PSK (+ others)
        assert_eq!(classify(1, 0x188, 0), Ok(Security::Psk)); // WPA PSK
        assert_eq!(classify(1, 0, 0x400), Ok(Security::Sae)); // SAE only
        assert_eq!(classify(1, 0, 0x100 | 0x400), Ok(Security::Psk)); // transition mode uses PSK
        assert_eq!(classify(1, 0, 0), Ok(Security::Wep));
        assert!(classify(1, 0, 0x200).is_err()); // 802.1X
        assert!(classify(0, 0, 0).is_err()); // open: no password to give
    }

    #[test]
    fn errors_never_contain_the_password() {
        assert_eq!(scrub("bad psk hunter2 here", "hunter2"), "bad psk *** here");
        assert_eq!(scrub("x", ""), "x");
    }

    // ---- fake NetworkManager on a private dbus-daemon (never the real system bus) ----
    use gtk4::glib;
    use std::collections::{BTreeMap, HashMap};
    use std::io::{BufRead, BufReader};
    use std::sync::{atomic::{AtomicBool, Ordering}, Arc, Mutex};

    const NM_XML: &str = r#"<node>
      <interface name="org.freedesktop.NetworkManager">
        <method name="GetDevices"><arg name="d" type="ao" direction="out"/></method>
        <method name="AddAndActivateConnection2">
          <arg type="a{sa{sv}}" direction="in"/><arg type="o" direction="in"/><arg type="o" direction="in"/><arg type="a{sv}" direction="in"/>
          <arg type="o" direction="out"/><arg type="o" direction="out"/><arg type="a{sv}" direction="out"/>
        </method>
        <method name="DeactivateConnection"><arg type="o" direction="in"/></method>
      </interface>
      <interface name="org.freedesktop.NetworkManager.Device">
        <property name="DeviceType" type="u" access="read"/><property name="ActiveConnection" type="o" access="read"/>
      </interface>
      <interface name="org.freedesktop.NetworkManager.Device.Wireless">
        <method name="GetAllAccessPoints"><arg type="ao" direction="out"/></method>
        <property name="ActiveAccessPoint" type="o" access="read"/>
      </interface>
      <interface name="org.freedesktop.NetworkManager.AccessPoint">
        <property name="Flags" type="u" access="read"/><property name="WpaFlags" type="u" access="read"/>
        <property name="RsnFlags" type="u" access="read"/><property name="Ssid" type="ay" access="read"/>
        <property name="Strength" type="y" access="read"/>
      </interface>
      <interface name="org.freedesktop.NetworkManager.Connection.Active">
        <property name="State" type="u" access="read"/><property name="Connection" type="o" access="read"/>
      </interface>
      <interface name="org.freedesktop.NetworkManager.Settings">
        <method name="ListConnections"><arg type="ao" direction="out"/></method>
      </interface>
      <interface name="org.freedesktop.NetworkManager.Settings.Connection">
        <method name="GetSettings"><arg type="a{sa{sv}}" direction="out"/></method>
        <method name="Delete"/>
      </interface>
    </node>"#;

    const DEV_WIRED: &str = "/org/freedesktop/NetworkManager/Devices/1";
    const DEV_WIFI: &str = "/org/freedesktop/NetworkManager/Devices/2";
    const PROF: &str = "/org/freedesktop/NetworkManager/Settings/";
    const ACT: &str = "/org/freedesktop/NetworkManager/ActiveConnection/";

    #[derive(Clone)]
    struct ApDef { path: String, ssid: Vec<u8>, strength: u8, flags: u32, wpa: u32, rsn: u32 }
    #[derive(Clone, PartialEq, Debug)]
    struct ProfDef { kind: String, id: String, ssid: Option<Vec<u8>> }
    struct Added { settings: BTreeMap<String, BTreeMap<String, String>>, device: String, ap: String, options: BTreeMap<String, String> }

    #[derive(Default)]
    struct World {
        aps: Vec<ApDef>,
        profiles: BTreeMap<String, ProfDef>,
        states: Vec<u32>,
        polls: usize,
        added: Vec<Added>,
        deleted: Vec<String>,
        deactivated: Vec<String>,
        add_error: bool,
        wifi_active_ap: Option<String>,
        wifi_active_conn: Option<String>,
        actives: HashMap<String, String>,
        regs: HashMap<String, gio::RegistrationId>,
        seq: u32,
    }
    type Shared = Arc<Mutex<World>>;

    fn ap(n: u32, ssid: &str, strength: u8, flags: u32, wpa: u32, rsn: u32) -> ApDef {
        ApDef { path: format!("/org/freedesktop/NetworkManager/AccessPoint/{n}"), ssid: ssid.as_bytes().to_vec(), strength, flags, wpa, rsn }
    }

    fn op(p: &str) -> Variant { object_path(p).unwrap() }
    fn ao(paths: &[String]) -> Variant {
        Variant::tuple_from_iter([Variant::array_from_iter_with_type(VariantTy::OBJECT_PATH, paths.iter().map(|p| op(p)))])
    }

    fn repr(v: &Variant) -> String {
        match v.type_().as_str() {
            "s" => v.str().unwrap().to_string(),
            "u" => format!("u32:{}", v.get::<u32>().unwrap()),
            "ay" => format!("bytes:{}", String::from_utf8_lossy(&v.get::<Vec<u8>>().unwrap())),
            _ => v.print(false).to_string(),
        }
    }

    fn register(conn: &gio::DBusConnection, node: &gio::DBusNodeInfo, w: &Shared, path: &str, iface: &str) {
        let info = node.lookup_interface(iface).unwrap();
        let (w1, w2, n) = (w.clone(), w.clone(), node.clone());
        let id = conn.register_object(path, &info)
            .method_call(move |c, _s, p, i, m, params, inv| on_method(&c, &n, &w1, p, i.unwrap_or(""), m, &params, inv))
            .property(move |_c, _s, p, i, prop| on_prop(&w2, p, i, prop))
            .build().unwrap();
        w.lock().unwrap().regs.insert(format!("{path}|{iface}"), id);
    }

    fn on_prop(w: &Shared, path: &str, iface: &str, prop: &str) -> Variant {
        let mut w = w.lock().unwrap();
        let none = || op("/");
        match (iface, prop) {
            (NM_DEVICE, "DeviceType") => (if path == DEV_WIFI { 2u32 } else { 1u32 }).to_variant(),
            (NM_DEVICE, "ActiveConnection") => w.wifi_active_conn.as_deref().filter(|_| path == DEV_WIFI).map(op).unwrap_or_else(none),
            (NM_WIRELESS, "ActiveAccessPoint") => w.wifi_active_ap.as_deref().map(op).unwrap_or_else(none),
            (NM_AP, _) => {
                let a = w.aps.iter().find(|a| a.path == path).cloned().expect("known AP");
                match prop {
                    "Flags" => a.flags.to_variant(),
                    "WpaFlags" => a.wpa.to_variant(),
                    "RsnFlags" => a.rsn.to_variant(),
                    "Ssid" => a.ssid.to_variant(),
                    _ => a.strength.to_variant(),
                }
            }
            (NM_ACTIVE, "State") => {
                let i = w.polls.min(w.states.len().saturating_sub(1));
                w.polls += 1;
                w.states.get(i).copied().unwrap_or(1).to_variant()
            }
            (NM_ACTIVE, "Connection") => w.actives.get(path).map(|p| op(p)).unwrap_or_else(none),
            _ => panic!("unexpected property {iface}.{prop}"),
        }
    }

    fn profile_settings(p: &ProfDef) -> Variant {
        let mut groups = vec![("connection", dict(vec![("type", p.kind.to_variant()), ("id", p.id.to_variant())]))];
        if let Some(s) = &p.ssid { groups.push(("802-11-wireless", dict(vec![("ssid", s.to_variant())]))); }
        Variant::tuple_from_iter([settings(groups)])
    }

    fn on_method(conn: &gio::DBusConnection, node: &gio::DBusNodeInfo, w: &Shared, path: &str, iface: &str, method: &str,
                 params: &Variant, inv: gio::DBusMethodInvocation) {
        let unit = Variant::tuple_from_iter(Vec::<Variant>::new());
        match (iface, method) {
            (NM, "GetDevices") => inv.return_value(Some(&ao(&[DEV_WIRED.into(), DEV_WIFI.into()]))),
            (NM_WIRELESS, "GetAllAccessPoints") => {
                let paths: Vec<String> = w.lock().unwrap().aps.iter().map(|a| a.path.clone()).collect();
                inv.return_value(Some(&ao(&paths)));
            }
            (NM_SETTINGS, "ListConnections") => {
                let paths: Vec<String> = w.lock().unwrap().profiles.keys().cloned().collect();
                inv.return_value(Some(&ao(&paths)));
            }
            (NM_SETTINGS_CONN, "GetSettings") => {
                let p = w.lock().unwrap().profiles.get(path).cloned();
                match p {
                    Some(p) => inv.return_value(Some(&profile_settings(&p))),
                    None => inv.return_dbus_error("org.freedesktop.NetworkManager.Settings.InvalidConnection", "gone"),
                }
            }
            (NM_SETTINGS_CONN, "Delete") => {
                let mut g = w.lock().unwrap();
                g.profiles.remove(path);
                g.deleted.push(path.to_string());
                drop(g);
                inv.return_value(Some(&unit));
            }
            (NM, "DeactivateConnection") => {
                let active = params.child_value(0).str().unwrap().to_string();
                let id = {
                    let mut g = w.lock().unwrap();
                    g.deactivated.push(active.clone());
                    g.regs.remove(&format!("{active}|{NM_ACTIVE}"))
                };
                if let Some(id) = id { let _ = conn.unregister_object(id); }
                inv.return_value(Some(&unit));
            }
            (NM, "AddAndActivateConnection2") => {
                if w.lock().unwrap().add_error {
                    return inv.return_dbus_error("org.freedesktop.NetworkManager.Settings.InvalidProperty", "802-11-wireless-security.psk: property is invalid");
                }
                let mut shown: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
                let all = params.child_value(0);
                let (mut id, mut ssid) = (String::new(), None);
                for i in 0..all.n_children() {
                    let e = all.child_value(i);
                    let (group, body) = (e.child_value(0).str().unwrap().to_string(), e.child_value(1));
                    let g = shown.entry(group.clone()).or_default();
                    for j in 0..body.n_children() {
                        let kv = body.child_value(j);
                        let (k, v) = (kv.child_value(0).str().unwrap().to_string(), kv.child_value(1).as_variant().unwrap());
                        if group == "connection" && k == "id" { id = repr(&v); }
                        if group == "802-11-wireless" && k == "ssid" { ssid = v.get::<Vec<u8>>(); }
                        g.insert(k, repr(&v));
                    }
                }
                let mut options = BTreeMap::new();
                let o = params.child_value(3);
                for j in 0..o.n_children() {
                    let kv = o.child_value(j);
                    options.insert(kv.child_value(0).str().unwrap().to_string(), repr(&kv.child_value(1).as_variant().unwrap()));
                }
                let (profile, active) = {
                    let mut g = w.lock().unwrap();
                    g.seq += 1;
                    let (profile, active) = (format!("{PROF}{}", 100 + g.seq), format!("{ACT}{}", g.seq));
                    g.profiles.insert(profile.clone(), ProfDef { kind: "802-11-wireless".into(), id, ssid });
                    g.actives.insert(active.clone(), profile.clone());
                    g.polls = 0;
                    g.added.push(Added {
                        settings: shown, device: params.child_value(1).str().unwrap().into(),
                        ap: params.child_value(2).str().unwrap().into(), options,
                    });
                    (profile, active)
                };
                register(conn, node, w, &profile, NM_SETTINGS_CONN);
                register(conn, node, w, &active, NM_ACTIVE);
                inv.return_value(Some(&Variant::tuple_from_iter([op(&profile), op(&active), dict(vec![])])));
            }
            _ => inv.return_dbus_error("org.freedesktop.DBus.Error.UnknownMethod", method),
        }
    }

    struct Daemon { child: std::process::Child, addr: String }
    impl Daemon {
        fn start() -> Daemon {
            // absolute path: other tests in this binary change PATH
            let bin = ["/usr/bin/dbus-daemon", "/bin/dbus-daemon", "/usr/local/bin/dbus-daemon"].into_iter()
                .find(|p| std::path::Path::new(p).exists()).unwrap_or("dbus-daemon");
            let mut child = std::process::Command::new(bin).args(["--session", "--nofork", "--print-address=1"])
                .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).spawn().expect("dbus-daemon");
            let mut line = String::new();
            BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
            Daemon { child, addr: line.trim().to_string() }
        }
        fn connect(&self) -> gio::DBusConnection {
            gio::DBusConnection::for_address_sync(
                &self.addr, gio::DBusConnectionFlags::AUTHENTICATION_CLIENT | gio::DBusConnectionFlags::MESSAGE_BUS_CONNECTION,
                None, None::<&gio::Cancellable>).unwrap()
        }
    }
    impl Drop for Daemon {
        fn drop(&mut self) { let _ = self.child.kill(); let _ = self.child.wait(); }
    }

    struct Env { _daemon: Daemon, world: Shared, client: gio::DBusConnection, stop: Arc<AtomicBool>, server: Option<std::thread::JoinHandle<()>>, remove_ap: Arc<Mutex<Vec<String>>> }
    impl Drop for Env {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(t) = self.server.take() { let _ = t.join(); }
        }
    }

    /// `with_nm = false` leaves the bus without a NetworkManager.
    fn env(world: World, with_nm: bool) -> Env {
        let daemon = Daemon::start();
        let world: Shared = Arc::new(Mutex::new(world));
        let stop = Arc::new(AtomicBool::new(false));
        let remove_ap = Arc::new(Mutex::new(Vec::<String>::new()));
        let mut server = None;
        if with_nm {
            let (addr, w, st, rm) = (daemon.addr.clone(), world.clone(), stop.clone(), remove_ap.clone());
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            server = Some(std::thread::spawn(move || {
                let ctx = glib::MainContext::new();
                ctx.with_thread_default(|| {
                    let conn = gio::DBusConnection::for_address_sync(
                        &addr, gio::DBusConnectionFlags::AUTHENTICATION_CLIENT | gio::DBusConnectionFlags::MESSAGE_BUS_CONNECTION,
                        None, None::<&gio::Cancellable>).unwrap();
                    let node = gio::DBusNodeInfo::for_xml(NM_XML).unwrap();
                    register(&conn, &node, &w, NM_PATH, NM);
                    register(&conn, &node, &w, NM_SETTINGS_PATH, NM_SETTINGS);
                    register(&conn, &node, &w, DEV_WIRED, NM_DEVICE);
                    register(&conn, &node, &w, DEV_WIFI, NM_DEVICE);
                    register(&conn, &node, &w, DEV_WIFI, NM_WIRELESS);
                    let (aps, profs, actives): (Vec<String>, Vec<String>, Vec<String>) = {
                        let g = w.lock().unwrap();
                        (g.aps.iter().map(|a| a.path.clone()).collect(), g.profiles.keys().cloned().collect(), g.wifi_active_conn.iter().cloned().collect())
                    };
                    for p in aps { register(&conn, &node, &w, &p, NM_AP); }
                    for p in profs { register(&conn, &node, &w, &p, NM_SETTINGS_CONN); }
                    for p in actives { register(&conn, &node, &w, &p, NM_ACTIVE); }
                    conn.call_sync(Some("org.freedesktop.DBus"), "/org/freedesktop/DBus", "org.freedesktop.DBus", "RequestName",
                        Some(&(NM, 0u32).to_variant()), None, gio::DBusCallFlags::NONE, 5000, None::<&gio::Cancellable>).unwrap();
                    ready_tx.send(()).unwrap();
                    while !st.load(Ordering::SeqCst) {
                        for p in rm.lock().unwrap().drain(..) {
                            if let Some(id) = w.lock().unwrap().regs.remove(&format!("{p}|{NM_AP}")) { let _ = conn.unregister_object(id); }
                        }
                        while ctx.iteration(false) {}
                        std::thread::sleep(Duration::from_millis(2));
                    }
                }).unwrap();
            }));
            ready_rx.recv_timeout(Duration::from_secs(10)).expect("fake NetworkManager ready");
        }
        let client = daemon.connect();
        Env { _daemon: daemon, world, client, stop, server, remove_ap }
    }

    fn wifi_world(aps: Vec<ApDef>, states: Vec<u32>) -> World {
        let mut w = World { aps, states, ..World::default() };
        // a wired profile and two Wi-Fi profiles whose display names collide with other things
        w.profiles.insert(format!("{PROF}1"), ProfDef { kind: "802-3-ethernet".into(), id: "Wired connection 1".into(), ssid: None });
        w.profiles.insert(format!("{PROF}2"), ProfDef { kind: "802-11-wireless".into(), id: "Renamed by user".into(), ssid: Some(b"HomeNet".to_vec()) });
        w.profiles.insert(format!("{PROF}3"), ProfDef { kind: "802-11-wireless".into(), id: "Dup".into(), ssid: Some("Café".as_bytes().to_vec()) });
        w.profiles.insert(format!("{PROF}4"), ProfDef { kind: "802-11-wireless".into(), id: "Dup".into(), ssid: Some(b"Other".to_vec()) });
        w
    }

    const T: Duration = Duration::from_secs(5);

    #[test]
    fn wpa_psk_join_waits_for_activation_and_sends_the_password_only_as_psk() {
        let pw = "pa ss\"w'$(id)\\ünï";
        let e = env(wifi_world(vec![ap(1, "Wired connection 1", 60, 1, 0, 0x188)], vec![1, 1, 2]), true);
        connect_on(&e.client, "Wired connection 1", pw, T).unwrap();
        let w = e.world.lock().unwrap();
        assert_eq!(w.added.len(), 1);
        let a = &w.added[0];
        assert!(w.polls >= 3, "success must wait for State=2, polled {}", w.polls);
        assert_eq!(a.settings["802-11-wireless-security"]["key-mgmt"], "wpa-psk");
        assert_eq!(a.settings["802-11-wireless-security"]["psk"], pw);
        assert_eq!(a.settings["802-11-wireless"]["ssid"], "bytes:Wired connection 1");
        assert_eq!(a.options["persist"], "disk");
        assert_eq!((a.device.as_str(), a.ap.as_str()), (DEV_WIFI, "/org/freedesktop/NetworkManager/AccessPoint/1"));
        let leaks: Vec<_> = a.settings.iter().flat_map(|(g, m)| m.iter().map(move |(k, v)| (g, k, v)))
            .filter(|(g, k, v)| v.contains(pw) && !(**g == "802-11-wireless-security" && **k == "psk")).collect();
        assert!(leaks.is_empty(), "{leaks:?}");
        assert!(w.deleted.is_empty() && w.deactivated.is_empty());
    }

    #[test]
    fn sae_only_and_wep_profiles() {
        let e = env(wifi_world(vec![ap(1, "Sae", 50, 1, 0, 0x400), ap(2, "Wep", 50, 1, 0, 0)], vec![2]), true);
        connect_on(&e.client, "Sae", "secret", T).unwrap();
        connect_on(&e.client, "Wep", "abcde", T).unwrap();
        let w = e.world.lock().unwrap();
        let s = &w.added[0].settings["802-11-wireless-security"];
        assert_eq!((s["key-mgmt"].as_str(), s["psk"].as_str()), ("sae", "secret"));
        let s = &w.added[1].settings["802-11-wireless-security"];
        assert_eq!(s["key-mgmt"], "none");
        assert_eq!(s["wep-key0"], "abcde");
        assert_eq!((s["wep-tx-keyidx"].as_str(), s["wep-key-type"].as_str()), ("u32:0", "u32:1"));
    }

    #[test]
    fn denial_removes_only_the_new_profile_and_never_a_same_named_wired_one() {
        let e = env(wifi_world(vec![ap(1, "Wired connection 1", 60, 1, 0, 0x188)], vec![1, 3]), true);
        let before = e.world.lock().unwrap().profiles.clone();
        let err = connect_on(&e.client, "Wired connection 1", "badpassword", T).unwrap_err();
        assert!(!err.contains("badpassword"), "{err}");
        let w = e.world.lock().unwrap();
        assert_eq!(w.deleted, vec![format!("{PROF}101")]);
        assert_eq!(w.deactivated, vec![format!("{ACT}1")]);
        assert_eq!(w.profiles, before, "existing profiles (wired, renamed, duplicate names) stay untouched");
    }

    #[test]
    fn stuck_activation_times_out_and_cleans_up_its_own_objects() {
        let e = env(wifi_world(vec![ap(1, "Slow", 60, 1, 0, 0x188)], vec![1]), true);
        let start = std::time::Instant::now();
        let err = connect_on(&e.client, "Slow", "password1", Duration::from_millis(1500)).unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        assert!(start.elapsed() < Duration::from_secs(6));
        let w = e.world.lock().unwrap();
        assert_eq!(w.deleted, vec![format!("{PROF}101")]);
    }

    #[test]
    fn vanished_unsupported_or_failing_networks_change_nothing() {
        let e = env(wifi_world(vec![ap(1, "Enterprise", 60, 1, 0, 0x200), ap(2, "Gone", 60, 1, 0, 0x188)], vec![2]), true);
        assert!(connect_on(&e.client, "Nowhere", "password1", T).unwrap_err().contains("not found"));
        assert!(connect_on(&e.client, "Enterprise", "password1", T).unwrap_err().contains("more than a password"));
        e.remove_ap.lock().unwrap().push("/org/freedesktop/NetworkManager/AccessPoint/2".into()); // listed, object vanished
        std::thread::sleep(Duration::from_millis(100));
        assert!(connect_on(&e.client, "Gone", "password1", T).unwrap_err().contains("not found"));
        let w = e.world.lock().unwrap();
        assert!(w.added.is_empty() && w.deleted.is_empty() && w.deactivated.is_empty());
    }

    #[test]
    fn method_failure_before_paths_exist_deletes_nothing() {
        let mut w = wifi_world(vec![ap(1, "Home", 60, 1, 0, 0x188)], vec![2]);
        w.add_error = true;
        let e = env(w, true);
        let before = e.world.lock().unwrap().profiles.clone();
        let err = connect_on(&e.client, "Home", "hunter22", T).unwrap_err();
        assert!(err.contains("could not start") && !err.contains("hunter22"), "{err}");
        let w = e.world.lock().unwrap();
        assert!(w.deleted.is_empty() && w.deactivated.is_empty());
        assert_eq!(w.profiles, before);
    }

    #[test]
    fn strongest_matching_ap_wins_and_ties_use_path_order() {
        let e = env(wifi_world(vec![ap(1, "Mesh", 40, 1, 0, 0x188), ap(3, "Mesh", 80, 1, 0, 0x188), ap(2, "Mesh", 80, 1, 0, 0x188)], vec![2]), true);
        connect_on(&e.client, "Mesh", "password1", T).unwrap();
        assert_eq!(e.world.lock().unwrap().added[0].ap, "/org/freedesktop/NetworkManager/AccessPoint/2");
    }

    #[test]
    fn missing_network_manager_is_a_clean_error() {
        let e = env(World::default(), false);
        let err = connect_on(&e.client, "Home", "password1", Duration::from_secs(2)).unwrap_err();
        assert!(err.contains("cannot list network devices") && !err.contains("password1"), "{err}");
    }

    #[test]
    fn saved_ssids_come_from_ssid_bytes_not_profile_names() {
        let e = env(wifi_world(vec![], vec![2]), true);
        let mut got = saved_ssids_on(&e.client, T).unwrap();
        got.sort();
        let mut want = vec![b"HomeNet".to_vec(), "Café".as_bytes().to_vec(), b"Other".to_vec()];
        want.sort();
        assert_eq!(got, want); // the wired profile and display names are not SSIDs
    }

    #[test]
    fn disconnect_targets_only_the_active_connection_of_that_ssid() {
        let mut w = wifi_world(vec![ap(1, "HomeNet", 60, 1, 0, 0x188)], vec![2]);
        w.wifi_active_ap = Some("/org/freedesktop/NetworkManager/AccessPoint/1".into());
        w.wifi_active_conn = Some(format!("{ACT}90"));
        let e = env(w, true);
        assert!(disconnect_on(&e.client, "Renamed by user", T).is_err());
        assert!(disconnect_on(&e.client, "Wired connection 1", T).is_err());
        assert!(e.world.lock().unwrap().deactivated.is_empty());
        disconnect_on(&e.client, "HomeNet", T).unwrap();
        assert_eq!(e.world.lock().unwrap().deactivated, vec![format!("{ACT}90")]);
    }
}
