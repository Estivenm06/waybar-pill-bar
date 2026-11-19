//! One bounded wttr.in request supplies every weather surface.
//!
//! The UI owns `State`: send `state.refresh(read_location(&location_path()))` on
//! startup, explicit refresh, and location-file changes. Apply worker updates
//! before rendering; rejected generations must never repaint a newer location.
use chrono::{DateTime, Local, NaiveDate, NaiveTime};
use serde_json::Value;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

const LOCATION_LIMIT: usize = 64 * 1024;
const RESPONSE_LIMIT: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct WeatherReport {
    pub location: String,
    pub description: String,
    pub icon: String,
    pub temperature_c: f64,
    pub feels_like_c: Option<f64>,
    pub wind_kmh: Option<f64>,
    pub wind_direction: Option<String>,
    pub humidity_pct: Option<u8>,
    pub forecast: Vec<ForecastDay>,
    pub fetched_at: DateTime<Local>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ForecastDay {
    pub date: NaiveDate,
    pub icon: String,
    pub low_c: Option<f64>,
    pub high_c: Option<f64>,
}

/// The encoded path and display-name policy together identify a location.
/// Private fields ensure callers cannot supply an arbitrary URL.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LocationQuery {
    path: String,
    name: Option<String>,
}

impl LocationQuery {
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or("Current location")
    }
}

pub fn location_path() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
        .join(".local/state/omarchy/settings/weather.json")
}

/// Missing, malformed, oversized, or unreadable settings use IP autodetection.
/// The shared Omarchy settings file is never modified.
pub fn read_location(path: &Path) -> LocationQuery {
    let read = || -> Result<LocationQuery, String> {
        let file = File::open(path).map_err(|_| "Location unavailable")?;
        if !file.metadata().map_err(|_| "Location unavailable")?.is_file() {
            return Err("Location is not a regular file".into());
        }
        parse_location(&read_bounded(file, LOCATION_LIMIT)?)
    };
    read().unwrap_or_default()
}

fn read_bounded(reader: impl Read, limit: usize) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    reader.take(limit as u64 + 1).read_to_end(&mut bytes)
        .map_err(|_| "Weather data could not be read".to_string())?;
    if bytes.len() > limit { return Err("Weather data exceeds size limit".into()); }
    Ok(bytes)
}

fn text(value: &Value) -> Option<&str> {
    value.as_str().map(str::trim).filter(|s| !s.is_empty())
}

fn finite(value: &Value) -> Option<f64> {
    value.as_f64().or_else(|| value.as_str()?.trim().parse().ok()).filter(|n| n.is_finite())
}

fn encode_path(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 15) as usize] as char);
        }
    }
    out
}

fn parse_location(bytes: &[u8]) -> Result<LocationQuery, String> {
    if bytes.len() > LOCATION_LIMIT { return Err("Location exceeds size limit".into()); }
    let value: Value = serde_json::from_slice(bytes).map_err(|_| "Invalid location JSON")?;
    if !value.is_object() { return Err("Invalid location settings".into()); }
    let name = text(&value["name"]).map(str::to_owned);
    // Settings coordinates must be JSON numbers, not loosely parsed strings.
    let coordinates = value["latitude"].as_f64().zip(value["longitude"].as_f64())
        .filter(|(lat, lon)| lat.is_finite() && lon.is_finite()
            && (-90.0..=90.0).contains(lat) && (-180.0..=180.0).contains(lon));
    let path = if let Some((lat, lon)) = coordinates {
        encode_path(&format!("{lat},{lon}"))
    } else {
        name.as_deref().map(encode_path).unwrap_or_default()
    };
    Ok(LocationQuery { path, name })
}

fn weather_url(query: &LocationQuery) -> String {
    format!("https://wttr.in/{}?format=j1", query.path)
}

/// Exactly the installed omarchy-weather-icon condition mapping.
fn icon(code: Option<u32>, night: bool) -> &'static str {
    match code {
        Some(113) => if night { "\u{e32b}" } else { "\u{e30d}" },
        Some(116) => if night { "\u{e32e}" } else { "\u{e302}" },
        Some(119 | 122) => "\u{e33d}",
        Some(143 | 248 | 260) => "\u{e313}",
        Some(176 | 263 | 353) => if night { "\u{e333}" } else { "\u{e308}" },
        Some(179 | 227 | 230 | 323 | 326 | 368) => if night { "\u{e327}" } else { "\u{e30a}" },
        Some(182 | 185 | 281 | 284 | 311 | 314 | 317 | 320 | 350 | 362 | 365 | 374 | 377) => "\u{e3ad}",
        Some(200 | 386 | 389 | 392 | 395) => "\u{e31d}",
        Some(266 | 293 | 296 | 299 | 302 | 305 | 308 | 356 | 359) => "\u{e318}",
        Some(329 | 332 | 335 | 338 | 371) => "\u{e31a}",
        _ => "\u{e33d}",
    }
}

fn unsigned(value: &Value) -> Option<u32> {
    value.as_u64().and_then(|n| u32::try_from(n).ok())
        .or_else(|| value.as_str()?.trim().parse().ok())
}

fn is_night(day: Option<&Value>, time: NaiveTime) -> bool {
    let Some(astronomy) = day.and_then(|d| d["astronomy"].as_array()).and_then(|a| a.first()) else {
        return false;
    };
    let sunrise = text(&astronomy["sunrise"]).and_then(|s| NaiveTime::parse_from_str(s, "%I:%M %p").ok());
    let sunset = text(&astronomy["sunset"]).and_then(|s| NaiveTime::parse_from_str(s, "%I:%M %p").ok());
    matches!((sunrise, sunset), (Some(rise), Some(set)) if rise < set && (time < rise || time >= set))
}

fn hourly_time(value: &Value) -> Option<u32> {
    let hhmm = unsigned(value)?;
    let hours = hhmm / 100;
    let minutes = hhmm % 100;
    (hours < 24 && minutes < 60).then_some(hours * 60 + minutes)
}

fn parse_report(bytes: &[u8], query: &LocationQuery, fetched_at: DateTime<Local>) -> Result<WeatherReport, String> {
    if bytes.len() > RESPONSE_LIMIT { return Err("Weather response exceeds size limit".into()); }
    let value: Value = serde_json::from_slice(bytes).map_err(|_| "Invalid weather response")?;
    let current = value["current_condition"].as_array().and_then(|a| a.first())
        .filter(|v| v.is_object()).ok_or("Weather has no current condition")?;
    let temperature_c = finite(&current["temp_C"]).ok_or("Weather has no valid temperature")?;
    let days = value["weather"].as_array();
    let today = fetched_at.date_naive();
    let today_row = days.and_then(|rows| rows.iter().find(|row| {
        text(&row["date"]).and_then(|s| s.parse::<NaiveDate>().ok()) == Some(today)
    }));
    let description = current["weatherDesc"].as_array().and_then(|a| a.first())
        .and_then(|v| text(&v["value"])).unwrap_or("Condition unavailable").to_owned();
    let provider_location = value["nearest_area"].as_array().and_then(|a| a.first())
        .and_then(|area| area["areaName"].as_array()).and_then(|a| a.first())
        .and_then(|name| text(&name["value"]));
    let location = query.name.as_deref().or(provider_location).unwrap_or("Current location").to_owned();
    let mut forecast = Vec::with_capacity(3);
    if let Some(days) = days {
        for day in days {
            let Some(date) = text(&day["date"]).and_then(|s| s.parse::<NaiveDate>().ok()) else { continue };
            if date < today || forecast.iter().any(|row: &ForecastDay| row.date == date) { continue; }
            let noon = day["hourly"].as_array().and_then(|hours| hours.iter()
                .filter_map(|hour| hourly_time(&hour["time"]).map(|time| (time.abs_diff(720), hour)))
                .min_by_key(|(distance, _)| *distance).map(|(_, hour)| hour));
            forecast.push(ForecastDay {
                date,
                icon: icon(noon.and_then(|h| unsigned(&h["weatherCode"])), false).to_owned(),
                low_c: finite(&day["mintempC"]),
                high_c: finite(&day["maxtempC"]),
            });
            if forecast.len() == 3 { break; }
        }
    }
    Ok(WeatherReport {
        location,
        description,
        icon: icon(unsigned(&current["weatherCode"]), is_night(today_row, fetched_at.time())).to_owned(),
        temperature_c,
        feels_like_c: finite(&current["FeelsLikeC"]),
        wind_kmh: finite(&current["windspeedKmph"]).filter(|n| *n >= 0.0),
        wind_direction: text(&current["winddir16Point"]).map(str::to_owned),
        humidity_pct: unsigned(&current["humidity"]).filter(|n| *n <= 100).map(|n| n as u8),
        forecast,
        fetched_at,
    })
}

fn fetch(agent: &ureq::Agent, query: &LocationQuery) -> Result<WeatherReport, String> {
    let mut response = agent.get(weather_url(query)).call().map_err(|error| match error {
        ureq::Error::StatusCode(code) => format!("Weather service returned HTTP {code}"),
        _ => "Weather request failed".to_owned(),
    })?;
    // ureq also rejects redirects at the agent level. Keep the response check
    // explicit so a non-success response can never be parsed as current data.
    if !response.status().is_success() { return Err("Weather service returned an unsuccessful response".into()); }
    let bytes = read_bounded(response.body_mut().as_reader(), RESPONSE_LIMIT)?;
    parse_report(&bytes, query, Local::now())
}

#[derive(Debug)]
pub enum Command {
    Refresh { query: LocationQuery, generation: u64 },
}

#[derive(Debug)]
pub enum Update {
    Started { generation: u64 },
    Finished { generation: u64, result: Result<WeatherReport, String> },
}

/// Last good data and its timestamp remain separate from in-flight/error state.
#[derive(Debug, Default)]
pub struct State {
    pub query: LocationQuery,
    pub generation: u64,
    pub report: Option<WeatherReport>,
    pub fetching: bool,
    pub error: Option<String>,
    pub last_success: Option<DateTime<Local>>,
}

impl State {
    /// Call before sending the command, so a location edit clears old readings
    /// immediately even if the previous HTTP request is still completing.
    pub fn refresh(&mut self, query: LocationQuery) -> Command {
        if self.query != query {
            self.report = None;
            self.last_success = None;
        }
        self.query = query.clone();
        self.generation = self.generation.checked_add(1).expect("weather generation exhausted");
        self.fetching = true;
        self.error = None;
        Command::Refresh { query, generation: self.generation }
    }

    /// Returns false for an obsolete reply; no state changes in that case.
    pub fn apply(&mut self, update: Update) -> bool {
        let generation = match &update {
            Update::Started { generation } | Update::Finished { generation, .. } => *generation,
        };
        if generation != self.generation { return false; }
        match update {
            Update::Started { .. } => self.fetching = true,
            Update::Finished { result, .. } => {
                self.fetching = false;
                match result {
                    Ok(report) => {
                        self.last_success = Some(report.fetched_at);
                        self.report = Some(report);
                        self.error = None;
                    }
                    Err(error) => self.error = Some(error),
                }
            }
        }
        true
    }
}

pub fn poll_delay(success: bool) -> Duration {
    Duration::from_secs(if success { 600 } else { 30 })
}

/// Starts one serial worker. Returning false from `publish`, or dropping every
/// command sender, stops it. Refresh commands wake timed waits and coalesce.
pub fn spawn<F>(publish: F) -> Sender<Command>
where F: FnMut(Update) -> bool + Send + 'static {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(10)))
            .max_redirects(0)
            .build().into();
        run_worker(rx, publish, |query| fetch(&agent, query));
    });
    tx
}

static WORKER: std::sync::OnceLock<Sender<Command>> = std::sync::OnceLock::new();

/// Starts the shared weather worker (once) and requests the first report. Every update is applied to
/// `State::weather` on the UI thread.
pub fn start(poster: &crate::wayland::Poster) {
    if WORKER.get().is_some() { return; }
    let p = poster.clone();
    let tx = spawn(move |update| {
        p.post(move |st, _| { st.weather.apply(update); });
        true
    });
    if WORKER.set(tx).is_err() { return; }
    poster.post(|st, _| request(&mut st.weather));
}

/// Re-reads the saved location and asks the worker for a fresh report (the Refresh/Retry button, or after the
/// location file changed). Sets `error` when the worker is unavailable.
pub fn request(state: &mut State) {
    let command = state.refresh(read_location(&location_path()));
    if WORKER.get().is_none_or(|tx| tx.send(command).is_err()) {
        state.fetching = false;
        state.error = Some("Weather worker unavailable".into());
    }
}

fn run_worker(
    rx: Receiver<Command>,
    mut publish: impl FnMut(Update) -> bool,
    mut fetch: impl FnMut(&LocationQuery) -> Result<WeatherReport, String>,
) {
    let Ok(mut command) = rx.recv() else { return };
    loop {
        while let Ok(newest) = rx.try_recv() { command = newest; }
        let Command::Refresh { query, generation } = &command;
        if !publish(Update::Started { generation: *generation }) { return; }
        let result = fetch(query);
        let delay = poll_delay(result.is_ok());
        if !publish(Update::Finished { generation: *generation, result }) { return; }
        match rx.recv_timeout(delay) {
            Ok(next) => command = next,
            Err(RecvTimeoutError::Timeout) => {},
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Timelike};
    use serde_json::json;
    use std::cell::RefCell;
    use std::io::Cursor;

    fn now() -> DateTime<Local> {
        // Fixed instant converted to the host zone; fixture dates follow it.
        DateTime::parse_from_rfc3339("2026-06-10T12:00:00Z").unwrap().with_timezone(&Local)
    }

    fn fixture() -> Value {
        let date = now().date_naive();
        json!({
            "nearest_area": [{"areaName": [{"value": "Sample City"}]}],
            "current_condition": [{"temp_C": "21", "FeelsLikeC": "20", "windspeedKmph": "12",
                "winddir16Point": "NW", "humidity": "65", "weatherCode": "113",
                "weatherDesc": [{"value": "Sunny"}]}],
            "weather": [
                {"date": date.to_string(), "mintempC": "11", "maxtempC": "23",
                 "astronomy": [{"sunrise": "06:00 AM", "sunset": "08:00 PM"}],
                 "hourly": [{"time": "0", "weatherCode": "200"}, {"time": "1200", "weatherCode": "113"}]},
                {"date": date.succ_opt().unwrap().to_string(), "mintempC": "10", "maxtempC": "22",
                 "hourly": [{"time": "900", "weatherCode": "116"}, {"time": "1300", "weatherCode": "119"}]}
            ]
        })
    }

    fn report() -> WeatherReport {
        parse_report(&serde_json::to_vec(&fixture()).unwrap(), &LocationQuery::default(), now()).unwrap()
    }

    fn query(name: &str) -> LocationQuery {
        parse_location(&serde_json::to_vec(&json!({"name": name})).unwrap()).unwrap()
    }

    #[test]
    fn coordinates_win_and_preserve_display_name() {
        let q = parse_location(br#"{"name":"  Sample City  ","latitude":12.5,"longitude":-45}"#).unwrap();
        assert_eq!(q.path, "12.5%2C-45");
        assert_eq!(q.name.as_deref(), Some("Sample City"));
        assert_eq!(weather_url(&q), "https://wttr.in/12.5%2C-45?format=j1");
        let r = parse_report(&serde_json::to_vec(&fixture()).unwrap(), &q, now()).unwrap();
        assert_eq!(r.location, "Sample City");
    }

    #[test]
    fn names_are_path_values_not_urls() {
        let q = query(" https://example.invalid/a?x=1#é ");
        assert_eq!(q.path, "https%3A%2F%2Fexample.invalid%2Fa%3Fx%3D1%23%C3%A9");
        assert!(weather_url(&q).starts_with("https://wttr.in/https%3A"));
        for (lat, lon) in [(91, 0), (0, 181), (-91, 0), (0, -181)] {
            let q = parse_location(&serde_json::to_vec(&json!({"name":"Fallback", "latitude":lat, "longitude":lon})).unwrap()).unwrap();
            assert_eq!(q.path, "Fallback");
        }
        let q = parse_location(br#"{"name":"Fallback","latitude":"12","longitude":13}"#).unwrap();
        assert_eq!(q.path, "Fallback");
    }

    #[test]
    fn absent_or_malformed_settings_autodetect() {
        for bytes in [b"{".as_slice(), b"null", b"[]", b"not json"] {
            assert_eq!(parse_location(bytes).unwrap_or_default(), LocationQuery::default());
        }
        assert_eq!(parse_location(br#"{"name":"  "}"#).unwrap(), LocationQuery::default());
        assert_eq!(weather_url(&LocationQuery::default()), "https://wttr.in/?format=j1");
        // This tests a missing file without reading any real location settings.
        let path = std::env::temp_dir().join(format!("weather-missing-{}", std::process::id())).join("missing.json");
        assert_eq!(read_location(&path), LocationQuery::default());
    }

    #[test]
    fn one_structured_report_supplies_all_fields() {
        let r = report();
        assert_eq!(r.temperature_c, 21.0);
        assert_eq!(r.feels_like_c, Some(20.0));
        assert_eq!(r.wind_kmh, Some(12.0));
        assert_eq!(r.wind_direction.as_deref(), Some("NW"));
        assert_eq!(r.humidity_pct, Some(65));
        assert_eq!(r.location, "Sample City");
        assert_eq!(r.description, "Sunny");
        assert_eq!(r.fetched_at, now());
        assert_eq!(r.forecast.len(), 2);
        assert_eq!(r.forecast[0].icon, icon(Some(113), false));
        assert_eq!(r.forecast[1].icon, icon(Some(119), false));
        assert_eq!(r.forecast[0].low_c, Some(11.0));
        assert_eq!(r.forecast[0].high_c, Some(23.0));
    }

    #[test]
    fn missing_optional_fields_never_become_zero() {
        let v = json!({"current_condition":[{"temp_C":"21"}], "weather":[{"date":now().date_naive().to_string()}]});
        let r = parse_report(&serde_json::to_vec(&v).unwrap(), &LocationQuery::default(), now()).unwrap();
        assert_eq!(r.feels_like_c, None);
        assert_eq!(r.wind_kmh, None);
        assert_eq!(r.humidity_pct, None);
        assert_eq!(r.wind_direction, None);
        assert_eq!(r.forecast[0].low_c, None);
        assert_eq!(r.forecast[0].high_c, None);
        assert_eq!(r.forecast[0].icon, icon(None, false));
    }

    #[test]
    fn invalid_and_oversized_data_is_rejected() {
        assert!(read_bounded(Cursor::new([1; 5]), 4).is_err());
        assert_eq!(read_bounded(Cursor::new([1; 4]), 4).unwrap(), vec![1; 4]);
        assert!(parse_location(&vec![b' '; LOCATION_LIMIT + 1]).is_err());
        assert!(parse_report(&vec![b' '; RESPONSE_LIMIT + 1], &LocationQuery::default(), now()).is_err());
        for bytes in [b"{".as_slice(), b"{}", br#"{"current_condition":[{"temp_C":"NaN"}]}"#,
            br#"{"current_condition":[{"temp_C":"inf"}]}"#] {
            assert!(parse_report(bytes, &LocationQuery::default(), now()).is_err());
        }
        let v = json!({"current_condition":[{"temp_C":21,"FeelsLikeC":"NaN", "windspeedKmph":-5,"humidity":101}]});
        let r = parse_report(&serde_json::to_vec(&v).unwrap(), &LocationQuery::default(), now()).unwrap();
        assert_eq!(r.feels_like_c, None);
        assert_eq!(r.wind_kmh, None);
        assert_eq!(r.humidity_pct, None);
    }

    #[test]
    fn forecast_uses_only_available_distinct_dates_up_to_three() {
        let mut v = fixture();
        let today = now().date_naive();
        v["weather"] = json!([
            {"date":"bad"}, {"date":today.pred_opt().unwrap().to_string()},
            {"date":today.to_string()}, {"date":today.to_string()},
            {"date":(today + chrono::Duration::days(2)).to_string()},
            {"date":(today + chrono::Duration::days(4)).to_string()},
            {"date":(today + chrono::Duration::days(5)).to_string()}
        ]);
        let r = parse_report(&serde_json::to_vec(&v).unwrap(), &LocationQuery::default(), now()).unwrap();
        assert_eq!(r.forecast.iter().map(|d| d.date).collect::<Vec<_>>(),
            vec![today, today + chrono::Duration::days(2), today + chrono::Duration::days(4)]);
    }

    #[test]
    fn native_glyphs_use_sunrise_and_sunset() {
        let day = json!({"astronomy":[{"sunrise":"06:00 AM", "sunset":"08:00 PM"}]});
        assert!(is_night(Some(&day), NaiveTime::from_hms_opt(5, 59, 0).unwrap()));
        assert!(!is_night(Some(&day), NaiveTime::from_hms_opt(6, 0, 0).unwrap()));
        assert!(is_night(Some(&day), NaiveTime::from_hms_opt(20, 0, 0).unwrap()));
        assert_eq!(icon(Some(113), true), "\u{e32b}");
        assert_eq!(icon(Some(116), false), "\u{e302}");
        assert_eq!(icon(Some(377), false), "\u{e3ad}");
        assert_eq!(icon(Some(395), true), "\u{e31d}");
        assert_eq!(icon(None, false), "\u{e33d}");
        assert!(!is_night(None, now().time().with_hour(0).unwrap()));
    }

    #[test]
    fn same_location_failure_keeps_last_good_readings() {
        let mut state = State::default();
        state.refresh(query("First"));
        let good = report();
        assert!(state.apply(Update::Finished { generation: state.generation, result: Ok(good.clone()) }));
        state.refresh(query("First"));
        assert_eq!(state.report.as_ref(), Some(&good));
        assert!(state.fetching);
        state.apply(Update::Finished { generation: state.generation, result: Err("Offline".into()) });
        assert_eq!(state.report.as_ref(), Some(&good));
        assert_eq!(state.last_success, Some(good.fetched_at));
        assert_eq!(state.error.as_deref(), Some("Offline"));
        assert!(!state.fetching);
    }

    #[test]
    fn changed_location_clears_readings_and_rejects_late_results() {
        let mut state = State::default();
        state.refresh(query("First"));
        let old_generation = state.generation;
        state.apply(Update::Finished { generation: old_generation, result: Ok(report()) });
        state.refresh(query("Second"));
        assert_eq!(state.report, None);
        assert_eq!(state.last_success, None);
        assert!(state.fetching);
        assert!(!state.apply(Update::Finished { generation: old_generation, result: Ok(report()) }));
        assert!(!state.apply(Update::Started { generation: old_generation }));
        assert_eq!(state.report, None);
        state.apply(Update::Finished { generation: state.generation, result: Err("Offline".into()) });
        assert_eq!(state.report, None);
        assert_eq!(state.last_success, None);
        assert_eq!(state.error.as_deref(), Some("Offline"));
    }

    #[test]
    fn poll_policy_preserves_existing_intervals() {
        assert_eq!(poll_delay(true), Duration::from_secs(600));
        assert_eq!(poll_delay(false), Duration::from_secs(30));
    }

    #[test]
    fn worker_coalesces_refreshes_and_wakes_without_sleeping() {
        let (tx, rx) = mpsc::channel();
        for generation in 1..=3 {
            tx.send(Command::Refresh { query: query("First"), generation }).unwrap();
        }
        let seen = RefCell::new(Vec::new());
        let fetched = RefCell::new(Vec::new());
        run_worker(rx, |update| {
            match update {
                Update::Started { generation } => seen.borrow_mut().push(generation),
                Update::Finished { generation: 3, .. } => {
                    tx.send(Command::Refresh { query: query("Second"), generation: 4 }).unwrap();
                }
                Update::Finished { generation: 4, .. } => return false,
                _ => panic!("unexpected generation"),
            }
            true
        }, |q| {
            fetched.borrow_mut().push(q.name.clone());
            Ok(report())
        });
        assert_eq!(*seen.borrow(), vec![3, 4]);
        assert_eq!(*fetched.borrow(), vec![Some("First".into()), Some("Second".into())]);
    }

    #[test]
    fn worker_late_result_cannot_restore_old_location() {
        let state = RefCell::new(State::default());
        let (tx, rx) = mpsc::channel();
        tx.send(state.borrow_mut().refresh(query("First"))).unwrap();
        let mut calls = 0;
        run_worker(rx, |update| {
            let finished_new = matches!(&update, Update::Finished { generation: 2, .. });
            let old = matches!(&update, Update::Finished { generation: 1, .. });
            let accepted = state.borrow_mut().apply(update);
            if old {
                assert!(!accepted);
                assert!(state.borrow().report.is_none());
            }
            !finished_new
        }, |q| {
            calls += 1;
            if calls == 1 {
                tx.send(state.borrow_mut().refresh(query("Second"))).unwrap();
            }
            let mut r = report();
            r.location = q.name.clone().unwrap();
            Ok(r)
        });
        assert_eq!(calls, 2);
        assert_eq!(state.borrow().report.as_ref().unwrap().location, "Second");
    }

    #[test]
    fn disconnected_command_channel_stops_worker() {
        let (tx, rx) = mpsc::channel();
        drop(tx);
        run_worker(rx, |_| panic!("no work after disconnect"), |_| panic!("no network after disconnect"));
    }
}
