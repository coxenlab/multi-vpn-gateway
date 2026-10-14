//! 每条通道今天的流量。运行环境在服务时每 10 秒读一次内层入口的 `/connections`,按连接 id
//! 求增量、按 `chains` 里的 `ch-<id>` 归到通道,写入 `<data_dir>/traffic-today.json`(0600,原子写),
//! 本地日期一变就清零。两次采样之间开始又结束的连接会漏计。
//! 控制口读不到(验证环境、入口没起来)时跳过这一轮,不写文件,不记事件。
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use crate::AppState;

const SAMPLE_EVERY: Duration = Duration::from_secs(10);
const FILE: &str = "traffic-today.json";

#[derive(Serialize, Deserialize, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Bytes {
    pub up: u64,
    pub down: u64,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq)]
pub struct Today {
    /// 本地日期 `YYYY-MM-DD`。
    pub date: String,
    /// 连接带 `upload`/`download` 字段时为真;从没见过带字段的连接时为假,界面隐藏这个数字。
    pub available: bool,
    pub channels: BTreeMap<String, Bytes>,
    /// 上次采样时每条连接的累计值 `[上行, 下行]`。一起落盘,重启后仍在的连接不重复计入。
    #[serde(default)]
    pub connections: BTreeMap<String, [u64; 2]>,
}

pub fn file_path(data_dir: &Path) -> PathBuf {
    data_dir.join(FILE)
}

fn local_date() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// 连接走的通道:`chains` 里 `ch-` 开头的那一项(命门 #7)。
fn channel_of(conn: &Value) -> Option<&str> {
    conn.get("chains")?.as_array()?.iter()
        .filter_map(Value::as_str)
        .find_map(|name| name.strip_prefix("ch-"))
}

/// 纯函数:把一次 `/connections` 结果累加进今天。日期变了先清零;差值为负按新连接处理;
/// 快照里有连接但都没有字节字段时 `available` 为假,没有连接时保持原值。返回是否有变化。
pub fn accumulate(state: &mut Today, snapshot: &Value, today: &str) -> bool {
    let before = state.clone();
    if state.date != today {
        state.date = today.to_string();
        state.channels.clear();
    }
    let conns = snapshot.get("connections").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    let mut seen = BTreeMap::new();
    let mut with_bytes = false;
    for conn in conns {
        let (Some(up), Some(down)) = (conn.get("upload").and_then(Value::as_u64), conn.get("download").and_then(Value::as_u64))
            else { continue };
        with_bytes = true;
        let (Some(id), Some(cid)) = (conn.get("id").and_then(Value::as_str), channel_of(conn)) else { continue };
        let (du, dd) = match state.connections.get(id) {
            Some(&[pu, pd]) if up >= pu && down >= pd => (up - pu, down - pd),
            _ => (up, down),
        };
        if du > 0 || dd > 0 {
            let total = state.channels.entry(cid.to_string()).or_default();
            total.up = total.up.saturating_add(du);
            total.down = total.down.saturating_add(dd);
        }
        seen.insert(id.to_string(), [up, down]);
    }
    if !conns.is_empty() {
        state.available = with_bytes;
    }
    state.connections = seen;
    *state != before
}

/// 读回落盘的结果;文件不在或读不懂时从空开始。
pub fn load(path: &Path) -> Today {
    std::fs::read(path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

pub fn save(path: &Path, state: &Today) -> anyhow::Result<()> {
    crate::manager::atomic_write_0600(path, &serde_json::to_vec(state)?)
}

/// 接口返回的形状:文件不是今天的就只带 `available`,流量为空。
pub fn view(saved: &Today, today: &str) -> Value {
    let channels = if saved.date == today { json!(saved.channels) } else { json!({}) };
    json!({"date": today, "available": saved.available, "channels": channels})
}

/// 读内层入口的 `/connections`;读不到或不是正常响应时为 None(不当成「没有连接」)。
async fn fetch(mihomo: &crate::mihomo::Controller) -> Option<Value> {
    let response = mihomo.client.get(format!("{}/connections", mihomo.base))
        .bearer_auth(&mihomo.secret)
        .timeout(Duration::from_secs(5))
        .send().await.ok()?;
    if !response.status().is_success() { return None; }
    response.json::<Value>().await.ok().filter(Value::is_object)
}

/// 采一次:运行环境不在服务或读不到时跳过;有变化才写文件。返回是否写了。
async fn sample(state: &AppState, path: &Path, today: &mut Today) -> bool {
    if !crate::runtime::serving(state) { return false; }
    let Some(snapshot) = fetch(&state.mihomo).await else { return false };
    if !accumulate(today, &snapshot, &local_date()) { return false; }
    if let Err(e) = save(path, today) {
        eprintln!("traffic: write {} failed: {e}", path.display());
        return false;
    }
    true
}

/// 由 [`crate::app::serve`] 启动。
pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        let path = file_path(&state.cfg.data_dir);
        let mut today = load(&path);
        let mut tick = tokio::time::interval(SAMPLE_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            if state.lifecycle.is_closing() { break; }
            sample(&state, &path, &mut today).await;
        }
    });
}

/// `GET /api/traffic/today`。
pub async fn today(State(st): State<AppState>) -> Json<Value> {
    Json(view(&load(&file_path(&st.cfg.data_dir)), &local_date()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn conn(id: &str, chains: &[&str], up: u64, down: u64) -> Value {
        json!({"id": id, "chains": chains, "upload": up, "download": down, "metadata": {"host": "app.example.test"}})
    }

    fn snapshot(conns: Vec<Value>) -> Value {
        json!({"connections": conns, "uploadTotal": 0, "downloadTotal": 0})
    }

    fn bytes(up: u64, down: u64) -> Bytes {
        Bytes { up, down }
    }

    #[test]
    fn same_day_accumulates_deltas_per_channel() {
        let mut t = Today::default();
        assert!(accumulate(&mut t, &snapshot(vec![
            conn("a", &["ch-c1"], 100, 1000),
            conn("b", &["ch-c2"], 5, 50),
            conn("d", &["DIRECT"], 7, 70),
        ]), "2026-09-29"));
        assert_eq!(t.date, "2026-09-29");
        assert!(t.available);
        assert_eq!(t.channels, BTreeMap::from([("c1".into(), bytes(100, 1000)), ("c2".into(), bytes(5, 50))]));
        // a 继续增长只计差值;b 结束;c 是新连接整笔计入;直连不计。
        accumulate(&mut t, &snapshot(vec![
            conn("a", &["ch-c1"], 150, 1600),
            conn("c", &["ch-c1"], 10, 20),
            conn("d", &["DIRECT"], 9, 90),
        ]), "2026-09-29");
        assert_eq!(t.channels["c1"], bytes(160, 1620));
        assert_eq!(t.channels["c2"], bytes(5, 50));
        assert!(!t.connections.contains_key("b"));
        // 入口重载后同一个 id 的累计值变小:按新连接处理,不出负数。
        accumulate(&mut t, &snapshot(vec![conn("a", &["ch-c1"], 3, 4)]), "2026-09-29");
        assert_eq!(t.channels["c1"], bytes(163, 1624));
        // 没有变化时不写。
        assert!(!accumulate(&mut t, &snapshot(vec![conn("a", &["ch-c1"], 3, 4)]), "2026-09-29"));
    }

    #[test]
    fn midnight_resets_and_keeps_counting_only_new_bytes() {
        let mut t = Today::default();
        accumulate(&mut t, &snapshot(vec![conn("a", &["ch-c1"], 100, 200)]), "2026-09-29");
        assert!(accumulate(&mut t, &snapshot(vec![conn("a", &["ch-c1"], 130, 260)]), "2026-09-30"));
        assert_eq!(t.date, "2026-09-30");
        // 跨过午夜仍在的连接只计午夜之后的部分。
        assert_eq!(t.channels, BTreeMap::from([("c1".into(), bytes(30, 60))]));
        // 空快照过午夜:清零、available 保持。
        assert!(accumulate(&mut t, &snapshot(vec![]), "2026-10-01"));
        assert!(t.channels.is_empty());
        assert!(t.available);
    }

    #[test]
    fn missing_byte_fields_make_it_unavailable() {
        let mut t = Today::default();
        // 还没见过连接:不可用。
        accumulate(&mut t, &snapshot(vec![]), "2026-09-29");
        assert!(!t.available);
        let no_bytes = json!({"connections": [{"id": "a", "chains": ["ch-c1"]}, {"id": "b", "chains": ["DIRECT"]}]});
        accumulate(&mut t, &no_bytes, "2026-09-29");
        assert!(!t.available);
        assert!(t.channels.is_empty());
        assert_eq!(view(&t, "2026-09-29"), json!({"date": "2026-09-29", "available": false, "channels": {}}));
        // mihomo 没有连接时可能给 null:按没有连接处理,available 不变。
        accumulate(&mut t, &snapshot(vec![conn("a", &["ch-c1"], 1, 2)]), "2026-09-29");
        assert!(t.available);
        accumulate(&mut t, &json!({"connections": null}), "2026-09-29");
        assert!(t.available);
    }

    #[test]
    fn saved_file_reads_back_and_view_follows_date() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = file_path(dir.path());
        assert_eq!(load(&path), Today::default());
        assert_eq!(view(&load(&path), "2026-09-29"), json!({"date": "2026-09-29", "available": false, "channels": {}}));
        let mut t = Today::default();
        accumulate(&mut t, &snapshot(vec![conn("a", &["ch-c1"], 100, 200)]), "2026-09-29");
        save(&path, &t).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let mut back = load(&path);
        assert_eq!(back, t);
        // 重启后仍在的连接不重复计入。
        assert!(!accumulate(&mut back, &snapshot(vec![conn("a", &["ch-c1"], 100, 200)]), "2026-09-29"));
        assert_eq!(view(&back, "2026-09-29"), json!({"date": "2026-09-29", "available": true, "channels": {"c1": {"up": 100, "down": 200}}}));
        // 文件是昨天的:流量为空,available 保留。
        assert_eq!(view(&back, "2026-09-30"), json!({"date": "2026-09-30", "available": true, "channels": {}}));
        std::fs::write(&path, b"not json").unwrap();
        assert_eq!(load(&path), Today::default());
    }

    fn state(dir: &Path) -> AppState {
        let mut cfg = crate::config::Config::from_getter(|_| None);
        cfg.data_dir = dir.into(); cfg.dev_mode = true; cfg.vm_profile = "vpnmgr-test".into();
        cfg.mihomo_ctrl_url = "http://127.0.0.1:1".into();
        AppState {
            mihomo: crate::mihomo::Controller::new(cfg.mihomo_ctrl_url.clone(), String::new()),
            cfg: Arc::new(cfg), lifecycle: Default::default(), docker: Arc::new(std::sync::RwLock::new(None)),
            health: crate::health::shared(), tunnel: crate::tunnel::handle(), novnc: crate::novnc::handle(),
            self_heal_enabled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    #[tokio::test]
    async fn unreachable_controller_or_idle_runtime_skips_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = file_path(dir.path());
        let mut st = state(dir.path());
        let mut t = Today::default();
        assert!(!sample(&st, &path, &mut t).await);
        assert!(!path.exists());
        assert_eq!(t, Today::default());
        Arc::make_mut(&mut st.cfg).managed_vm = true;
        assert!(!sample(&st, &path, &mut t).await);
        assert!(!path.exists());
    }
}
