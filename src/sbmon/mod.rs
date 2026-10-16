//! sing-box monitoring, built into the hub.
//!
//! A port of the Komari plugin `komari-singbox-monitor`: the sb-agents on the
//! landing nodes report each inbound's ports, traffic and connection activity
//! here, and watched users (an inbound tag each) get online/offline alerts with
//! the domains they visited, through a Telegram bot of their own -- separate
//! from the hub's node alerts in `notify`.
//!
//! The HTTP surface keeps the plugin's paths under `/api/sbmonitor/v1`, so an
//! sb-agent already registered against Komari keeps working once the hub
//! answers on the same domain, and `state.json` plus `history/` copy across
//! unchanged. See `engine` for the state machine.

pub mod engine;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use anyhow::{Context, Result};
use axum::body::Bytes;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use rust_embed::RustEmbed;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::api::{answer, Admin};
use crate::{App, Shared};
use engine::{Config, Engine, State as SbState};

pub const API: &str = "/api/sbmonitor/v1";
/// Settings live in the hub's `setting` table under this prefix.
const PREFIX: &str = "sbmon_";
/// The plugin allowed reports up to 1 MiB; a busy watched inbound with many
/// visits since the last report can approach that.
pub const MAX_REPORT: usize = 1024 * 1024;
const INSTALLER: &str = include_str!("../../sbmon/installer/install-agent.sh");
const UNINSTALLER: &str = include_str!("../../sbmon/installer/uninstall-agent.sh");

/// The sb-agent binaries and their `checksums.txt`, built from `sbmon/agent`
/// into `sbmon/bin` before the hub (see `sbmon/build-agent.sh`). A file of the
/// same name under the data directory's `sbmonitor/bin/` takes precedence, so
/// a new agent can be handed out without rebuilding the hub.
#[derive(RustEmbed)]
#[folder = "sbmon/bin"]
struct AgentBinaries;

pub struct Sbmon {
    engine: Mutex<Engine>,
    dir: PathBuf,
    flushing: AtomicBool,
    wake: tokio::sync::Notify,
}

fn config_from_db(app_db: &crate::db::Db) -> Config {
    Config::from_lookup(|key| app_db.get(&format!("{PREFIX}{key}")))
}

impl Sbmon {
    /// Opens `<dir>/state.json`. A missing file starts empty; an unreadable one
    /// stops the hub rather than silently resetting every node's identity.
    pub fn open(dir: PathBuf, db: &crate::db::Db) -> Result<Self> {
        std::fs::create_dir_all(dir.join("history"))
            .with_context(|| format!("creating {}", dir.display()))?;
        let file = dir.join("state.json");
        let state = match std::fs::read(&file) {
            Ok(bytes) => serde_json::from_slice::<SbState>(&bytes).with_context(|| {
                format!("{} is not a valid sing-box monitor state; fix or move it away", file.display())
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => SbState::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", file.display())),
        };
        info!(
            "sing-box monitor {}: {} nodes, {} watched sessions, {} alerts queued",
            engine::VERSION,
            state.nodes.len(),
            state.watch.len(),
            state.pending.len()
        );
        Ok(Self {
            engine: Mutex::new(Engine::new(state, config_from_db(db))),
            dir,
            flushing: AtomicBool::new(false),
            wake: tokio::sync::Notify::new(),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Engine> {
        self.engine.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Writes the state through a temporary file, so a crash mid-write leaves
    /// the previous copy.
    fn persist(&self, engine: &mut Engine) {
        engine.state.updated_at = engine::iso(Utc::now());
        let file = self.dir.join("state.json");
        let temp = self.dir.join("state.json.tmp");
        let result = serde_json::to_vec(&engine.state)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| Ok(std::fs::write(&temp, bytes)?))
            .and_then(|()| Ok(std::fs::rename(&temp, &file)?));
        if let Err(e) = result {
            warn!("saving sing-box monitor state failed: {e:#}");
        }
    }

    fn history_file(&self, node_id: &str, day: &str) -> PathBuf {
        self.dir.join("history").join(node_id).join(format!("{day}.jsonl"))
    }
}

fn reload_config(app: &App) {
    let config = config_from_db(&app.db);
    app.sbmon.lock().config = config;
}

// ---- background work -------------------------------------------------------

/// The plugin's crons: offline checks every 10 s, minute history, a daily
/// clean-up, and Telegram delivery every 20 s or when an alert is queued.
pub async fn run(app: Shared) {
    tokio::spawn(deliver(app.clone()));
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(10));
    let mut ticks: u64 = 0;
    let mut cleaned = String::new();
    loop {
        tick.tick().await;
        ticks += 1;
        let now = Utc::now();
        let sb = &app.sbmon;
        let queued = {
            let mut engine = sb.lock();
            let before = engine.state.pending.len();
            if engine.check_offline(now) {
                sb.persist(&mut engine);
            }
            engine.state.pending.len() > before
        };
        if queued {
            sb.wake.notify_one();
        }
        if ticks.is_multiple_of(6) {
            let (lines, day) = {
                let engine = sb.lock();
                (engine.history_lines(now), engine.config.date_key(now))
            };
            for line in lines {
                if let Err(e) = append_line(&sb.history_file(&line.node_id, &line.day), &line.line) {
                    warn!("writing sing-box history failed: {e:#}");
                }
            }
            if day != cleaned {
                cleaned = day;
                cleanup_history(sb, now);
            }
        }
    }
}

fn append_line(file: &Path, line: &str) -> Result<()> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(file)?;
    writeln!(f, "{line}")?;
    Ok(())
}

fn cleanup_history(sb: &Sbmon, now: chrono::DateTime<Utc>) {
    let keep = {
        let mut engine = sb.lock();
        engine.prune_daily(now);
        sb.persist(&mut engine);
        engine.config.history_days
    };
    let cutoff = (now - chrono::Duration::days(keep as i64)).format("%Y-%m-%d").to_string();
    let Ok(nodes) = std::fs::read_dir(sb.dir.join("history")) else { return };
    for node in nodes.flatten() {
        let Ok(files) = std::fs::read_dir(node.path()) else { continue };
        for file in files.flatten() {
            let name = file.file_name().to_string_lossy().into_owned();
            if let Some(day) = name.strip_suffix(".jsonl") {
                if day.len() == 10 && day < cutoff.as_str() {
                    let _ = std::fs::remove_file(file.path());
                }
            }
        }
    }
}

async fn deliver(app: Shared) {
    loop {
        tokio::select! {
            () = app.sbmon.wake.notified() => {}
            () = tokio::time::sleep(std::time::Duration::from_secs(20)) => {}
        }
        flush(&app).await;
    }
}

async fn send_telegram(app: &App, token: &str, chat: &str, text: &str) -> Result<()> {
    let url = format!("https://api.telegram.org/bot{token}/sendMessage");
    let response = app
        .http
        .post(url)
        .json(&json!({"chat_id": chat, "text": text, "parse_mode": "HTML", "disable_web_page_preview": true}))
        .send()
        .await
        // The URL carries the token; reqwest's error repeats it.
        .map_err(|e| anyhow::anyhow!("{}", e.without_url()))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("HTTP {status}: {}", body.chars().take(200).collect::<String>());
    }
    Ok(())
}

/// Sends queued alerts in order, stopping at the first failure; the queue is
/// persisted, so an outage or a restart delays alerts rather than losing them.
async fn flush(app: &App) {
    let sb = &app.sbmon;
    if sb.flushing.swap(true, Ordering::AcqRel) {
        return;
    }
    loop {
        let next = {
            let engine = sb.lock();
            let config = &engine.config;
            match engine.state.pending.first() {
                Some(item) if config.telegram_configured() => {
                    Some((item.clone(), config.tg_bot_token.clone(), config.tg_chat_id.clone()))
                }
                _ => None,
            }
        };
        let Some((item, token, chat)) = next else { break };
        if let Err(e) = send_telegram(app, &token, &chat, &item.text).await {
            warn!("sing-box monitor Telegram alert not sent, retrying later: {e:#}");
            break;
        }
        let mut engine = sb.lock();
        if engine.state.pending.first() == Some(&item) {
            engine.state.pending.remove(0);
        }
        sb.persist(&mut engine);
    }
    sb.flushing.store(false, Ordering::Release);
}

// ---- agent API: JSON in and out, as sb-agent expects ----------------------

fn agent_json(status: u16, value: Value) -> Response {
    let mut response = Json(value).into_response();
    *response.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST);
    response.headers_mut().insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
    response
}

fn agent_fail(status: u16, message: &str) -> Response {
    agent_json(status, json!({"ok": false, "error": message}))
}

fn json_body(body: &Bytes) -> Option<Value> {
    serde_json::from_slice::<Value>(body).ok().filter(Value::is_object)
}

fn bearer(headers: &HeaderMap) -> String {
    let value = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).unwrap_or_default();
    match value.split_once(char::is_whitespace) {
        Some((scheme, token)) if scheme.eq_ignore_ascii_case("bearer") => token.trim().to_owned(),
        _ => String::new(),
    }
}

async fn agent_register(State(app): State<Shared>, body: Bytes) -> Response {
    let Some(body) = json_body(&body) else { return agent_fail(400, "请求内容不是有效 JSON") };
    let sb = &app.sbmon;
    let mut engine = sb.lock();
    match engine.register(&body, Utc::now()) {
        Ok(value) => {
            sb.persist(&mut engine);
            info!("sing-box agent registered: {}", value["name"].as_str().unwrap_or_default());
            agent_json(200, value)
        }
        Err(engine::Refusal(status, message)) => agent_fail(status, message),
    }
}

async fn agent_report(State(app): State<Shared>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(body) = json_body(&body) else { return agent_fail(400, "请求内容不是有效 JSON") };
    let sb = &app.sbmon;
    let (result, queued) = {
        let mut engine = sb.lock();
        let before = engine.state.pending.len();
        let result = engine.report(&bearer(&headers), &body, Utc::now());
        if result.is_ok() {
            sb.persist(&mut engine);
        }
        let queued = engine.state.pending.len() > before;
        (result, queued)
    };
    if queued {
        sb.wake.notify_one();
    }
    match result {
        Ok(value) => agent_json(200, value),
        Err(engine::Refusal(status, message)) => agent_fail(status, message),
    }
}

fn script(text: &'static str) -> Response {
    ([(header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8"), (header::CACHE_CONTROL, "no-store")], text)
        .into_response()
}

async fn agent_binary(State(app): State<Shared>, UrlPath(file): UrlPath<String>) -> Response {
    let allowed = matches!(file.as_str(), "sb-agent-linux-amd64" | "sb-agent-linux-arm64" | "checksums.txt");
    if !allowed {
        return agent_fail(404, "没有这个文件");
    }
    let content_type =
        if file.ends_with(".txt") { "text/plain; charset=utf-8" } else { "application/octet-stream" };
    let local = app.sbmon.dir.join("bin").join(&file);
    let bytes = match std::fs::read(&local) {
        Ok(bytes) => bytes,
        Err(_) => match AgentBinaries::get(&file) {
            Some(embedded) => embedded.data.into_owned(),
            None => return agent_fail(404, "hub 没有附带 sb-agent 程序，放到数据目录的 sbmonitor/bin/ 下"),
        },
    };
    ([(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, "no-store")], bytes).into_response()
}

/// The routes sb-agent and its installer call. Merged outside the panel's
/// layers: these replies are JSON the agent parses, not the panel's text.
pub fn agent_routes<S: Clone + Send + Sync + 'static>(app: Shared) -> Router<S> {
    Router::new()
        .route(&format!("{API}/agent/register"), post(agent_register))
        .route(&format!("{API}/agent/report"), post(agent_report))
        .route(&format!("{API}/install.sh"), get(|| async { script(INSTALLER) }))
        .route(&format!("{API}/uninstall.sh"), get(|| async { script(UNINSTALLER) }))
        .route(&format!("{API}/bin/{{file}}"), get(agent_binary))
        .layer(tower_http::limit::RequestBodyLimitLayer::new(MAX_REPORT))
        .with_state(app)
}

// ---- panel API -------------------------------------------------------------

fn ok() -> Response {
    Json(json!({"ok": true})).into_response()
}

fn refuse(message: &str) -> Response {
    answer(StatusCode::BAD_REQUEST, message.to_owned())
}

/// Admin reads carry the node list, so like the hub's own they are kept out of
/// shared caches.
fn private(value: Value) -> Response {
    ([(header::CACHE_CONTROL, "no-store, no-transform")], Json(value)).into_response()
}

async fn admin_state(_: Admin, State(app): State<Shared>) -> Response {
    private(app.sbmon.lock().admin_state(Utc::now()))
}

async fn admin_watch(_: Admin, State(app): State<Shared>) -> Response {
    private(app.sbmon.lock().admin_watch(Utc::now()))
}

#[derive(Deserialize)]
struct NodeBody {
    #[serde(default)]
    node_id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    port: Value,
    #[serde(default)]
    alias: String,
}

async fn create_registration(_: Admin, State(app): State<Shared>, Json(body): Json<NodeBody>) -> Response {
    let sb = &app.sbmon;
    let mut engine = sb.lock();
    match engine.create_registration(&body.name, Utc::now()) {
        Ok(value) => {
            sb.persist(&mut engine);
            Json(value).into_response()
        }
        Err(message) => refuse(message),
    }
}

fn edit(app: &App, change: impl FnOnce(&mut Engine) -> Result<(), &'static str>) -> Response {
    let sb = &app.sbmon;
    let mut engine = sb.lock();
    match change(&mut engine) {
        Ok(()) => {
            sb.persist(&mut engine);
            ok()
        }
        Err(message) => refuse(message),
    }
}

async fn rename_node(_: Admin, State(app): State<Shared>, Json(body): Json<NodeBody>) -> Response {
    edit(&app, |e| e.rename_node(&body.node_id, &body.name))
}

async fn set_alias(_: Admin, State(app): State<Shared>, Json(body): Json<NodeBody>) -> Response {
    edit(&app, |e| e.set_alias(&body.node_id, &body.port, &body.alias))
}

async fn delete_node(_: Admin, State(app): State<Shared>, Json(body): Json<NodeBody>) -> Response {
    let result = edit(&app, |e| e.delete_node(&body.node_id));
    if result.status().is_success()
        && body.node_id.len() == 32
        && body.node_id.bytes().all(|b| b.is_ascii_hexdigit())
    {
        let _ = std::fs::remove_dir_all(app.sbmon.dir.join("history").join(&body.node_id));
    }
    result
}

#[derive(Deserialize)]
struct HistoryQuery {
    #[serde(default)]
    node_id: String,
    #[serde(default)]
    date: String,
}

async fn history(_: Admin, State(app): State<Shared>, Query(q): Query<HistoryQuery>) -> Response {
    let sb = &app.sbmon;
    let day = {
        let engine = sb.lock();
        if !engine.state.nodes.contains_key(&q.node_id) {
            return answer(StatusCode::NOT_FOUND, "节点不存在");
        }
        if q.date.is_empty() {
            engine.config.date_key(Utc::now())
        } else {
            q.date.clone()
        }
    };
    let valid = day.len() == 10 && chrono::NaiveDate::parse_from_str(&day, "%Y-%m-%d").is_ok();
    if !valid {
        return refuse("日期格式错误");
    }
    let points: Vec<Value> = match std::fs::read_to_string(sb.history_file(&q.node_id, &day)) {
        Ok(text) => {
            let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
            let start = lines.len().saturating_sub(2000);
            lines[start..].iter().filter_map(|l| serde_json::from_str(l).ok()).collect()
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            warn!("reading sing-box history failed: {e:#}");
            return answer(StatusCode::INTERNAL_SERVER_ERROR, "读取历史记录失败");
        }
    };
    private(json!({"ok": true, "node_id": q.node_id, "date": day, "points": points}))
}

/// The stored settings as the form edits them. The bot token is never sent
/// back; an empty token in a save keeps the stored one.
async fn get_config(_: Admin, State(app): State<Shared>) -> Response {
    let mut out = serde_json::Map::new();
    for key in engine::CONFIG_KEYS {
        if key == "tg_bot_token" {
            continue;
        }
        out.insert(key.into(), json!(app.db.get(&format!("{PREFIX}{key}")).unwrap_or_default()));
    }
    let config = app.sbmon.lock().config.clone();
    out.insert("tg_bot_token_set".into(), json!(!config.tg_bot_token.is_empty()));
    out.insert("effective".into(), config.view());
    private(Value::Object(out))
}

fn config_error(key: &str, value: &str) -> Option<String> {
    let number = |min: f64, max: f64, name: &str| {
        if value.trim().is_empty() {
            return None;
        }
        match value.trim().parse::<f64>() {
            Ok(n) if n.is_finite() && (min..=max).contains(&n) => None,
            _ => Some(format!("{name}要在 {min} 到 {max} 之间")),
        }
    };
    match key {
        "timezone_offset" => number(-12.0, 14.0, "统计时区"),
        "offline_seconds" => number(15.0, 3600.0, "节点离线判定秒数"),
        "history_days" => number(1.0, 365.0, "历史保留天数"),
        "session_idle_seconds" => number(30.0, 86400.0, "空闲判定秒数"),
        "offline_grace_seconds" => number(10.0, 3600.0, "下线确认秒数"),
        "online_confirm_seconds" => number(0.0, 600.0, "上线确认秒数"),
        "notify_online" | "notify_offline" if !matches!(value, "" | "true" | "false") => {
            Some(format!("{key} 只能是 true 或 false"))
        }
        "tg_bot_token" if !value.is_empty() && !value.contains(':') => {
            Some("Bot Token 格式不对，应是「数字:字母串」".into())
        }
        k if engine::CONFIG_KEYS.contains(&k) => None,
        _ => Some(format!("没有这个设置项：{key}")),
    }
}

async fn save_config(_: Admin, State(app): State<Shared>, Json(body): Json<Value>) -> Response {
    let Some(map) = body.as_object() else { return refuse("设置格式不对") };
    for (key, value) in map {
        let Some(value) = value.as_str() else { return refuse(&format!("设置 {key} 的值格式不对")) };
        if let Some(message) = config_error(key, value) {
            return refuse(&message);
        }
    }
    for (key, value) in map {
        let value = value.as_str().unwrap_or_default().trim();
        if key == "tg_bot_token" && value.is_empty() {
            continue;
        }
        if let Err(e) = app.db.set(&format!("{PREFIX}{key}"), value) {
            return crate::api::fail(e);
        }
    }
    reload_config(&app);
    app.sbmon.wake.notify_one();
    ok()
}

/// Clears the stored bot token, which an empty save leaves alone.
async fn clear_token(_: Admin, State(app): State<Shared>) -> Response {
    if let Err(e) = app.db.set(&format!("{PREFIX}tg_bot_token"), "") {
        return crate::api::fail(e);
    }
    reload_config(&app);
    ok()
}

async fn test_telegram(_: Admin, State(app): State<Shared>) -> Response {
    let config = app.sbmon.lock().config.clone();
    if !config.telegram_configured() {
        return refuse("先填写并保存 Bot Token 和 Chat ID");
    }
    let text = format!("\u{1F514} <b>sing-box 连接监控</b>\n测试消息，发送于 {}", engine::iso(Utc::now()));
    match send_telegram(&app, &config.tg_bot_token, &config.tg_chat_id, &text).await {
        Ok(()) => ok(),
        Err(e) => refuse(&format!("发送失败：{e:#}")),
    }
}

/// The panel's routes, mounted with the hub's own so they share its session
/// check, same-origin rule and plain-text errors.
pub fn admin_routes() -> Router<Shared> {
    Router::new()
        .route(&format!("{API}/admin/state"), get(admin_state))
        .route(&format!("{API}/admin/watch"), get(admin_watch))
        .route(&format!("{API}/admin/history"), get(history))
        .route(&format!("{API}/admin/config"), get(get_config).put(save_config))
        .route(&format!("{API}/admin/config/token"), axum::routing::delete(clear_token))
        .route(&format!("{API}/admin/registration"), post(create_registration))
        .route(&format!("{API}/admin/node/rename"), post(rename_node))
        .route(&format!("{API}/admin/node/alias"), post(set_alias))
        .route(&format!("{API}/admin/node/delete"), post(delete_node))
        .route(&format!("{API}/admin/telegram/test"), post(test_telegram))
}

#[cfg(test)]
mod tests;
