//! 运行期结构化事件：内存环供 UI 增量读取，JSONL 供跨进程追溯。

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use axum::extract::Query;
use axum::http::{header, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Days, Local, NaiveDate};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

pub const RETAINED_DAYS: u32 = 14;
const RING_CAPACITY: usize = 1_000;
const CHANNEL_CAPACITY: usize = 1_024;
const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
const FILE_PREFIX: &str = "vpnmgr-";
const FILE_SUFFIX: &str = ".jsonl";
/// 记录开关的持久化标记:文件存在 = 关闭(默认开启)。
const DISABLED_MARKER: &str = "disabled";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    fn rank(self) -> u8 {
        match self {
            Self::Debug => 0,
            Self::Info => 1,
            Self::Warn => 2,
            Self::Error => 3,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub seq: u64,
    pub ts: String,
    pub ts_ms: u64,
    pub level: Level,
    pub src: String,
    pub event: String,
    pub msg: String,
    pub detail: Value,
}

#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub level: Option<Level>,
    pub src: Option<String>,
    pub event: Option<String>,
    pub q: Option<String>,
}

impl Filter {
    fn matches(&self, event: &Event) -> bool {
        if self
            .level
            .is_some_and(|level| event.level.rank() < level.rank())
        {
            return false;
        }
        if self.src.as_deref().is_some_and(|src| event.src != src) {
            return false;
        }
        if self
            .event
            .as_deref()
            .is_some_and(|code| event.event != code)
        {
            return false;
        }
        if let Some(q) = self.q.as_deref() {
            let q = q.to_lowercase();
            if !event.msg.to_lowercase().contains(&q) {
                return false;
            }
        }
        true
    }
}

struct Inner {
    ring: VecDeque<Event>,
    next_seq: u64,
    dropped: u64,
    /// 记录总开关:关闭时内存环与磁盘都不写(开关切换本身除外,走 force 旁路)。
    enabled: bool,
    data_dir: Option<PathBuf>,
    sender: Option<mpsc::Sender<Event>>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            ring: VecDeque::with_capacity(RING_CAPACITY),
            next_seq: 0,
            dropped: 0,
            enabled: true,
            data_dir: None,
            sender: None,
        }
    }
}

#[derive(Default)]
struct EventStore {
    inner: Mutex<Inner>,
}

impl EventStore {
    #[allow(clippy::too_many_arguments, clippy::result_large_err)]
    fn record_at(
        &self,
        at: DateTime<Local>,
        level: Level,
        src: &str,
        code: &str,
        msg: impl Into<String>,
        detail: Value,
        persist: bool,
    ) -> Event {
        self.record_inner(at, level, src, code, msg, detail, persist, false)
    }

    /// `force=true` 绕过记录总开关——只给开关切换本身用,别处一律走 `record_at`。
    #[allow(clippy::too_many_arguments, clippy::result_large_err)]
    fn record_inner(
        &self,
        at: DateTime<Local>,
        level: Level,
        src: &str,
        code: &str,
        msg: impl Into<String>,
        detail: Value,
        persist: bool,
        force: bool,
    ) -> Event {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !force && !inner.enabled {
            // 关闭期间既不占 seq 也不进环/磁盘;回一条未记录的占位事件(seq=0)。
            return Event {
                seq: 0,
                ts: at.to_rfc3339_opts(chrono::SecondsFormat::Millis, false),
                ts_ms: at.timestamp_millis().max(0) as u64,
                level,
                src: src.to_string(),
                event: code.to_string(),
                msg: redact_text(&msg.into()),
                detail: sanitize_detail(detail),
            };
        }
        inner.next_seq = inner.next_seq.saturating_add(1);
        let event = Event {
            seq: inner.next_seq,
            ts: at.to_rfc3339_opts(chrono::SecondsFormat::Millis, false),
            ts_ms: at.timestamp_millis().max(0) as u64,
            level,
            src: src.to_string(),
            event: code.to_string(),
            msg: redact_text(&msg.into()),
            detail: sanitize_detail(detail),
        };
        if inner.ring.len() == RING_CAPACITY {
            inner.ring.pop_front();
        }
        inner.ring.push_back(event.clone());
        if persist && level != Level::Debug {
            match inner.sender.as_ref().map(|tx| tx.try_send(event.clone())) {
                Some(Ok(())) => {}
                Some(Err(_)) | None => inner.dropped = inner.dropped.saturating_add(1),
            }
        }
        event
    }

    fn load(&self, mut events: Vec<Event>) {
        if events.len() > RING_CAPACITY {
            events.drain(..events.len() - RING_CAPACITY);
        }
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        for event in events {
            let mut event = sanitize_event(event);
            inner.next_seq = inner.next_seq.saturating_add(1);
            event.seq = inner.next_seq;
            if inner.ring.len() == RING_CAPACITY {
                inner.ring.pop_front();
            }
            inner.ring.push_back(event);
        }
    }

    fn snapshot(&self, since_seq: u64, limit: usize, filter: &Filter) -> (Vec<Event>, u64) {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let events = inner
            .ring
            .iter()
            .filter(|event| event.seq > since_seq && filter.matches(event))
            .take(limit.min(RING_CAPACITY))
            .cloned()
            .collect();
        (events, inner.next_seq)
    }
}

fn global() -> &'static EventStore {
    static STORE: OnceLock<EventStore> = OnceLock::new();
    STORE.get_or_init(EventStore::default)
}

fn init_guard() -> &'static Mutex<()> {
    static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    GUARD.get_or_init(|| Mutex::new(()))
}

/// 初始化日志目录与独立写盘任务。相同目录重复调用是 no-op。
pub fn init(data_dir: &Path) {
    let _guard = init_guard().lock().unwrap_or_else(|e| e.into_inner());
    let store = global();
    let first_init = {
        let mut inner = store.inner.lock().unwrap_or_else(|e| e.into_inner());
        match inner.data_dir.as_deref() {
            Some(existing) if existing != data_dir => {
                eprintln!(
                    "[events] 已初始化到 {},忽略不同目录 {}",
                    existing.display(),
                    data_dir.display()
                );
                return;
            }
            Some(_) if inner.sender.is_some() => return,
            Some(_) => false,
            None => {
                inner.data_dir = Some(data_dir.to_path_buf());
                true
            }
        }
    };

    let logs_dir = data_dir.join("logs");
    if let Err(e) = std::fs::create_dir_all(&logs_dir) {
        eprintln!("[events] 创建日志目录失败: {e}");
    }
    if let Err(e) = cleanup_old_logs(&logs_dir, Local::now().date_naive()) {
        eprintln!("[events] 清理过期日志失败: {e}");
    }
    {
        // 记录开关随 data_dir 持久化:标记文件存在 = 上次被关掉,重启后保持关闭。
        let disabled = logs_dir.join(DISABLED_MARKER).exists();
        let mut inner = store.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.enabled = !disabled;
    }

    if first_init {
        match load_persisted(&logs_dir, Local::now().date_naive()) {
            Ok(events) => store.load(events),
            Err(e) => eprintln!("[events] 回读历史日志失败: {e}"),
        }
    }

    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        eprintln!("[events] 当前没有 Tokio runtime,日志仅保留在内存");
        return;
    };
    let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
    {
        let mut inner = store.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.sender = Some(tx);
    }
    runtime.spawn(writer(rx, logs_dir));
}

pub fn emit(level: Level, src: &str, event: &str, msg: impl Into<String>, detail: Value) -> Event {
    global().record_at(Local::now(), level, src, event, msg, detail, true)
}

fn emit_memory_only(level: Level, src: &str, event: &str, msg: &str, detail: Value) {
    global().record_at(Local::now(), level, src, event, msg, detail, false);
}

/// 审计事件:每一个改变系统状态的操作留一条,记清「做了什么、对谁、改前是什么、改后是什么」。
///
/// detail 约定(缺的字段省略,不写 null 占位):
/// `target_kind`(channel/rule/rules/note/mirror/config/container/entry/system)、
/// `target_id`、`target_name`、`before`、`after`、`result`(ok/failed)、`error`。
/// ⚠️ 命门 #5:`before`/`after` 只放脱敏后的可见字段快照——密码、secret 字段、
/// vnc_password、备注正文一律不进(备注只记长度)。
pub fn audit(action: &str, msg: impl Into<String>, detail: Value) -> Event {
    let message = msg.into();
    eprintln!("[audit:{action}] {message}");
    emit(Level::Info, "audit", action, message, detail)
}

/// 失败的审计记录:同 [`audit`],但落 Error 级——运行日志屏按级别筛选时不该把失败降级成普通信息。
/// 调用方仍需在 detail 里带 `"result": "failed"` 与 `error` 文本。
pub fn audit_failed(action: &str, msg: impl Into<String>, detail: Value) -> Event {
    let message = msg.into();
    eprintln!("[audit:{action}] {message}");
    emit(Level::Error, "audit", action, message, detail)
}

/// 绕过记录总开关写一条审计——只给开关切换本身用。
fn audit_forced(action: &str, msg: &str, detail: Value) {
    eprintln!("[audit:{action}] {msg}");
    global().record_inner(Local::now(), Level::Info, "audit", action, msg, detail, true, true);
}

/// 记录总开关当前状态(默认开启)。
pub fn is_enabled() -> bool {
    global().inner.lock().unwrap_or_else(|e| e.into_inner()).enabled
}

/// 开/关运行事件记录。关闭后 [`emit`] 既不入内存环也不写盘,**唯一例外是开关切换本身**
/// ——否则「日志为什么断了」将无从追溯。状态持久化到 `<data_dir>/logs/disabled`。
pub fn set_enabled(on: bool) {
    let before = is_enabled();
    if !on {
        audit_forced(
            "logging_disabled",
            "运行事件记录已关闭(此后只记录重新开启)",
            json!({
                "target_kind": "system", "target_name": "event_log",
                "before": { "enabled": before }, "after": { "enabled": false }, "result": "ok"
            }),
        );
    }
    {
        let mut inner = global().inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.enabled = on;
    }
    if let Some(marker) = disabled_marker_path() {
        let written = if on {
            match std::fs::remove_file(&marker) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            }
        } else {
            std::fs::write(&marker, b"")
        };
        if let Err(e) = written {
            eprintln!("[events] 记录开关标记落盘失败: {e}");
        }
    }
    if on {
        audit_forced(
            "logging_enabled",
            "运行事件记录已开启",
            json!({
                "target_kind": "system", "target_name": "event_log",
                "before": { "enabled": before }, "after": { "enabled": true }, "result": "ok"
            }),
        );
    }
}

fn disabled_marker_path() -> Option<PathBuf> {
    let inner = global().inner.lock().unwrap_or_else(|e| e.into_inner());
    inner
        .data_dir
        .as_ref()
        .map(|dir| dir.join("logs").join(DISABLED_MARKER))
}

pub fn snapshot(since_seq: u64, limit: usize, filter: &Filter) -> (Vec<Event>, u64) {
    global().snapshot(since_seq, limit, filter)
}

pub fn dropped() -> u64 {
    global()
        .inner
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .dropped
}

pub fn log_path_today() -> Option<PathBuf> {
    let inner = global().inner.lock().unwrap_or_else(|e| e.into_inner());
    inner
        .data_dir
        .as_ref()
        .map(|dir| dir.join("logs").join(file_name(Local::now().date_naive())))
}

async fn writer(mut rx: mpsc::Receiver<Event>, logs_dir: PathBuf) {
    let mut open_date = None;
    let mut file = None;
    let mut size = 0u64;
    let mut full_date = None;
    while let Some(event) = rx.recv().await {
        let date = DateTime::parse_from_rfc3339(&event.ts)
            .map(|ts| ts.date_naive())
            .unwrap_or_else(|_| Local::now().date_naive());
        if open_date != Some(date) {
            let path = logs_dir.join(file_name(date));
            match tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .await
            {
                Ok(opened) => {
                    size = opened.metadata().await.map(|m| m.len()).unwrap_or(0);
                    file = Some(opened);
                    open_date = Some(date);
                    full_date = None;
                }
                Err(e) => {
                    note_write_drop(&format!("打开 {} 失败: {e}", path.display()));
                    continue;
                }
            }
        }
        let mut line = match serde_json::to_vec(&event) {
            Ok(line) => line,
            Err(e) => {
                note_write_drop(&format!("序列化事件失败: {e}"));
                continue;
            }
        };
        line.push(b'\n');
        if size.saturating_add(line.len() as u64) > MAX_FILE_BYTES {
            if full_date != Some(date) {
                full_date = Some(date);
                emit_memory_only(
                    Level::Warn,
                    "events",
                    "log_rotated_full",
                    "当日日志已到 10MB 上限,停止写盘",
                    json!({ "limit_bytes": MAX_FILE_BYTES }),
                );
            }
            continue;
        }
        if let Some(opened) = file.as_mut() {
            if let Err(e) = opened.write_all(&line).await {
                note_write_drop(&format!("写入运行日志失败: {e}"));
                continue;
            }
            if let Err(e) = opened.flush().await {
                note_write_drop(&format!("刷新运行日志失败: {e}"));
                continue;
            }
            size = size.saturating_add(line.len() as u64);
        }
    }
}

fn note_write_drop(message: &str) {
    eprintln!("[events] {message}");
    let mut inner = global().inner.lock().unwrap_or_else(|e| e.into_inner());
    inner.dropped = inner.dropped.saturating_add(1);
}

fn file_name(date: NaiveDate) -> String {
    format!("{FILE_PREFIX}{}{FILE_SUFFIX}", date.format("%Y-%m-%d"))
}

fn file_date(path: &Path) -> Option<NaiveDate> {
    let name = path.file_name()?.to_str()?;
    let date = name.strip_prefix(FILE_PREFIX)?.strip_suffix(FILE_SUFFIX)?;
    NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()
}

fn cleanup_old_logs(logs_dir: &Path, today: NaiveDate) -> std::io::Result<()> {
    let cutoff = today
        .checked_sub_days(Days::new((RETAINED_DAYS - 1) as u64))
        .unwrap_or(today);
    for entry in std::fs::read_dir(logs_dir)? {
        let path = entry?.path();
        if file_date(&path).is_some_and(|date| date < cutoff) {
            std::fs::remove_file(path)?;
        }
    }
    Ok(())
}

/// 闭区间 [from, to] 内的日志文件(按日期升序)。
fn retained_paths_between(
    logs_dir: &Path,
    from: NaiveDate,
    to: NaiveDate,
) -> std::io::Result<Vec<PathBuf>> {
    let mut paths = std::fs::read_dir(logs_dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| file_date(path).is_some_and(|date| date >= from && date <= to))
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

fn retained_paths(logs_dir: &Path, today: NaiveDate, days: u32) -> std::io::Result<Vec<PathBuf>> {
    let oldest = today
        .checked_sub_days(Days::new(days.saturating_sub(1) as u64))
        .unwrap_or(today);
    retained_paths_between(logs_dir, oldest, today)
}

/// 保留窗口最早的一天(今天往前 RETAINED_DAYS 天)。
fn oldest_retained(today: NaiveDate) -> NaiveDate {
    today
        .checked_sub_days(Days::new((RETAINED_DAYS - 1) as u64))
        .unwrap_or(today)
}

fn load_persisted(logs_dir: &Path, today: NaiveDate) -> std::io::Result<Vec<Event>> {
    let mut loaded = VecDeque::with_capacity(RING_CAPACITY);
    for path in retained_paths(logs_dir, today, RETAINED_DAYS)? {
        for line in std::fs::read_to_string(path)?.lines() {
            if let Ok(event) = serde_json::from_str::<Event>(line) {
                if loaded.len() == RING_CAPACITY {
                    loaded.pop_front();
                }
                loaded.push_back(sanitize_event(event));
            }
        }
    }
    Ok(loaded.into())
}

fn sanitize_event(mut event: Event) -> Event {
    event.msg = redact_text(&event.msg);
    event.detail = sanitize_detail(event.detail);
    event
}

fn export_persisted(logs_dir: &Path, from: NaiveDate, to: NaiveDate) -> std::io::Result<String> {
    let mut body = String::new();
    for path in retained_paths_between(logs_dir, from, to)? {
        for line in std::fs::read_to_string(path)?.lines() {
            let Ok(event) = serde_json::from_str::<Event>(line) else {
                continue;
            };
            let encoded = serde_json::to_string(&sanitize_event(event))
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            body.push_str(&encoded);
            body.push('\n');
        }
    }
    Ok(body)
}

fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "password",
        "secret",
        "token",
        "private_key",
        "master_key",
        "authorization",
    ]
    .iter()
    .any(|needle| key.contains(needle))
}

/// 递归脱敏:嵌套 object / array 任意深度都过一遍,敏感键在任何一层都换成 `[REDACTED]`,
/// 字符串值走 [`redact_text`]。审计的 before/after 快照本就是嵌套结构,不能再整块丢弃。
/// (深度由 serde_json 解析期的递归上限兜底,detail 又都是本进程内构造的。)
fn sanitize_value(value: Value) -> Value {
    match value {
        Value::String(s) => Value::String(redact_text(&s)),
        Value::Array(items) => Value::Array(items.into_iter().map(sanitize_value).collect()),
        Value::Object(input) => {
            let mut output = Map::new();
            for (key, value) in input {
                let value = if is_sensitive_key(&key) {
                    Value::String("[REDACTED]".into())
                } else {
                    sanitize_value(value)
                };
                output.insert(key, value);
            }
            Value::Object(output)
        }
        other => other,
    }
}

/// detail 顶层恒为 object(JSONL 行结构不变),非 object 一律丢成空 object。
fn sanitize_detail(detail: Value) -> Value {
    match detail {
        Value::Object(_) => sanitize_value(detail),
        _ => json!({}),
    }
}

fn redact_text(input: &str) -> String {
    let mut output = input.to_string();
    for key in [
        "password",
        "secret",
        "token",
        "private_key",
        "master_key",
        "authorization",
    ] {
        let mut search_from = 0;
        loop {
            let lower = output.to_ascii_lowercase();
            let Some(found) = lower[search_from..].find(key) else {
                break;
            };
            let start = search_from + found;
            let tail = &output[start + key.len()..];
            let Some(sep_rel) = tail.find(['=', ':']) else {
                break;
            };
            let value_start = start + key.len() + sep_rel + 1;
            let value_end = output[value_start..]
                .find(|c: char| c == ',' || c == ';' || c.is_whitespace() || c == '}' || c == ']')
                .map(|n| value_start + n)
                .unwrap_or(output.len());
            output.replace_range(value_start..value_end, "[REDACTED]");
            search_from = value_start + "[REDACTED]".len();
        }
    }
    output
}

#[derive(Debug, Deserialize)]
pub struct EventsQuery {
    #[serde(default)]
    since_seq: u64,
    #[serde(default = "default_limit")]
    limit: usize,
    level: Option<Level>,
    src: Option<String>,
    event: Option<String>,
    q: Option<String>,
}

fn default_limit() -> usize {
    200
}

pub async fn list(Query(query): Query<EventsQuery>) -> Json<Value> {
    let filter = Filter {
        level: query.level,
        src: query.src,
        event: query.event,
        q: query.q,
    };
    let (events, seq) = snapshot(
        query.since_seq,
        query.limit.clamp(1, RING_CAPACITY),
        &filter,
    );
    Json(json!({
        "events": events,
        "seq": seq,
        "dropped": dropped(),
        "enabled": is_enabled(),
        "retained_days": RETAINED_DAYS,
        "file": log_path_today(),
    }))
}

#[derive(Debug, Deserialize)]
pub struct EnabledBody {
    enabled: bool,
}

/// GET /api/events/enabled —— 读记录总开关。
pub async fn enabled_get() -> Json<Value> {
    Json(json!({ "enabled": is_enabled() }))
}

/// POST /api/events/enabled —— 开/关记录。关闭本身会被记录一条(审计不能自我抹除)。
pub async fn set_enabled_route(Json(body): Json<EnabledBody>) -> Json<Value> {
    set_enabled(body.enabled);
    Json(json!({ "enabled": is_enabled() }))
}

#[derive(Debug, Deserialize)]
pub struct ExportQuery {
    #[serde(default = "default_days")]
    days: u32,
    /// 起止日期(YYYY-MM-DD,闭区间)。给了任一个就按区间导,否则沿用 days 语义。
    /// 收字符串而非 NaiveDate:chrono 没开 serde feature,解析放 export_range 里,失败即 400。
    from: Option<String>,
    to: Option<String>,
}

fn parse_day(value: &Option<String>) -> Result<Option<NaiveDate>, ()> {
    match value.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(text) => NaiveDate::parse_from_str(text, "%Y-%m-%d").map(Some).map_err(|_| ()),
    }
}

fn default_days() -> u32 {
    1
}

/// 把请求区间收敛成实际要导的闭区间:缺 to 补今天、缺 from 补 to;两者都缺则按 days
/// 往前推。再裁剪到保留窗口(今天往前 RETAINED_DAYS 天)——窗口外没有文件,给个空
/// 文件不如直说。区间反了或整段落在窗口外都回 None(调用方 400)。
fn export_range(query: &ExportQuery, today: NaiveDate) -> Option<(NaiveDate, NaiveDate)> {
    let (Ok(query_from), Ok(query_to)) = (parse_day(&query.from), parse_day(&query.to)) else {
        return None;
    };
    let (from, to) = match (query_from, query_to) {
        (None, None) => {
            let days = query.days.clamp(1, RETAINED_DAYS);
            let from = today
                .checked_sub_days(Days::new(days.saturating_sub(1) as u64))
                .unwrap_or(today);
            (from, today)
        }
        (from, to) => {
            let to = to.unwrap_or(today);
            (from.unwrap_or(to), to)
        }
    };
    if from > to {
        return None;
    }
    let clamped = (from.max(oldest_retained(today)), to.min(today));
    (clamped.0 <= clamped.1).then_some(clamped)
}

pub async fn export(Query(query): Query<ExportQuery>) -> Response {
    let Some(path) = log_path_today() else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "event log not initialized",
        )
            .into_response();
    };
    let logs_dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let Some((from, to)) = export_range(&query, Local::now().date_naive()) else {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            format!("导出区间非法或不在最近 {RETAINED_DAYS} 天保留窗口内"),
        )
            .into_response();
    };
    let body = tokio::task::spawn_blocking(move || export_persisted(&logs_dir, from, to)).await;
    let Ok(Ok(body)) = body else {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "event log export failed",
        )
            .into_response();
    };
    let mut response = body.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    let filename = format!("vpnmgr-events-{from}_{to}.jsonl");
    if let Ok(value) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
        response.headers_mut().insert(header::CONTENT_DISPOSITION, value);
    }
    response
}

#[doc(hidden)]
pub use serde_json;

#[macro_export]
macro_rules! ev {
    ($level:ident, $src:expr, $event:expr, $msg:expr, $detail:tt) => {{
        let message = ($msg).to_string();
        eprintln!("[{}:{}] {}", $src, $event, message);
        $crate::events::emit(
            $crate::ev!(@level $level),
            $src,
            $event,
            message,
            $crate::events::serde_json::json!($detail),
        )
    }};
    (@level debug) => { $crate::events::Level::Debug };
    (@level info) => { $crate::events::Level::Info };
    (@level warn) => { $crate::events::Level::Warn };
    (@level error) => { $crate::events::Level::Error };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ms: i64) -> DateTime<Local> {
        DateTime::from_timestamp_millis(ms)
            .unwrap()
            .with_timezone(&Local)
    }

    #[test]
    fn ring_is_bounded_and_since_seq_is_incremental() {
        let store = EventStore::default();
        for i in 0..1_050 {
            store.record_at(
                at(i),
                Level::Info,
                "test",
                "tick",
                format!("{i}"),
                json!({}),
                false,
            );
        }
        let (events, seq) = store.snapshot(0, RING_CAPACITY, &Filter::default());
        assert_eq!(events.len(), RING_CAPACITY);
        assert_eq!(events[0].seq, 51);
        assert_eq!(seq, 1_050);
        let (events, _) = store.snapshot(1_048, 10, &Filter::default());
        assert_eq!(
            events.iter().map(|event| event.seq).collect::<Vec<_>>(),
            vec![1_049, 1_050]
        );
    }

    #[test]
    fn concurrent_records_keep_unique_monotonic_sequence() {
        let store = std::sync::Arc::new(EventStore::default());
        let mut threads = vec![];
        for _ in 0..8 {
            let store = store.clone();
            threads.push(std::thread::spawn(move || {
                for i in 0..100 {
                    store.record_at(
                        at(i),
                        Level::Info,
                        "test",
                        "parallel",
                        "x",
                        json!({}),
                        false,
                    );
                }
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
        let (events, seq) = store.snapshot(0, RING_CAPACITY, &Filter::default());
        assert_eq!(events.len(), 800);
        assert_eq!(seq, 800);
        assert!(events.windows(2).all(|pair| pair[0].seq + 1 == pair[1].seq));
    }

    #[test]
    fn combined_filters_match_contract() {
        let store = EventStore::default();
        store.record_at(
            at(1),
            Level::Info,
            "tunnel",
            "ready",
            "SSH Ready",
            json!({}),
            false,
        );
        store.record_at(
            at(2),
            Level::Warn,
            "watchdog",
            "heal_start",
            "开始修复",
            json!({}),
            false,
        );
        store.record_at(
            at(3),
            Level::Error,
            "watchdog",
            "heal_failed",
            "修复 FAILED",
            json!({}),
            false,
        );
        let filter = Filter {
            level: Some(Level::Warn),
            src: Some("watchdog".into()),
            event: Some("heal_failed".into()),
            q: Some("failed".into()),
        };
        let (events, _) = store.snapshot(0, 10, &filter);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "heal_failed");
    }

    #[test]
    fn full_writer_queue_increments_visible_drop_count() {
        let store = EventStore::default();
        let (tx, _rx) = mpsc::channel(1);
        store.inner.lock().unwrap().sender = Some(tx);
        store.record_at(at(1), Level::Info, "test", "one", "one", json!({}), true);
        store.record_at(at(2), Level::Info, "test", "two", "two", json!({}), true);
        assert_eq!(store.inner.lock().unwrap().dropped, 1);
    }

    #[test]
    fn detail_and_message_redact_secrets_at_any_depth() {
        let store = EventStore::default();
        let event = store.record_at(
            at(1),
            Level::Error,
            "test",
            "secret",
            "failed password=hunter2 token:abc",
            json!({
                "password": "hunter2",
                "api_secret": "abc",
                "ok": true,
                "before": { "password": "hunter2", "ok": 1, "deep": [{ "auth_token": "abc" }] },
            }),
            false,
        );
        let encoded = serde_json::to_string(&event).unwrap();
        assert!(!encoded.contains("hunter2"));
        assert!(!encoded.contains("\"abc\""));
        assert_eq!(event.detail["password"], "[REDACTED]");
        // 审计的 before/after 是嵌套快照:整块保留,敏感键逐层替换,其余原样。
        assert_eq!(event.detail["before"]["password"], "[REDACTED]");
        assert_eq!(event.detail["before"]["ok"], 1);
        assert_eq!(event.detail["before"]["deep"][0]["auth_token"], "[REDACTED]");
    }

    #[test]
    fn disabled_logging_records_nothing_but_the_switch_itself() {
        let store = EventStore::default();
        store.record_at(at(1), Level::Info, "test", "before", "before", json!({}), false);
        store.inner.lock().unwrap().enabled = false;
        store.record_at(at(2), Level::Info, "test", "muted", "muted", json!({}), false);
        let (events, seq) = store.snapshot(0, 10, &Filter::default());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "before");
        assert_eq!(seq, 1, "关闭期间不占 seq");
        // 开关切换本身走 force 旁路,关闭状态下仍要留痕。
        store.record_inner(
            at(3),
            Level::Info,
            "audit",
            "logging_disabled",
            "off",
            json!({}),
            false,
            true,
        );
        let (events, _) = store.snapshot(0, 10, &Filter::default());
        assert_eq!(
            events.iter().map(|e| e.event.as_str()).collect::<Vec<_>>(),
            vec!["before", "logging_disabled"]
        );
    }

    #[test]
    fn export_range_picks_only_the_requested_days() {
        let dir = tempfile::tempdir().unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 8, 6).unwrap();
        let line = |code: &str| {
            let event = Event {
                seq: 1,
                ts: "2026-08-06T09:41:07.812+08:00".into(),
                ts_ms: 1,
                level: Level::Info,
                src: "test".into(),
                event: code.into(),
                msg: code.into(),
                detail: json!({}),
            };
            format!("{}\n", serde_json::to_string(&event).unwrap())
        };
        for (offset, code) in [(2u64, "oldest"), (1, "middle"), (0, "newest")] {
            let date = today.checked_sub_days(Days::new(offset)).unwrap();
            std::fs::write(dir.path().join(file_name(date)), line(code)).unwrap();
        }
        let middle = today.checked_sub_days(Days::new(1)).unwrap();
        let exported = export_persisted(dir.path(), middle, middle).unwrap();
        assert_eq!(exported.lines().count(), 1);
        assert!(exported.contains("middle"));

        // from/to 缺一补齐;区间反了或整段在保留窗口外 → None(端点 400)。
        let day = |d: NaiveDate| Some(d.format("%Y-%m-%d").to_string());
        let q = |days: u32, from: Option<String>, to: Option<String>| ExportQuery { days, from, to };
        assert_eq!(export_range(&q(1, None, None), today), Some((today, today)));
        assert_eq!(export_range(&q(1, day(middle), None), today), Some((middle, today)));
        assert_eq!(export_range(&q(1, None, day(middle)), today), Some((middle, middle)));
        assert_eq!(export_range(&q(1, day(today), day(middle)), today), None);
        let ancient = today.checked_sub_days(Days::new(90)).unwrap();
        assert_eq!(export_range(&q(1, day(ancient), day(ancient)), today), None);
        assert_eq!(export_range(&q(1, Some("八月六号".into()), None), today), None);
        // 越界的 days / 未来的 to 都裁回保留窗口。
        assert_eq!(
            export_range(&q(999, None, None), today),
            Some((oldest_retained(today), today))
        );
    }

    #[tokio::test]
    async fn writer_uses_daily_jsonl_and_cleanup_keeps_fourteen_days() {
        let dir = tempfile::tempdir().unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 8, 6).unwrap();
        let old = dir
            .path()
            .join(file_name(today.checked_sub_days(Days::new(14)).unwrap()));
        let keep = dir
            .path()
            .join(file_name(today.checked_sub_days(Days::new(13)).unwrap()));
        std::fs::write(&old, "old").unwrap();
        std::fs::write(&keep, "keep").unwrap();
        cleanup_old_logs(dir.path(), today).unwrap();
        assert!(!old.exists());
        assert!(keep.exists());

        let (tx, rx) = mpsc::channel(2);
        let task = tokio::spawn(writer(rx, dir.path().to_path_buf()));
        tx.send(Event {
            seq: 1,
            ts: "2026-08-06T09:41:07.812+08:00".into(),
            ts_ms: 1,
            level: Level::Info,
            src: "test".into(),
            event: "written".into(),
            msg: "ok".into(),
            detail: json!({}),
        })
        .await
        .unwrap();
        drop(tx);
        task.await.unwrap();
        let text = std::fs::read_to_string(dir.path().join("vpnmgr-2026-08-06.jsonl")).unwrap();
        let parsed: Event = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(parsed.event, "written");
    }

    #[test]
    fn persisted_events_reload_in_file_order_with_fresh_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 8, 6).unwrap();
        let event = |seq: u64, ts: &str, code: &str| Event {
            seq,
            ts: ts.into(),
            ts_ms: seq,
            level: Level::Info,
            src: "test".into(),
            event: code.into(),
            msg: code.into(),
            detail: json!({}),
        };
        let older =
            serde_json::to_string(&event(90, "2026-08-05T23:59:00+08:00", "older")).unwrap();
        let newer = serde_json::to_string(&event(2, "2026-08-06T00:01:00+08:00", "newer")).unwrap();
        std::fs::write(
            dir.path().join("vpnmgr-2026-08-05.jsonl"),
            format!("{older}\n"),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("vpnmgr-2026-08-06.jsonl"),
            format!("{newer}\n"),
        )
        .unwrap();
        let store = EventStore::default();
        store.load(load_persisted(dir.path(), today).unwrap());
        let (events, seq) = store.snapshot(0, 10, &Filter::default());
        assert_eq!(
            events
                .iter()
                .map(|event| event.event.as_str())
                .collect::<Vec<_>>(),
            vec!["older", "newer"]
        );
        assert_eq!(
            events.iter().map(|event| event.seq).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(seq, 2);
    }

    #[test]
    fn persisted_events_are_sanitized_on_reload_and_export() {
        let dir = tempfile::tempdir().unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 8, 6).unwrap();
        let path = dir.path().join(file_name(today));
        let legacy = Event {
            seq: 9,
            ts: "2026-08-06T09:41:07.812+08:00".into(),
            ts_ms: 9,
            level: Level::Error,
            src: "legacy".into(),
            event: "failed".into(),
            msg: "request password=hunter2".into(),
            detail: json!({"api_token": "abc", "ok": false}),
        };
        std::fs::write(
            &path,
            format!(
                "{}\nnot-json password=plaintext\n",
                serde_json::to_string(&legacy).unwrap()
            ),
        )
        .unwrap();

        let loaded = load_persisted(dir.path(), today).unwrap();
        let loaded_text = serde_json::to_string(&loaded).unwrap();
        assert!(!loaded_text.contains("hunter2"));
        assert!(!loaded_text.contains("\"abc\""));
        assert_eq!(loaded[0].detail["api_token"], "[REDACTED]");

        let exported = export_persisted(dir.path(), today, today).unwrap();
        assert!(!exported.contains("hunter2"));
        assert!(!exported.contains("plaintext"));
        assert!(!exported.contains("\"abc\""));
        let exported_event: Event = serde_json::from_str(exported.trim()).unwrap();
        assert_eq!(exported_event.detail["api_token"], "[REDACTED]");
    }

    #[test]
    fn loading_history_keeps_the_ring_bounded() {
        let store = EventStore::default();
        for i in 0..RING_CAPACITY {
            store.record_at(
                at(i as i64),
                Level::Info,
                "live",
                "tick",
                "live",
                json!({}),
                false,
            );
        }
        let history = (0..RING_CAPACITY)
            .map(|i| Event {
                seq: i as u64,
                ts: "2026-08-06T09:41:07.812+08:00".into(),
                ts_ms: i as u64,
                level: Level::Info,
                src: "history".into(),
                event: "loaded".into(),
                msg: "history".into(),
                detail: json!({}),
            })
            .collect();
        store.load(history);
        let (events, seq) = store.snapshot(0, RING_CAPACITY, &Filter::default());
        assert_eq!(events.len(), RING_CAPACITY);
        assert_eq!(seq, (RING_CAPACITY * 2) as u64);
        assert!(events.iter().all(|event| event.src == "history"));
    }
}
