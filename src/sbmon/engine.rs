//! The sing-box monitor's state machine, ported from the Komari plugin
//! `komari-singbox-monitor` (plugin/script.js, 0.3.2).
//!
//! Everything here is pure: callers pass the current time in and do the file
//! and network work themselves (see `super::http`). The persisted shape is the
//! plugin's `state.json`, field for field, so a Komari install migrates by
//! copying that file and the `history/` directory beside it, and the sb-agents
//! on the landing nodes keep their node ids and tokens.

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub const VERSION: &str = "0.7.0";

const MAX_EVENTS: usize = 500;
const MAX_USER_EVENTS: usize = 300;
const MAX_PENDING: usize = 100;
const MAX_SESSION_HOSTS: usize = 200;
const MAX_RECENT_DOMAINS: usize = 60;
pub const TG_CHUNK: usize = 3500;
/// A watched session is only closed while its node's agent is still reporting.
/// If the agent goes quiet we cannot tell whether the user left, so wait; after
/// `AGENT_LOST_MS` the session is closed with a note.
const AGENT_SILENT_MS: i64 = 30_000;
const AGENT_LOST_MS: i64 = 600_000;
/// How long sing-box may be reported stopped before an alert: a restart takes
/// seconds, and an alert for every config reload would be noise.
const STATUS_GRACE_MS: i64 = 60_000;
/// How long an sb-agent may stay silent before an alert.
const AGENT_LOST_ALERT_MS: i64 = 300_000;
/// Daily traffic buckets older than this are dropped. The plugin kept them
/// forever; the panel only ever reads today and this month.
const DAILY_KEEP_DAYS: i64 = 400;
/// Open connections kept per watched inbound for the live view.
const MAX_OPEN: usize = 100;
/// Speed is the traffic over about this long, from the latest readings.
const RATE_WINDOW_MS: i64 = 4_000;
/// A speed reading older than this shows as zero: the agent went quiet.
const RATE_STALE_MS: i64 = 12_000;

// ---- time and text helpers -------------------------------------------------

/// The plugin's timestamp format, `Date.prototype.toISOString`: fixed width, so
/// two of them compare as strings the way they compare as times.
pub fn iso(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc))
}

fn millis(s: &str) -> Option<i64> {
    parse_time(s).map(|d| d.timestamp_millis())
}

pub fn sha256_hex(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::fill(&mut buf[..]);
    hex::encode(buf)
}

/// `String(value || "")` from the plugin: what a JSON value reads as text.
fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => {
            if n.as_f64() == Some(0.0) {
                String::new()
            } else {
                n.to_string()
            }
        }
        Value::Bool(true) => "true".into(),
        _ => String::new(),
    }
}

/// `Number(value)` restricted to what a report sends: numbers, or numeric text.
fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) if s.trim().is_empty() => Some(0.0),
        Value::String(s) => s.trim().parse().ok(),
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        Value::Null => Some(0.0),
        _ => None,
    }
}

/// Removes control characters except tab and newlines, then cuts to `max` characters.
pub fn clean_text(value: &str, max: usize) -> String {
    value
        .chars()
        .filter(|&c| !matches!(c, '\0' | '\u{8}' | '\u{b}' | '\u{c}' | '\u{e}'..='\u{1f}' | '\u{7f}'))
        .take(max)
        .collect()
}

/// A display name: trimmed, no control characters at all, at most 80 characters.
pub fn clean_name(value: &str) -> String {
    value.trim().chars().filter(|&c| !matches!(c, '\0'..='\u{1f}' | '\u{7f}')).take(80).collect()
}

fn port_key(v: &Value) -> String {
    match number(v) {
        Some(p) if p.fract() == 0.0 && (1.0..=65535.0).contains(&p) => format!("{}", p as i64),
        _ => String::new(),
    }
}

fn escape_html(value: &str) -> String {
    value.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

// ---- configuration ---------------------------------------------------------

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Rule {
    pub tag: String,
    pub label: String,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub timezone_offset: f64,
    pub offline_seconds: f64,
    pub history_days: f64,
    /// Days of per-user connection log kept (see `userlog`).
    pub userlog_days: f64,
    pub session_idle_seconds: f64,
    pub offline_grace_seconds: f64,
    pub online_confirm_seconds: f64,
    pub watch_rules: Vec<Rule>,
    pub tg_bot_token: String,
    pub tg_chat_id: String,
    pub notify_online: bool,
    pub notify_offline: bool,
    /// sing-box stopped / recovered and sb-agent lost / back, through the same bot.
    pub notify_status: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self::from_lookup(|_| None)
    }
}

/// Every setting the module keeps, under its Komari plugin key. Stored in the
/// hub's `setting` table with an `sbmon_` prefix.
pub const CONFIG_KEYS: [&str; 13] = [
    "timezone_offset",
    "offline_seconds",
    "history_days",
    "userlog_days",
    "session_idle_seconds",
    "offline_grace_seconds",
    "online_confirm_seconds",
    "watch_rules",
    "tg_bot_token",
    "tg_chat_id",
    "notify_online",
    "notify_offline",
    "notify_status",
];

fn clamp_number(value: Option<String>, fallback: f64, min: f64, max: f64) -> f64 {
    match value.and_then(|v| v.trim().parse::<f64>().ok()) {
        Some(n) if n.is_finite() => n.clamp(min, max),
        _ => fallback,
    }
}

/// `tag=label` pairs separated by commas or newlines. A bare `tag` is its own label.
pub fn parse_watch_rules(raw: &str) -> Vec<Rule> {
    let mut rules = Vec::new();
    let mut seen = HashSet::new();
    for piece in raw.split([',', '\n']) {
        let trimmed = piece.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (tag, label) = match trimmed.split_once('=') {
            Some((t, l)) => (t, l),
            None => (trimmed, trimmed),
        };
        let tag = clean_text(tag, 100).trim().to_owned();
        if tag.is_empty() || !seen.insert(tag.clone()) {
            continue;
        }
        let label = Some(clean_name(label)).filter(|l| !l.is_empty()).unwrap_or_else(|| tag.clone());
        rules.push(Rule { tag, label });
    }
    rules
}

impl Config {
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let switch = |key: &str| get(key).is_none_or(|v| v != "false" && v != "off" && v != "0");
        Self {
            timezone_offset: clamp_number(get("timezone_offset"), 8.0, -12.0, 14.0),
            offline_seconds: clamp_number(get("offline_seconds"), 60.0, 15.0, 3600.0),
            history_days: clamp_number(get("history_days"), 30.0, 1.0, 365.0),
            userlog_days: clamp_number(get("userlog_days"), 7.0, 1.0, 90.0),
            session_idle_seconds: clamp_number(get("session_idle_seconds"), 300.0, 30.0, 86400.0),
            offline_grace_seconds: clamp_number(get("offline_grace_seconds"), 60.0, 10.0, 3600.0),
            online_confirm_seconds: clamp_number(get("online_confirm_seconds"), 15.0, 0.0, 600.0),
            watch_rules: parse_watch_rules(&get("watch_rules").unwrap_or_default()),
            tg_bot_token: clean_text(&get("tg_bot_token").unwrap_or_default(), 100).trim().to_owned(),
            tg_chat_id: clean_text(&get("tg_chat_id").unwrap_or_default(), 60).trim().to_owned(),
            notify_online: switch("notify_online"),
            notify_offline: switch("notify_offline"),
            notify_status: switch("notify_status"),
        }
    }

    pub fn telegram_configured(&self) -> bool {
        !self.tg_bot_token.is_empty() && !self.tg_chat_id.is_empty()
    }

    fn rule_for(&self, tag: &str) -> Option<&Rule> {
        self.watch_rules.iter().find(|r| r.tag == tag)
    }

    fn shift(&self, t: DateTime<Utc>) -> DateTime<Utc> {
        t + chrono::Duration::milliseconds((self.timezone_offset * 3_600_000.0) as i64)
    }

    /// `YYYY-MM-DD` in the statistics time zone.
    pub fn date_key(&self, t: DateTime<Utc>) -> String {
        self.shift(t).format("%Y-%m-%d").to_string()
    }

    fn local_label(&self, at: &str, with_date: bool) -> String {
        let Some(t) = parse_time(at) else { return at.to_owned() };
        let s = self.shift(t);
        if with_date {
            s.format("%m-%d %H:%M:%S").to_string()
        } else {
            s.format("%H:%M:%S").to_string()
        }
    }

    fn local_day(&self, at: &str) -> String {
        parse_time(at).map(|t| self.date_key(t)).unwrap_or_default()
    }

    /// What the panel may see: the bot token is reported only as set or unset.
    pub fn view(&self) -> Value {
        json!({
            "timezoneOffset": self.timezone_offset,
            "offlineSeconds": self.offline_seconds,
            "historyDays": self.history_days,
            "userlogDays": self.userlog_days,
            "sessionIdleSeconds": self.session_idle_seconds,
            "offlineGraceSeconds": self.offline_grace_seconds,
            "onlineConfirmSeconds": self.online_confirm_seconds,
            "watchRules": self.watch_rules,
            "tgChatId": self.tg_chat_id,
            "tgConfigured": self.telegram_configured(),
            "notifyOnline": self.notify_online,
            "notifyOffline": self.notify_offline,
            "notifyStatus": self.notify_status,
        })
    }
}

// ---- persisted state (the plugin's state.json) -----------------------------

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Singbox {
    pub running: bool,
    pub version: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Inbound {
    pub port: i64,
    #[serde(rename = "type")]
    pub kind: String,
    pub tag: String,
    pub users: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct Counter {
    pub port: i64,
    pub upload_total: f64,
    pub download_total: f64,
    pub upload_rate: f64,
    pub download_rate: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Totals {
    pub upload: f64,
    pub download: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct Node {
    pub id: String,
    pub name: String,
    pub hostname: String,
    pub arch: String,
    pub agent_version: String,
    pub agent_token_hash: String,
    pub created_at: String,
    pub last_seen: Option<String>,
    pub singbox: Singbox,
    pub inbounds: Vec<Inbound>,
    pub aliases: BTreeMap<String, String>,
    pub current: BTreeMap<String, Counter>,
    pub last_totals: BTreeMap<String, Totals>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_activity: Option<Value>,
    /// The hub node this sb-agent was installed with, when it came in through the
    /// hub's own agent install (`install.sh` registers it with the node token).
    /// Such a node takes its name from the hub node and goes when that node does.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hub_node_id: Option<i64>,
    /// Watching users is switched off for this server in the panel: its
    /// inbounds are still counted, but nobody on it goes online or offline.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub watch_off: bool,
    /// When sing-box was first reported stopped, for as long as it stays stopped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub singbox_down_since: Option<String>,
    /// A stop alert went out for the current outage.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub singbox_down_alerted: bool,
    /// An alert went out that this sb-agent stopped reporting.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub agent_lost_alerted: bool,
    /// Last time (ms) the hub saw this machine's own agent disconnected. The
    /// sb-agent's silence only counts from here, so when the machine, or the
    /// hub's own network, comes back the sb-agent gets the full grace again.
    /// Not persisted: after a restart every agent starts disconnected anyway.
    #[serde(skip)]
    pub host_down_ms: Option<i64>,
    /// Each inbound's byte totals (upload, download) as the agent last counted
    /// them (sb-agent 0.7.0 `tag_bytes`), to take differences against.
    #[serde(skip)]
    pub tag_bytes: BTreeMap<String, (f64, f64)>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct Registration {
    pub name: String,
    pub expires_at: String,
    pub used: bool,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct Event {
    pub id: String,
    pub hash: String,
    pub node_id: String,
    pub time: String,
    pub level: String,
    pub message: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Visit {
    pub host: String,
    pub first: String,
    pub last: String,
    pub count: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct Watch {
    pub node_id: String,
    pub tag: String,
    pub label: String,
    pub online: bool,
    pub pending: bool,
    pub session_start: Option<String>,
    pub session_conn: i64,
    pub last_active: Option<String>,
    pub last_source: String,
    pub conn_count: i64,
    pub domains: Vec<String>,
    pub live_tracked: bool,
    pub live: i64,
    pub visits: Vec<Visit>,
    pub visits_dropped: i64,
    /// Traffic of the current session (sb-agent 0.7.0 with clash_api).
    pub session_up: f64,
    pub session_down: f64,
    /// Connections open right now, as the agent last listed them.
    #[serde(skip)]
    pub open: Vec<OpenConn>,
    /// (ms, session upload, session download) readings for the speed.
    #[serde(skip)]
    pub samples: VecDeque<(i64, f64, f64)>,
}

/// One connection open on a watched inbound.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct OpenConn {
    pub dst: String,
    pub net: String,
    /// Unix ms.
    pub start: i64,
    pub up: f64,
    pub down: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct UserEvent {
    pub id: String,
    pub kind: String,
    pub label: String,
    pub tag: String,
    pub node_id: String,
    pub node_name: String,
    pub source: String,
    pub domains: Vec<String>,
    pub time: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_start: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_conn: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visits: Option<Vec<Visit>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_up: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_down: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Pending {
    pub text: String,
    pub at: String,
}

type Daily = BTreeMap<String, BTreeMap<String, BTreeMap<String, Totals>>>;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct State {
    pub schema: u32,
    pub nodes: BTreeMap<String, Node>,
    pub registrations: BTreeMap<String, Registration>,
    pub daily: Daily,
    /// Traffic per inbound user, `day → node → "tag|user" → totals`, from the
    /// agents' clash_api readings (sb-agent 0.5.0 with clash_api enabled).
    pub daily_users: Daily,
    pub events: Vec<Event>,
    pub watch: BTreeMap<String, Watch>,
    pub user_events: Vec<UserEvent>,
    pub pending: Vec<Pending>,
    pub updated_at: String,
}

impl Default for State {
    fn default() -> Self {
        Self {
            schema: 2,
            nodes: BTreeMap::new(),
            registrations: BTreeMap::new(),
            daily: BTreeMap::new(),
            daily_users: BTreeMap::new(),
            events: Vec::new(),
            watch: BTreeMap::new(),
            user_events: Vec::new(),
            pending: Vec::new(),
            updated_at: String::new(),
        }
    }
}

/// A refusal in the agent API: status and the message the agent prints.
#[derive(Debug, PartialEq)]
pub struct Refusal(pub u16, pub &'static str);

/// One line of minute history for one node: `{time, points:[...]}`.
pub struct HistoryLine {
    pub node_id: String,
    pub day: String,
    pub line: String,
}

// ---- the engine ------------------------------------------------------------

pub struct Engine {
    pub state: State,
    pub config: Config,
    /// Counts alerts queued and user events recorded, which are written to disk
    /// at once; everything else waits for the next periodic save. A counter
    /// rather than the queue's length, which stops growing at its cap.
    pub notable: u64,
}

fn watch_key(node_id: &str, tag: &str) -> String {
    format!("{node_id}:{tag}")
}

fn calculate_delta(current: f64, previous: f64) -> f64 {
    if !previous.is_finite() || previous < 0.0 {
        return 0.0;
    }
    if current >= previous {
        current - previous
    } else {
        current
    }
}

fn safe_count(v: &Value) -> f64 {
    match v.as_f64() {
        Some(n) if n >= 0.0 && n.fract() == 0.0 && n <= 9_007_199_254_740_991.0 => n,
        _ => 0.0,
    }
}

fn clean_inbound(item: &Value) -> Option<Inbound> {
    let item = item.as_object()?;
    let port = item.get("port").and_then(Value::as_f64)?;
    if port.fract() != 0.0 || !(1.0..=65535.0).contains(&port) {
        return None;
    }
    let users = item
        .get("users")
        .and_then(Value::as_array)
        .map(|list| list.iter().map(|u| clean_name(&text(u))).filter(|u| !u.is_empty()).take(100).collect())
        .unwrap_or_default();
    Some(Inbound {
        port: port as i64,
        kind: clean_text(&text(item.get("type").unwrap_or(&Value::Null)), 40),
        tag: clean_text(&text(item.get("tag").unwrap_or(&Value::Null)), 100),
        users,
    })
}

fn clean_counter(item: &Value) -> Option<Counter> {
    let item = item.as_object()?;
    let port = item.get("port").and_then(Value::as_f64)?;
    if port.fract() != 0.0 || !(1.0..=65535.0).contains(&port) {
        return None;
    }
    let get = |k: &str| safe_count(item.get(k).unwrap_or(&Value::Null));
    Some(Counter {
        port: port as i64,
        upload_total: get("upload_total"),
        download_total: get("download_total"),
        upload_rate: get("upload_rate"),
        download_rate: get("download_rate"),
    })
}

fn human_duration(from: &str, to: &str) -> String {
    let (Some(a), Some(b)) = (millis(from), millis(to)) else { return "0s".into() };
    let seconds = ((b - a) as f64 / 1000.0).round().max(0.0) as i64;
    let (hours, minutes) = (seconds / 3600, (seconds % 3600) / 60);
    if hours > 0 {
        format!("{hours}h{minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{seconds}s")
    }
}

/// Bytes as the panel shows them: 1024-based, three significant figures.
pub fn human_bytes(value: f64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = value.max(0.0);
    let mut unit = 0;
    while v >= 1024.0 && unit < units.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{v:.0} B")
    } else if v >= 100.0 {
        format!("{v:.0} {}", units[unit])
    } else if v >= 10.0 {
        format!("{v:.1} {}", units[unit])
    } else {
        format!("{v:.2} {}", units[unit])
    }
}

/// Upload and download per second over the latest readings: from the newest
/// back to the oldest one within `RATE_WINDOW_MS`, or the one before the newest
/// when that is all there is (agents reporting every five seconds).
fn speed(samples: &VecDeque<(i64, f64, f64)>, now_ms: i64) -> (f64, f64) {
    let Some(&(last_ms, up, down)) = samples.back() else { return (0.0, 0.0) };
    if now_ms - last_ms > RATE_STALE_MS || samples.len() < 2 {
        return (0.0, 0.0);
    }
    let previous = samples.iter().rev().skip(1);
    let mut base = samples[samples.len() - 2];
    for &sample in previous {
        if last_ms - sample.0 > RATE_WINDOW_MS {
            break;
        }
        base = sample;
    }
    let seconds = (last_ms - base.0) as f64 / 1000.0;
    if seconds <= 0.0 {
        return (0.0, 0.0);
    }
    (((up - base.1) / seconds).max(0.0), ((down - base.2) / seconds).max(0.0))
}

fn sorted_visits(watch: &Watch) -> Vec<Visit> {
    let mut visits = watch.visits.clone();
    visits.sort_by(|a, b| a.first.cmp(&b.first));
    visits
}

fn add_visit(watch: &mut Watch, host: &str, at: &str) {
    let item = match watch.visits.iter_mut().position(|v| v.host == host) {
        Some(i) => &mut watch.visits[i],
        None => {
            if watch.visits.len() >= MAX_SESSION_HOSTS {
                watch.visits_dropped += 1;
                return;
            }
            watch.visits.push(Visit { host: host.into(), first: at.into(), last: at.into(), count: 0 });
            watch.visits.last_mut().expect("just pushed")
        }
    };
    item.count += 1;
    if at < item.first.as_str() {
        item.first = at.into();
    }
    if at > item.last.as_str() {
        item.last = at.into();
    }
}

struct Activity {
    conn: i64,
    live: i64,
    live_tracked: bool,
    source: String,
    domains: Vec<String>,
    visits: Vec<(String, String)>,
}

impl Engine {
    pub fn new(state: State, config: Config) -> Self {
        Self { state, config, notable: 0 }
    }

    fn push_user_event(
        &mut self,
        kind: &str,
        watch: &Watch,
        node_name: &str,
        time: &str,
        domains: Vec<String>,
    ) -> &mut UserEvent {
        self.notable += 1;
        self.state.user_events.push(UserEvent {
            id: random_hex(8),
            kind: kind.into(),
            label: watch.label.clone(),
            tag: watch.tag.clone(),
            node_id: watch.node_id.clone(),
            node_name: node_name.into(),
            source: watch.last_source.clone(),
            domains: domains.into_iter().take(8).collect(),
            time: time.into(),
            ..Default::default()
        });
        if self.state.user_events.len() > MAX_USER_EVENTS {
            let drop = self.state.user_events.len() - MAX_USER_EVENTS;
            self.state.user_events.drain(..drop);
        }
        self.state.user_events.last_mut().expect("just pushed")
    }

    pub fn enqueue_telegram(&mut self, text: String, now: DateTime<Utc>) {
        self.notable += 1;
        self.state.pending.push(Pending { text, at: iso(now) });
        if self.state.pending.len() > MAX_PENDING {
            let drop = self.state.pending.len() - MAX_PENDING;
            self.state.pending.drain(..drop);
        }
    }

    fn cleanup_registrations(&mut self, now: DateTime<Utc>) {
        let now = now.timestamp_millis();
        self.state.registrations.retain(|_, r| millis(&r.expires_at).is_some_and(|t| t > now));
    }

    fn add_daily(&mut self, node_id: &str, port: &str, upload: f64, download: f64, now: DateTime<Utc>) {
        let day = self.config.date_key(now);
        let slot = self
            .state
            .daily
            .entry(day)
            .or_default()
            .entry(node_id.into())
            .or_default()
            .entry(port.into())
            .or_default();
        slot.upload += upload;
        slot.download += download;
    }

    // ---- agent API ----

    pub fn register(&mut self, body: &Value, now: DateTime<Utc>) -> Result<Value, Refusal> {
        self.cleanup_registrations(now);
        let hash = sha256_hex(&text(body.get("registration_token").unwrap_or(&Value::Null)));
        let Some(registration) = self.state.registrations.get(&hash) else {
            return Err(Refusal(401, "注册密钥无效或已使用"));
        };
        if registration.used {
            return Err(Refusal(401, "注册密钥无效或已使用"));
        }
        if millis(&registration.expires_at).is_none_or(|t| t <= now.timestamp_millis()) {
            return Err(Refusal(401, "注册密钥已过期"));
        }
        let requested = clean_name(&text(body.get("name").unwrap_or(&Value::Null)));
        let name = if requested.is_empty() { registration.name.clone() } else { requested };
        if name.is_empty() {
            return Err(Refusal(400, "节点名称不能为空"));
        }
        let node_id = random_hex(16);
        let agent_token = random_hex(32);
        let field = |k: &str, max| clean_text(&text(body.get(k).unwrap_or(&Value::Null)), max);
        self.state.nodes.insert(
            node_id.clone(),
            Node {
                id: node_id.clone(),
                name: name.clone(),
                hostname: field("hostname", 120),
                arch: field("arch", 30),
                agent_version: field("agent_version", 30),
                agent_token_hash: sha256_hex(&agent_token),
                created_at: iso(now),
                ..Default::default()
            },
        );
        let registration = self.state.registrations.get_mut(&hash).expect("checked above");
        registration.used = true;
        registration.node_id = Some(node_id.clone());
        Ok(json!({"ok": true, "node_id": node_id, "agent_token": agent_token, "name": name}))
    }

    pub fn report(&mut self, bearer: &str, body: &Value, now: DateTime<Utc>) -> Result<Value, Refusal> {
        let node_id = clean_text(&text(body.get("node_id").unwrap_or(&Value::Null)), 100);
        let valid_id = node_id.len() == 32 && node_id.bytes().all(|b| matches!(b, b'a'..=b'f' | b'0'..=b'9'));
        let authed = valid_id
            && self.state.nodes.get(&node_id).is_some_and(|n| sha256_hex(bearer) == n.agent_token_hash);
        if !authed {
            return Err(Refusal(401, "Agent 身份验证失败"));
        }
        let now_iso = iso(now);
        let list = |k: &str| body.get(k).and_then(Value::as_array).cloned().unwrap_or_default();
        let inbounds: Vec<Inbound> = list("inbounds").iter().filter_map(clean_inbound).collect();
        let counters: Vec<Counter> = list("counters").iter().filter_map(clean_counter).collect();

        // Traffic deltas against the previous totals, booked by receipt time:
        // an agent's clock must not create arbitrary daily buckets.
        let mut deltas = Vec::new();
        let mut status = Vec::new();
        {
            let node = self.state.nodes.get_mut(&node_id).expect("authed");
            let field = |k: &str| text(body.get(k).unwrap_or(&Value::Null));
            let keep = |new: String, old: &str, max| clean_text(if new.is_empty() { old } else { &new }, max);
            node.hostname = keep(field("hostname"), &node.hostname, 120);
            node.arch = keep(field("arch"), &node.arch, 30);
            node.agent_version = keep(field("agent_version"), &node.agent_version, 30);
            node.last_seen = Some(now_iso.clone());
            let singbox = body.get("singbox");
            node.singbox = Singbox {
                running: singbox.and_then(|s| s.get("running")).and_then(Value::as_bool).unwrap_or(false),
                version: clean_text(
                    &singbox.and_then(|s| s.get("version")).map(text).unwrap_or_default(),
                    120,
                ),
            };
            node.inbounds = inbounds;
            if node.agent_lost_alerted {
                node.agent_lost_alerted = false;
                status.push(format!("\u{2705} <b>sb-agent 恢复上报</b>\n节点  {}", escape_html(&node.name)));
            }
            if node.singbox.running {
                if node.singbox_down_alerted {
                    let since = node.singbox_down_since.clone().unwrap_or_else(|| now_iso.clone());
                    status.push(format!(
                        "\u{1F7E2} <b>sing-box 已恢复</b>\n节点  {}\n停了  {}",
                        escape_html(&node.name),
                        human_duration(&since, &now_iso)
                    ));
                }
                node.singbox_down_since = None;
                node.singbox_down_alerted = false;
            } else if node.singbox_down_since.is_none() {
                node.singbox_down_since = Some(now_iso.clone());
            }
            let mut next = BTreeMap::new();
            for counter in counters {
                let key = port_key(&json!(counter.port));
                if key.is_empty() || next.contains_key(&key) {
                    continue;
                }
                if let Some(previous) = node.last_totals.get(&key) {
                    deltas.push((
                        key.clone(),
                        calculate_delta(counter.upload_total, previous.upload),
                        calculate_delta(counter.download_total, previous.download),
                    ));
                }
                node.last_totals.insert(
                    key.clone(),
                    Totals { upload: counter.upload_total, download: counter.download_total },
                );
                next.insert(key, counter);
            }
            node.current = next;
        }
        for (port, up, down) in deltas {
            self.add_daily(&node_id, &port, up, down, now);
        }
        for text in status {
            self.enqueue_telegram(text, now);
        }
        if let Some(items) = body.get("user_traffic").and_then(Value::as_array) {
            let day = self.config.date_key(now);
            for item in items.iter().take(500) {
                let get = |k: &str| item.get(k).unwrap_or(&Value::Null);
                let tag = clean_text(&text(get("tag")), 100);
                if tag.is_empty() {
                    continue;
                }
                let user = clean_name(&text(get("user")));
                let (up, down) = (safe_count(get("upload")), safe_count(get("download")));
                if up == 0.0 && down == 0.0 {
                    continue;
                }
                let slot = self
                    .state
                    .daily_users
                    .entry(day.clone())
                    .or_default()
                    .entry(node_id.clone())
                    .or_default()
                    .entry(format!("{tag}|{user}"))
                    .or_default();
                slot.upload += up;
                slot.download += down;
            }
        }

        if let Some(events) = body.get("events").and_then(Value::as_array) {
            let start = self.state.events.len().saturating_sub(100);
            let mut known: HashSet<String> =
                self.state.events[start..].iter().map(|e| e.hash.clone()).collect();
            for raw in events.iter().take(50) {
                let message = clean_text(&text(raw), 2000).trim().to_owned();
                if message.is_empty() {
                    continue;
                }
                let hash = sha256_hex(&format!("{node_id}:{message}"));
                if !known.insert(hash.clone()) {
                    continue;
                }
                self.state.events.push(Event {
                    id: random_hex(8),
                    hash,
                    node_id: node_id.clone(),
                    time: now_iso.clone(),
                    level: "WARN".into(),
                    message,
                });
            }
            if self.state.events.len() > MAX_EVENTS {
                let drop = self.state.events.len() - MAX_EVENTS;
                self.state.events.drain(..drop);
            }
        }

        // Tags the agent reported this time. One it left out had nothing to say:
        // no new connection, no probe and nothing still open.
        let mut reported = BTreeSet::new();
        if let Some(activity) = body.get("activity").and_then(Value::as_array) {
            let mut seen = Vec::new();
            for raw in activity.iter().take(50) {
                let Some(raw) = raw.as_object() else { continue };
                let get = |k: &str| raw.get(k).unwrap_or(&Value::Null);
                let tag = clean_text(&text(get("tag")), 100);
                // A missing count is NaN in the plugin, and the entry is skipped.
                let Some(conn) = raw.get("conn_count").and_then(number).filter(|c| c.is_finite()) else {
                    continue;
                };
                if tag.is_empty() {
                    continue;
                }
                let source = clean_text(&text(get("last_source")), 120);
                let domains: Vec<String> = get("domains")
                    .as_array()
                    .map(|d| {
                        d.iter()
                            .map(|x| clean_text(&text(x), 253))
                            .filter(|x| !x.is_empty())
                            .take(50)
                            .collect()
                    })
                    .unwrap_or_default();
                let probes = number(get("probes")).filter(|n| n.is_finite()).unwrap_or(0.0);
                let live = number(get("live")).filter(|n| n.is_finite()).unwrap_or(0.0);
                let now_ms = now.timestamp_millis();
                let mut visits = Vec::new();
                for item in get("visits").as_array().map(|v| &v[..v.len().min(500)]).unwrap_or(&[]) {
                    let Some(item) = item.as_object() else { continue };
                    let host = clean_text(&text(item.get("host").unwrap_or(&Value::Null)), 253);
                    if host.is_empty() {
                        continue;
                    }
                    // Trust the agent's log time only when it is plausible.
                    let at = item.get("t").and_then(number).map(|t| t * 1000.0);
                    let time = match at {
                        Some(at)
                            if at.is_finite()
                                && at <= (now_ms + 60_000) as f64
                                && at >= (now_ms - 2 * 86_400_000) as f64 =>
                        {
                            DateTime::from_timestamp_millis((at as i64).min(now_ms))
                                .map(iso)
                                .unwrap_or_else(|| now_iso.clone())
                        }
                        _ => now_iso.clone(),
                    };
                    visits.push((host, time));
                }
                seen.push(json!({"tag": tag, "conn": conn, "source": source, "probes": probes, "live": live, "domains": domains.len()}));
                reported.insert(tag.clone());
                let activity = Activity {
                    conn: conn.floor().max(0.0) as i64,
                    live: live.floor().max(0.0) as i64,
                    live_tracked: get("live_tracked") == &Value::Bool(true),
                    source,
                    domains,
                    visits,
                };
                self.handle_activity(&node_id, &tag, activity, now);
            }
            // Diagnostic: last raw activity the agent reported, independent of watch_rules.
            if let Some(node) = self.state.nodes.get_mut(&node_id) {
                node.last_activity = Some(json!({"at": now_iso, "items": seen}));
            }
        }
        // Nothing open on a watched tag the agent left out. Without this the
        // panel kept showing the last count through the offline grace period.
        for watch in self.state.watch.values_mut() {
            if watch.node_id == node_id && !reported.contains(&watch.tag) {
                watch.live = 0;
            }
        }
        self.apply_live(&node_id, body, now);
        Ok(self.agent_answer(&node_id, &now_iso))
    }

    /// What the agent is told after a report: which inbounds to list open
    /// connections for, and whether to send a live report every second (a
    /// watched user is online, or about to be, on this machine).
    fn agent_answer(&self, node_id: &str, now_iso: &str) -> Value {
        let node = self.state.nodes.get(node_id);
        let watching = node.is_some_and(|n| !n.watch_off);
        let tags: Vec<&String> = if watching {
            node.map(|n| {
                n.inbounds.iter().map(|i| &i.tag).filter(|t| self.config.rule_for(t).is_some()).collect()
            })
            .unwrap_or_default()
        } else {
            Vec::new()
        };
        let fast =
            watching && self.state.watch.values().any(|w| w.node_id == node_id && (w.online || w.pending));
        json!({"ok": true, "server_time": now_iso, "watch_tags": tags, "fast": fast})
    }

    /// A live report (sb-agent 0.7.0 `agent/live`, every second while a watched
    /// user is online there): open connections per inbound, each inbound's byte
    /// totals, and the live count, which keeps the session going and may
    /// confirm a tentative one.
    pub fn live(&mut self, bearer: &str, body: &Value, now: DateTime<Utc>) -> Result<Value, Refusal> {
        let node_id = clean_text(&text(body.get("node_id").unwrap_or(&Value::Null)), 100);
        let authed = self.state.nodes.get(&node_id).is_some_and(|n| sha256_hex(bearer) == n.agent_token_hash);
        if !authed {
            return Err(Refusal(401, "Agent 身份验证失败"));
        }
        let now_iso = iso(now);
        if let Some(node) = self.state.nodes.get_mut(&node_id) {
            node.last_seen = Some(now_iso.clone());
        }
        let watch_off = self.state.nodes.get(&node_id).is_some_and(|n| n.watch_off);
        for item in body.get("live").and_then(Value::as_array).map(|v| &v[..v.len().min(50)]).unwrap_or(&[]) {
            let tag = clean_text(&text(item.get("tag").unwrap_or(&Value::Null)), 100);
            let live =
                item.get("live").and_then(number).filter(|n| n.is_finite()).unwrap_or(0.0).max(0.0) as i64;
            let key = watch_key(&node_id, &tag);
            if watch_off {
                continue;
            }
            let Some(mut watch) = self.state.watch.remove(&key) else { continue };
            if watch.online || watch.pending {
                watch.live = live;
                watch.live_tracked = true;
                if live > 0 {
                    watch.last_active = Some(now_iso.clone());
                }
                self.confirm(&mut watch, live, now);
            }
            self.state.watch.insert(key, watch);
        }
        self.apply_live(&node_id, body, now);
        Ok(self.agent_answer(&node_id, &now_iso))
    }

    /// Books `tag_bytes` and `open` from a report or a live report onto the
    /// sessions in progress on that node.
    fn apply_live(&mut self, node_id: &str, body: &Value, now: DateTime<Utc>) {
        let now_ms = now.timestamp_millis();
        if let Some(items) = body.get("tag_bytes").and_then(Value::as_array) {
            for item in items.iter().take(200) {
                let tag = clean_text(&text(item.get("tag").unwrap_or(&Value::Null)), 100);
                if tag.is_empty() {
                    continue;
                }
                let (up, down) = (
                    safe_count(item.get("up").unwrap_or(&Value::Null)),
                    safe_count(item.get("down").unwrap_or(&Value::Null)),
                );
                let Some(node) = self.state.nodes.get_mut(node_id) else { return };
                // The first reading after a hub restart only sets the base;
                // totals that went down mean the agent restarted from zero.
                let delta = match node.tag_bytes.insert(tag.clone(), (up, down)) {
                    Some((pu, pd)) if up >= pu && down >= pd => (up - pu, down - pd),
                    Some(_) => (up, down),
                    None => (0.0, 0.0),
                };
                if let Some(watch) = self.state.watch.get_mut(&watch_key(node_id, &tag)) {
                    if watch.online || watch.pending {
                        watch.session_up += delta.0;
                        watch.session_down += delta.1;
                        watch.samples.push_back((now_ms, watch.session_up, watch.session_down));
                        while watch.samples.len() > 12 {
                            watch.samples.pop_front();
                        }
                    }
                }
            }
        }
        if let Some(items) = body.get("open").and_then(Value::as_array) {
            let mut by_tag: BTreeMap<String, Vec<OpenConn>> = BTreeMap::new();
            for item in items.iter().take(1000) {
                let get = |k: &str| item.get(k).unwrap_or(&Value::Null);
                let tag = clean_text(&text(get("tag")), 100);
                let dst = clean_text(&text(get("dst")), 253).trim().to_lowercase();
                if tag.is_empty() || dst.is_empty() {
                    continue;
                }
                let start = number(get("start"))
                    .filter(|t| {
                        t.is_finite()
                            && *t <= (now_ms + 60_000) as f64
                            && *t >= (now_ms - 30 * 86_400_000) as f64
                    })
                    .map_or(now_ms, |t| (t as i64).min(now_ms));
                let list = by_tag.entry(tag).or_default();
                if list.len() < MAX_OPEN {
                    list.push(OpenConn {
                        dst,
                        net: clean_text(&text(get("net")), 8).to_lowercase(),
                        start,
                        up: safe_count(get("up")),
                        down: safe_count(get("down")),
                    });
                }
            }
            for watch in self.state.watch.values_mut().filter(|w| w.node_id == node_id) {
                watch.open = if watch.online || watch.pending {
                    by_tag.remove(&watch.tag).unwrap_or_default()
                } else {
                    Vec::new()
                };
            }
        }
    }

    /// Announces a tentative session once `session_confirmed` holds.
    fn confirm(&mut self, watch: &mut Watch, live: i64, now: DateTime<Utc>) {
        if !(watch.pending && self.session_confirmed(watch, live, now)) {
            return;
        }
        let now_iso = iso(now);
        watch.pending = false;
        watch.online = true;
        let hosts: Vec<String> = watch.visits.iter().map(|v| v.host.clone()).collect();
        let node_name = self.state.nodes.get(&watch.node_id).map(|n| n.name.clone()).unwrap_or_default();
        let start = watch.session_start.clone().unwrap_or_else(|| now_iso.clone());
        self.push_user_event("online", watch, &node_name, &start, hosts.clone());
        if self.config.notify_online {
            let text = self.format_online(watch, &node_name, &hosts);
            self.enqueue_telegram(text, now);
        }
    }

    /// Whether a tentative session is announced as online. A lone short request
    /// (an OCSP check, a background refresh while the phone is locked) is not; a
    /// second real connection, or one that stays open for
    /// `online_confirm_seconds`, is. Agents that cannot report live connections
    /// keep the old behaviour: the first real connection is enough.
    fn session_confirmed(&self, watch: &Watch, live: i64, now: DateTime<Utc>) -> bool {
        if !watch.live_tracked || self.config.online_confirm_seconds <= 0.0 {
            return true;
        }
        if watch.session_conn >= 2 {
            return true;
        }
        let start = watch.session_start.as_deref().and_then(millis).unwrap_or(i64::MAX);
        live > 0 && (now.timestamp_millis() - start) as f64 >= self.config.online_confirm_seconds * 1000.0
    }

    /// Advances the session state machine for one watched inbound tag. A tag is
    /// active while the agent sees new real connections or connections still
    /// open; `check_offline` handles the offline transition.
    fn handle_activity(&mut self, node_id: &str, tag: &str, info: Activity, now: DateTime<Utc>) {
        let Some(rule) = self.config.rule_for(tag).cloned() else { return };
        if self.state.nodes.get(node_id).is_some_and(|n| n.watch_off) {
            return;
        }
        let key = watch_key(node_id, tag);
        if info.conn <= 0 && info.live <= 0 {
            if let Some(watch) = self.state.watch.get_mut(&key) {
                watch.live = 0;
            }
            return;
        }
        let now_iso = iso(now);
        let mut watch = self.state.watch.remove(&key).unwrap_or_else(|| Watch {
            node_id: node_id.into(),
            tag: tag.into(),
            ..Default::default()
        });
        watch.label = rule.label;
        if !info.source.is_empty() {
            watch.last_source = info.source;
        }
        watch.live_tracked = info.live_tracked;
        watch.live = info.live;
        watch.last_active = Some(now_iso.clone());
        watch.conn_count += info.conn;

        // Agents before 0.3.0 send bare domains without times.
        let visits = if info.visits.is_empty() {
            info.domains.into_iter().map(|h| (h, now_iso.clone())).collect()
        } else {
            info.visits
        };
        let mut cleaned = Vec::new();
        for (raw, time) in visits {
            let host = clean_text(&raw, 253).trim().to_lowercase();
            if host.is_empty() {
                continue;
            }
            let time = if !time.is_empty() && time <= now_iso { time } else { now_iso.clone() };
            // Rolling "recently visited" list for the panel: most recent last, deduped.
            watch.domains.retain(|d| d != &host);
            watch.domains.push(host.clone());
            cleaned.push((host, time));
        }
        if watch.domains.len() > MAX_RECENT_DOMAINS {
            let drop = watch.domains.len() - MAX_RECENT_DOMAINS;
            watch.domains.drain(..drop);
        }

        if !watch.online && !watch.pending {
            watch.pending = true;
            watch.session_start = Some(
                cleaned
                    .iter()
                    .map(|(_, t)| t.clone())
                    .fold(now_iso.clone(), |a, t| if t < a { t } else { a }),
            );
            watch.session_conn = 0;
            watch.visits = Vec::new();
            watch.visits_dropped = 0;
            watch.session_up = 0.0;
            watch.session_down = 0.0;
            watch.samples.clear();
            watch.open.clear();
        }
        for (host, time) in &cleaned {
            add_visit(&mut watch, host, time);
        }
        watch.session_conn += info.conn;
        self.confirm(&mut watch, info.live, now);
        self.state.watch.insert(key, watch);
    }

    /// Closes a session once nothing has been open or newly connected for
    /// `offline_grace_seconds` (agents without live tracking:
    /// `session_idle_seconds`). The session ends at the last moment it was seen
    /// active, not when the check runs. Returns whether anything changed.
    pub fn check_offline(&mut self, now: DateTime<Utc>) -> bool {
        let now_ms = now.timestamp_millis();
        let mut changed = false;
        let keys: Vec<String> = self.state.watch.keys().cloned().collect();
        for key in keys {
            let watch = &self.state.watch[&key];
            let Some(last_active) = watch.last_active.as_deref().and_then(millis) else { continue };
            if !watch.online && !watch.pending {
                continue;
            }
            let limit = if watch.live_tracked {
                self.config.offline_grace_seconds
            } else {
                self.config.session_idle_seconds
            };
            if ((now_ms - last_active) as f64) <= limit * 1000.0 {
                continue;
            }
            let node = self.state.nodes.get(&watch.node_id);
            let mut note = String::new();
            if watch.live_tracked {
                let silent = node
                    .and_then(|n| n.last_seen.as_deref())
                    .and_then(millis)
                    .map_or(i64::MAX, |s| now_ms - s);
                if silent > AGENT_SILENT_MS {
                    if silent <= AGENT_LOST_MS {
                        continue;
                    }
                    note = "探针失联，下线时间按最后一次上报计".into();
                }
            }
            let node_name = node.map(|n| n.name.clone()).unwrap_or_default();
            changed = true;
            let mut watch = self.state.watch.remove(&key).expect("present");
            watch.live = 0;
            watch.open.clear();
            watch.samples.clear();
            if watch.pending {
                // Never confirmed (a lone short request): drop it without any alert.
                watch.pending = false;
                watch.visits.clear();
                self.state.watch.insert(key, watch);
                continue;
            }
            watch.online = false;
            let end = watch.last_active.clone().unwrap_or_default();
            let visits = sorted_visits(&watch);
            let hosts = visits.iter().map(|v| v.host.clone()).collect();
            let event = self.push_user_event("offline", &watch, &node_name, &end, hosts);
            event.session_start = watch.session_start.clone();
            event.session_conn = Some(watch.session_conn);
            event.visits = Some(visits.into_iter().take(60).collect());
            event.note = Some(note.clone());
            if watch.session_up + watch.session_down > 0.0 {
                event.session_up = Some(watch.session_up);
                event.session_down = Some(watch.session_down);
            }
            if self.config.notify_offline {
                for text in self.format_offline(&watch, &node_name, &end, &note) {
                    self.enqueue_telegram(text, now);
                }
            }
            self.state.watch.insert(key, watch);
        }
        changed
    }

    pub fn format_online(&self, watch: &Watch, node_name: &str, domains: &[String]) -> String {
        let start = watch.session_start.as_deref().unwrap_or_default();
        let mut lines = vec![
            format!("\u{1F7E2} <b>{} 上线</b>", escape_html(&watch.label)),
            format!("节点  {}", escape_html(if node_name.is_empty() { &watch.node_id } else { node_name })),
            format!("时间  {}", self.config.local_label(start, false)),
        ];
        let sample: Vec<&str> = domains.iter().take(3).map(String::as_str).collect();
        if !sample.is_empty() {
            lines.push(format!("访问  {}", escape_html(&sample.join(", "))));
        }
        lines.join("\n")
    }

    /// A summary followed by every domain visited in the session with its time,
    /// split to stay under Telegram's message size limit.
    pub fn format_offline(&self, watch: &Watch, node_name: &str, end: &str, note: &str) -> Vec<String> {
        let start = watch.session_start.as_deref().filter(|s| !s.is_empty()).unwrap_or(end);
        let with_date = self.config.local_day(start) != self.config.local_day(end);
        let label = escape_html(&watch.label);
        let mut head = vec![
            format!("\u{26AA} <b>{label} 下线</b>"),
            format!("节点  {}", escape_html(if node_name.is_empty() { &watch.node_id } else { node_name })),
            format!("上线  {}", self.config.local_label(start, with_date)),
            format!("下线  {}", self.config.local_label(end, with_date)),
            format!("时长  {}", human_duration(start, end)),
            format!("连接  {} 次", watch.session_conn),
        ];
        if watch.session_up + watch.session_down > 0.0 {
            head.push(format!(
                "流量  ↓ {}  ↑ {}",
                human_bytes(watch.session_down),
                human_bytes(watch.session_up)
            ));
        }
        if !note.is_empty() {
            head.push(format!("备注  {}", escape_html(note)));
        }
        let visits = sorted_visits(watch);
        let mut lines: Vec<String> = visits
            .iter()
            .map(|v| {
                let mut line =
                    format!("{}  {}", self.config.local_label(&v.first, with_date), escape_html(&v.host));
                if v.count > 1 {
                    line += &format!(" ×{}（至 {}）", v.count, self.config.local_label(&v.last, with_date));
                }
                line
            })
            .collect();
        if watch.visits_dropped > 0 {
            lines.push(format!("另有 {} 次访问的域名超出上限未列出", watch.visits_dropped));
        }
        head.push(String::new());
        head.push(if lines.is_empty() {
            "访问记录  无".into()
        } else {
            format!("<b>访问记录</b>（{} 个域名）", visits.len())
        });
        let mut messages = Vec::new();
        let mut current = head.join("\n");
        for line in lines {
            if current.chars().count() + line.chars().count() + 1 > TG_CHUNK {
                messages.push(std::mem::take(&mut current));
                current = format!("<b>{label} 访问记录（续）</b>");
            }
            current.push('\n');
            current.push_str(&line);
        }
        messages.push(current);
        messages
    }

    // ---- panel API ----

    fn node_online(&self, node: &Node, now: DateTime<Utc>) -> bool {
        node.last_seen.as_deref().and_then(millis).is_some_and(|s| {
            s > 0 && ((now.timestamp_millis() - s) as f64) <= self.config.offline_seconds * 1000.0
        })
    }

    fn port_traffic(&self, node_id: &str, port: &str, month: bool, now: DateTime<Utc>) -> Value {
        let today = self.config.date_key(now);
        let prefix = &today[..7];
        let (mut up, mut down) = (0.0, 0.0);
        for (day, by_node) in &self.state.daily {
            let hit = if month { day.starts_with(prefix) } else { day == &today };
            if !hit {
                continue;
            }
            if let Some(t) = by_node.get(node_id).and_then(|p| p.get(port)) {
                up += t.upload;
                down += t.download;
            }
        }
        json!({"upload": up, "download": down, "total": up + down})
    }

    fn public_node(&self, node: &Node, now: DateTime<Utc>) -> Value {
        let online = self.node_online(node, now);
        let inbounds: BTreeMap<i64, &Inbound> = node.inbounds.iter().map(|i| (i.port, i)).collect();
        let mut ports: BTreeSet<i64> = inbounds.keys().copied().collect();
        ports.extend(node.current.keys().filter_map(|k| k.parse::<i64>().ok()));
        let ports: Vec<Value> = ports
            .into_iter()
            .map(|port| {
                let key = port.to_string();
                let blank = Inbound { port, ..Default::default() };
                let inbound = inbounds.get(&port).copied().unwrap_or(&blank);
                let current = node.current.get(&key);
                let display = node
                    .aliases
                    .get(&key)
                    .cloned()
                    .or_else(|| inbound.users.first().cloned())
                    .or_else(|| Some(inbound.tag.clone()).filter(|t| !t.is_empty()))
                    .unwrap_or_else(|| key.clone());
                json!({
                    "port": port,
                    "type": inbound.kind,
                    "tag": inbound.tag,
                    "users": inbound.users,
                    "alias": node.aliases.get(&key).cloned().unwrap_or_default(),
                    "displayName": display,
                    "uploadRate": if online { current.map_or(0.0, |c| c.upload_rate) } else { 0.0 },
                    "downloadRate": if online { current.map_or(0.0, |c| c.download_rate) } else { 0.0 },
                    "today": self.port_traffic(&node.id, &key, false, now),
                    "month": self.port_traffic(&node.id, &key, true, now),
                })
            })
            .collect();
        json!({
            "id": node.id,
            "name": node.name,
            "hostname": node.hostname,
            "arch": node.arch,
            "agentVersion": node.agent_version,
            "createdAt": node.created_at,
            "lastSeen": node.last_seen,
            "online": online,
            "singbox": node.singbox,
            "ports": ports,
            "lastActivity": node.last_activity,
            "users": self.user_traffic(&node.id, now),
            "hubNodeId": node.hub_node_id,
            "watchUsers": !node.watch_off,
            "singboxDownSince": node.singbox_down_since,
        })
    }

    /// This node's traffic per inbound user, today and this month, busiest first.
    fn user_traffic(&self, node_id: &str, now: DateTime<Utc>) -> Vec<Value> {
        let today = self.config.date_key(now);
        let month = &today[..7];
        let mut sums: BTreeMap<&str, [f64; 4]> = BTreeMap::new();
        for (day, by_node) in self.state.daily_users.range(month.to_owned()..) {
            if !day.starts_with(month) {
                continue;
            }
            for (key, t) in by_node.get(node_id).into_iter().flatten() {
                let s = sums.entry(key.as_str()).or_default();
                s[2] += t.upload;
                s[3] += t.download;
                if *day == today {
                    s[0] += t.upload;
                    s[1] += t.download;
                }
            }
        }
        let mut rows: Vec<(&str, [f64; 4])> = sums.into_iter().collect();
        rows.sort_by(|a, b| (b.1[2] + b.1[3]).total_cmp(&(a.1[2] + a.1[3])));
        rows.into_iter()
            .map(|(key, s)| {
                let (tag, user) = key.split_once('|').unwrap_or((key, ""));
                json!({
                    "tag": tag, "user": user,
                    "today": {"upload": s[0], "download": s[1], "total": s[0] + s[1]},
                    "month": {"upload": s[2], "download": s[3], "total": s[2] + s[3]},
                })
            })
            .collect()
    }

    pub fn admin_state(&mut self, now: DateTime<Utc>) -> Value {
        self.cleanup_registrations(now);
        let mut nodes: Vec<&Node> = self.state.nodes.values().collect();
        nodes.sort_by(|a, b| a.name.cmp(&b.name));
        let nodes: Vec<Value> = nodes.into_iter().map(|n| self.public_node(n, now)).collect();
        let events: Vec<Value> = self
            .state
            .events
            .iter()
            .rev()
            .take(100)
            .map(|e| {
                let name = self.state.nodes.get(&e.node_id).map(|n| n.name.as_str()).unwrap_or_default();
                json!({"id": e.id, "nodeId": e.node_id, "nodeName": name, "time": e.time, "level": e.level, "message": e.message})
            })
            .collect();
        json!({
            "ok": true,
            "version": VERSION,
            "server_time": iso(now),
            "config": self.config.view(),
            "nodes": nodes,
            "events": events,
            "pending": self.state.pending.len(),
        })
    }

    /// The connection panel: one row per watched inbound tag per node, seeded
    /// from reported inbounds so a watched user shows as offline even before
    /// their first connection.
    pub fn admin_watch(&self, now: DateTime<Utc>) -> Value {
        let events: Vec<&UserEvent> = self.state.user_events.iter().rev().take(120).collect();
        json!({
            "ok": true,
            "version": VERSION,
            "server_time": iso(now),
            "timezone_offset": self.config.timezone_offset,
            "session_idle_seconds": self.config.session_idle_seconds,
            "offline_grace_seconds": self.config.offline_grace_seconds,
            "online_confirm_seconds": self.config.online_confirm_seconds,
            "telegram_configured": self.config.telegram_configured(),
            "rules": self.config.watch_rules,
            "watched": self.watch_rows(now),
            "events_rev": self.events_rev(),
            "events": events,
        })
    }

    /// The live frame the panel's stream carries: the watched rows only. The
    /// page fetches the events again when `events_rev` moves.
    pub fn admin_watch_live(&self, now: DateTime<Utc>) -> Value {
        json!({"server_time": iso(now), "events_rev": self.events_rev(), "watched": self.watch_rows(now)})
    }

    fn events_rev(&self) -> String {
        let last = self.state.user_events.last().map(|e| e.id.as_str()).unwrap_or_default();
        format!("{}:{last}", self.state.user_events.len())
    }

    fn watch_rows(&self, now: DateTime<Utc>) -> Vec<Value> {
        let now_ms = now.timestamp_millis();
        let mut rows: BTreeMap<String, serde_json::Map<String, Value>> = BTreeMap::new();
        for node in self.state.nodes.values().filter(|n| !n.watch_off) {
            let node_online = self.node_online(node, now);
            for inbound in &node.inbounds {
                let Some(rule) = self.config.rule_for(&inbound.tag) else { continue };
                let row = json!({
                    "label": rule.label, "tag": inbound.tag, "port": inbound.port,
                    "nodeId": node.id, "nodeName": node.name, "nodeOnline": node_online,
                    "online": false, "live": 0, "liveTracked": false, "sessionStart": null, "sessionConn": 0,
                    "lastActive": null, "lastSource": "", "connTotal": 0, "domains": [],
                });
                rows.insert(watch_key(&node.id, &inbound.tag), row.as_object().cloned().unwrap_or_default());
            }
        }
        for (key, watch) in &self.state.watch {
            let node = self.state.nodes.get(&watch.node_id);
            let base = rows.entry(key.clone()).or_insert_with(|| {
                json!({
                    "label": watch.label, "tag": watch.tag, "port": null, "nodeId": watch.node_id,
                    "nodeName": node.map(|n| n.name.clone()).unwrap_or_default(), "nodeOnline": false,
                })
                .as_object()
                .cloned()
                .unwrap_or_default()
            });
            let domains: Vec<&String> = watch.domains.iter().rev().take(20).collect();
            let live = if watch.online || watch.pending { watch.live } else { 0 };
            base.insert("online".into(), json!(watch.online));
            base.insert("live".into(), json!(live));
            base.insert("liveTracked".into(), json!(watch.live_tracked));
            base.insert(
                "sessionStart".into(),
                if watch.online { json!(watch.session_start) } else { Value::Null },
            );
            base.insert("sessionConn".into(), json!(if watch.online { watch.session_conn } else { 0 }));
            base.insert("lastActive".into(), json!(watch.last_active));
            base.insert("lastSource".into(), json!(watch.last_source));
            base.insert("connTotal".into(), json!(watch.conn_count));
            base.insert("domains".into(), json!(domains));
            let active = watch.online || watch.pending;
            let (rate_up, rate_down) = if active { speed(&watch.samples, now_ms) } else { (0.0, 0.0) };
            let mut visits = if active { watch.visits.clone() } else { Vec::new() };
            visits.sort_by(|a, b| b.count.cmp(&a.count).then(b.last.cmp(&a.last)));
            visits.truncate(100);
            base.insert("pending".into(), json!(watch.pending));
            base.insert(
                "pendingStart".into(),
                if watch.pending { json!(watch.session_start) } else { Value::Null },
            );
            base.insert("rateUp".into(), json!(rate_up));
            base.insert("rateDown".into(), json!(rate_down));
            base.insert("sessionUp".into(), json!(if active { watch.session_up } else { 0.0 }));
            base.insert("sessionDown".into(), json!(if active { watch.session_down } else { 0.0 }));
            base.insert("open".into(), json!(if active { &watch.open[..] } else { &[] }));
            base.insert("sessionVisits".into(), json!(visits));
            base.insert("visitsDropped".into(), json!(if active { watch.visits_dropped } else { 0 }));
        }
        let mut watched: Vec<Value> = rows.into_values().map(Value::Object).collect();
        let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
        watched
            .sort_by(|a, b| s(a, "label").cmp(&s(b, "label")).then(s(a, "nodeName").cmp(&s(b, "nodeName"))));
        watched
    }

    pub fn create_registration(&mut self, name: &str, now: DateTime<Utc>) -> Result<Value, &'static str> {
        let name = clean_name(name);
        if name.is_empty() {
            return Err("请输入节点名称");
        }
        self.cleanup_registrations(now);
        let token = random_hex(24);
        let expires = iso(now + chrono::Duration::minutes(10));
        self.state.registrations.insert(
            sha256_hex(&token),
            Registration {
                name: name.clone(),
                expires_at: expires.clone(),
                used: false,
                created_at: iso(now),
                node_id: None,
            },
        );
        Ok(json!({"ok": true, "token": token, "name": name, "expires_at": expires}))
    }

    /// Registers the sb-agent installed alongside hub node `hub_id`. Run again for
    /// the same hub node -- a reinstall, a rebuilt machine -- it keeps the sb
    /// node and its history and issues a new agent token, so one machine never
    /// shows up twice.
    pub fn register_linked(
        &mut self,
        hub_id: i64,
        hub_name: &str,
        body: &Value,
        now: DateTime<Utc>,
    ) -> Value {
        let field = |k: &str, max| clean_text(&text(body.get(k).unwrap_or(&Value::Null)), max);
        let agent_token = random_hex(32);
        let existing =
            self.state.nodes.values().find(|n| n.hub_node_id == Some(hub_id)).map(|n| n.id.clone());
        let node_id = existing.unwrap_or_else(|| random_hex(16));
        let name =
            Some(clean_name(hub_name)).filter(|n| !n.is_empty()).unwrap_or_else(|| format!("节点 {hub_id}"));
        let node = self.state.nodes.entry(node_id.clone()).or_insert_with(|| Node {
            id: node_id.clone(),
            created_at: iso(now),
            hub_node_id: Some(hub_id),
            ..Default::default()
        });
        node.name = name.clone();
        node.hostname = field("hostname", 120);
        node.arch = field("arch", 30);
        node.agent_version = field("agent_version", 30);
        node.agent_token_hash = sha256_hex(&agent_token);
        json!({"ok": true, "node_id": node_id, "agent_token": agent_token, "name": name})
    }

    /// Follows the hub's node list: a linked node takes its hub node's name, and
    /// one whose hub node was deleted is deleted too. A node registered on its
    /// own (a one-time key, or carried over from Komari) whose name matches a
    /// hub node nobody is linked to yet is linked to it. Returns whether
    /// anything changed and the nodes deleted, whose history files go too.
    pub fn sync_hub_nodes(&mut self, names: &BTreeMap<i64, String>) -> (bool, Vec<String>) {
        let mut changed = false;
        let mut gone = Vec::new();
        let mut taken: BTreeSet<i64> = self.state.nodes.values().filter_map(|n| n.hub_node_id).collect();
        for node in self.state.nodes.values_mut().filter(|n| n.hub_node_id.is_none()) {
            let found = names.iter().find(|(id, name)| !taken.contains(id) && clean_name(name) == node.name);
            if let Some((&id, _)) = found {
                node.hub_node_id = Some(id);
                taken.insert(id);
                changed = true;
            }
        }
        for node in self.state.nodes.values_mut() {
            let Some(hub_id) = node.hub_node_id else { continue };
            match names.get(&hub_id) {
                None => gone.push(node.id.clone()),
                Some(name) if !name.is_empty() && *name != node.name => {
                    node.name = clean_name(name);
                    changed = true;
                }
                Some(_) => {}
            }
        }
        for id in &gone {
            let _ = self.delete_node(id);
            changed = true;
        }
        (changed, gone)
    }

    /// Switches watching users on this server on or off. Switched off, any
    /// session in progress there is dropped without an alert.
    pub fn set_watch(&mut self, node_id: &str, enabled: bool) -> Result<(), &'static str> {
        let node = self.state.nodes.get_mut(node_id).ok_or("节点不存在")?;
        node.watch_off = !enabled;
        if !enabled {
            self.state.watch.retain(|_, w| w.node_id != node_id);
        }
        Ok(())
    }

    /// sing-box stopped for longer than [`STATUS_GRACE_MS`], and sb-agents silent
    /// for [`AGENT_LOST_ALERT_MS`] while their machine is still up (`hub_offline`
    /// lists hub nodes whose own agent is down; the hub alerts for those). The
    /// silence is counted from the later of the last report and the last time
    /// the machine was seen down, so a blip of the hub's own network, which
    /// drops every agent at once, does not read as sb-agents that stopped.
    /// Returns whether anything changed.
    pub fn check_status(&mut self, now: DateTime<Utc>, hub_offline: &HashSet<i64>) -> bool {
        if !self.config.notify_status {
            return false;
        }
        let now_ms = now.timestamp_millis();
        let mut alerts = Vec::new();
        for node in self.state.nodes.values_mut() {
            let seen = node.last_seen.as_deref().and_then(millis);
            let silent = seen.map_or(0, |s| now_ms - s);
            let host_down = node.hub_node_id.is_some_and(|id| hub_offline.contains(&id));
            if host_down {
                node.host_down_ms = Some(now_ms);
            }
            let quiet = seen.map_or(0, |s| now_ms - s.max(node.host_down_ms.unwrap_or(0)));
            if seen.is_some() && quiet > AGENT_LOST_ALERT_MS && !node.agent_lost_alerted && !host_down {
                node.agent_lost_alerted = true;
                alerts.push(format!(
                    "\u{26A0}\u{FE0F} <b>sb-agent 失联</b>\n节点  {}\n最后上报  {}\n机器本身还在线，多半是 sb-agent 停了：systemctl status sb-agent",
                    escape_html(&node.name),
                    self.config.local_label(node.last_seen.as_deref().unwrap_or_default(), true)
                ));
            }
            // Judged only while the agent reports: a silent agent says nothing about sing-box.
            let Some(since) = node.singbox_down_since.as_deref().and_then(millis) else { continue };
            if !node.singbox_down_alerted && silent <= AGENT_SILENT_MS && now_ms - since > STATUS_GRACE_MS {
                node.singbox_down_alerted = true;
                alerts.push(format!(
                    "\u{1F534} <b>sing-box 已停止</b>\n节点  {}\n从  {} 起",
                    escape_html(&node.name),
                    self.config.local_label(node.singbox_down_since.as_deref().unwrap_or_default(), true)
                ));
            }
        }
        let changed = !alerts.is_empty();
        for text in alerts {
            self.enqueue_telegram(text, now);
        }
        changed
    }

    /// Ties an sb node registered on its own (a one-time key, or carried over
    /// from Komari) to hub node `hub`, or unties it with `None`. Afterwards it
    /// behaves as if installed with the hub's agent.
    pub fn link_node(&mut self, node_id: &str, hub: Option<(i64, &str)>) -> Result<(), &'static str> {
        if !self.state.nodes.contains_key(node_id) {
            return Err("节点不存在");
        }
        if let Some((hub_id, _)) = hub {
            if self.state.nodes.values().any(|n| n.hub_node_id == Some(hub_id) && n.id != node_id) {
                return Err("那个节点已经关联了另一台 sing-box");
            }
        }
        let node = self.state.nodes.get_mut(node_id).expect("checked above");
        node.hub_node_id = hub.map(|(id, _)| id);
        if let Some((_, name)) = hub {
            let name = clean_name(name);
            if !name.is_empty() {
                node.name = name;
            }
        }
        Ok(())
    }

    pub fn rename_node(&mut self, node_id: &str, name: &str) -> Result<(), &'static str> {
        let name = clean_name(name);
        let node = self.state.nodes.get_mut(node_id).ok_or("节点不存在")?;
        if node.hub_node_id.is_some() {
            return Err("这台机器随整机探针安装，名字跟着「节点」页走，在那里改");
        }
        if name.is_empty() {
            return Err("节点名称不能为空");
        }
        node.name = name;
        Ok(())
    }

    pub fn set_alias(&mut self, node_id: &str, port: &Value, alias: &str) -> Result<(), &'static str> {
        let key = port_key(port);
        let node = self.state.nodes.get_mut(node_id).ok_or("节点不存在")?;
        if key.is_empty() {
            return Err("端口无效");
        }
        let alias = clean_name(alias);
        if alias.is_empty() {
            node.aliases.remove(&key);
        } else {
            node.aliases.insert(key, alias);
        }
        Ok(())
    }

    pub fn delete_node(&mut self, node_id: &str) -> Result<(), &'static str> {
        if self.state.nodes.remove(node_id).is_none() {
            return Err("节点不存在");
        }
        for by_node in self.state.daily.values_mut().chain(self.state.daily_users.values_mut()) {
            by_node.remove(node_id);
        }
        self.state.events.retain(|e| e.node_id != node_id);
        self.state.user_events.retain(|e| e.node_id != node_id);
        self.state.watch.retain(|_, w| w.node_id != node_id);
        Ok(())
    }

    /// Lines to append to each reporting node's minute history.
    pub fn history_lines(&self, now: DateTime<Utc>) -> Vec<HistoryLine> {
        let day = self.config.date_key(now);
        let now_iso = iso(now);
        self.state
            .nodes
            .values()
            .filter(|n| self.node_online(n, now))
            .map(|node| {
                let points: Vec<Value> = node
                    .current
                    .values()
                    .map(|c| json!({"port": c.port, "upload_rate": c.upload_rate, "download_rate": c.download_rate}))
                    .collect();
                HistoryLine {
                    node_id: node.id.clone(),
                    day: day.clone(),
                    line: json!({"time": now_iso, "points": points}).to_string(),
                }
            })
            .collect()
    }

    /// Drops daily traffic buckets past [`DAILY_KEEP_DAYS`].
    pub fn prune_daily(&mut self, now: DateTime<Utc>) {
        let cutoff = self.config.date_key(now - chrono::Duration::days(DAILY_KEEP_DAYS));
        self.state.daily.retain(|day, _| day.as_str() >= cutoff.as_str());
        self.state.daily_users.retain(|day, _| day.as_str() >= cutoff.as_str());
    }
}
