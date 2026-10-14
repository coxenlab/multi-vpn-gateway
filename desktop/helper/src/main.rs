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
//! - 对账走 2s 轮询并与 IPC 修改串行，防止旧代路由跨过 stop/ensure。
//!   新身份要求应用续租；旧身份保留开机恢复。
//! - 安全命门:本二进制与 mihomo 必须装在 root 属主目录(install 脚本负责),
//!   指向用户可写路径 = 本地提权洞。

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
mod lease;
mod routes;
mod resolver;
use lease::{Lease, Mode, Backoff};

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// macOS `staff` 组(所有本地用户都在),socket 0660 root:staff = 远程/其他机制挡在外;
/// 本机用户间的隔离靠 peer-uid 鉴权(见 [`peer_uid`] / [`authorized`])。
const STAFF_GID: u32 = 20;

/// 助手身份:套接字、安装目录、TUN 设备名,取值与 desktop/core `identity.rs` 同名字段一致。
struct Identity {
    sock_path: &'static str,
    base_dir: &'static str,
    /// TUN 设备名与 app 侧约定 pin 死,路由管理零猜测。
    device: &'static str,
}

/// 旧应用(VPN 管理网关)。
const LEGACY: Identity = Identity {
    sock_path: "/var/run/vpnmgr-helper.sock",
    base_dir: "/Library/PrivilegedHelperTools/vpnmgr",
    device: "utun225",
};

/// 通道管家。
const TONGDAO: Identity = Identity {
    sock_path: "/var/run/tongdao-helper.sock",
    base_dir: "/Library/PrivilegedHelperTools/tongdao",
    device: "utun226",
};

/// 按可执行文件所在目录选身份;两个已知目录都不是 → None,调用方直接退出。
/// 不拿旧身份兜底:启动时会先删套接字、pkill 引擎,误用旧身份就会抢走旧助手的套接字、杀掉旧引擎。
fn identity_for_exe(exe: &Path) -> Option<&'static Identity> {
    let dir = exe.parent()?;
    [&LEGACY, &TONGDAO].into_iter().find(|id| dir == Path::new(id.base_dir))
}

impl Identity {
    fn leased(&self) -> bool { self.device == TONGDAO.device }
    fn config_path(&self) -> String { format!("{}/mihomo.yaml", self.base_dir) }
    fn state_path(&self) -> String { format!("{}/state.json", self.base_dir) }
    fn owner_path(&self) -> String { format!("{}/owner.uid", self.base_dir) }
    fn mihomo_bin(&self) -> String { format!("{}/mihomo", self.base_dir) }
    fn mihomo_log(&self) -> String { format!("{}/mihomo.log", self.base_dir) }
}

/// app 期望的运行态(IPC 下发,落盘 state.json,重启机后恢复)。
#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Eq)]
struct Desired {
    #[serde(default)]
    generation: u64,
    running: bool,
    config: String,
    #[serde(default)]
    v4: Vec<String>,
    #[serde(default)]
    v6: Vec<String>,
    #[serde(default)]
    dns_port: u16,
    // Never resurrect DNS redirection from persisted state after a reboot.
    #[serde(default, skip_serializing)]
    domains: Vec<String>,
}

struct Inner {
    id: &'static Identity,
    desired: Desired,
    applied_generation: Option<u64>,
    child: Option<Child>,
    engine_config: String,
    /// 我们亲手 add 成功的 (cidr, is_v6)——删除只针对它,绝不碰别人的路由。
    owned: HashSet<(String, bool)>,
    /// desired 里但 add 时已存在(File exists)的网段:不接管、不删、不反复重试。
    shadowed: HashSet<(String, bool)>,
    lease: Lease,
    route_mode: Mode,
    backoff: Backoff,
    last_readback: Option<Instant>,
    last_shadow_retry: Option<Instant>,
    resolvers: Option<resolver::Resolvers>,
    resolver_status: resolver::Status,
    resolver_error: Option<String>,
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

fn read_owner_uid(id: &Identity) -> Option<u32> {
    std::fs::read_to_string(id.owner_path()).ok().and_then(|s| s.trim().parse().ok())
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

fn route_add(cidr: &str, v6: bool, device: &str) -> AddResult {
    route_add_mode(cidr, v6, device, Mode::Forward)
}

fn route_mode_args(add: bool, cidr: &str, v6: bool, device: &str, mode: Mode) -> Vec<String> {
    let mut args = route_args(add, cidr, v6, device);
    if !add { args.extend(["-interface".into(), device.into()]); }
    if add && mode == Mode::Reject { args.push("-reject".into()); }
    args
}

fn route_add_mode(cidr: &str, v6: bool, device: &str, mode: Mode) -> AddResult {
    match Command::new("/sbin/route").args(route_mode_args(true, cidr, v6, device, mode)).output() {
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
fn route_del(cidr: &str, v6: bool, device: &str) -> bool {
    match Command::new("/sbin/route").args(route_args(false, cidr, v6, device)).output() {
        Ok(o) if o.status.success() || String::from_utf8_lossy(&o.stderr).contains("not in table") => true,
        Ok(o) if !o.status.success() => {
            eprintln!("[helper] route delete {cidr}: {}", String::from_utf8_lossy(&o.stderr).trim());
            false
        }
        Err(e) => { eprintln!("[helper] route delete {cidr} 执行失败: {e}"); false },
        _ => false,
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

fn spawn_mihomo(id: &Identity) -> std::io::Result<Child> {
    let log = if id.leased() { open_engine_log(Path::new(&id.mihomo_log()))? }
        else { std::fs::File::create(id.mihomo_log())? };
    let log2 = log.try_clone()?;
    Command::new(id.mihomo_bin())
        .args(["-f", &id.config_path(), "-d", id.base_dir])
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log2))
        .spawn()
}

fn open_engine_log(path: &Path) -> std::io::Result<std::fs::File> {
    if std::fs::metadata(path).is_ok_and(|m| m.len() > 5 * 1024 * 1024) {
        std::fs::rename(path, path.with_extension("log.1"))?;
    }
    std::fs::OpenOptions::new().create(true).append(true).open(path)
}

fn kill_child(child: &mut Option<Child>) {
    if let Some(c) = child.as_mut() {
        if c.try_wait().ok().flatten().is_none() {
            if let Err(e) = c.kill() {
                eprintln!("[helper] 停止引擎失败: {e}");
                return;
            }
        }
        if let Err(e) = c.wait() {
            eprintln!("[helper] 等待引擎退出失败: {e}");
            return;
        }
        *child = None;
    }
}

/// helper 启动时清掉可能的孤儿 mihomo(上代 helper 被 SIGKILL 等极端情况)。
fn kill_strays(id: &Identity) {
    let _ = Command::new("/usr/bin/pkill").args(["-f", &id.mihomo_bin()]).output();
}

// ── 状态持久化 ───────────────────────────────────────────────────────────────

fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let parent = path.parent().ok_or_else(|| std::io::Error::other("missing parent"))?;
    let temp = parent.join(format!(".helper-{}-{}.tmp", std::process::id(), SEQUENCE.fetch_add(1, Ordering::Relaxed)));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)?;
        std::fs::File::open(parent)?.sync_all()
    })();
    if result.is_err() { let _ = std::fs::remove_file(temp); }
    result
}

fn accept_desired(g: &mut Inner, desired: Desired, path: &std::path::Path) -> Result<bool, String> {
    validate_dns(g.id, &desired)?;
    if desired.generation < g.desired.generation || desired.generation == 0 {
        return Err("stale generation".into());
    }
    if desired.generation == g.desired.generation {
        return if desired == g.desired { Ok(false) } else { Err("generation conflict".into()) };
    }
    let bytes = serde_json::to_vec(&desired).map_err(|e| e.to_string())?;
    atomic_write(path, &bytes).map_err(|e| format!("state persistence failed: {e}"))?;
    g.desired = desired;
    Ok(true)
}

fn validate_dns(id: &Identity, desired: &Desired) -> Result<(), String> {
    if desired.domains.is_empty() && desired.dns_port == 0 { return Ok(()); }
    if !id.leased() { return Err("legacy helper does not support DNS ownership".into()); }
    if desired.dns_port < 1024 || desired.domains.iter().any(|d| !resolver::valid_domain(d))
        || !desired.config.contains(&format!("127.0.0.1:{}", desired.dns_port)) {
        return Err("invalid DNS configuration".into());
    }
    if !desired.domains.is_empty() && !desired.v4.iter().any(|s| s == "198.19.128.0/17") {
        return Err("DNS domains require the fake-IP route".into());
    }
    Ok(())
}

fn dns_request(req: &serde_json::Value) -> Result<(u16, Vec<String>), String> {
    let port = match req.get("dns_port") {
        None => 0,
        Some(v) => v.as_u64().and_then(|n| u16::try_from(n).ok())
            .ok_or("invalid DNS port")?,
    };
    let domains = match req.get("domains") {
        None => vec![],
        Some(v) => v.as_array().ok_or("invalid domains")?.iter()
            .map(|v| v.as_str().map(String::from).ok_or_else(|| "invalid domain".to_string()))
            .collect::<Result<Vec<_>, _>>()?,
    };
    Ok((port, domains))
}

// Files are published only after engine/routes are ready. Cleanup also runs on
// lease expiry and engine loss, and failures must prevent an applied response.
fn reconcile_resolvers(g: &mut Inner, ready: bool) -> bool {
    let domains = if ready { g.desired.domains.clone() } else { vec![] };
    let result = match g.resolvers.as_mut() {
        Some(files) => files.reconcile(&domains, g.desired.dns_port),
        None if domains.is_empty() => Ok(resolver::Status::default()),
        None => Err(std::io::Error::other("resolver ownership unavailable")),
    };
    match result {
        Ok(status) => {
            let complete = status.taken.is_empty();
            g.resolver_status = status; g.resolver_error = None;
            if !complete { g.applied_generation = None; }
            complete
        }
        Err(error) => {
            let message = error.to_string();
            if g.resolver_error.as_deref() != Some(&message) { eprintln!("[helper] DNS 对账失败: {message}"); }
            g.resolver_error = Some(message); g.applied_generation = None;
            false
        }
    }
}

fn load_state(id: &Identity) -> Desired {
    std::fs::read_to_string(id.state_path())
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
    // 对账与 IPC 修改共用一把锁;旧路由批次不能跨过 stop/ensure 的代次继续落地。
    let mut g = shared.lock().unwrap();
    if g.id.leased() { reconcile_leased(&mut g, Instant::now()); return; }
    let id = g.id;
    let alive = child_alive(&mut g.child);
    if alive && g.desired.running && g.engine_config != g.desired.config {
        kill_child(&mut g.child);
        return;
    }
    if !g.desired.running {
        kill_child(&mut g.child);
        if !child_alive(&mut g.child) {
            g.owned.clear(); g.shadowed.clear();
            g.applied_generation = Some(g.desired.generation);
        }
        return;
    }
    if !alive {
        g.applied_generation = None;
        if let Err(e) = atomic_write(std::path::Path::new(&id.config_path()), g.desired.config.as_bytes()) {
            eprintln!("[helper] 配置写入失败: {e}");
            return;
        }
        match spawn_mihomo(id) {
            Ok(c) => {
                eprintln!("[helper] mihomo 已拉起 pid={}", c.id());
                g.child = Some(c); g.engine_config = g.desired.config.clone();
            }
            Err(e) => eprintln!("[helper] mihomo 拉起失败: {e}"),
        }
        g.owned.clear(); g.shadowed.clear();
        return; // 等下一拍确认引擎与 utun 已就绪。
    }
    let desired = desired_set(&g.desired);
    let to_add: Vec<_> = desired.iter().filter(|k| !g.owned.contains(k) && !g.shadowed.contains(k)).cloned().collect();
    let to_del: Vec<_> = g.owned.difference(&desired).cloned().collect();
    for (cidr, v6) in to_add {
        match route_add(&cidr, v6, id.device) {
            AddResult::Added => { g.owned.insert((cidr, v6)); }
            AddResult::Exists => { g.shadowed.insert((cidr, v6)); }
            AddResult::Failed => {}
        }
    }
    for (cidr, v6) in to_del {
        if route_del(&cidr, v6, id.device) { g.owned.remove(&(cidr, v6)); }
    }
    g.shadowed.retain(|k| desired.contains(k));
    if g.owned == desired && g.shadowed.is_empty() {
        g.applied_generation = Some(g.desired.generation);
    }
}

fn kernel_routes(device: &str) -> std::io::Result<routes::Snapshot> {
    let mut all = routes::Snapshot::new();
    for (family, v6) in [("inet", false), ("inet6", true)] {
        let out = Command::new("/usr/sbin/netstat").args(["-rn", "-f", family]).output()?;
        if !out.status.success() { return Err(std::io::Error::other("route readback failed")); }
        all.extend(routes::parse(&String::from_utf8_lossy(&out.stdout), v6, device));
    }
    Ok(all)
}

fn owned_in_snapshot(key: &(String, bool), mode: Mode, snapshot: &routes::Snapshot) -> bool {
    routes::canonical(&key.0, key.1, false)
        .is_some_and(|cidr| snapshot.contains(&(cidr, key.1, mode == Mode::Reject)))
}

trait RouteOps {
    fn snapshot(&mut self, device: &str) -> Option<routes::Snapshot>;
    fn add(&mut self, cidr: &str, v6: bool, device: &str, mode: Mode) -> AddResult;
    fn delete(&mut self, cidr: &str, v6: bool, device: &str, mode: Mode) -> bool;
}
struct SystemRoutes;
impl RouteOps for SystemRoutes {
    fn snapshot(&mut self, device: &str) -> Option<routes::Snapshot> { kernel_routes(device).ok() }
    fn add(&mut self, cidr: &str, v6: bool, device: &str, mode: Mode) -> AddResult { route_add_mode(cidr,v6,device,mode) }
    fn delete(&mut self, cidr: &str, v6: bool, device: &str, mode: Mode) -> bool {
        Command::new("/sbin/route").args(route_mode_args(false,cidr,v6,device,mode)).output()
            .is_ok_and(|o| o.status.success() || String::from_utf8_lossy(&o.stderr).contains("not in table"))
    }
}

fn withdraw_routes(g: &mut Inner) -> bool {
    let selected = g.owned.clone();
    withdraw_selected(g, selected, &mut SystemRoutes)
}

/// 模式切换/停止前重新核对所有权；不删除已被别人替换的路由。
fn withdraw_selected(g: &mut Inner, selected: HashSet<(String,bool)>, ops: &mut impl RouteOps) -> bool {
    if g.owned.is_empty() || selected.is_empty() { return true; }
    let Some(snapshot)=ops.snapshot(g.id.device) else { return false };
    let mode=g.route_mode;
    g.owned.retain(|key| owned_in_snapshot(key,mode,&snapshot));
    let owned:Vec<_>=g.owned.intersection(&selected).cloned().collect();
    for (cidr,v6) in owned {
        if ops.delete(&cidr,v6,g.id.device,mode) {g.owned.remove(&(cidr,v6));}
    }
    g.owned.is_disjoint(&selected)
}

fn reconcile_leased(g: &mut Inner, now: Instant) {
    let mode = g.lease.mode(g.desired.running, now);
    if mode == Mode::Off {
        let dns_clean = reconcile_resolvers(g, false);
        let clean = withdraw_routes(g);
        kill_child(&mut g.child);
        if dns_clean && clean && !child_alive(&mut g.child) {
            g.shadowed.clear(); g.route_mode = Mode::Off; g.backoff.reset();
            g.engine_config.clear(); g.applied_generation = Some(g.desired.generation);
        }
        return;
    }
    let alive = child_alive(&mut g.child);
    if alive && g.engine_config != g.desired.config {
        reconcile_resolvers(g, false);
        if !withdraw_routes(g) { return; }
        kill_child(&mut g.child); g.engine_config.clear(); g.backoff.reset();
        return;
    }
    if !alive {
        reconcile_resolvers(g, false);
        if !g.engine_config.is_empty() {
            g.backoff.failed(now); g.engine_config.clear();
        }
        g.applied_generation = None;
        if !g.backoff.ready(now) { return; }
        if let Err(e) = atomic_write(Path::new(&g.id.config_path()), g.desired.config.as_bytes()) {
            eprintln!("[helper] 配置写入失败: {e}"); g.backoff.failed(now); return;
        }
        match spawn_mihomo(g.id) {
            Ok(child) => { g.child=Some(child); g.engine_config=g.desired.config.clone(); g.backoff.spawned(now); }
            Err(e) => { eprintln!("[helper] 引擎重启失败: {e}"); g.backoff.failed(now); }
        }
        return;
    }
    g.backoff.healthy(now);
    reconcile_routes(g, mode, now, &mut SystemRoutes);
    let routes_ready = g.applied_generation == Some(g.desired.generation);
    reconcile_resolvers(g, routes_ready);
}

fn reconcile_routes(g: &mut Inner, mode: Mode, now: Instant, ops: &mut impl RouteOps) {
    g.applied_generation = None;
    if g.route_mode != mode {
        let selected = g.owned.clone();
        if !withdraw_selected(g, selected, ops) { return; }
        g.shadowed.clear(); g.route_mode = mode; g.applied_generation = None;
    }
    if g.last_readback.is_none_or(|last| now.duration_since(last) >= Duration::from_secs(10)) {
        let Some(snapshot) = ops.snapshot(g.id.device) else { return; };
        g.owned.retain(|key| owned_in_snapshot(key, mode, &snapshot));
        g.last_readback = Some(now);
    }
    if g.last_shadow_retry.is_none_or(|last| now.duration_since(last) >= Duration::from_secs(30)) {
        g.shadowed.clear(); g.last_shadow_retry = Some(now);
    }
    let desired = desired_set(&g.desired);
    let removed = g.owned.difference(&desired).cloned().collect();
    if !withdraw_selected(g, removed, ops) { return; }
    let missing: Vec<_> = desired.iter().filter(|k| !g.owned.contains(k) && !g.shadowed.contains(k)).cloned().collect();
    for (cidr, v6) in missing {
        match ops.add(&cidr, v6, g.id.device, mode) {
            AddResult::Added => {g.owned.insert((cidr,v6));}
            AddResult::Exists => {g.shadowed.insert((cidr,v6));}
            AddResult::Failed => {}
        }
    }
    g.shadowed.retain(|k| desired.contains(k));
    g.applied_generation = (g.owned == desired && g.shadowed.is_empty()).then_some(g.desired.generation);
}

// ── IPC ──────────────────────────────────────────────────────────────────────

fn status_json(g: &mut Inner) -> serde_json::Value {
    let alive = child_alive(&mut g.child);
    let mode = if g.id.leased() { g.lease.mode(g.desired.running, Instant::now()) }
        else if g.desired.running { Mode::Forward } else { Mode::Off };
    let running = mode != Mode::Off;
    let applied = g.applied_generation == Some(g.desired.generation) && alive == running
        && (!g.id.leased() || g.route_mode == mode);
    let mut status = serde_json::json!({
        "ok": true,
        "version": VERSION,
        "running": running,
        "alive": alive,
        "device": g.id.device,
        "config": g.desired.config,
        "v4": g.desired.v4,
        "v6": g.desired.v6,
        "applied": g.owned.len(),
        "shadowed": g.shadowed.len(),
        "generation": g.desired.generation,
        "applied_generation": if applied { g.applied_generation } else { None },
        "pending": !applied,
    });
    if g.id.leased() {
        status["lease_remaining"] = g.lease.remaining(Instant::now()).into();
        status["upstream_ok"] = g.lease.upstream_ok.into();
        status["mode"] = g.route_mode.name().into();
        status["dns_capable"] = g.resolvers.is_some().into();
        status["dns_port"] = g.desired.dns_port.into();
        status["domains"] = serde_json::json!(g.desired.domains);
        status["resolver_files"] = g.resolver_status.owned.len().into();
        status["resolver_taken"] = g.resolver_status.taken.len().into();
        status["resolver_error"] = serde_json::json!(g.resolver_error);
        let desired_domains: HashSet<_> = g.desired.domains.iter().collect();
        let actual_domains: HashSet<_> = g.resolver_status.owned.iter().collect();
        status["domains_ready"] = (applied && alive && running && !desired_domains.is_empty()
            && desired_domains == actual_domains && g.resolver_error.is_none()
            && g.resolver_status.taken.is_empty()).into();
    }
    status
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
        let id = g.id;
        match cmd {
            "ping" => serde_json::json!({ "ok": true, "version": VERSION }),
            "status" => status_json(&mut g),
            "ensure" => {
                let config = req.get("config").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let v4 = str_vec(&req, "v4");
                let v6 = str_vec(&req, "v6");
                let config_changed = config != g.desired.config;
                let generation = req.get("generation").and_then(|v| v.as_u64()).unwrap_or(0);
                let accepted = dns_request(&req).and_then(|(dns_port, domains)| {
                    let desired = Desired { generation, running: true, config, v4, v6, dns_port, domains };
                    accept_desired(&mut g, desired, std::path::Path::new(&id.state_path()))
                });
                match accepted {
                Err(error) => serde_json::json!({"ok":false,"error":error,"generation":g.desired.generation}),
                Ok(changed) => {
                if id.leased() {
                    g.lease.ensure(Instant::now(), req["lease_secs"].as_u64().unwrap_or(90), req["upstream_ok"].as_bool().unwrap_or(true));
                }
                if changed && config_changed && !id.leased() {
                    // 配置变了(实际只会是分流口变):杀掉,reconciler 按新配置重拉
                    kill_child(&mut g.child);
                    g.owned.clear();
                    g.shadowed.clear();
                }
                status_json(&mut g)
                }
                }
            }
            "renew" if id.leased() => {
                let generation = g.desired.generation;
                let running = g.desired.running;
                match g.lease.renew(Instant::now(), generation, req["generation"].as_u64().unwrap_or(0), running,
                    req["lease_secs"].as_u64().unwrap_or(90), req["upstream_ok"].as_bool().unwrap_or(true)) {
                    Ok(()) => status_json(&mut g),
                    Err(error) => serde_json::json!({"ok":false,"error":error,"generation":generation}),
                }
            }
            "stop" => {
                let mut desired = g.desired.clone();
                desired.running = false;
                desired.generation = req.get("generation").and_then(|v| v.as_u64()).unwrap_or(0);
                match accept_desired(&mut g, desired, std::path::Path::new(&id.state_path())) {
                Err(error) => serde_json::json!({"ok":false,"error":error,"generation":g.desired.generation}),
                Ok(_) => {
                if id.leased() {
                    g.lease.clear();
                    reconcile_leased(&mut g, Instant::now());
                } else {
                kill_child(&mut g.child);
                if !child_alive(&mut g.child) {
                    g.owned.clear(); g.shadowed.clear(); g.applied_generation = Some(g.desired.generation);
                }
                }
                status_json(&mut g)
                }
                }
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
    // 身份按安装目录选,未知目录直接退出:下面的删套接字、pkill 都按身份执行。
    let exe = std::env::current_exe();
    let Some(id) = exe.as_deref().ok().and_then(identity_for_exe) else {
        eprintln!("[helper] 不在已知安装目录({exe:?}),退出");
        std::process::exit(1);
    };
    eprintln!("[helper] 安装目录 {}", id.base_dir);
    let _ = std::fs::create_dir_all(id.base_dir);
    let cleanup_only = std::env::args().skip(1).any(|arg| arg == "--cleanup-resolver");
    if cleanup_only && (!id.leased() || unsafe { libc::geteuid() } != 0) {
        eprintln!("[helper] DNS 清理仅供通道管家 root 助手使用");
        std::process::exit(1);
    }
    let resolvers = if id.leased() {
        let result = (|| -> std::io::Result<resolver::Resolvers> {
            std::fs::create_dir_all("/etc/resolver")?;
            let mut files = resolver::Resolvers::open(Path::new("/etc/resolver"),
                &Path::new(id.base_dir).join("resolver-owned.json"))?;
            // Before any engine resurrection, withdraw last boot's DNS files.
            files.reconcile(&[], 0)?;
            Ok(files)
        })();
        match result {
            Ok(files) => Some(files),
            Err(error) => {
                eprintln!("[helper] 启动时 DNS 清理失败: {error}");
                std::process::exit(1);
            }
        }
    } else { None };
    if cleanup_only { return; }
    kill_strays(id);
    let owner = read_owner_uid(id);
    eprintln!("[helper] owner uid = {owner:?}");

    let shared: Shared = Arc::new(Mutex::new(Inner {
        id,
        desired: load_state(id),
        applied_generation: None,
        child: None,
        engine_config: String::new(),
        owned: HashSet::new(),
        shadowed: HashSet::new(),
        lease: Lease::default(), route_mode: Mode::Off, backoff: Backoff::default(),
        last_readback: None, last_shadow_retry: None,
        resolvers, resolver_status: resolver::Status::default(), resolver_error: None,
    }));

    // 新身份的租约不落盘，开机不恢复入口；旧身份仍按持久化 desired 恢复。
    {
        let shared = shared.clone();
        std::thread::spawn(move || loop {
            reconcile(&shared);
            std::thread::sleep(Duration::from_secs(2));
        });
    }

    // IPC socket(/var/run 重启清空,启动时重建;0660 root:staff + peer-uid 鉴权)
    let _ = std::fs::remove_file(id.sock_path);
    let listener = match UnixListener::bind(id.sock_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[helper] 绑定 {} 失败: {e}", id.sock_path);
            std::process::exit(1);
        }
    };
    let _ = std::fs::set_permissions(id.sock_path, std::fs::Permissions::from_mode(0o660));
    let _ = std::os::unix::fs::chown(id.sock_path, None, Some(STAFF_GID));

    for stream in listener.incoming().flatten() {
        handle_conn(stream, &shared, owner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_inner() -> Inner {
        Inner { id: &LEGACY, desired: Desired::default(), applied_generation: None, child: None,
            engine_config: String::new(), owned: HashSet::new(), shadowed: HashSet::new(),
            lease: Lease::default(), route_mode: Mode::Off, backoff: Backoff::default(),
            last_readback: None, last_shadow_retry: None,
            resolvers: None, resolver_status: resolver::Status::default(), resolver_error: None }
    }

    fn dns_desired() -> Desired {
        Desired { generation: 1, running: true, config: "listen: 127.0.0.1:12000".into(),
            v4: vec!["198.19.128.0/17".into()], dns_port: 12000,
            domains: vec!["corp.test".into()], ..Default::default() }
    }

    #[test]
    fn dns_request_validation_is_atomic_and_domains_are_not_persisted() {
        let dir = TestDir::new(); let path = dir.0.join("state.json");
        let mut g = empty_inner();
        assert!(accept_desired(&mut g, dns_desired(), &path).is_err());
        g.id = &TONGDAO;
        for mutation in 0..4 {
            let mut d = dns_desired();
            match mutation {
                0 => d.domains.push("../escape".into()),
                1 => d.dns_port = 53,
                2 => d.config.clear(),
                _ => d.v4.clear(),
            }
            assert!(accept_desired(&mut g, d, &path).is_err());
            assert_eq!(g.desired.generation, 0); assert!(!path.exists());
        }
        for request in [serde_json::json!({"domains":["corp.test",1]}),
            serde_json::json!({"domains":"corp.test"}), serde_json::json!({"dns_port":65536}),
            serde_json::json!({"dns_port":-1}), serde_json::json!({"dns_port":"12000"})] {
            assert!(dns_request(&request).is_err());
        }
        assert!(accept_desired(&mut g, dns_desired(), &path).unwrap());
        let disk = std::fs::read_to_string(path).unwrap();
        assert!(!disk.contains("corp.test"));
        let restored: Desired = serde_json::from_str(&disk).unwrap();
        assert!(restored.domains.is_empty());
        assert_eq!(restored.dns_port, 12000);
    }

    #[test]
    fn dns_readiness_requires_confirmed_files_and_lease_expiry_cleans_them() {
        let dir = TestDir::new(); let mut g = empty_inner(); g.id = &TONGDAO;
        g.resolvers = Some(resolver::Resolvers::open(&dir.0, &dir.0.join("owned.json")).unwrap());
        g.desired = dns_desired();
        assert!(reconcile_resolvers(&mut g, false));
        assert!(!dir.0.join("tongdao.corp.test").exists());
        g.child = Some(Command::new("/bin/sleep").arg("60").spawn().unwrap());
        let now = Instant::now(); g.lease.ensure(now, 90, true); g.route_mode = Mode::Forward;
        g.applied_generation = Some(1);
        assert!(reconcile_resolvers(&mut g, true));
        assert_eq!(status_json(&mut g)["domains_ready"], true);
        // Neither test owns kernel routes; Off cleanup touches only our temp files/child.
        reconcile_leased(&mut g, now + Duration::from_secs(91));
        assert!(!dir.0.join("tongdao.corp.test").exists());
        assert!(g.child.is_none());
        assert_eq!(status_json(&mut g)["domains_ready"], false);
    }

    #[test]
    fn dns_conflicts_and_cleanup_failures_cannot_report_applied() {
        let dir = TestDir::new(); let mut g = empty_inner(); g.id = &TONGDAO;
        g.resolvers = Some(resolver::Resolvers::open(&dir.0, &dir.0.join("owned.json")).unwrap());
        g.desired = dns_desired(); g.applied_generation = Some(1);
        let path = dir.0.join("tongdao.corp.test");
        std::fs::write(&path, "foreign").unwrap();
        assert!(!reconcile_resolvers(&mut g, true));
        assert_eq!(g.applied_generation, None);
        assert_eq!(g.resolver_status.taken, vec!["corp.test"]);
        std::fs::remove_file(&path).unwrap();
        assert!(reconcile_resolvers(&mut g, true));
        std::fs::remove_file(&path).unwrap(); std::fs::create_dir(&path).unwrap();
        g.applied_generation = Some(1);
        assert!(!reconcile_resolvers(&mut g, false));
        assert!(g.resolver_error.is_some()); assert_eq!(g.applied_generation, None);
        std::fs::remove_dir(&path).unwrap();
        assert!(reconcile_resolvers(&mut g, false));
        assert!(g.resolver_error.is_none());
    }

    #[test]
    fn leased_boot_is_off_and_renew_ipc_cannot_change_generation_or_desired() {
        use std::io::Read;
        let mut g = empty_inner(); g.id = &TONGDAO;
        g.desired = Desired {generation:7,running:true,config:"fixture".into(),v4:vec!["192.0.2.0/24".into()],v6:vec![],..Default::default()};
        assert_eq!(status_json(&mut g)["running"],false);
        assert_eq!(status_json(&mut g)["mode"],"off");
        g.lease.ensure(Instant::now(),90,true);
        let desired=g.desired.clone();
        let shared=Arc::new(Mutex::new(g));
        for (generation, expected) in [(6,false),(7,true)] {
            let (mut client,server)=UnixStream::pair().unwrap();
            writeln!(client,"{}",serde_json::json!({"cmd":"renew","generation":generation,"lease_secs":90,"upstream_ok":false})).unwrap();
            client.shutdown(std::net::Shutdown::Write).unwrap();
            handle_conn(server,&shared,Some(unsafe {libc::geteuid()}));
            let mut text=String::new();client.read_to_string(&mut text).unwrap();
            let response:serde_json::Value=serde_json::from_str(&text).unwrap();
            assert_eq!(response["ok"],expected);
            assert!(shared.lock().unwrap().desired==desired);
        }
        let g=shared.lock().unwrap();
        assert_eq!(g.lease.mode(true,Instant::now()),Mode::Reject);
    }

    #[test]
    fn reject_route_keeps_device_ownership_and_foreign_replacement_is_not_owned() {
        let args=route_mode_args(true,"192.0.2.0/24",false,"utun226",Mode::Reject);
        assert_eq!(args, ["-n","add","-net","192.0.2.0/24","-interface","utun226","-reject"]);
        let key=("192.0.2.0/24".into(),false);
        let owned=routes::parse("Destination Gateway Flags Netif\n192.0.2/24 link#8 URS utun226\n",false,"utun226");
        assert!(owned_in_snapshot(&key,Mode::Reject,&owned));
        assert!(!owned_in_snapshot(&key,Mode::Forward,&owned));
        let foreign=routes::parse("Destination Gateway Flags Netif\n192.0.2/24 link#8 URS utun225\n",false,"utun226");
        assert!(!owned_in_snapshot(&key,Mode::Reject,&foreign));
        let mut legacy=empty_inner();legacy.desired.running=true;
        let status=status_json(&mut legacy);
        assert_eq!(status["running"],true);
        assert!(status.get("lease_remaining").is_none());
    }

    #[test]
    fn engine_log_appends_and_rotates_only_after_limit() {
        let dir=TestDir::new();let path=dir.0.join("mihomo.log");
        open_engine_log(&path).unwrap().write_all(b"first").unwrap();
        open_engine_log(&path).unwrap().write_all(b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(),b"firstsecond");
        std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(5*1024*1024+1).unwrap();
        open_engine_log(&path).unwrap().write_all(b"new").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(),b"new");
        assert_eq!(std::fs::metadata(path.with_extension("log.1")).unwrap().len(),5*1024*1024+1);
    }

    #[test]
    fn expired_lease_stops_owned_test_child_without_restoring_persisted_intent() {
        let mut g=empty_inner();g.id=&TONGDAO;
        g.desired=Desired {generation:9,running:true,config:"fixture".into(),..Default::default()};
        let now=Instant::now();g.lease.ensure(now,1,true);
        g.child=Some(Command::new("/bin/sleep").arg("60").spawn().unwrap());
        g.engine_config="fixture".into();
        reconcile_leased(&mut g,now+Duration::from_secs(2));
        assert!(g.child.is_none());assert!(g.owned.is_empty());
        assert_eq!(g.route_mode,Mode::Off);
        assert!(g.desired.running,"持久意图保留，但不带租约不会自动重启");
        assert_eq!(g.applied_generation,Some(9));
    }

    #[test]
    fn route_reconciliation_switches_modes_repairs_loss_and_retries_shadowed() {
        #[derive(Default)]
        struct Fake { live: routes::Snapshot, collision: bool, calls: Vec<String> }
        impl RouteOps for Fake {
            fn snapshot(&mut self, _: &str) -> Option<routes::Snapshot> { Some(self.live.clone()) }
            fn add(&mut self, cidr:&str,v6:bool,_:&str,mode:Mode)->AddResult {
                self.calls.push(format!("add {} {cidr}",mode.name()));
                if self.collision {return AddResult::Exists;}
                self.live.insert((cidr.into(),v6,mode==Mode::Reject));AddResult::Added
            }
            fn delete(&mut self,cidr:&str,v6:bool,_:&str,mode:Mode)->bool {
                self.calls.push(format!("delete {} {cidr}",mode.name()));
                self.live.remove(&(cidr.into(),v6,mode==Mode::Reject));true
            }
        }
        let mut g=empty_inner();g.id=&TONGDAO;
        g.desired=Desired{generation:4,running:true,v4:vec!["192.0.2.0/24".into()],..Default::default()};
        let now=Instant::now();let mut fake=Fake::default();
        reconcile_routes(&mut g,Mode::Forward,now,&mut fake);
        assert_eq!(g.owned.len(),1);assert_eq!(g.applied_generation,Some(4));
        reconcile_routes(&mut g,Mode::Reject,now+Duration::from_secs(2),&mut fake);
        assert_eq!(fake.calls,["add forward 192.0.2.0/24","delete forward 192.0.2.0/24","add reject 192.0.2.0/24"]);
        fake.live.clear(); // 换网导致本设备路由消失。
        reconcile_routes(&mut g,Mode::Reject,now+Duration::from_secs(10),&mut fake);
        assert_eq!(fake.calls.len(),4);assert_eq!(g.owned.len(),1);
        let selected=g.owned.clone();
        assert!(withdraw_selected(&mut g,selected,&mut fake));
        assert!(g.owned.is_empty() && fake.live.is_empty());
        fake.collision=true;
        reconcile_routes(&mut g,Mode::Forward,now+Duration::from_secs(12),&mut fake);
        assert_eq!(g.shadowed.len(),1);let count=fake.calls.len();
        fake.collision=false;
        reconcile_routes(&mut g,Mode::Forward,now+Duration::from_secs(28),&mut fake);
        assert_eq!(fake.calls.len(),count);
        reconcile_routes(&mut g,Mode::Forward,now+Duration::from_secs(32),&mut fake);
        assert_eq!(g.owned.len(),1);assert!(g.shadowed.is_empty());
        fake.live.clear(); // 同网段已被其他设备替换，不再有本设备的记录。
        let count=fake.calls.len();
        let selected=g.owned.clone();
        assert!(withdraw_selected(&mut g,selected,&mut fake));
        assert_eq!(fake.calls.len(),count,"绝不删除已不属于本设备的路由");
    }

    struct TestDir(std::path::PathBuf);
    impl TestDir {
        fn new() -> Self {
            static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("vpnmgr-helper-test-{}-{unique}-{sequence}", std::process::id()));
            std::fs::create_dir(&path).unwrap(); Self(path)
        }
    }
    impl Drop for TestDir { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }

    #[test]
    fn generation_is_persisted_and_stale_or_conflicting_requests_are_rejected() {
        let dir = TestDir::new(); let path = dir.0.join("state.json"); let mut g = empty_inner();
        let desired = Desired { generation: 2, running: true, config: "fixture".into(), ..Default::default() };
        assert!(accept_desired(&mut g, desired.clone(), &path).unwrap());
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let bytes = std::fs::read(&path).unwrap();
        let stored: Desired = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(stored.generation, 2);
        assert!(!accept_desired(&mut g, desired.clone(), &path).unwrap());
        let mut stale = desired.clone(); stale.generation = 1;
        assert!(accept_desired(&mut g, stale, &path).is_err());
        let mut conflict = desired; conflict.config = "different".into();
        assert!(accept_desired(&mut g, conflict, &path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(status_json(&mut g)["pending"].as_bool().unwrap());
    }

    #[test]
    fn persistence_failure_keeps_previous_desired_state() {
        let dir = TestDir::new(); let mut g = empty_inner();
        let desired = Desired { generation: 1, running: true, ..Default::default() };
        assert!(accept_desired(&mut g, desired, &dir.0.join("missing/state.json")).is_err());
        assert_eq!(g.desired.generation, 0);
        assert!(!g.desired.running);
    }

    #[test]
    fn stop_generation_cannot_be_overwritten_by_older_ensure() {
        let dir = TestDir::new(); let path = dir.0.join("state.json"); let mut g = empty_inner();
        let ensure = Desired { generation: 1, running: true, ..Default::default() };
        accept_desired(&mut g, ensure.clone(), &path).unwrap();
        let stop = Desired { generation: 2, running: false, ..Default::default() };
        accept_desired(&mut g, stop, &path).unwrap();
        assert!(accept_desired(&mut g, ensure, &path).is_err());
        let shared = Arc::new(Mutex::new(g));
        reconcile(&shared); // 没有子进程/路由,只确认停止代次;不调用宿主网络工具。
        let status = status_json(&mut shared.lock().unwrap());
        assert_eq!(status["applied_generation"], 2);
        assert_eq!(status["pending"], false);
    }


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
            generation: 1,
            running: true,
            config: String::new(),
            v4: vec!["10.0.0.0/8".into()],
            v6: vec!["fd12::/32".into()],
            ..Default::default()
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
            generation: 2,
            running: false,
            config: "c".into(),
            v4: vec!["1.2.3.4/32".into()],
            v6: vec![],
            ..Default::default()
        })
        .unwrap();
        let d2: Desired = serde_json::from_str(&s).unwrap();
        assert_eq!(d2.v4, vec!["1.2.3.4/32"]);
    }

    #[test]
    fn legacy_dir_selects_unchanged_identity() {
        let id = identity_for_exe(Path::new("/Library/PrivilegedHelperTools/vpnmgr/vpnmgr-helper")).unwrap();
        assert_eq!(id.sock_path, "/var/run/vpnmgr-helper.sock");
        assert_eq!(id.base_dir, "/Library/PrivilegedHelperTools/vpnmgr");
        assert_eq!(id.device, "utun225");
        assert_eq!(id.state_path(), "/Library/PrivilegedHelperTools/vpnmgr/state.json");
        assert_eq!(id.mihomo_bin(), "/Library/PrivilegedHelperTools/vpnmgr/mihomo");
    }

    #[test]
    fn tongdao_dir_selects_tongdao_identity() {
        let id = identity_for_exe(Path::new("/Library/PrivilegedHelperTools/tongdao/vpnmgr-helper")).unwrap();
        assert_eq!(id.sock_path, "/var/run/tongdao-helper.sock");
        assert_eq!(id.base_dir, "/Library/PrivilegedHelperTools/tongdao");
        assert_eq!(id.device, "utun226");
        for v in [id.sock_path, id.base_dir, id.device] {
            assert!(!v.contains("vpnmgr") && !v.contains("utun225"), "{v}");
        }
        // pkill -f 的匹配串两边互不包含,不会误杀对方引擎。
        assert!(!LEGACY.mihomo_bin().contains(&TONGDAO.mihomo_bin()));
        assert!(!TONGDAO.mihomo_bin().contains(&LEGACY.mihomo_bin()));
        let mut g = empty_inner();
        g.id = id;
        assert_eq!(status_json(&mut g)["device"], "utun226");
    }

    #[test]
    fn unknown_dirs_select_no_identity() {
        for exe in [
            "/Library/PrivilegedHelperTools/vpnmgr2/vpnmgr-helper",
            "/Library/PrivilegedHelperTools/tongdao-dev/vpnmgr-helper",
            "/Library/PrivilegedHelperTools/vpnmgr/sub/vpnmgr-helper",
            "/Library/PrivilegedHelperTools/vpnmgr-helper",
            "/tmp/vpnmgr-helper",
            "/tmp/Library/PrivilegedHelperTools/vpnmgr/vpnmgr-helper",
            "vpnmgr-helper",
            "/",
        ] {
            assert!(identity_for_exe(Path::new(exe)).is_none(), "{exe}");
        }
    }

    #[test]
    fn str_vec_extracts() {
        let v: serde_json::Value = serde_json::from_str(r#"{"v4":["a","b"],"bad":123}"#).unwrap();
        assert_eq!(str_vec(&v, "v4"), vec!["a", "b"]);
        assert!(str_vec(&v, "missing").is_empty());
    }
}
