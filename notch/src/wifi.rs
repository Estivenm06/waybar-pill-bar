//! Wi-Fi joins through NetworkManager's D-Bus API (system bus), so a typed password only ever travels inside the
//! method call that creates the profile: never in argv, the environment, a file or a command string.
//! Profiles are identified by the object paths NetworkManager returns, never by SSID-as-name.
//! Every call is synchronous (`zbus::block_on`) and meant for worker threads.
use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use zbus::wire::{DynamicType, ObjectPath, OwnedObjectPath, OwnedVariant, Variant};
use zbus::{block_on, Connection};

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

type Props = HashMap<String, OwnedVariant>;
type Profile = HashMap<String, HashMap<String, Variant<'static>>>;

/// One D-Bus conversation with NetworkManager; no call starts after the overall deadline, and each call is bounded
/// by the connection's method timeout.
struct Bus<'a> {
    conn: &'a Connection,
    deadline: Instant,
}

impl Bus<'_> {
    fn call<B: zbus::export::serde::Serialize + DynamicType>(&self, path: &str, iface: &str, method: &str, body: &B) -> Result<zbus::Message, String> {
        if self.deadline.saturating_duration_since(Instant::now()).is_zero() {
            return Err("timed out".into());
        }
        block_on(self.conn.call_method(Some(NM), path, Some(iface), method, body)).map_err(|e| match e {
            zbus::Error::MethodError(_, Some(desc), _) => desc,
            e => e.to_string(),
        })
    }

    fn props(&self, path: &str, iface: &str) -> Result<Props, String> {
        let m = self.call(path, PROPS, "GetAll", &(iface,))?;
        m.body().deserialize::<Props>().map_err(|e| e.to_string())
    }

    fn paths(&self, path: &str, iface: &str, method: &str) -> Result<Vec<String>, String> {
        let m = self.call(path, iface, method, &())?;
        let v = m.body().deserialize::<Vec<OwnedObjectPath>>().map_err(|e| e.to_string())?;
        Ok(v.iter().map(|p| p.as_str().to_owned()).collect())
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

fn u32_of(d: &Props, key: &str) -> u32 {
    d.get(key).and_then(|v| v.downcast_ref::<u32>().ok()).unwrap_or(0)
}

fn u8_of(d: &Props, key: &str) -> u8 {
    d.get(key).and_then(|v| v.downcast_ref::<u8>().ok()).unwrap_or(0)
}

fn bytes_of(d: &Props, key: &str) -> Vec<u8> {
    d.get(key).and_then(|v| (**v).try_clone().ok()).and_then(|v| v.downcast::<Vec<u8>>().ok()).unwrap_or_default()
}

fn string_of(d: &Props, key: &str) -> Option<String> {
    d.get(key).and_then(|v| v.downcast_ref::<&str>().ok()).map(str::to_owned)
}

/// An object-path property; "/" means "none" in NetworkManager.
fn path_of(d: &Props, key: &str) -> Option<String> {
    d.get(key).and_then(|v| v.downcast_ref::<ObjectPath>().ok()).map(|p| p.as_str().to_owned()).filter(|p| p != "/")
}

fn scrub(msg: &str, secret: &str) -> String {
    if secret.is_empty() { msg.to_string() } else { msg.replace(secret, "***") }
}

fn dict(entries: Vec<(&str, Variant<'static>)>) -> HashMap<String, Variant<'static>> {
    entries.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

fn object_path(p: &str) -> Result<ObjectPath<'static>, String> {
    ObjectPath::try_from(p.to_string()).map_err(|_| "NetworkManager returned an invalid object path".to_string())
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
                device: dev.clone(), ap: ap.clone(), strength: u8_of(&p, "Strength"),
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

fn new_profile(ssid: &str, password: &str, sec: &Security) -> Profile {
    let s = |v: &str| Variant::from(v.to_string());
    let security = match sec {
        Security::Psk => vec![("key-mgmt", s("wpa-psk")), ("psk", s(password))],
        Security::Sae => vec![("key-mgmt", s("sae")), ("psk", s(password))],
        Security::Wep => vec![
            ("key-mgmt", s("none")), ("wep-key0", s(password)),
            ("wep-tx-keyidx", Variant::from(0u32)), ("wep-key-type", Variant::from(1u32)),
        ],
    };
    HashMap::from([
        ("connection".to_string(), dict(vec![("type", s("802-11-wireless")), ("id", s(ssid))])),
        ("802-11-wireless".to_string(), dict(vec![("ssid", Variant::new(ssid.as_bytes().to_vec()))])),
        ("802-11-wireless-security".to_string(), dict(security)),
    ])
}

/// Creates a profile for `ssid` with `password`, activates it and waits for NetworkManager to report ACTIVATED.
/// Only the profile and active connection created here are ever cleaned up on failure.
pub fn connect_on(conn: &Connection, ssid: &str, password: &str, timeout: Duration) -> Result<(), String> {
    let bus = Bus { conn, deadline: Instant::now() + timeout };
    let ap = find_ap(&bus, ssid)?;
    let sec = classify(ap.flags, ap.wpa, ap.rsn)?;
    let body = (new_profile(ssid, password, &sec), object_path(&ap.device)?, object_path(&ap.ap)?, dict(vec![("persist", Variant::from("disk".to_string()))]));
    let reply = bus
        .call(NM_PATH, NM, "AddAndActivateConnection2", &body)
        .map_err(|e| format!("could not start the connection: {}", scrub(&e, password)))?;
    let (profile, active, _): (OwnedObjectPath, OwnedObjectPath, Props) =
        reply.body().deserialize().map_err(|_| "NetworkManager returned an unexpected reply".to_string())?;
    let (profile, active) = (profile.as_str().to_owned(), active.as_str().to_owned());

    let outcome = wait_activated(&bus, &active);
    if outcome.is_err() {
        // bounded best-effort cleanup of exactly what this call created
        let cleanup = Bus { conn, deadline: Instant::now() + CALL_TIMEOUT };
        if let Ok(p) = object_path(&active) {
            let _ = cleanup.call(NM_PATH, NM, "DeactivateConnection", &(p,));
        }
        let _ = cleanup.call(&profile, NM_SETTINGS_CONN, "Delete", &());
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
pub fn disconnect_on(conn: &Connection, ssid: &str, timeout: Duration) -> Result<(), String> {
    let bus = Bus { conn, deadline: Instant::now() + timeout };
    let mut done = false;
    for dev in bus.wifi_devices().map_err(|e| format!("cannot list network devices: {e}"))? {
        let Some(ap) = path_of(&bus.props(&dev, NM_WIRELESS)?, "ActiveAccessPoint") else { continue };
        let Ok(p) = bus.props(&ap, NM_AP) else { continue };
        if bytes_of(&p, "Ssid") != ssid.as_bytes() { continue; }
        let Some(active) = path_of(&bus.props(&dev, NM_DEVICE)?, "ActiveConnection") else { continue };
        bus.call(NM_PATH, NM, "DeactivateConnection", &(object_path(&active)?,))
            .map_err(|e| format!("could not disconnect: {e}"))?;
        done = true;
    }
    if done { Ok(()) } else { Err("not connected to that network".into()) }
}

/// SSIDs of the saved Wi-Fi profiles (actual SSID bytes, never profile names). Secrets are never requested.
pub fn saved_ssids_on(conn: &Connection, timeout: Duration) -> Result<Vec<Vec<u8>>, String> {
    let bus = Bus { conn, deadline: Instant::now() + timeout };
    let mut out = Vec::new();
    for profile in bus.paths(NM_SETTINGS_PATH, NM_SETTINGS, "ListConnections")? {
        let Ok(reply) = bus.call(&profile, NM_SETTINGS_CONN, "GetSettings", &()) else { continue };
        let Ok(all) = reply.body().deserialize::<HashMap<String, Props>>() else { continue };
        let kind = all.get("connection").and_then(|g| string_of(g, "type"));
        let ssid = all.get("802-11-wireless").map(|g| bytes_of(g, "ssid"));
        if kind.as_deref() == Some("802-11-wireless") {
            out.extend(ssid.filter(|s| !s.is_empty()));
        }
    }
    Ok(out)
}

fn system_bus() -> Result<Connection, String> {
    block_on(async {
        zbus::connection::Builder::system().method_timeout(CALL_TIMEOUT).build().await
    }).map_err(|e| format!("cannot reach the system bus: {e}"))
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
    use std::collections::{BTreeMap, HashMap};
    use std::io::{BufRead, BufReader};
    use std::sync::{Arc, Mutex};
    use zbus::fdo;
    use zbus::{interface, ObjectServer};

    const DEV_WIRED: &str = "/org/freedesktop/NetworkManager/Devices/1";
    const DEV_WIFI: &str = "/org/freedesktop/NetworkManager/Devices/2";
    const PROF: &str = "/org/freedesktop/NetworkManager/Settings/";
    const ACT: &str = "/org/freedesktop/NetworkManager/ActiveConnection/";
    const AP_BASE: &str = "/org/freedesktop/NetworkManager/AccessPoint/";

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
        seq: u32,
    }
    type Shared = Arc<Mutex<World>>;

    fn ap(n: u32, ssid: &str, strength: u8, flags: u32, wpa: u32, rsn: u32) -> ApDef {
        ApDef { path: format!("{AP_BASE}{n}"), ssid: ssid.as_bytes().to_vec(), strength, flags, wpa, rsn }
    }

    fn op(p: &str) -> OwnedObjectPath { OwnedObjectPath::try_from(p).unwrap() }
    fn none() -> OwnedObjectPath { op("/") }
    fn ops(paths: Vec<String>) -> Vec<OwnedObjectPath> { paths.iter().map(|p| op(p)).collect() }

    fn repr(v: &OwnedVariant) -> String {
        match v.value_signature().to_string().as_str() {
            "s" => v.downcast_ref::<&str>().unwrap().to_string(),
            "u" => format!("u32:{}", v.downcast_ref::<u32>().unwrap()),
            "ay" => format!("bytes:{}", String::from_utf8_lossy(&(**v).try_clone().unwrap().downcast::<Vec<u8>>().unwrap())),
            _ => v.to_string(),
        }
    }

    struct Root(Shared);
    #[interface(name = "org.freedesktop.NetworkManager")]
    impl Root {
        #[zbus(name = "GetDevices")]
        fn get_devices(&self) -> Vec<OwnedObjectPath> { ops(vec![DEV_WIRED.into(), DEV_WIFI.into()]) }

        #[zbus(name = "DeactivateConnection")]
        async fn deactivate_connection(&self, active: OwnedObjectPath, #[zbus(object_server)] server: &ObjectServer) {
            self.0.lock().unwrap().deactivated.push(active.as_str().to_string());
            let _ = server.remove::<Active, _>(active.as_str()).await;
        }

        #[zbus(name = "AddAndActivateConnection2")]
        async fn add_and_activate_connection2(
            &self, conn: HashMap<String, HashMap<String, OwnedVariant>>, device: OwnedObjectPath, ap: OwnedObjectPath,
            options: HashMap<String, OwnedVariant>, #[zbus(object_server)] server: &ObjectServer,
        ) -> fdo::Result<(OwnedObjectPath, OwnedObjectPath, HashMap<String, OwnedVariant>)> {
            if self.0.lock().unwrap().add_error {
                return Err(fdo::Error::InvalidArgs("802-11-wireless-security.psk: property is invalid".into()));
            }
            let mut shown: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
            let (mut id, mut ssid) = (String::new(), None);
            for (group, body) in &conn {
                let g = shown.entry(group.clone()).or_default();
                for (k, v) in body {
                    if group == "connection" && k == "id" { id = repr(v); }
                    if group == "802-11-wireless" && k == "ssid" { ssid = (**v).try_clone().unwrap().downcast::<Vec<u8>>().ok(); }
                    g.insert(k.clone(), repr(v));
                }
            }
            let options = options.iter().map(|(k, v)| (k.clone(), repr(v))).collect();
            let (profile, active) = {
                let mut g = self.0.lock().unwrap();
                g.seq += 1;
                let (profile, active) = (format!("{PROF}{}", 100 + g.seq), format!("{ACT}{}", g.seq));
                g.profiles.insert(profile.clone(), ProfDef { kind: "802-11-wireless".into(), id, ssid });
                g.polls = 0;
                g.added.push(Added { settings: shown, device: device.as_str().into(), ap: ap.as_str().into(), options });
                (profile, active)
            };
            let _ = server.at(profile.as_str(), SettingsConn(self.0.clone(), profile.clone())).await;
            let _ = server.at(active.as_str(), Active(self.0.clone())).await;
            Ok((op(&profile), op(&active), HashMap::new()))
        }
    }

    struct Settings(Shared);
    #[interface(name = "org.freedesktop.NetworkManager.Settings")]
    impl Settings {
        #[zbus(name = "ListConnections")]
        fn list_connections(&self) -> Vec<OwnedObjectPath> { ops(self.0.lock().unwrap().profiles.keys().cloned().collect()) }
    }

    struct SettingsConn(Shared, String);
    #[interface(name = "org.freedesktop.NetworkManager.Settings.Connection")]
    impl SettingsConn {
        #[zbus(name = "GetSettings")]
        fn get_settings(&self) -> fdo::Result<HashMap<String, HashMap<String, OwnedVariant>>> {
            let p = self.0.lock().unwrap().profiles.get(&self.1).cloned().ok_or_else(|| fdo::Error::Failed("gone".into()))?;
            let ov = |v: Variant<'static>| OwnedVariant::try_from(v).unwrap();
            let mut groups = HashMap::from([(
                "connection".to_string(),
                HashMap::from([("type".to_string(), ov(p.kind.clone().into())), ("id".to_string(), ov(p.id.clone().into()))]),
            )]);
            if let Some(s) = &p.ssid {
                groups.insert("802-11-wireless".into(), HashMap::from([("ssid".to_string(), ov(Variant::new(s.clone())))]));
            }
            Ok(groups)
        }

        #[zbus(name = "Delete")]
        fn delete(&self) {
            let mut g = self.0.lock().unwrap();
            g.profiles.remove(&self.1);
            g.deleted.push(self.1.clone());
        }
    }

    struct Device(Shared, bool);
    #[interface(name = "org.freedesktop.NetworkManager.Device")]
    impl Device {
        #[zbus(property)]
        fn device_type(&self) -> u32 { if self.1 { 2 } else { 1 } }
        #[zbus(property)]
        fn active_connection(&self) -> OwnedObjectPath {
            self.0.lock().unwrap().wifi_active_conn.as_deref().filter(|_| self.1).map(op).unwrap_or_else(none)
        }
    }

    struct Wireless(Shared);
    #[interface(name = "org.freedesktop.NetworkManager.Device.Wireless")]
    impl Wireless {
        #[zbus(name = "GetAllAccessPoints")]
        fn get_all_access_points(&self) -> Vec<OwnedObjectPath> { ops(self.0.lock().unwrap().aps.iter().map(|a| a.path.clone()).collect()) }
        #[zbus(property)]
        fn active_access_point(&self) -> OwnedObjectPath { self.0.lock().unwrap().wifi_active_ap.as_deref().map(op).unwrap_or_else(none) }
    }

    struct Ap(ApDef);
    #[interface(name = "org.freedesktop.NetworkManager.AccessPoint")]
    impl Ap {
        #[zbus(property)]
        fn flags(&self) -> u32 { self.0.flags }
        #[zbus(property)]
        fn wpa_flags(&self) -> u32 { self.0.wpa }
        #[zbus(property)]
        fn rsn_flags(&self) -> u32 { self.0.rsn }
        #[zbus(property)]
        fn ssid(&self) -> Vec<u8> { self.0.ssid.clone() }
        #[zbus(property)]
        fn strength(&self) -> u8 { self.0.strength }
    }

    struct Active(Shared);
    #[interface(name = "org.freedesktop.NetworkManager.Connection.Active")]
    impl Active {
        #[zbus(property)]
        fn state(&self) -> u32 {
            let mut w = self.0.lock().unwrap();
            let i = w.polls.min(w.states.len().saturating_sub(1));
            w.polls += 1;
            w.states.get(i).copied().unwrap_or(1)
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
        fn builder(&self) -> zbus::connection::Builder<'static> {
            zbus::connection::Builder::address(self.addr.as_str()).method_timeout(Duration::from_secs(5))
        }
    }
    impl Drop for Daemon {
        fn drop(&mut self) { let _ = self.child.kill(); let _ = self.child.wait(); }
    }

    struct Env { _daemon: Daemon, world: Shared, client: Connection, server: Connection }

    /// `with_nm = false` leaves the bus without a NetworkManager.
    fn env(world: World, with_nm: bool) -> Env {
        let daemon = Daemon::start();
        let world: Shared = Arc::new(Mutex::new(world));
        let server = block_on(async {
            let mut b = daemon.builder();
            if with_nm {
                let (aps, profs, active) = {
                    let g = world.lock().unwrap();
                    (g.aps.clone(), g.profiles.keys().cloned().collect::<Vec<_>>(), g.wifi_active_conn.clone())
                };
                b = b.serve_at(NM_PATH, Root(world.clone()))
                    .serve_at(NM_SETTINGS_PATH, Settings(world.clone()))
                    .serve_at(DEV_WIRED, Device(world.clone(), false))
                    .serve_at(DEV_WIFI, Device(world.clone(), true))
                    .serve_at(DEV_WIFI, Wireless(world.clone()));
                for a in aps { b = b.serve_at(a.path.clone(), Ap(a)); }
                for p in profs { b = b.serve_at(p.clone(), SettingsConn(world.clone(), p)); }
                if let Some(a) = active { b = b.serve_at(a, Active(world.clone())); }
                b = b.name(NM);
            }
            b.build().await.unwrap()
        });
        let client = block_on(daemon.builder().build()).unwrap();
        Env { _daemon: daemon, world, client, server }
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
        let mut expected = before;
        expected.remove(&format!("{PROF}101"));
        let mut now = w.profiles.clone();
        now.remove(&format!("{PROF}101"));
        assert_eq!(now, expected, "existing profiles (wired, renamed, duplicate names) stay untouched");
        assert!(!w.profiles.contains_key(&format!("{PROF}101")));
    }

    #[test]
    fn stuck_activation_times_out_and_cleans_up_its_own_objects() {
        let e = env(wifi_world(vec![ap(1, "Slow", 60, 1, 0, 0x188)], vec![1]), true);
        let start = Instant::now();
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
        // listed, object vanished
        assert!(block_on(e.server.object_server().remove::<Ap, _>(format!("{AP_BASE}2"))).unwrap());
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
        w.wifi_active_ap = Some(format!("{AP_BASE}1"));
        w.wifi_active_conn = Some(format!("{ACT}90"));
        let e = env(w, true);
        assert!(disconnect_on(&e.client, "Renamed by user", T).is_err());
        assert!(disconnect_on(&e.client, "Wired connection 1", T).is_err());
        assert!(e.world.lock().unwrap().deactivated.is_empty());
        disconnect_on(&e.client, "HomeNet", T).unwrap();
        assert_eq!(e.world.lock().unwrap().deactivated, vec![format!("{ACT}90")]);
    }
}
