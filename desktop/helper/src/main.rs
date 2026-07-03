//! vpnmgr-helper — root LaunchDaemon(ClashX Meta 同款提权模型,任务 #6 层3)。
//!
//! 职责(全部需要 root):
//! 1. 监管宿主 mihomo#2(TUN 引擎):按 app 下发的冻结配置起停、崩溃自动拉起;
//! 2. utun 路由对账:把 app 下发的 IP-CIDR 集合增删到 `route`(最长前缀与 ClashX TUN 共存);
//! 3. unix socket IPC(0660 root:staff + peer-uid 鉴权)收 app 指令,状态持久化、重启机自恢复。
//!
//! 设计要点:
//! - **鉴权**:socket 组权限只挡住远程/其他机制,本机所有用户都在 staff 组,故 accept 后用
//!   `getpeereid` 核对连接方 uid,只放行 root 与安装时记录的 owner uid(`owner.uid`)。
//! - mihomo#2 配置由 app 生成、经 IPC 全量下发,helper 只落盘不理解内容;
//!   规则变更**只动路由表**(mihomo#2 配置冻结,utun 永不重建、路由不丢)。
//! - 路由所有权明确:只有**我们亲手 `route add` 成功**的网段进 `owned`,删除只删 owned——
//!   预先存在的同网段路由(`route add` 报 "File exists")记入 `shadowed`,既不接管也绝不删,
//!   避免误拆 LAN / 其他 VPN / 另一通道的路由。
//! - 对账走 2s 轮询 reconciler,route 执行**不持锁**(快照→放锁→执行→回锁提交),
//!   避免长批路由把并发 IPC(status/ping)阻塞到 app 端超时。
//! - 安全命门:本二进制与 mihomo 必须装在 root 属主目录(install 脚本负责),
//!   指向用户可写路径 = 本地提权洞。

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const SOCK_PATH: &str = "/var/run/vpnmgr-helper.sock";
const BASE_DIR: &str = "/Library/PrivilegedHelperTools/vpnmgr";
/// macOS `staff` 组(所有本地用户都在),socket 0660 root:staff = 远程/其他机制挡在外;
/// 本机用户间的隔离靠 peer-uid 鉴权(见 [`peer_uid`] / [`authorized`])。
const STAFF_GID: u32 = 20;
/// TUN 设备名与 app 侧约定 pin 死(desktop/core entry.rs 同名常量),路由管理零猜测。
const DEVICE: &str = "utun225";

fn config_path() -> String { format!("{BASE_DIR}/mihomo.yaml") }
fn state_path() -> String { format!("{BASE_DIR}/state.json") }
fn owner_path() -> String { format!("{BASE_DIR}/owner.uid") }
fn mihomo_bin() -> String { format!("{BASE_DIR}/mihomo") }
fn mihomo_log() -> String { format!("{BASE_DIR}/mihomo.log") }

/// app 期望的运行态(IPC 下发,落盘 state.json,重启机后恢复)。
#[derive(Serialize, Deserialize, Default, Clone)]
struct Desired {
    running: bool,
    config: String,
    #[serde(default)]
    v4: Vec<String>,
    #[serde(default)]
    v6: Vec<String>,
}

struct Inner {
    desired: Desired,
    child: Option<Child>,
    /// 我们亲手 add 成功的 (cidr, is_v6)——删除只针对它,绝不碰别人的路由。
    owned: HashSet<(String, bool)>,
    /// desired 里但 add 时已存在(File exists)的网段:不接管、不删、不反复重试。
    shadowed: HashSet<(String, bool)>,
}

type Shared = Arc<Mutex<Inner>>;

// ── peer 鉴权 ─────────────────────────────────────────────────────────────────

/// 取 unix socket 对端 euid(macOS `getpeereid`)。失败 → None(按拒绝处理)。
fn peer_uid(stream: &UnixStream) -> Option<u32> {
    let mut euid: libc::uid_t = 0;
    let mut egid: libc::gid_t = 0;
    let r = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut euid, &mut egid) };
    if r == 0 {
        Some(euid)
    } else {
        None
    }
}

/// 只放行 root 与安装时记录的 owner uid。owner.uid 缺失 → 仅 root(安全默认)。
fn authorized(stream: &UnixStream, owner: Option<u32>) -> bool {
    match peer_uid(stream) {
        Some(0) => true,
        Some(uid) => owner == Some(uid),
        None => false,
    }
}

fn read_owner_uid() -> Option<u32> {
    std::fs::read_to_string(owner_path()).ok().and_then(|s| s.trim().parse().ok())
}

// ── 路由命令(纯函数,可测) ────────────────────────────────────────────────

fn route_args(add: bool, cidr: &str, v6: bool, device: &str) -> Vec<String> {
    let mut a: Vec<String> = vec!["-n".into(), if add { "add" } else { "delete" }.into()];
    if v6 {
        a.push("-inet6".into());
    }
    a.push("-net".into());
    a.push(cidr.into());
    if add {
        a.push("-interface".into());
        a.push(device.into());
    }
    a
}

#[derive(Debug, PartialEq, Eq)]
enum AddResult {
    /// 亲手创建成功 → 归 owned,删除时可安全回收。
    Added,
    /// 目的网段已有路由(File exists)→ 归 shadowed,不接管不删。
    Exists,
    /// 其它失败 → 下拍重试(utun 未就绪等时序)。
    Failed,
}

/// route 结果 → AddResult(纯分类,便于测试)。
/// ⚠️ macOS 命门:`route add` 撞已有目的网段时 **exit 仍为 0**、只把 "File exists" 写 stderr
/// (实测:exit=0),故必须**先**看 stderr、再看 exit,否则会把「撞车未接管」误判成 Added,
/// 后续 unbind 时 `route delete` 就会拆掉别人(LAN/其他 VPN/另一通道)的路由。
fn classify_add(success: bool, stderr: &str) -> AddResult {
    if stderr.contains("File exists") {
        AddResult::Exists
    } else if success {
        AddResult::Added
    } else {
        AddResult::Failed
    }
}

fn route_add(cidr: &str, v6: bool) -> AddResult {
    match Command::new("/sbin/route").args(route_args(true, cidr, v6, DEVICE)).output() {
        Ok(o) => {
            let r = classify_add(o.status.success(), &String::from_utf8_lossy(&o.stderr));
            if r == AddResult::Failed {
                eprintln!("[helper] route add {cidr} 失败: {}", String::from_utf8_lossy(&o.stderr).trim());
            }
            r
        }
        Err(e) => {
            eprintln!("[helper] route add {cidr} 执行失败: {e}");
            AddResult::Failed
        }
    }
}

/// 删路由(只对 owned 调用):失败多半是路由已不在,记日志即可。
fn route_del(cidr: &str, v6: bool) {
    match Command::new("/sbin/route").args(route_args(false, cidr, v6, DEVICE)).output() {
        Ok(o) if !o.status.success() => {
            eprintln!("[helper] route delete {cidr}: {}", String::from_utf8_lossy(&o.stderr).trim());
        }
        Err(e) => eprintln!("[helper] route delete {cidr} 执行失败: {e}"),
        _ => {}
    }
}

// ── mihomo 子进程 ────────────────────────────────────────────────────────────

fn child_alive(child: &mut Option<Child>) -> bool {
    match child {
        Some(c) => match c.try_wait() {
            Ok(None) => true,
            _ => {
                *child = None;
                false
            }
        },
        None => false,
    }
}

fn spawn_mihomo() -> std::io::Result<Child> {
    let log = std::fs::File::create(mihomo_log())?;
    let log2 = log.try_clone()?;
    Command::new(mihomo_bin())
        .args(["-f", &config_path(), "-d", BASE_DIR])
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log2))
        .spawn()
}

fn kill_child(child: &mut Option<Child>) {
    if let Some(mut c) = child.take() {
        let _ = c.kill();
        let _ = c.wait();
    }
}

/// helper 启动时清掉可能的孤儿 mihomo(上代 helper 被 SIGKILL 等极端情况)。
fn kill_strays() {
    let _ = Command::new("/usr/bin/pkill").args(["-f", &mihomo_bin()]).output();
}

// ── 状态持久化 ───────────────────────────────────────────────────────────────

fn save_state(d: &Desired) {
    if let Ok(s) = serde_json::to_string(d) {
        let _ = std::fs::write(state_path(), s);
        let _ = std::fs::set_permissions(state_path(), std::fs::Permissions::from_mode(0o600));
    }
}

fn load_state() -> Desired {
    std::fs::read_to_string(state_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

// ── reconciler:每 2s 一拍,把现实拉向 desired ──────────────────────────────

fn desired_set(d: &Desired) -> HashSet<(String, bool)> {
    d.v4.iter()
        .map(|c| (c.clone(), false))
        .chain(d.v6.iter().map(|c| (c.clone(), true)))
        .collect()
}

fn reconcile(shared: &Shared) {
    // ① 快照(持锁,短)
    let (running, alive, desired) = {
        let mut g = shared.lock().unwrap();
        let alive = child_alive(&mut g.child);
        let desired = if g.desired.running { desired_set(&g.desired) } else { HashSet::new() };
        (g.desired.running, alive, desired)
    };

    // ② 引擎该跑却没跑 → 起(持锁);utun 重建后内核已清路由,owned/shadowed 归零全量重加
    if running && !alive {
        let mut g = shared.lock().unwrap();
        if !child_alive(&mut g.child) {
            let _ = std::fs::write(config_path(), &g.desired.config);
            let _ = std::fs::set_permissions(config_path(), std::fs::Permissions::from_mode(0o600));
            match spawn_mihomo() {
                Ok(c) => {
                    eprintln!("[helper] mihomo 已拉起 pid={}", c.id());
                    g.child = Some(c);
                }
                Err(e) => eprintln!("[helper] mihomo 拉起失败: {e}"),
            }
            g.owned.clear();
            g.shadowed.clear();
        }
        return; // utun 未就绪,路由留到下拍
    }

    // ③ 引擎该停却在跑 → 停(持锁);utun 随子进程销毁,内核自动清路由
    if !running {
        let mut g = shared.lock().unwrap();
        if child_alive(&mut g.child) {
            eprintln!("[helper] 停止 mihomo(desired.running=false)");
            kill_child(&mut g.child);
        }
        g.owned.clear();
        g.shadowed.clear();
        return;
    }

    // ④ running && alive:路由对账。快照 owned/shadowed 后**放锁**再 shell route,末尾回锁提交。
    let (owned, shadowed) = {
        let g = shared.lock().unwrap();
        (g.owned.clone(), g.shadowed.clone())
    };
    let to_add: Vec<_> = desired
        .iter()
        .filter(|k| !owned.contains(k) && !shadowed.contains(k))
        .cloned()
        .collect();
    let to_del: Vec<_> = owned.difference(&desired).cloned().collect();
    let drop_shadow: Vec<_> = shadowed.difference(&desired).cloned().collect();

    let mut add_owned = Vec::new();
    let mut add_shadow = Vec::new();
    for (cidr, v6) in to_add {
        match route_add(&cidr, v6) {
            AddResult::Added => add_owned.push((cidr, v6)),
            AddResult::Exists => {
                eprintln!("[helper] {cidr} 已有系统路由,不接管(shadowed)");
                add_shadow.push((cidr, v6));
            }
            AddResult::Failed => {} // 下拍重试
        }
    }
    for (cidr, v6) in &to_del {
        route_del(cidr, *v6);
    }

    let mut g = shared.lock().unwrap();
    for k in add_owned {
        g.owned.insert(k);
    }
    for k in add_shadow {
        g.shadowed.insert(k);
    }
    for k in to_del {
        g.owned.remove(&k);
    }
    for k in drop_shadow {
        g.shadowed.remove(&k);
    }
}

// ── IPC ──────────────────────────────────────────────────────────────────────

fn status_json(g: &mut Inner) -> serde_json::Value {
    let alive = child_alive(&mut g.child);
    serde_json::json!({
        "ok": true,
        "version": VERSION,
        "running": g.desired.running,
        "alive": alive,
        "device": DEVICE,
        "config": g.desired.config,
        "v4": g.desired.v4,
        "v6": g.desired.v6,
        "applied": g.owned.len(),
        "shadowed": g.shadowed.len(),
    })
}

fn handle_conn(stream: UnixStream, shared: &Shared, owner: Option<u32>) {
    if !authorized(&stream, owner) {
        let mut w = stream;
        let _ = writeln!(w, "{}", serde_json::json!({ "ok": false, "error": "unauthorized" }));
        return;
    }
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let req: serde_json::Value = serde_json::from_str(&line).unwrap_or_else(|_| serde_json::json!({}));
    let cmd = req.get("cmd").and_then(|v| v.as_str()).unwrap_or("");
    let resp = {
        let mut g = shared.lock().unwrap();
        match cmd {
            "ping" => serde_json::json!({ "ok": true, "version": VERSION }),
            "status" => status_json(&mut g),
            "ensure" => {
                let config = req.get("config").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let v4 = str_vec(&req, "v4");
                let v6 = str_vec(&req, "v6");
                let config_changed = config != g.desired.config;
                g.desired = Desired { running: true, config, v4, v6 };
                save_state(&g.desired);
                if config_changed {
                    // 配置变了(实际只会是分流口变):杀掉,reconciler 按新配置重拉
                    kill_child(&mut g.child);
                    g.owned.clear();
                    g.shadowed.clear();
                }
                status_json(&mut g)
            }
            "stop" => {
                g.desired.running = false;
                save_state(&g.desired);
                kill_child(&mut g.child);
                g.owned.clear();
                g.shadowed.clear();
                status_json(&mut g)
            }
            _ => serde_json::json!({ "ok": false, "error": format!("未知指令: {cmd}") }),
        }
    };
    let mut w = stream;
    let _ = writeln!(w, "{resp}");
}

fn str_vec(v: &serde_json::Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(|x| x.as_array())
        .map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

fn main() {
    eprintln!("[helper] vpnmgr-helper v{VERSION} 启动");
    let _ = std::fs::create_dir_all(BASE_DIR);
    kill_strays();
    let owner = read_owner_uid();
    eprintln!("[helper] owner uid = {owner:?}");

    let shared: Shared = Arc::new(Mutex::new(Inner {
        desired: load_state(),
        child: None,
        owned: HashSet::new(),
        shadowed: HashSet::new(),
    }));

    // reconciler 线程:重启机后按持久化 desired 自动恢复(mihomo + 路由),app 不在也能用
    {
        let shared = shared.clone();
        std::thread::spawn(move || loop {
            reconcile(&shared);
            std::thread::sleep(Duration::from_secs(2));
        });
    }

    // IPC socket(/var/run 重启清空,启动时重建;0660 root:staff + peer-uid 鉴权)
    let _ = std::fs::remove_file(SOCK_PATH);
    let listener = match UnixListener::bind(SOCK_PATH) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[helper] 绑定 {SOCK_PATH} 失败: {e}");
            std::process::exit(1);
        }
    };
    let _ = std::fs::set_permissions(SOCK_PATH, std::fs::Permissions::from_mode(0o660));
    let _ = std::os::unix::fs::chown(SOCK_PATH, None, Some(STAFF_GID));

    for stream in listener.incoming().flatten() {
        handle_conn(stream, &shared, owner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_args_v4_add() {
        assert_eq!(
            route_args(true, "10.0.0.0/8", false, "utun225"),
            vec!["-n", "add", "-net", "10.0.0.0/8", "-interface", "utun225"]
        );
    }

    #[test]
    fn route_args_v6_add() {
        assert_eq!(
            route_args(true, "fd00::/8", true, "utun225"),
            vec!["-n", "add", "-inet6", "-net", "fd00::/8", "-interface", "utun225"]
        );
    }

    #[test]
    fn route_args_delete_no_interface() {
        // delete 只针对 owned(亲手加的);不带 -interface 仍安全,因为绝不删别人的
        assert_eq!(
            route_args(false, "10.0.0.0/8", false, "utun225"),
            vec!["-n", "delete", "-net", "10.0.0.0/8"]
        );
    }

    #[test]
    fn classify_add_three_states() {
        assert_eq!(classify_add(true, ""), AddResult::Added);
        // macOS 命门:撞车时 exit=0 且 stderr 含 "File exists" → 必须判 Exists(不能被 success 抢先)
        assert_eq!(
            classify_add(true, "route: writing to routing socket: File exists"),
            AddResult::Exists
        );
        assert_eq!(
            classify_add(false, "route: writing to routing socket: File exists"),
            AddResult::Exists
        );
        assert_eq!(classify_add(false, "not in table"), AddResult::Failed);
    }

    #[test]
    fn desired_set_splits_v4_v6() {
        let d = Desired {
            running: true,
            config: String::new(),
            v4: vec!["10.0.0.0/8".into()],
            v6: vec!["fd12::/32".into()],
        };
        let s = desired_set(&d);
        assert!(s.contains(&("10.0.0.0/8".to_string(), false)));
        assert!(s.contains(&("fd12::/32".to_string(), true)));
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn desired_roundtrip_and_defaults() {
        let d: Desired = serde_json::from_str(r#"{"running":true,"config":"x"}"#).unwrap();
        assert!(d.running && d.v4.is_empty() && d.v6.is_empty());
        let s = serde_json::to_string(&Desired {
            running: false,
            config: "c".into(),
            v4: vec!["1.2.3.4/32".into()],
            v6: vec![],
        })
        .unwrap();
        let d2: Desired = serde_json::from_str(&s).unwrap();
        assert_eq!(d2.v4, vec!["1.2.3.4/32"]);
    }

    #[test]
    fn str_vec_extracts() {
        let v: serde_json::Value = serde_json::from_str(r#"{"v4":["a","b"],"bad":123}"#).unwrap();
        assert_eq!(str_vec(&v, "v4"), vec!["a", "b"]);
        assert!(str_vec(&v, "missing").is_empty());
    }
}
