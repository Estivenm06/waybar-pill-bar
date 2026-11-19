//! PulseAudio/PipeWire state through `pactl` (JSON): master volume, output list, playing streams and the
//! default-output switch. Pure parsing is split from the process calls so it can be tested.
use serde_json::{json, Value};
use std::process::{Command, Stdio};

/// `pactl --format=json <args>` as an array; `Err` on spawn failure, non-zero exit, invalid JSON or non-array output.
pub fn pactl_json(args: &[&str]) -> Result<Vec<Value>, ()> {
    let o = pactl().arg("--format=json").args(args).stderr(Stdio::null()).output().map_err(|_| ())?;
    if !o.status.success() { return Err(()); }
    serde_json::from_slice::<Value>(&o.stdout).ok().and_then(|v| v.as_array().cloned()).ok_or(())
}

/// The `pactl` command. Unit tests swap in an executable stub so no test ever touches the real audio server.
fn pactl() -> Command {
    #[cfg(test)]
    return Command::new(stub::program());
    #[cfg(not(test))]
    Command::new("pactl")
}

/// Trimmed stdout of a successful `pactl <args>`.
fn pactl_out(args: &[&str]) -> Option<String> {
    let o = pactl().args(args).stderr(Stdio::null()).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Highest channel volume in percent; `None` when the channels are missing or any is malformed.
pub fn max_pct(v: &Value) -> Option<f64> {
    let mut best: Option<f64> = None;
    for c in v.as_object()?.values() {
        let p = c["value_percent"].as_str().and_then(|s| s.trim().trim_end_matches('%').parse::<f64>().ok())
            .or_else(|| c["value"].as_f64().map(|x| x / 65536.0 * 100.0))
            .filter(|p| p.is_finite() && *p >= 0.0)?;
        best = Some(best.map_or(p, |b| b.max(p)));
    }
    best
}

fn nonblank(v: &Value) -> Option<&str> { v.as_str().filter(|s| !s.trim().is_empty()) }

/// Master + output list from the default sink name and `pactl list sinks`. Never substitutes another sink for a missing default.
pub fn normalize_audio_outputs(default: Option<&str>, sinks: &[Value]) -> Value {
    let desc = |s: &Value| nonblank(&s["description"]).or(nonblank(&s["name"])).unwrap_or("").to_string();
    let default = default.filter(|d| !d.is_empty());
    let master = default.and_then(|d| sinks.iter().find(|s| nonblank(&s["name"]) == Some(d))).map(|s| json!({
        "name": s["name"], "desc": desc(s), "vol": max_pct(&s["volume"]), "mute": s["mute"].as_bool(),
    }));
    // skip sinks whose every port is unplugged (e.g. HDMI with nothing attached), but keep the current default
    let outputs: Vec<Value> = sinks.iter().filter_map(|s| {
        let name = nonblank(&s["name"])?;
        let is_def = default == Some(name);
        let ports = s["ports"].as_array().cloned().unwrap_or_default();
        let plugged = ports.is_empty() || ports.iter().any(|p| p["availability"].as_str() != Some("not available"));
        (is_def || plugged).then(|| json!({"name": name, "desc": desc(s), "default": is_def, "bt_mac": bluetooth_sink_mac(s), "mute": s["mute"].as_bool()}))
    }).collect();
    json!({"master": master, "outputs": outputs})
}

pub fn normalize_audio_streams(inputs: &[Value]) -> Vec<Value> {
    inputs.iter().filter_map(|i| {
        let id = i["index"].as_u64()?;
        let p = &i["properties"];
        let name = ["application.name", "application.process.binary", "media.name"].iter()
            .find_map(|k| nonblank(&p[*k])).unwrap_or("Unknown");
        Some(json!({"id": id, "name": name, "vol": max_pct(&i["volume"]), "mute": i["mute"].as_bool(),
                    "corked": i["corked"].as_bool().unwrap_or(false)}))
    }).collect()
}

/// `{"available", "master": {name, desc, vol, mute}|null, "outputs": [{name, desc, default, bt_mac, mute}]}`.
pub fn gather_audio_outputs() -> Value {
    let Ok(sinks) = pactl_json(&["list", "sinks"]) else {
        return json!({"available": false, "master": null, "outputs": []});
    };
    let default = pactl_out(&["get-default-sink"]);
    let mut v = normalize_audio_outputs(default.as_deref(), &sinks);
    v["available"] = json!(true);
    v
}

/// Outputs plus playing streams (`streams_available`, `streams: [{id, name, vol, mute, corked}]`).
pub fn gather_volume() -> Value {
    let mut v = gather_audio_outputs();
    let streams = if v["available"].as_bool().unwrap_or(false) { pactl_json(&["list", "sink-inputs"]).ok() } else { None };
    v["streams_available"] = json!(streams.is_some());
    v["streams"] = json!(streams.map(|s| normalize_audio_streams(&s)).unwrap_or_default());
    v
}

/// Default sink volume in percent (may exceed 100) and mute; `None` without an audio server or default sink.
pub fn master() -> Option<(f64, bool)> {
    let v = gather_audio_outputs();
    if !v["available"].as_bool().unwrap_or(false) { return None; }
    let m = &v["master"];
    if m.is_null() { return None; }
    Some((m["vol"].as_f64().unwrap_or(0.0), m["mute"].as_bool().unwrap_or(false)))
}

/// `AA:BB:CC:DD:EE:FF` (any case) → uppercase; exactly six two-digit hex octets, nothing else.
fn normalize_mac(s: &str) -> Option<String> {
    let parts: Vec<&str> = s.split(':').collect();
    (parts.len() == 6 && parts.iter().all(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_hexdigit())))
        .then(|| s.to_ascii_uppercase())
}

/// MAC of the Bluetooth device behind a playback sink record. Sources/monitors, non-Bluetooth sinks and malformed
/// addresses give `None`; aliases and arbitrary name substrings are never matched.
pub fn bluetooth_sink_mac(s: &Value) -> Option<String> {
    let name = nonblank(&s["name"])?;
    if name.ends_with(".monitor") { return None; }
    let p = &s["properties"];
    let by_name = name.starts_with("bluez_output.");
    if !(by_name || p["device.api"].as_str() == Some("bluez5") || p["device.bus"].as_str() == Some("bluetooth")) { return None; }
    nonblank(&p["api.bluez5.address"]).and_then(normalize_mac).or_else(|| {
        let rest = name.strip_prefix("bluez_output.")?;
        normalize_mac(&rest.split('.').next()?.replace('_', ":"))
    })
}

pub fn run_pactl(args: &[&str]) -> Result<(), String> {
    let o = pactl().args(args).stdin(Stdio::null()).output().map_err(|e| e.to_string())?;
    if o.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&o.stderr).trim().to_string()) }
}

/// Highest sink-input ID the mixer rows can address (the UI packs a tag into the top bits of a hit ID).
pub const MAX_STREAM_ID: u64 = 0x3FFF_FFFF;

/// Playing applications of a `gather_volume()` document: valid numeric IDs, uncorked (muted ones stay so they can
/// be unmuted). Borrowed from the document, never cloned.
pub fn playing_streams(v: &Value) -> impl Iterator<Item = &Value> {
    v["streams"].as_array().into_iter().flatten()
        .filter(|s| s["id"].as_u64().is_some_and(|i| i <= MAX_STREAM_ID) && s["corked"].as_bool() != Some(true))
}

fn pactl_error(what: &str, e: String) -> String {
    if e.is_empty() { format!("{what} failed") } else { format!("{what} failed: {e}") }
}

/// Set one playing application's volume (capped at 100 %). Never touches the master or another stream.
pub fn set_stream_volume(id: u64, percent: u8) -> Result<(), String> {
    run_pactl(&["set-sink-input-volume", &id.to_string(), &format!("{}%", percent.min(100))])
        .map_err(|e| pactl_error("Application volume change", e))
}

/// Toggle one playing application's mute. Never touches the master or another stream.
pub fn toggle_stream_mute(id: u64) -> Result<(), String> {
    run_pactl(&["set-sink-input-mute", &id.to_string(), "toggle"]).map_err(|e| pactl_error("Application mute", e))
}

/// Indices of sink inputs not already playing on `target_index`.
fn sink_input_moves(inputs: &[Value], target_index: u64) -> Vec<u64> {
    inputs.iter().filter_map(|i| {
        let idx = i["index"].as_u64()?;
        (i["sink"].as_u64() != Some(target_index)).then_some(idx)
    }).collect()
}

/// Make `name` the default output and move playing streams to it. Verified; never touches volume or mute.
/// Streams are only moved after the default really changed; a stream that vanished meanwhile is not an error.
pub fn select_audio_output(name: &str) -> Result<(), String> {
    let sinks = pactl_json(&["list", "sinks"]).map_err(|_| "Audio server unavailable".to_string())?;
    let target = sinks.iter().find(|s| s["name"].as_str() == Some(name))
        .ok_or_else(|| format!("Audio output {name} is not available"))?;
    let tidx = target["index"].as_u64().unwrap_or(u64::MAX);
    run_pactl(&["set-default-sink", name]).map_err(|e| format!("Could not select {name}: {e}"))?;
    if pactl_out(&["get-default-sink"]).as_deref() != Some(name) {
        return Err(format!("Could not make {name} the default output"));
    }
    let inputs = pactl_json(&["list", "sink-inputs"]).map_err(|_| "Could not list playing applications".to_string())?;
    let mut failed = 0;
    for idx in sink_input_moves(&inputs, tidx) {
        if run_pactl(&["move-sink-input", &idx.to_string(), name]).is_err() {
            let alive = pactl_json(&["list", "sink-inputs"]).map(|v| v.iter().any(|i| i["index"].as_u64() == Some(idx))).unwrap_or(true);
            if alive { failed += 1; }
        }
    }
    if failed == 0 { Ok(()) } else { Err(format!("Could not move {failed} playing application(s) to {name}")) }
}
/// Executable `pactl` stand-in for unit tests: JSON listings come from files in its private directory and every
/// invocation is appended to `log`, so tests assert the exact arguments that reached the mixer.
#[cfg(test)]
pub mod stub {
    use std::path::PathBuf;
    use std::sync::{LazyLock, Mutex, MutexGuard};

    static LOCK: Mutex<()> = Mutex::new(());

    const SCRIPT: &str = r#"#!/bin/sh
d=$(dirname "$0")
echo "$*" >> "$d/log"
case "$*" in
  "--format=json list sinks") [ -f "$d/no-server" ] && exit 1; cat "$d/sinks.json" ;;
  "--format=json list sink-inputs") [ -f "$d/no-inputs" ] && exit 1; cat "$d/sink-inputs.json" ;;
  "get-default-sink") cat "$d/default" ;;
  set-sink-input-*) if [ -f "$d/fail-set" ]; then cat "$d/fail-set" >&2; exit 1; fi ;;
esac
exit 0
"#;

    static DIR: LazyLock<PathBuf> = LazyLock::new(|| {
        use std::os::unix::fs::PermissionsExt;
        let d = std::env::temp_dir().join(format!("waybar-notch-pactl-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("pactl");
        std::fs::write(&p, SCRIPT).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        d
    });

    /// Path of the stub every `pactl` call of the test binary goes through.
    pub fn program() -> PathBuf { DIR.join("pactl") }

    /// Exclusive, freshly reset use of the stub (one mixer scenario at a time).
    pub struct Pactl { _guard: MutexGuard<'static, ()> }

    impl Pactl {
        pub fn install() -> Pactl {
            let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
            for f in ["log", "no-server", "no-inputs", "fail-set"] { let _ = std::fs::remove_file(DIR.join(f)); }
            let p = Pactl { _guard: guard };
            p.set("sinks.json", r#"[{"index":1,"name":"sink0","description":"Speakers","mute":false,"ports":[],"volume":{"front-left":{"value_percent":"50%"},"front-right":{"value_percent":"50%"}}}]"#);
            p.set("sink-inputs.json", "[]");
            p.set("default", "sink0\n");
            p
        }
        pub fn set(&self, file: &str, content: &str) { std::fs::write(DIR.join(file), content).unwrap(); }
        pub fn clear(&self, file: &str) { let _ = std::fs::remove_file(DIR.join(file)); }
        /// Sink inputs as `(index, application name, volume %, mute, corked)`.
        pub fn set_inputs(&self, inputs: &[(u64, &str, u32, bool, bool)]) {
            let v: Vec<serde_json::Value> = inputs.iter().map(|(i, n, v, m, c)| serde_json::json!({
                "index": i, "sink": 1, "mute": m, "corked": c, "properties": {"application.name": n},
                "volume": {"front-left": {"value_percent": format!("{v}%")}, "front-right": {"value_percent": format!("{v}%")}},
            })).collect();
            self.set("sink-inputs.json", &serde_json::Value::Array(v).to_string());
        }
        /// Every argument line the stub received since `install`.
        pub fn log(&self) -> Vec<String> {
            std::fs::read_to_string(DIR.join("log")).unwrap_or_default().lines().map(String::from).collect()
        }
        /// Only the lines that change something.
        pub fn mutations(&self) -> Vec<String> {
            self.log().into_iter().filter(|l| !l.starts_with("--format=json list") && l != "get-default-sink").collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sink(name: &str, desc: &str, vol: Value, ports: Value) -> Value {
        json!({"name": name, "description": desc, "mute": false, "volume": vol, "ports": ports})
    }
    fn ch(p: &str) -> Value { json!({"front-left": {"value_percent": p}, "front-right": {"value_percent": "10%"}}) }

    #[test]
    fn audio_outputs_select_default_not_first() {
        let sinks = [sink("a", "A", ch("30%"), json!([])), sink("b", "B", ch("0%"), json!([]))];
        let v = normalize_audio_outputs(Some("b"), &sinks);
        assert_eq!(v["master"]["name"], "b");
        assert_eq!(v["master"]["vol"], 10.0);
        let v = normalize_audio_outputs(None, &sinks);
        assert!(v["master"].is_null());
        assert_eq!(v["outputs"].as_array().unwrap().len(), 2);
        assert!(normalize_audio_outputs(Some("gone"), &sinks)["master"].is_null());
    }

    #[test]
    fn audio_outputs_filter_unplugged_but_keep_default() {
        let off = json!([{"availability": "not available"}]);
        let sinks = [sink("hdmi", "", ch("0%"), off.clone()), sink("usb", "", ch("0%"), json!([])), sink("hdmi2", "H2", ch("0%"), off)];
        let v = normalize_audio_outputs(Some("hdmi2"), &sinks);
        let names: Vec<&str> = v["outputs"].as_array().unwrap().iter().map(|o| o["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["usb", "hdmi2"]);
        assert_eq!(v["outputs"][0]["desc"], "usb"); // blank description falls back to the name
        assert_eq!(v["outputs"][1]["default"], true);
    }

    #[test]
    fn audio_volume_preserves_zero_overdrive_and_missing() {
        assert_eq!(max_pct(&json!({"l": {"value_percent": "0%"}})), Some(0.0));
        assert_eq!(max_pct(&json!({"l": {"value_percent": "120%"}, "r": {"value": 32768}})), Some(120.0));
        assert_eq!(max_pct(&json!({"l": {"value": 32768}})), Some(50.0));
        assert_eq!(max_pct(&json!({"l": {"value_percent": "x"}})), None);
        assert_eq!(max_pct(&json!({})), None);
        assert_eq!(max_pct(&Value::Null), None);
        let s = json!({"name": "n", "volume": {}});
        let v = normalize_audio_outputs(Some("n"), &[s]);
        assert!(v["master"]["vol"].is_null() && v["master"]["mute"].is_null());
    }

    #[test]
    fn audio_streams_keep_names_and_drop_invalid_ids() {
        let inputs = [
            json!({"index": 3, "properties": {"application.name": "écho"}, "volume": {"l": {"value_percent": "40%"}}}),
            json!({"index": 4, "properties": {"application.name": "", "application.process.binary": "音楽"}}),
            json!({"index": "x", "properties": {}}),
            json!({"properties": {}}),
        ];
        let s = normalize_audio_streams(&inputs);
        assert_eq!(s.len(), 2);
        assert_eq!((s[0]["name"].as_str(), s[0]["vol"].as_f64()), (Some("écho"), Some(40.0)));
        assert_eq!(s[1]["name"], "音楽");
        assert!(s[1]["vol"].is_null() && s[1]["mute"].is_null());
        let outs = normalize_audio_outputs(Some("a"), &[sink("", "x", ch("1%"), json!([]))]);
        assert!(outs["outputs"].as_array().unwrap().is_empty() && outs["master"].is_null());
    }

    fn bt_sink(name: &str, props: Value) -> Value { json!({"name": name, "properties": props}) }

    #[test]
    fn bluetooth_sink_mac_is_exact() {
        let p = |a: &str| bt_sink("alsa_output.x", json!({"device.api": "bluez5", "api.bluez5.address": a}));
        assert_eq!(bluetooth_sink_mac(&p("aa:bb:cc:dd:ee:ff")).as_deref(), Some("AA:BB:CC:DD:EE:FF"));
        assert_eq!(bluetooth_sink_mac(&bt_sink("bluez_output.AA_BB_CC_DD_EE_0F.1", json!({}))).as_deref(), Some("AA:BB:CC:DD:EE:0F"));
        // invalid property falls back to the name
        let bad = bt_sink("bluez_output.11_22_33_44_55_66.1", json!({"api.bluez5.address": "zz"}));
        assert_eq!(bluetooth_sink_mac(&bad).as_deref(), Some("11:22:33:44:55:66"));
        assert_eq!(bluetooth_sink_mac(&bt_sink("bluez_output.AA_BB_CC.1", json!({}))), None);
        assert_eq!(bluetooth_sink_mac(&bt_sink("bluez_output.AA_BB_CC_DD_EE_FF.1.monitor", json!({}))), None);
        assert_eq!(bluetooth_sink_mac(&bt_sink("alsa_output.pci", json!({"api.bluez5.address": "AA:BB:CC:DD:EE:FF"}))), None);
        assert_eq!(bluetooth_sink_mac(&bt_sink("alsa_output.AA_BB_CC_DD_EE_FF", json!({"device.bus": "pci"}))), None);
        assert_eq!(bluetooth_sink_mac(&p("AA:BB:CC:DD:EE")), None);
    }

    #[test]
    fn outputs_carry_exact_mac_and_mute_and_identical_aliases_stay_distinct() {
        let mk = |n: &str| json!({"name": n, "description": "Buds", "mute": true, "properties": {}});
        let sinks = [mk("bluez_output.AA_AA_AA_AA_AA_AA.1"), mk("bluez_output.BB_BB_BB_BB_BB_BB.1"), mk("alsa")];
        let v = normalize_audio_outputs(Some("bluez_output.BB_BB_BB_BB_BB_BB.1"), &sinks);
        assert_eq!(v["outputs"][0]["bt_mac"], "AA:AA:AA:AA:AA:AA");
        assert_eq!(v["outputs"][1]["bt_mac"], "BB:BB:BB:BB:BB:BB");
        assert!(v["outputs"][2]["bt_mac"].is_null());
        assert_eq!(v["outputs"][1]["default"], true);
        assert_eq!(v["outputs"][0]["mute"], true);
    }

    #[test]
    fn only_inputs_on_other_sinks_are_moved() {
        let inputs = [json!({"index": 3, "sink": 7}), json!({"index": 4, "sink": 1}), json!({"sink": 1})];
        assert_eq!(sink_input_moves(&inputs, 7), vec![4]);
    }

    #[test]
    fn playing_streams_lists_uncorked_numeric_ids_only_and_keeps_muted() {
        let inputs = [
            json!({"index": 7, "mute": true, "properties": {"application.name": "Muted"}}),
            json!({"index": 8, "corked": true, "properties": {"application.name": "Paused"}}),
            json!({"index": 9, "properties": {"application.name": "Live"}}),
            json!({"index": "x", "properties": {}}),
            json!({"index": 5_000_000_000u64, "properties": {"application.name": "Huge"}}),
        ];
        let doc = json!({"streams": normalize_audio_streams(&inputs)});
        let names: Vec<&str> = playing_streams(&doc).map(|s| s["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["Muted", "Live"]);
        assert_eq!(playing_streams(&json!({})).count(), 0);
    }

    #[test]
    fn stream_commands_address_only_their_sink_input() {
        let p = stub::Pactl::install();
        p.set_inputs(&[(7, "Player", 40, false, false), (8, "Browser", 60, false, false)]);
        set_stream_volume(7, 35).unwrap();
        toggle_stream_mute(8).unwrap();
        set_stream_volume(7, 250).unwrap();
        assert_eq!(p.mutations(), ["set-sink-input-volume 7 35%", "set-sink-input-mute 8 toggle", "set-sink-input-volume 7 100%"]);
        // never a sink-level (master) change
        assert!(p.log().iter().all(|l| !l.contains("set-sink-volume") && !l.contains("set-sink-mute")));
    }

    #[test]
    fn stream_command_failure_reports_the_mixer_error() {
        let p = stub::Pactl::install();
        p.set("fail-set", "Failure: No such entity\n");
        let e = set_stream_volume(7, 10).unwrap_err();
        assert!(e.contains("No such entity"), "{e}");
        let e = toggle_stream_mute(7).unwrap_err();
        assert!(e.contains("No such entity"), "{e}");
        p.set("fail-set", "");
        assert!(set_stream_volume(7, 10).unwrap_err().contains("failed"));
    }

    #[test]
    fn gather_volume_flags_server_and_enumeration_failures() {
        let p = stub::Pactl::install();
        p.set_inputs(&[(7, "Player", 40, false, false), (8, "Paused", 40, false, true)]);
        let v = gather_volume();
        assert_eq!((v["available"].as_bool(), v["streams_available"].as_bool()), (Some(true), Some(true)));
        assert_eq!(playing_streams(&v).count(), 1);
        assert_eq!(v["master"]["vol"], 50.0);
        p.set("no-inputs", "");
        let v = gather_volume();
        assert_eq!((v["available"].as_bool(), v["streams_available"].as_bool()), (Some(true), Some(false)));
        assert_eq!(playing_streams(&v).count(), 0, "stale streams are never kept");
        p.set("no-server", "");
        let v = gather_volume();
        assert_eq!(v["available"].as_bool(), Some(false));
        assert!(v["master"].is_null() && v["outputs"].as_array().unwrap().is_empty());
    }
}
