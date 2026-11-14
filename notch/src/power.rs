//! Battery and external-power detection from `/sys/class/power_supply`, so no supply name is hardcoded.
use std::path::Path;

const ROOT: &str = "/sys/class/power_supply";

fn read(p: &Path) -> Option<String> { std::fs::read_to_string(p).ok().map(|s| s.trim().to_string()) }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatteryState { Charging, Discharging, Full, NotCharging, Unknown }

impl BatteryState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Charging => "charging",
            Self::Discharging => "discharging",
            Self::Full => "full",
            Self::NotCharging => "not_charging",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct BatterySnapshot {
    pub name: String,
    pub state: BatteryState,
    pub percentage: Option<f64>,
    pub power_w: Option<f64>,
    pub energy_now_wh: Option<f64>,
    pub energy_full_wh: Option<f64>,
    pub energy_design_wh: Option<f64>,
    pub health_pct: Option<f64>,
    pub voltage_v: Option<f64>,
    pub cycles: Option<u64>,
    pub technology: Option<String>,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub time_to_empty_secs: Option<u64>,
    pub time_to_full_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PowerSnapshot {
    /// False when the supply directory itself could not be read.
    pub available: bool,
    /// `None` when external power cannot be determined.
    pub ac_online: Option<bool>,
    pub batteries: Vec<BatterySnapshot>,
    /// Overall charge; `None` unless derivable from complete readings.
    pub percentage: Option<f64>,
}

/// Finite, non-negative numeric sysfs attribute.
fn num(d: &Path, f: &str) -> Option<f64> {
    read(&d.join(f))?.parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.0)
}
/// Finite signed attribute, returned as magnitude (drivers disagree on the sign of rates).
fn rate(d: &Path, f: &str) -> Option<f64> {
    read(&d.join(f))?.parse::<f64>().ok().filter(|v| v.is_finite()).map(f64::abs)
}
fn text(d: &Path, f: &str) -> Option<String> { read(&d.join(f)).filter(|s| !s.is_empty()) }
fn ratio(num: Option<f64>, den: Option<f64>) -> Option<f64> {
    match (num, den) { (Some(n), Some(d)) if d > 0.0 => Some(n / d).filter(|r| r.is_finite()), _ => None }
}
fn finite(v: Option<f64>) -> Option<f64> { v.filter(|v| v.is_finite() && *v >= 0.0) }
fn secs(x: f64) -> Option<u64> {
    if !x.is_finite() || x < 0.0 { return None; }
    let r = x.round();
    (r < 18446744073709551616.0).then_some(r as u64)
}

/// Typed readings for every present internal battery plus the external-power state.
pub fn snapshot() -> PowerSnapshot { snapshot_at(Path::new(ROOT)) }

fn snapshot_at(root: &Path) -> PowerSnapshot {
    let unavailable = PowerSnapshot { available: false, ac_online: None, batteries: vec![], percentage: None };
    let Ok(rd) = std::fs::read_dir(root) else { return unavailable };
    let mut names = Vec::new();
    for e in rd {
        match e { Ok(e) => names.push(e.file_name().to_string_lossy().to_string()), Err(_) => return unavailable }
    }
    names.sort();
    let batteries: Vec<BatterySnapshot> = names.iter().filter_map(|n| battery_at(&root.join(n))).collect();

    let (mut candidates, mut offline, mut online) = (0usize, 0usize, false);
    for n in &names {
        let d = root.join(n);
        let ty = read(&d.join("type")).unwrap_or_default();
        if !(ty == "Mains" || ty.starts_with("USB")) || read(&d.join("scope")).as_deref() == Some("Device") { continue; }
        candidates += 1;
        match read(&d.join("online")).as_deref() { Some("1") => online = true, Some("0") => offline += 1, _ => {} }
    }
    let ac_online = if online { Some(true) } else if candidates > 0 && offline == candidates { Some(false) } else { None };

    let percentage = match batteries.as_slice() {
        [] => None,
        [one] => one.percentage,
        many => {
            let (mut now, mut full) = (0.0, 0.0);
            let mut ok = true;
            for b in many {
                match (b.energy_now_wh, b.energy_full_wh) {
                    (Some(n), Some(f)) if f > 0.0 && n <= f => { now += n; full += f; }
                    _ => { ok = false; break; }
                }
            }
            if ok { ratio(Some(now), Some(full)).map(|r| r * 100.0).filter(|p| (0.0..=100.0).contains(p)) } else { None }
        }
    };
    PowerSnapshot { available: true, ac_online, batteries, percentage }
}

/// One internal battery; `None` for other supply types, peripherals and absent packs.
fn battery_at(d: &Path) -> Option<BatterySnapshot> {
    if read(&d.join("type")).as_deref() != Some("Battery") { return None; }
    if read(&d.join("scope")).as_deref() == Some("Device") { return None; }
    if read(&d.join("present")).as_deref() == Some("0") { return None; }
    let name = d.file_name()?.to_string_lossy().to_string();
    let state = match read(&d.join("status")).as_deref() {
        Some("Charging") => BatteryState::Charging,
        Some("Discharging") => BatteryState::Discharging,
        Some("Full") => BatteryState::Full,
        Some("Not charging") => BatteryState::NotCharging,
        _ => BatteryState::Unknown,
    };
    let voltage_now = num(d, "voltage_now").filter(|v| *v > 0.0);
    let voltage_v = voltage_now.map(|v| v / 1e6);
    let current = rate(d, "current_now");
    let power_w = finite(rate(d, "power_now").map(|p| p / 1e6))
        .or_else(|| finite(Some(current? * voltage_now? / 1e12)));

    let (e_now, e_full, e_des) = (num(d, "energy_now"), num(d, "energy_full"), num(d, "energy_full_design"));
    let (c_now, c_full, c_des) = (num(d, "charge_now"), num(d, "charge_full"), num(d, "charge_full_design"));
    let conv_v = num(d, "voltage_min_design").filter(|v| *v > 0.0).or(voltage_now);
    let wh = |e: Option<f64>, c: Option<f64>| {
        finite(e.map(|e| e / 1e6)).or_else(|| finite(Some(c? * conv_v? / 1e12)))
    };
    let (energy_now_wh, energy_full_wh, energy_design_wh) = (wh(e_now, c_now), wh(e_full, c_full), wh(e_des, c_des));

    let pct = |r: Option<f64>| r.map(|r| r * 100.0).filter(|p| (0.0..=100.0).contains(p));
    let percentage = num(d, "capacity").filter(|c| *c <= 100.0)
        .or_else(|| pct(ratio(e_now, e_full)))
        .or_else(|| pct(ratio(c_now, c_full)));
    let health_pct = ratio(e_full, e_des).or_else(|| ratio(c_full, c_des)).map(|r| r * 100.0).filter(|h| h.is_finite());

    let zero_power = power_w == Some(0.0);
    let cur = current.filter(|c| *c > 0.0);
    let (mut time_to_empty_secs, mut time_to_full_secs) = (None, None);
    if !zero_power {
        let w = power_w.filter(|w| *w > 0.0);
        match state {
            BatteryState::Discharging => {
                time_to_empty_secs = w.and_then(|w| secs(energy_now_wh? / w * 3600.0))
                    .or_else(|| secs(c_now? / cur? * 3600.0));
            }
            BatteryState::Charging => {
                let by_energy = w.and_then(|w| {
                    let remaining = match (e_now, e_full, c_now, c_full) {
                        (Some(n), Some(f), ..) if f >= n => (f - n) / 1e6,
                        (None, None, Some(n), Some(f)) if f >= n => (f - n) * conv_v? / 1e12,
                        _ => return None,
                    };
                    secs(remaining / w * 3600.0)
                });
                time_to_full_secs = by_energy.or_else(|| match (c_now, c_full) {
                    (Some(n), Some(f)) if f >= n => secs((f - n) / cur? * 3600.0),
                    _ => None,
                });
            }
            _ => {}
        }
    }

    Some(BatterySnapshot {
        name, state, percentage, power_w,
        energy_now_wh, energy_full_wh, energy_design_wh, health_pct, voltage_v,
        cycles: read(&d.join("cycle_count")).and_then(|s| s.parse().ok()),
        technology: text(d, "technology"), manufacturer: text(d, "manufacturer"), model: text(d, "model_name"),
        time_to_empty_secs, time_to_full_secs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn empty_root() -> std::path::PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!("pwr-test-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }
    fn add(root: &Path, name: &str, ty: &str, files: &[(&str, &str)]) {
        let d = root.join(name);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("type"), format!("{ty}\n")).unwrap();
        for (k, v) in files { fs::write(d.join(k), format!("{v}\n")).unwrap(); }
    }
    fn fixture(files: &[(&str, &str)]) -> std::path::PathBuf {
        let root = empty_root();
        add(&root, "BAT0", "Battery", files);
        root
    }
    fn energy(status: &str) -> Vec<(&'static str, String)> {
        vec![("status", status.into()), ("energy_now", "24000000".into()), ("energy_full", "48000000".into()),
             ("energy_full_design", "60000000".into()), ("power_now", "12000000".into())]
    }
    fn snap(files: &[(&str, &str)]) -> Option<BatterySnapshot> {
        let root = fixture(files);
        let s = snapshot_at(&root).batteries.into_iter().next();
        fs::remove_dir_all(&root).unwrap();
        s
    }
    fn snap_owned(v: Vec<(&'static str, String)>) -> Option<BatterySnapshot> {
        let r: Vec<(&str, &str)> = v.iter().map(|(a, b)| (*a, b.as_str())).collect();
        snap(&r)
    }

    #[test]
    fn picks_only_internal_batteries_in_lexical_order() {
        let root = empty_root();
        add(&root, "zeta", "Battery", &[]);
        add(&root, "hidpp_battery_0", "Battery", &[("scope", "Device")]);
        add(&root, "ups", "UPS", &[]);
        add(&root, "BAT1", "Battery", &[]);
        add(&root, "BAT0", "Battery", &[("present", "0")]);
        let s = snapshot_at(&root);
        fs::remove_dir_all(&root).unwrap();
        assert!(s.available);
        assert_eq!(s.batteries.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(), ["BAT1", "zeta"]);
    }

    #[test]
    fn peripheral_only_is_empty_and_missing_root_is_unavailable() {
        let root = fixture(&[("scope", "Device")]);
        let s = snapshot_at(&root);
        fs::remove_dir_all(&root).unwrap();
        assert!(s.available && s.batteries.is_empty() && s.percentage.is_none());
        let s = snapshot_at(Path::new("/nonexistent-power-root"));
        assert!(!s.available && s.ac_online.is_none() && s.batteries.is_empty());
    }

    #[test]
    fn discharge_metrics_and_ac_does_not_reclassify() {
        let root = fixture(&[]);
        add(&root, "AC", "Mains", &[("online", "1")]);
        for (k, v) in energy("Discharging") { fs::write(root.join("BAT0").join(k), format!("{v}\n")).unwrap(); }
        let p = snapshot_at(&root);
        fs::remove_dir_all(&root).unwrap();
        assert_eq!(p.ac_online, Some(true));
        let s = &p.batteries[0];
        assert_eq!(s.state, BatteryState::Discharging);
        assert_eq!(s.percentage, Some(50.0));
        assert_eq!(s.health_pct, Some(80.0));
        assert_eq!(s.power_w, Some(12.0));
        assert_eq!(s.time_to_empty_secs, Some(7200));
        assert_eq!(s.time_to_full_secs, None);
        assert_eq!(p.percentage, Some(50.0));
    }

    #[test]
    fn charging_estimates_time_to_full() {
        let s = snap_owned(energy("Charging")).unwrap();
        assert_eq!(s.time_to_full_secs, Some(7200));
        assert_eq!(s.time_to_empty_secs, None);
    }

    #[test]
    fn charge_only_battery_converts_with_signed_current() {
        for cur in ["1000000", "-1000000"] {
            let s = snap(&[("status", "Discharging"), ("charge_now", "2000000"), ("charge_full", "4000000"),
                ("charge_full_design", "5000000"), ("voltage_now", "12000000"), ("current_now", cur)]).unwrap();
            assert_eq!((s.energy_now_wh, s.energy_full_wh, s.energy_design_wh), (Some(24.0), Some(48.0), Some(60.0)));
            assert_eq!((s.power_w, s.percentage, s.health_pct), (Some(12.0), Some(50.0), Some(80.0)));
            assert_eq!((s.time_to_empty_secs, s.voltage_v), (Some(7200), Some(12.0)));
        }
        // No voltage: ratios and charge/current estimate survive, no Wh/W invented.
        let s = snap(&[("status", "Charging"), ("charge_now", "2000000"), ("charge_full", "4000000"),
            ("charge_full_design", "5000000"), ("current_now", "1000000")]).unwrap();
        assert_eq!((s.energy_now_wh, s.power_w), (None, None));
        assert_eq!((s.percentage, s.health_pct, s.time_to_full_secs), (Some(50.0), Some(80.0), Some(7200)));
    }

    #[test]
    fn design_voltage_preferred_for_charge_conversion() {
        let s = snap(&[("status", "Discharging"), ("charge_now", "1000000"), ("voltage_min_design", "10000000"),
            ("voltage_now", "12000000")]).unwrap();
        assert_eq!((s.energy_now_wh, s.voltage_v), (Some(10.0), Some(12.0)));
    }

    #[test]
    fn lone_design_energy_does_not_suppress_charge_fallback() {
        let s = snap(&[("status", "Discharging"), ("energy_full_design", "60000000"), ("charge_now", "2000000"),
            ("charge_full", "4000000"), ("voltage_now", "12000000"), ("power_now", "12000000")]).unwrap();
        assert_eq!((s.energy_now_wh, s.energy_full_wh, s.energy_design_wh), (Some(24.0), Some(48.0), Some(60.0)));
        assert_eq!(s.time_to_empty_secs, Some(7200));
    }

    #[test]
    fn percentage_precedence_and_validation() {
        let s = snap(&[("capacity", "51"), ("energy_now", "24000000"), ("energy_full", "48000000")]).unwrap();
        assert_eq!(s.percentage, Some(51.0));
        let s = snap(&[("energy_now", "60000000"), ("energy_full", "48000000"), ("charge_now", "2000000"),
            ("charge_full", "4000000")]).unwrap();
        assert_eq!(s.percentage, Some(50.0));
    }

    #[test]
    fn charging_never_mixes_native_and_converted_operands() {
        let s = snap(&[("status", "Charging"), ("energy_now", "24000000"), ("charge_full", "4000000"),
            ("voltage_now", "12000000"), ("power_now", "12000000")]).unwrap();
        assert_eq!(s.time_to_full_secs, None);
    }

    #[test]
    fn multi_pack_total_is_weighted_and_all_or_nothing() {
        let root = empty_root();
        add(&root, "BAT0", "Battery", &[("status", "Charging"), ("energy_now", "10000000"), ("energy_full", "20000000")]);
        add(&root, "BAT1", "Battery", &[("status", "Discharging"), ("energy_now", "30000000"), ("energy_full", "60000000")]);
        let p = snapshot_at(&root);
        assert_eq!(p.batteries.len(), 2);
        assert_eq!((p.batteries[0].state, p.batteries[1].state), (BatteryState::Charging, BatteryState::Discharging));
        assert_eq!(p.percentage, Some(50.0));
        fs::remove_file(root.join("BAT1/energy_full")).unwrap();
        assert_eq!(snapshot_at(&root).percentage, None);
        fs::remove_dir_all(&root).unwrap();

        let root = empty_root();
        add(&root, "BAT0", "Battery", &[("charge_now", "1000000"), ("charge_full", "2000000"), ("voltage_now", "10000000")]);
        add(&root, "BAT1", "Battery", &[("charge_now", "3000000"), ("charge_full", "6000000"), ("voltage_now", "20000000")]);
        let p = snapshot_at(&root);
        fs::remove_dir_all(&root).unwrap();
        assert_eq!(p.percentage, Some(50.0));
    }

    #[test]
    fn external_power_three_way() {
        let cases: [(&[(&str, &str, &str)], Option<bool>); 5] = [
            (&[("AC", "Mains", "1")], Some(true)),
            (&[("AC", "Mains", "0")], Some(false)),
            (&[("AC", "Mains", "0"), ("ucsi", "USB_PD", "1")], Some(true)),
            (&[("AC", "Mains", "0"), ("ucsi", "USB_C", "weird")], None),
            (&[], None),
        ];
        for (sup, want) in cases {
            let root = empty_root();
            for (n, t, o) in sup { add(&root, n, t, &[("online", o)]); }
            let p = snapshot_at(&root);
            fs::remove_dir_all(&root).unwrap();
            assert_eq!(p.ac_online, want, "{sup:?}");
        }
    }

    #[test]
    fn states_and_zero_rate() {
        for (st, want) in [("Full", BatteryState::Full), ("Not charging", BatteryState::NotCharging), ("weird", BatteryState::Unknown)] {
            let s = snap_owned(energy(st)).unwrap();
            assert_eq!(s.state, want);
            assert_eq!((s.time_to_empty_secs, s.time_to_full_secs), (None, None));
        }
        // Measured zero power blocks every estimate, even with a contradictory current.
        let mut v = energy("Discharging");
        v.retain(|(k, _)| *k != "power_now");
        v.extend([("power_now", "0".into()), ("charge_now", "2000000".into()), ("current_now", "1000000".into())]);
        let s = snap_owned(v).unwrap();
        assert_eq!((s.power_w, s.time_to_empty_secs), (Some(0.0), None));
    }

    #[test]
    fn invalid_inputs_never_become_fake_numbers() {
        assert!(snap(&[("present", "0"), ("capacity", "50")]).is_none());
        let s = snap(&[("capacity", "250"), ("energy_now", "-5"), ("energy_full", "nan"), ("energy_full_design", "0"),
            ("power_now", "inf"), ("cycle_count", "0"), ("status", "Discharging")]).unwrap();
        assert_eq!((s.percentage, s.energy_now_wh, s.energy_full_wh, s.health_pct, s.power_w), (None, None, None, None, None));
        assert_eq!((s.cycles, s.time_to_empty_secs), (Some(0), None));
        // Overflow: huge energy / tiny power must not saturate.
        let s = snap(&[("status", "Discharging"), ("energy_now", "1e300"), ("power_now", "1")]).unwrap();
        assert_eq!(s.time_to_empty_secs, None);
        let s = snap(&[("energy_now", "1e308"), ("energy_full", "1"), ("energy_full_design", "1e-308")]).unwrap();
        assert!(s.health_pct.is_none_or(|h| h.is_finite()));
    }
}
