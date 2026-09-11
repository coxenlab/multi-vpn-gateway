//! 分流口健康看门狗。命门 #1 不破:登录判据仍是 SOCKS5 探活(`manager::probe`,经 docker.sock
//! 走 VM 内网),本模块只额外看**宿主→mihomo 分流口**的可达性——那是用户 Clash 真正拨的口,
//! 也是 SOCKS5 探活摸不到的盲区。
//!
//! 背景病:宿主分流口曾由 lima 端口转发伺服,而 lima 的转发会 **静默丢失**(睡醒/网络抖动后
//! 不重建),更狠的是 2026-08-04 实测的 gRPC 僵死——listener 还在、TCP 照常 accept、数据
//! 永不转发,且**永不自我恢复**(`grpc: the client connection is closing`)。故分流口/控制口
//! 已整体改由 app 自持的 SSH 转发伺服([`crate::tunnel`]),lima 不再参与主链路。
//!
//! 判据必须端到端([`proxy_serves`] 真握手)。旧的「TCP 能连上即健康」在僵死转发下恒真,
//! 那次事故里连续数小时误报 healthy:看门狗一次没触发、用户点手动修复还回「已修复」。
//!
//! 自愈是两级梯子:
//! 1. 重建 app 自持的 SSH 转发 —— 秒级,不动任何容器,**通道不需要重新登录**。睡醒断链
//!    (Mac 合盖冻结 VM)是最常见诱因,靠墙钟跳变识别、跳过防抖立即重建。
//! 2. 重建反复无效 → `docker restart mihomo`(问题多半在容器自身),再按新容器 IP 重挂转发。
//!
//! 两级都失败才放弃,横幅引导用户重开 app。带防抖 + 冷却,避免连击。状态吐给 /api/system。
//!
//! 盲区 #3(实测 2026-07-02,auto-memory `watchdog-transport-dead-misdiagnosed-vmdown`):
//! docker.sock 走 colima 的 SSH 主连接(mux),mux 死透时 docker ping 也挂,只看它会把
//! 「VM 活着、仅传输层断」误诊成 vm_down。故 docker 不可达时先做鉴别诊断
//! ([`crate::vm::ssh_reachable`],独立 SSH 连接):可达 = 传输层故障(`TransportDead`,
//! 与 `ForwardDead` 同一条梯子),不可达才是真 `VmDown`。转发活着但 docker 仍不可达 →
//! `TransportDegraded` 降级稳态(分流可用、容器管理不可用),重开 app 由 boot 自愈收尾。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;

use crate::{docker, infra, AppState};

/// 网关健康态(给前端横幅分流)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GatewayHealth {
    /// 容器在跑 + 宿主分流口可达。
    Healthy,
    /// 容器在跑但宿主分流口连不上 = app SSH 转发失效(本工具自愈目标态)。
    ForwardDead,
    /// mihomo 容器没在跑(不自动重启——交给 ensure/手动,避免盖住更深的问题)。
    ContainerDown,
    /// docker 不可达但 VM 的 sshd 可达且分流口也不通 = 传输层(mux/转发)坏死。
    /// 可自愈:直上二级备援隧道(一级 restart 需要 docker.sock,此态下不可用)。
    TransportDead,
    /// Docker 不可达或运行状态无法读取，但分流口可达 = 降级稳态:
    /// 分流可用、容器管理不可用。不折腾,横幅引导重开 app(boot 自愈重建底座)。
    TransportDegraded,
    /// docker 与 VM sshd 都不可达(真·VM 死,不在本模块自愈范围,弹横幅引导重开 app)。
    VmDown,
}

impl GatewayHealth {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::ForwardDead => "forward_dead",
            Self::ContainerDown => "container_down",
            Self::TransportDead => "transport_dead",
            Self::TransportDegraded => "transport_degraded",
            Self::VmDown => "vm_down",
        }
    }
}

/// 吐给 /api/system 的快照(看门狗每 tick 刷新)。
#[derive(Debug, Clone, Serialize)]
pub struct HealthSnapshot {
    pub gateway_health: GatewayHealth,
    /// 分流口端到端可用(真握手,不是「能 connect」;见 [`proxy_serves`])。
    pub proxy_port_reachable: bool,
    /// 正在尝试自愈(已确认 forward_dead、未放弃)。
    pub healing: bool,
    /// 自动自愈已放弃(两级梯子都无效),需手动修复/查诊断。
    pub gave_up: bool,
    /// 连续出站探测失败，与分流口健康正交；不能独自证明 usernet 槽位占满。
    pub vm_egress_dead: bool,
    pub usernet: Option<crate::usernet::Snapshot>,
    pub egress_guard_checked_at: Option<String>,
    pub egress_guard_applied: Option<bool>,
}

impl Default for HealthSnapshot {
    fn default() -> Self {
        // 首次 tick 前的乐观默认;看门狗启动即跑首检覆盖它。
        Self {
            gateway_health: GatewayHealth::Healthy,
            proxy_port_reachable: true,
            healing: false,
            gave_up: false,
            vm_egress_dead: false,
            usernet: None,
            egress_guard_checked_at: None,
            egress_guard_applied: None,
        }
    }
}

/// 共享快照句柄(AppState 持有,/api/system 读、看门狗写)。
pub type SharedHealth = Arc<Mutex<HealthSnapshot>>;

pub fn shared() -> SharedHealth {
    Arc::new(Mutex::new(HealthSnapshot::default()))
}

// ── 决策(纯函数,时间注入便于测试)─────────────────────────────────────────

const FAIL_STREAK_BEFORE_HEAL: u32 = 2; // 连续 2 次确认才动,防瞬时抖动
const HEAL_COOLDOWN_MS: u64 = 45_000; // 自愈后冷却,避免连击
const FLAP_WINDOW_MS: u64 = 15 * 60_000; // 抖动统计窗口
const FLAP_GIVEUP_COUNT: usize = 3; // 窗口内重建隧道 ≥3 次仍坏 → 升级二级(重启 mihomo)

/// 看门狗内部状态。
#[derive(Debug, Default)]
pub struct Watchdog {
    fail_streak: u32,
    last_heal_ms: Option<u64>,
    heal_times_ms: Vec<u64>,
    /// 已升级到二级(重启 mihomo 容器)。再失败即放弃。
    restarted: bool,
    gave_up: bool,
    outage_started_ms: Option<u64>,
    outage_heal_count: u32,
    outage_final_action: Option<Action>,
    recovery: Option<Recovery>,
}

/// 一次决策的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    /// 一级:重建 app 自持的 SSH 转发(秒级、不动容器、不需重登通道)。
    Tunnel,
    /// 二级:`docker restart mihomo`(隧道重建仍不过数据 = 疑似容器自身异常)。
    Restart,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Tunnel => "tunnel",
            Self::Restart => "restart",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recovery {
    pub outage_ms: u64,
    pub heal_count: u32,
    pub final_action: Option<Action>,
}

impl Watchdog {
    /// 纯决策:吃当前 health + 单调毫秒时钟,推进内部状态,返回动作。
    pub fn decide(&mut self, health: GatewayHealth, now_ms: u64) -> Action {
        self.recovery = None;
        if health != GatewayHealth::Healthy && self.outage_started_ms.is_none() {
            self.outage_started_ms = Some(now_ms);
            self.outage_heal_count = 0;
            self.outage_final_action = None;
        }
        let action = match health {
            GatewayHealth::Healthy => {
                if let Some(started_ms) = self.outage_started_ms.take() {
                    self.recovery = Some(Recovery {
                        outage_ms: now_ms.saturating_sub(started_ms),
                        heal_count: self.outage_heal_count,
                        final_action: self.outage_final_action,
                    });
                }
                // 恢复:清零,允许将来再自愈(梯子从一级重新开始)。
                self.fail_streak = 0;
                self.gave_up = false;
                self.restarted = false;
                self.heal_times_ms.clear();
                self.outage_heal_count = 0;
                self.outage_final_action = None;
                Action::None
            }
            // 分流口不过数据:先重建自持隧道(廉价、不动容器);连续无效才升级重启 mihomo。
            GatewayHealth::ForwardDead | GatewayHealth::TransportDead => {
                self.fail_streak += 1;
                if self.gave_up
                    || self.fail_streak < FAIL_STREAK_BEFORE_HEAL // 防抖:再观察一拍
                    || self
                        .last_heal_ms
                        .is_some_and(|t| now_ms.saturating_sub(t) < HEAL_COOLDOWN_MS) // 冷却中
                {
                    Action::None
                } else {
                    self.heal_times_ms.retain(|&t| now_ms.saturating_sub(t) < FLAP_WINDOW_MS);
                    if self.heal_times_ms.len() >= FLAP_GIVEUP_COUNT {
                        // 隧道反复重建仍不过数据 = 问题不在转发链,升级重启 mihomo 容器(只试一次)。
                        if self.restarted {
                            self.gave_up = true;
                            Action::None
                        } else {
                            self.restarted = true;
                            self.last_heal_ms = Some(now_ms);
                            Action::Restart
                        }
                    } else {
                        self.heal_times_ms.push(now_ms);
                        self.last_heal_ms = Some(now_ms);
                        Action::Tunnel
                    }
                }
            }
            GatewayHealth::TransportDegraded => {
                // 降级稳态:分流口活着、docker 不可达 = 分流可用,容器管理不可用。
                // 不折腾(清 gave_up,否则只有 Healthy 能清 → 永久错误红横幅)。
                self.fail_streak = 0;
                self.gave_up = false;
                Action::None
            }
            // VM/容器层面的问题不在本模块自愈范围。
            GatewayHealth::ContainerDown | GatewayHealth::VmDown => Action::None,
        };
        if action != Action::None {
            self.outage_heal_count = self.outage_heal_count.saturating_add(1);
            self.outage_final_action = Some(action);
        }
        action
    }

    pub fn gave_up(&self) -> bool {
        self.gave_up
    }

    pub fn fail_streak(&self) -> u32 { self.fail_streak }

    pub fn heal_count(&self) -> u32 { self.outage_heal_count }

    pub fn take_recovery(&mut self) -> Option<Recovery> { self.recovery.take() }

    /// 已知外因(睡眠唤醒)导致的必然断链:免掉防抖与冷却,下一拍就动手。
    /// 同时清 `gave_up`——上一轮放弃是针对醒来前那个世界的判断,不该压住这次自愈。
    pub fn force_heal(&mut self) {
        self.fail_streak = FAIL_STREAK_BEFORE_HEAL;
        self.last_heal_ms = None;
        self.gave_up = false;
    }

    /// healing = 正在尝试自愈(forward_dead / transport_dead 且未放弃)。
    pub fn healing(&self, health: GatewayHealth) -> bool {
        matches!(health, GatewayHealth::ForwardDead | GatewayHealth::TransportDead) && !self.gave_up
    }
}

// ── 检测(I/O)─────────────────────────────────────────────────────────────

/// 分流口探测结果:失败时保留失败阶段——「无人监听」「连得上但不回话(僵死转发典型)」
/// 「mihomo 回包异常」是三种不同的病,压成一个 bool 就没法事后定位(2026-08-26 复盘教训)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyProbe {
    Ok,
    /// 端口配置无效(空/0/非数字)。
    BadPort,
    /// TCP connect 出错(无人监听 / 被拒)= 宿主侧转发进程不在。
    ConnectFailed,
    /// TCP connect 3s 无响应(SYN 无人应,罕见)。
    ConnectTimeout,
    /// 连接建立但 3s 内握手无回音 = 僵死转发 / 链路后段不通的典型形态。
    Stalled,
    /// 握手中途连接被关(EOF / 写失败)。
    Closed,
    /// 回包不是 `05 00`(对端不是正常 SOCKS5)。
    BadReply,
}

impl ProxyProbe {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::BadPort => "bad_port",
            Self::ConnectFailed => "connect_failed",
            Self::ConnectTimeout => "connect_timeout",
            Self::Stalled => "stalled",
            Self::Closed => "closed",
            Self::BadReply => "bad_reply",
        }
    }
}

/// 宿主侧探分流口是否**真的过数据**(命门 #4:127.0.0.1)。这是用户 Clash 真正拨的口。
///
/// ⚠️ 判据必须端到端,不能只看 TCP 能否建连:2026-08-04 事故里 lima 的转发僵死后
/// listener 照常 accept、数据永不转发,旧的「connect 成功即健康」判据因此连续数小时
/// 误报 healthy——看门狗一次没触发、手动修复还回「已修复」。这里发一次真实 SOCKS5
/// 握手(`05 01 00` → 期望 `05 00`),只与 mihomo 本地交互、不产生上游流量。
pub async fn probe_proxy(host_port: &str) -> ProxyProbe {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let port: u16 = match host_port.parse() {
        Ok(p) if p != 0 => p,
        _ => return ProxyProbe::BadPort,
    };
    let addr = format!("127.0.0.1:{port}");
    // connect 与握手分开限时:僵死转发的典型是 connect 成功后永远无字节,合在一个
    // 超时里就分不清「没人听」和「听了不干活」。
    let mut s = match tokio::time::timeout(
        Duration::from_secs(3),
        tokio::net::TcpStream::connect(&addr),
    )
    .await
    {
        Err(_) => return ProxyProbe::ConnectTimeout,
        Ok(Err(_)) => return ProxyProbe::ConnectFailed,
        Ok(Ok(s)) => s,
    };
    let handshake = async {
        if s.write_all(&[0x05, 0x01, 0x00]).await.is_err() {
            return ProxyProbe::Closed;
        }
        let mut resp = [0u8; 2];
        match s.read_exact(&mut resp).await {
            Err(_) => ProxyProbe::Closed,
            Ok(_) if resp == [0x05, 0x00] => ProxyProbe::Ok,
            Ok(_) => ProxyProbe::BadReply,
        }
    };
    match tokio::time::timeout(Duration::from_secs(3), handshake).await {
        Err(_) => ProxyProbe::Stalled,
        Ok(outcome) => outcome,
    }
}

/// bool 简写(隧道 ensure / 手动修复等只关心通不通的场合)。
pub async fn proxy_serves(host_port: &str) -> bool {
    probe_proxy(host_port).await == ProxyProbe::Ok
}

/// 综合判网关健康。附带返回分流口探测的失败阶段(没探到分流口那步则为 `Ok` 占位,
/// 此时 health 本身已说明故障层:container_down / vm_down)。
pub async fn check(state: &AppState) -> (GatewayHealth, ProxyProbe) {
    // ping 包 5s 超时:半死 sock(accept 后不回话)下 bollard 兜底超时是 120s,
    // 裸 await 会把 20s 一拍的看门狗拖到分钟级、快照陈旧(与 ssh 探针/隧道同等硬化)。
    let mut docker = match state.docker() {
        Some(d)
            if matches!(
                tokio::time::timeout(Duration::from_secs(5), docker::ping(&d)).await,
                Ok(Ok(_))
            ) =>
        {
            Some(d)
        }
        _ => None,
    };
    if docker.is_none() {
        // 先试原生 sock 快速重连:覆盖「hostagent/VM 复活但旧句柄(或隧道 sock)已死」
        // 与「启动时没连上」两种自动收敛,不必等人重开 app。
        if let Ok(Ok(d)) = tokio::time::timeout(Duration::from_secs(5), docker::connect_at(&state.cfg.docker_socket().display().to_string())).await {
            state.set_docker(Some(d.clone()));
            docker = Some(d);
            crate::ev!(info, "watchdog", "docker_reconnect", "Docker 原生连接已恢复", { "via": "native" });
        }
    }
    let Some(docker) = docker else {
        // 鉴别诊断(盲区 #3):docker.sock 与被检转发共享 mux 故障域,ping 挂 ≠ VM 死。
        // 独立 SSH 探针可达 = 仅传输层断(可自愈);再按分流口死活分「待救」vs「降级稳态」。
        let probe = probe_proxy(&state.cfg.mihomo_host_port).await;
        if probe == ProxyProbe::Ok { return (GatewayHealth::TransportDegraded, probe); }
        return if crate::vm::ssh_reachable(&state.cfg.vm_profile).await {
            (GatewayHealth::TransportDead, probe)
        } else { (GatewayHealth::VmDown, probe) };
    };
    let running = match docker::running_state(&docker, infra::MIHOMO_CONTAINER).await {
        Ok(false) => return (GatewayHealth::ContainerDown, ProxyProbe::Ok),
        Ok(true) => true,
        Err(error) => {
            crate::ev!(warn, "watchdog", "container_inspect_failed", "容器运行状态暂不可确认", { "error": error.to_string() });
            false
        }
    };
    let probe = probe_proxy(&state.cfg.mihomo_host_port).await;
    if !running {
        // Docker ping 可用但 inspect 失败，不能断言容器停止，更不能仅据此重启。
        if probe == ProxyProbe::Ok { (GatewayHealth::TransportDegraded, probe) }
        else { (GatewayHealth::TransportDead, probe) }
    } else if probe == ProxyProbe::Ok {
        (GatewayHealth::Healthy, probe)
    } else {
        (GatewayHealth::ForwardDead, probe)
    }
}

// ── 故障定位证据(只在失败拍采集,健康路径零成本)──────────────────────────

/// 宿主分流口当前被谁监听(lsof 快照)。区分「旧转发还活着占着口」vs「口上没人」——
/// 2026-08-26 heal 空转事故里 exit 255「端口可能被占用」只是猜测文案,这里落实证。
async fn port_listener_snapshot(host_port: &str) -> String {
    let Ok(port) = host_port.parse::<u16>() else { return "bad_port".into() };
    let out = tokio::time::timeout(
        Duration::from_secs(3),
        tokio::process::Command::new("lsof")
            .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fpc"])
            .output(),
    )
    .await;
    match out {
        Err(_) => "lsof_timeout".into(),
        Ok(Err(e)) => format!("lsof_failed:{e}"),
        Ok(Ok(o)) => {
            // -Fpc 输出形如 "p123\ncssh\n" 的字段行;拼成 "ssh(123)"。
            let text = String::from_utf8_lossy(&o.stdout);
            let (mut pid, mut items) = (String::new(), Vec::new());
            for line in text.lines() {
                match line.split_at(1) {
                    ("p", rest) => pid = rest.to_string(),
                    ("c", rest) => items.push(format!("{rest}({pid})")),
                    _ => {}
                }
            }
            if items.is_empty() { "none".into() } else { items.join(",") }
        }
    }
}

/// 绕开宿主转发,经**独立** SSH 连接从 VM 内直探 mihomo 分流口(同一份 SOCKS5 握手)。
/// 二分故障段:VM 侧通 = 死在「宿主口 → SSH 转发」段;VM 侧也不通 = mihomo 自身不响应。
async fn vm_side_probe(state: &AppState) -> String {
    let Some(docker) = state.docker() else { return "skipped:no_docker".into() };
    let Some(ip) = crate::docker::container_ip(&docker, infra::MIHOMO_CONTAINER).await else {
        return "skipped:no_ip".into();
    };
    let cfg = crate::vm::ssh_config_path(&state.cfg.vm_profile);
    if !cfg.exists() {
        return "skipped:no_ssh_config".into();
    }
    // /dev/tcp 是 bash 内建,VM(Ubuntu)必有;整段再套 timeout 防 head 挂死。
    let remote = format!(
        "timeout 3 bash -c 'exec 3<>/dev/tcp/{ip}/{port}; printf \"\\x05\\x01\\x00\" >&3; head -c 2 <&3 | od -An -tx1'",
        port = infra::MIHOMO_PROXY_PORT
    );
    let out = tokio::time::timeout(
        Duration::from_secs(8),
        tokio::process::Command::new("ssh")
            .args([
                "-F", &cfg.display().to_string(),
                "-o", "ControlMaster=no",
                "-o", "ControlPath=none",
                "-o", "BatchMode=yes",
                "-o", "ConnectTimeout=5",
                &format!("lima-colima-{}", &state.cfg.vm_profile),
                "--", &remote,
            ])
            .output(),
    )
    .await;
    match out {
        Err(_) => "ssh_timeout".into(),
        Ok(Err(e)) => format!("ssh_spawn_failed:{e}"),
        Ok(Ok(o)) => {
            let reply: Vec<&str> = std::str::from_utf8(&o.stdout)
                .unwrap_or("")
                .split_whitespace()
                .collect();
            if reply == ["05", "00"] {
                "ok".into()
            } else if !o.status.success() {
                format!("unreachable(exit {:?})", o.status.code())
            } else {
                format!("bad_reply:{}", reply.join(" "))
            }
        }
    }
}

/// 看门狗 / 启动共用:取 VPN 网段后下发 VM 层守卫;失败只记事件(看门狗下一拍再试)。
pub async fn ensure_egress_guard(state: crate::AppState, force: bool) -> bool {
    let result = async {
        let docker = state.docker().ok_or_else(|| anyhow::anyhow!("Docker 连接不可用"))?;
        let subnet = crate::docker::network_subnet(&docker, &state.cfg.vpn_net).await
            .ok_or_else(|| anyhow::anyhow!("取不到 VPN 网段"))?;
        crate::vm::ensure_egress_guard(&state.cfg.vm_profile, &subnet, force).await
    }.await;
    let applied = result.is_ok();
    if let Ok(mut snap) = state.health.lock() {
        snap.egress_guard_checked_at = Some(chrono::Utc::now().to_rfc3339());
        snap.egress_guard_applied = Some(result.is_ok());
    }
    if let Err(e) = result {
        crate::ev!(warn, "vm", "egress_guard_failed",
            "VM 私网出站守卫未下发:不可达目标可能持续占用出站连接", { "error": e.to_string() });
    }
    applied
}

/// VM 出站(usernet)探活:经**独立** SSH 从 VM 内对稳定公网锚点发真实 TCP 建连
/// (223.5.5.5 / 119.29.29.29 的 443,均为国内公共 DNS 的 anycast,只发 SYN 握手即断,
/// 任一成功即算通)。走 usernet 用户态 NAT 的完整出站路径——这正是宿主换网后会
/// 整体僵死的那一段,docker ping / 分流口探测对它全盲(2026-09-01 实测:僵死时
/// 二者全绿,通道却「已登录但探活不过」)。
///
/// 返回 None = 没探成(SSH 不可达/超时),不计入失败连击——SSH 走 vsock,与出站
/// NAT 不同故障域,SSH 挂时应由 vm_down 路径定性,别把它误记成出站僵死。
async fn vm_egress_probe(state: &AppState) -> Option<bool> {
    let cfg = crate::vm::ssh_config_path(&state.cfg.vm_profile);
    if !cfg.exists() {
        return None;
    }
    // 两锚点相或:单点被墙内路由抖动误伤时不误报。/dev/tcp 是 bash 内建,VM 必有。
    let remote = "timeout 3 bash -c 'exec 3<>/dev/tcp/223.5.5.5/443' 2>/dev/null \
                  || timeout 3 bash -c 'exec 3<>/dev/tcp/119.29.29.29/443' 2>/dev/null";
    let out = tokio::time::timeout(
        Duration::from_secs(12),
        tokio::process::Command::new("ssh")
            .args([
                "-F", &cfg.display().to_string(),
                "-o", "ControlMaster=no",
                "-o", "ControlPath=none",
                "-o", "BatchMode=yes",
                "-o", "ConnectTimeout=5",
                &format!("lima-colima-{}", &state.cfg.vm_profile),
                "--", remote,
            ])
            .output(),
    )
    .await;
    match out {
        Err(_) | Ok(Err(_)) => None,
        Ok(Ok(o)) => Some(o.status.success()),
    }
}

/// 传输层自愈(看门狗一级 / 手动修复共用):重建 app 自持的 SSH 转发,
/// 再把 docker.sock 也经隧道救回、换上新连接 → 下一拍 ping 通,状态收敛回 Healthy。
///
/// 与旧版的关键差别:隧道重建失败**如实抛错**。旧版会拿「端口能 connect」当成功佐证,
/// 而僵死转发恰好满足那个条件 → 反复误报「已修复」(2026-08-04 事故)。
pub async fn heal_transport(state: &AppState) -> anyhow::Result<()> {
    crate::tunnel::ensure(state).await?;
    // docker.sock 同船获救(实测 2026-07-02:lima 用户在 docker 组,免 sudo 直转)。
    // 失败不算致命——分流口已通,docker 留给下拍原生 sock 重连或重开 app。
    let sock = state.cfg.data_dir.join("docker-tun.sock");
    match crate::vm::spawn_docker_sock_tunnel(&state.cfg.vm_profile, &sock).await {
        Ok(()) => match tokio::time::timeout(Duration::from_secs(5), crate::docker::connect_at(&sock.display().to_string())).await {
            Ok(Ok(d)) => {
                state.set_docker(Some(d));
                crate::ev!(info, "watchdog", "docker_reconnect", "Docker 已经隧道连接恢复", { "via": "tunnel_sock" });
            }
            Ok(Err(e)) => { crate::ev!(warn, "watchdog", "docker_reconnect_failed", "Docker 隧道连接失败", { "via": "tunnel_sock", "error": e.to_string() }); }
            Err(_) => { crate::ev!(warn, "watchdog", "docker_reconnect_failed", "Docker 隧道连接超时", { "via": "tunnel_sock" }); }
        },
        Err(e) => { crate::ev!(warn, "watchdog", "docker_reconnect_failed", "Docker socket 隧道建立失败", { "via": "tunnel_sock", "error": e.to_string() }); }
    }
    Ok(())
}

// ── 看门狗循环 ───────────────────────────────────────────────────────────

const TICK_SECS: u64 = 20;
/// 墙钟比单调钟多走这么多 = 中间睡过(Mac 合盖冻结 VM)。取 3 拍,躲开调度抖动。
const WAKE_GAP_SECS: u64 = TICK_SECS * 3;

/// 后台修复持有独占权与维护许可，调用方无需阻塞健康采样。
fn spawn_repair(
    state: AppState,
    repair: tokio::sync::OwnedMutexGuard<()>,
    action: Action,
    fail_streak: u32,
    woke: bool,
) -> Option<tokio::task::JoinHandle<()>> {
    let activity = state.lifecycle.runtime().maintenance()?;
    Some(tokio::spawn(async move {
        let _activity = activity;
        let _repair = repair;
        if !state.self_heal_enabled() { return; }
        match action {
            Action::Tunnel => {
                // 重建决策的依据落盘:旧转发进程死活 + 端口被谁占着,heal 空转时靠它定责。
                let (tunnel_pid, tunnel_alive) = crate::tunnel::status(&state).await;
                let port_listener = port_listener_snapshot(&state.cfg.mihomo_host_port).await;
                crate::ev!(warn, "watchdog", "heal_start", "分流口不过数据,重建 SSH 转发", {
                    "action": action.as_str(), "fail_streak": fail_streak,
                    "reason": if woke { "wake_gap" } else { "health_check" },
                    "tunnel_pid": tunnel_pid, "tunnel_alive": tunnel_alive,
                    "port_listener": port_listener
                });
                let action_started = std::time::Instant::now();
                if !state.self_heal_enabled() { return; }
                match heal_transport(&state).await {
                    Ok(()) => { crate::ev!(info, "watchdog", "heal_done", "SSH 转发重建动作完成", { "action": action.as_str(), "duration_ms": action_started.elapsed().as_millis() as u64 }); }
                    Err(e) => { crate::ev!(error, "watchdog", "heal_failed", "SSH 转发重建失败", { "action": action.as_str(), "error": e.to_string() }); }
                }
            }
            Action::Restart => {
                let (tunnel_pid, tunnel_alive) = crate::tunnel::status(&state).await;
                let port_listener = port_listener_snapshot(&state.cfg.mihomo_host_port).await;
                crate::ev!(warn, "watchdog", "heal_start", "重建转发无效,升级重启分流路由", {
                    "action": action.as_str(), "fail_streak": fail_streak,
                    "reason": if woke { "wake_gap" } else { "health_check" },
                    "tunnel_pid": tunnel_pid, "tunnel_alive": tunnel_alive,
                    "port_listener": port_listener
                });
                let action_started = std::time::Instant::now();
                if proxy_serves(&state.cfg.mihomo_host_port).await {
                    crate::ev!(info, "watchdog", "heal_skipped", "分流链路已恢复，取消本次重启", { "action": action.as_str() });
                    return;
                }
                if !state.self_heal_enabled() { return; }
                let result = async {
                    let d = state.docker().ok_or_else(|| anyhow::anyhow!("docker 连接不可用"))?;
                    docker::restart(&d, infra::MIHOMO_CONTAINER).await?;
                    // 容器换了 IP,转发目标随之失效 → 立刻按新 IP 重建。
                    crate::tunnel::ensure(&state).await
                }.await;
                match result {
                    Ok(()) => { crate::ev!(info, "watchdog", "heal_done", "分流路由重启动作完成", { "action": action.as_str(), "duration_ms": action_started.elapsed().as_millis() as u64 }); }
                    Err(e) => { crate::ev!(error, "watchdog", "heal_failed", "分流路由重启失败", { "action": action.as_str(), "error": e.to_string() }); }
                }
            }
            Action::None => {}
        }
    }))
}

/// 后台看门狗:定时端到端探分流口,坏了重建 app 自持的 SSH 转发,状态写入快照。
/// 在 `app::serve` 起头 spawn(bin 与 Tauri 壳共用,单处接入)。VM 死时只如实报 vm_down、不自愈。
///
/// 睡醒立查:Mac 合盖会冻结 VM,醒来后隧道多半已断。靠「墙钟跳变」识别(不依赖 macOS
/// 唤醒通知,bin 与 Tauri 壳同款行为),识别到就跳过防抖立刻自愈,而不是让用户先撞一次墙。
pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        let mut wd = Watchdog::default();
        let started = std::time::Instant::now();
        let mut tick = tokio::time::interval(Duration::from_secs(TICK_SECS));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_wall = std::time::SystemTime::now();
        let mut last_health: Option<GatewayHealth> = None;
        let mut self_heal_was_enabled = true;
        let mut tick_count = 0_u64;
        // 三次观测通常跨约两分钟；只表示检测失败，不作为槽位耗尽的独立证明。
        const EGRESS_FAIL_BEFORE_DEAD: u32 = 3;
        let mut egress_fail_streak = 0_u32;
        let mut egress_dead = false;
        let mut usernet = crate::usernet::Sampler::default();
        loop {
            tick.tick().await;
            if !crate::runtime::serving(&state) {
                wd = Watchdog::default();
                last_wall = std::time::SystemTime::now();
                last_health = None;
                continue;
            }
            let Some(_activity) = state.lifecycle.runtime().maintenance() else { continue; };
            if !crate::runtime::serving(&state) { continue; }
            tick_count = tick_count.wrapping_add(1);
            let self_heal_enabled = state.self_heal_enabled();
            let self_heal_resumed = self_heal_enabled && !self_heal_was_enabled;
            if self_heal_resumed {
                // 暂停期间不累计故障梯子；恢复后从干净状态重新观察。
                wd = Watchdog::default();
            }
            self_heal_was_enabled = self_heal_enabled;
            let wall_gap = std::time::SystemTime::now()
                .duration_since(last_wall)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            last_wall = std::time::SystemTime::now();
            let woke = wall_gap >= WAKE_GAP_SECS;
            if woke {
                crate::ev!(warn, "watchdog", "wake_detected", "检测到睡眠唤醒,立即体检自愈", { "wall_gap_s": wall_gap });
                if self_heal_enabled {
                    wd.force_heal();
                }
            }

            match crate::tunnel::take_exited(&state).await {
                Ok(Some(status)) => { crate::ev!(error, "tunnel", "tunnel_exited", "SSH 转发进程非预期退出", { "exit_code": status.code(), "stderr": crate::tunnel::stderr_tail() }); }
                Ok(None) => {}
                Err(e) => { crate::ev!(warn, "tunnel", "tunnel_check_failed", "SSH 转发进程状态检查失败", { "error": e.to_string() }); }
            }
            // 暂停时仍回收无人观看/过期的 noVNC 入口，只由内部开关挡住恢复动作。
            let ensure_due = self_heal_resumed || woke || tick_count.is_multiple_of(3);
            if let Some(activity) = state.lifecycle.runtime().maintenance() {
                let owned = state.clone();
                tokio::spawn(async move {
                    let _activity = activity;
                    crate::novnc::watchdog_tick(owned, ensure_due).await;
                });
            }

            let now_ms = started.elapsed().as_millis() as u64;
            let probe_started = std::time::Instant::now();
            let (health, probe) = check(&state).await;
            let probe_ms = probe_started.elapsed().as_millis() as u64;
            crate::ev!(debug, "watchdog", "health_tick", "分流链路体检完成", { "health": health.as_str(), "probe": probe.as_str(), "probe_ms": probe_ms });
            if last_health.is_some_and(|previous| previous != health) {
                let previous = last_health.unwrap();
                if health == GatewayHealth::Healthy {
                    crate::ev!(info, "watchdog", "health_changed", "分流链路健康态已变化", { "from": previous.as_str(), "to": health.as_str() });
                } else {
                    crate::ev!(warn, "watchdog", "health_changed", "分流链路健康态已变化", { "from": previous.as_str(), "to": health.as_str(), "probe": probe.as_str() });
                }
            } else if last_health.is_none() && health != GatewayHealth::Healthy {
                crate::ev!(warn, "watchdog", "health_changed", "首次体检发现分流链路异常", { "from": "unknown", "to": health.as_str(), "probe": probe.as_str() });
            }
            // 进故障的第一拍立刻固化定位证据:20 秒瞬断只有这一拍能抓到现场。
            // (只在转坏那拍采集:lsof + 独立 SSH 直探合计可达 ~10s,不能每拍都做。)
            let is_fault = matches!(health, GatewayHealth::ForwardDead | GatewayHealth::TransportDead);
            let was_fault = matches!(
                last_health,
                Some(GatewayHealth::ForwardDead | GatewayHealth::TransportDead)
            );
            if is_fault && !was_fault {
                let (tunnel_pid, tunnel_alive) = crate::tunnel::status(&state).await;
                let port_listener = port_listener_snapshot(&state.cfg.mihomo_host_port).await;
                let vm_side = vm_side_probe(&state).await;
                crate::ev!(warn, "watchdog", "fault_located", "分流口故障现场证据", {
                    "probe": probe.as_str(),
                    "tunnel_pid": tunnel_pid,
                    "tunnel_alive": tunnel_alive,
                    "port_listener": port_listener,
                    "vm_side": vm_side
                });
            }
            last_health = Some(health);

            // VM 层私网出站守卫(命门 #8):每分钟经 SSH 幂等下发;睡醒 / 换网立即重下发
            // (豁免的宿主局域网段随之更新)。VM 已死时跳过(归 vm_down)。
            // 有意放在「暂停自动修复」分支之前:守卫是防容器漏私网连接占死 VM 出站的基础防护,
            // 不是一次自愈动作,暂停自愈期间照样下发(2026-09-10 用户拍板;设置页开关说明同步写明)。
            if ensure_due && health != GatewayHealth::VmDown {
                if let Some(activity) = state.lifecycle.runtime().maintenance() {
                    let owned = state.clone();
                    tokio::spawn(async move {
                        let _activity = activity;
                        ensure_egress_guard(owned, woke).await;
                    });
                }
            }

            // VM 出站僵死检测:与分流口健康正交(僵死时 docker ping/分流口全绿),每 3 拍
            // (60s)一探;睡醒拍立探——换网/睡醒正是僵死的诱因。VM 已死时跳过(归 vm_down)。
            if health != GatewayHealth::VmDown && (woke || tick_count.is_multiple_of(3)) {
                match vm_egress_probe(&state).await {
                    Some(true) => {
                        egress_fail_streak = 0;
                        if egress_dead {
                            egress_dead = false;
                            crate::ev!(info, "watchdog", "vm_egress_recovered", "VM 出站已恢复", {});
                        }
                    }
                    Some(false) => {
                        egress_fail_streak += 1;
                        if !egress_dead && egress_fail_streak >= EGRESS_FAIL_BEFORE_DEAD {
                            egress_dead = true;
                            crate::ev!(error, "watchdog", "vm_egress_dead",
                                "连续 3 次出站检测失败（约 2 分钟）:通道可能表现为已登录但内网不通或无法登录。可能原因包括不可达目标占用 VM 出站拨号槽位;若基础防护在位仍持续失败,可退出并重新打开 app 重建 VM",
                                { "fail_streak": egress_fail_streak });
                        }
                    }
                    None => {} // SSH 没探成,不计连击(故障域不同,由 vm_down 路径定性)
                }
                if let Some(observation) = usernet.sample_if_due(&state.cfg.vm_profile).await {
                    let was_at_limit = state.health.lock().ok()
                        .and_then(|s| s.usernet.as_ref().and_then(|u| u.at_default_limit));
                    if observation.at_default_limit == Some(true) && was_at_limit != Some(true) {
                        crate::ev!(warn, "watchdog", "usernet_dial_pressure",
                            "关联 usernet 的 SYN_SENT 数量达到已知版本默认拨号上限；共享网络，来源待定位", {
                                "network": observation.network, "pid": observation.pid,
                                "syn_sent": observation.syn_sent, "runtime": observation.runtime,
                                "destinations": observation.destinations,
                                "attribution": observation.attribution
                            });
                    }
                    if let Ok(mut snap) = state.health.lock() { snap.usernet = Some(observation); }
                }
            }

            if !self_heal_enabled {
                if let Ok(mut snap) = state.health.lock() {
                    snap.gateway_health = health;
                    snap.proxy_port_reachable =
                        matches!(health, GatewayHealth::Healthy | GatewayHealth::TransportDegraded);
                    snap.healing = false;
                    snap.gave_up = false;
                    snap.vm_egress_dead = egress_dead;
                }
                continue;
            }

            let gave_up_before = wd.gave_up();
            let repair = state.lifecycle.try_gateway_repair();
            let repair_busy = repair.is_none();
            let action = if repair_busy { Action::None } else { wd.decide(health, now_ms) };
            let recovery = if repair_busy { None } else { wd.take_recovery() };
            if let Ok(mut snap) = state.health.lock() {
                snap.gateway_health = health;
                snap.proxy_port_reachable =
                    matches!(health, GatewayHealth::Healthy | GatewayHealth::TransportDegraded);
                snap.healing = repair_busy || wd.healing(health);
                snap.gave_up = !repair_busy && wd.gave_up();
                snap.vm_egress_dead = egress_dead;
            }
            if !gave_up_before && wd.gave_up() {
                crate::ev!(error, "watchdog", "giveup", "自动修复已用尽,等待手动处理", { "heal_count": wd.heal_count(), "window_min": FLAP_WINDOW_MS / 60_000 });
            }
            if let Some(recovery) = recovery {
                crate::ev!(info, "watchdog", "recovered", "分流链路已恢复", {
                    "outage_ms": recovery.outage_ms,
                    "heal_count": recovery.heal_count,
                    "final_action": recovery.final_action.map(Action::as_str).unwrap_or("none")
                });
            }
            if action == Action::None { continue; }
            if let Some(repair) = repair {
                spawn_repair(state.clone(), repair, action, wd.fail_streak(), woke);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn failed_container_inspection_is_not_reported_as_stopped() {
        use axum::{http::StatusCode, response::IntoResponse, Router};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = proxy.local_addr().unwrap().port();
        let responder = tokio::spawn(async move {
            loop {
                let (mut connection, _) = proxy.accept().await.unwrap();
                let mut request = [0; 3];
                connection.read_exact(&mut request).await.unwrap();
                connection.write_all(&[5, 0]).await.unwrap();
            }
        });
        let scenario = Arc::new(Mutex::new("error"));
        let mode = scenario.clone();
        let app = Router::new().fallback(move |request: axum::extract::Request| {
          let mode = mode.clone();
          async move {
            if request.uri().path().ends_with("/_ping") { return StatusCode::OK.into_response(); }
            let mode = *mode.lock().unwrap();
            match mode {
                "error" => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                "missing" => StatusCode::NOT_FOUND.into_response(),
                "unknown" => axum::Json(serde_json::json!({"State":{}})).into_response(),
                "timeout" => { tokio::time::sleep(Duration::from_secs(30)).await; StatusCode::OK.into_response() }
                _ => axum::Json(serde_json::json!({"State":{"Running":mode == "running"}})).into_response(),
            }
          }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let docker = bollard::Docker::connect_with_http(&format!("http://{}", listener.local_addr().unwrap()),
            120, bollard::API_DEFAULT_VERSION).unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = crate::config::Config::from_getter(|_| None);
        cfg.data_dir = dir.path().into(); cfg.ui_port = 0; cfg.dev_mode = true;
        cfg.managed_vm = true; cfg.vm_profile = "vpnmgr-watchdog-test".into();
        cfg.mihomo_host_port = port.to_string();
        let (_ui, state) = crate::app::bootstrap(cfg).await.unwrap();
        state.set_docker(Some(docker.clone()));
        for (mode, expected) in [("error", GatewayHealth::TransportDegraded), ("unknown", GatewayHealth::TransportDegraded),
            ("stopped", GatewayHealth::ContainerDown), ("missing", GatewayHealth::ContainerDown),
            ("running", GatewayHealth::Healthy), ("timeout", GatewayHealth::TransportDegraded)] {
            *scenario.lock().unwrap() = mode;
            let (result, _) = tokio::time::timeout(Duration::from_secs(7), async {
                tokio::join!(check(&state), docker::container_ip(&docker, "mihomo"))
            }).await.expect("inspect and IP lookup must finish before the 120 second client timeout");
            assert_eq!(result, (expected, ProxyProbe::Ok), "{mode}: unknown is not stopped");
        }
        server.abort(); responder.abort();
        let _ = tokio::join!(server, responder);
    }

    #[tokio::test]
    async fn background_repair_keeps_runtime_and_excludes_manual_restarts_without_blocking_checks() {
        use axum::{http::StatusCode, response::IntoResponse, Router};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let entered = Arc::new(tokio::sync::Semaphore::new(0));
        let finish = Arc::new(tokio::sync::Semaphore::new(0));
        let posts = Arc::new(AtomicUsize::new(0));
        let (entry, gate, writes) = (entered.clone(), finish.clone(), posts.clone());
        let app = Router::new().fallback(move |request: axum::extract::Request| {
            let (entry, gate, writes) = (entry.clone(), gate.clone(), writes.clone());
            async move {
                let path = request.uri().path();
                if path.ends_with("/_ping") { return StatusCode::OK.into_response(); }
                if path.ends_with("/restart") {
                    writes.fetch_add(1, Ordering::SeqCst);
                    entry.add_permits(1);
                    gate.acquire().await.unwrap().forget();
                    return StatusCode::NO_CONTENT.into_response();
                }
                axum::Json(serde_json::json!({"Id":"owned-mihomo", "State":{"Running":true,
                    "StartedAt":if writes.load(Ordering::SeqCst) > 0 { "2026-09-11T00:01:00Z" } else { "2026-09-11T00:00:00Z" }}})).into_response()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let docker = bollard::Docker::connect_with_http(&format!("http://{}", listener.local_addr().unwrap()), 120, bollard::API_DEFAULT_VERSION).unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = proxy.local_addr().unwrap().port();
        let proxy_posts = posts.clone();
        let responder = tokio::spawn(async move {
            loop {
                let (mut socket, _) = proxy.accept().await.unwrap();
                let mut request = [0; 3];
                socket.read_exact(&mut request).await.unwrap();
                socket.write_all(if proxy_posts.load(Ordering::SeqCst) > 0 { &[5, 0] } else { b"HT" }).await.unwrap();
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = crate::config::Config::from_getter(|_| None);
        cfg.data_dir = dir.path().into(); cfg.ui_port = 0; cfg.dev_mode = true;
        cfg.managed_vm = true; cfg.vm_profile = "vpnmgr-watchdog-test".into();
        cfg.mihomo_host_port = port.to_string();
        let (_ui, state) = crate::app::bootstrap(cfg).await.unwrap();
        state.set_docker(Some(docker));
        let runtime = state.lifecycle.runtime();
        runtime.ensure(|| async { Ok(()) }).await.unwrap();
        let repair = state.lifecycle.try_gateway_repair().unwrap();
        let job = spawn_repair(state.clone(), repair, Action::Restart, 5, false).unwrap();
        tokio::time::timeout(Duration::from_secs(5), entered.acquire()).await.unwrap().unwrap().forget();
        assert_eq!(runtime.snapshot().active_tasks, 1);
        let manual = crate::routes::heal_proxy(axum::extract::State(state.clone())).await.0;
        assert_eq!(manual["pending"], true);
        assert_eq!(manual["ok"], false);
        assert_eq!(tokio::time::timeout(Duration::from_secs(2), check(&state)).await.unwrap(), (GatewayHealth::Healthy, ProxyProbe::Ok));
        assert!(!runtime.release_if_idle(Duration::ZERO, || async { panic!("repair still owns runtime") }, || async { Ok(()) }).await.unwrap());
        finish.add_permits(1);
        tokio::time::timeout(Duration::from_secs(2), job).await.unwrap().unwrap();
        assert_eq!(runtime.snapshot().active_tasks, 0);
        assert!(state.lifecycle.try_gateway_repair().is_some());
        assert_eq!(posts.load(Ordering::SeqCst), 1);
        // 暂停自动修复，或故障在执行前已恢复，都不能再重启。
        for enabled in [false, true] {
            state.set_self_heal_enabled(enabled);
            let repair = state.lifecycle.try_gateway_repair().unwrap();
            let job = spawn_repair(state.clone(), repair, Action::Restart, 5, false).unwrap();
            tokio::time::timeout(Duration::from_secs(5), job).await.unwrap().unwrap();
            assert_eq!(posts.load(Ordering::SeqCst), 1);
            assert_eq!(runtime.snapshot().active_tasks, 0);
        }
        server.abort(); responder.abort(); let _ = tokio::join!(server, responder);
    }

    #[test]
    fn healthy_resets_and_clears_giveup() {
        // 先制造一次放弃(且已升级过重启)
        let mut wd = Watchdog {
            gave_up: true,
            restarted: true,
            fail_streak: 5,
            heal_times_ms: vec![1, 2, 3],
            ..Default::default()
        };
        assert_eq!(wd.decide(GatewayHealth::Healthy, 100), Action::None);
        assert!(!wd.gave_up());
        assert!(!wd.restarted, "恢复后梯子复位,下次事故从一级重新开始");
        assert_eq!(wd.fail_streak, 0);
        assert!(wd.heal_times_ms.is_empty());
    }

    #[test]
    fn debounces_then_heals() {
        let mut wd = Watchdog::default();
        // 第 1 拍:仅 streak=1,不动(防瞬时)
        assert_eq!(wd.decide(GatewayHealth::ForwardDead, 0), Action::None);
        // 第 2 拍:确认 → 一级自愈(重建 SSH 转发)
        assert_eq!(wd.decide(GatewayHealth::ForwardDead, 20_000), Action::Tunnel);
    }

    #[test]
    fn cooldown_blocks_back_to_back_heals() {
        let mut wd = Watchdog::default();
        wd.decide(GatewayHealth::ForwardDead, 0);
        assert_eq!(wd.decide(GatewayHealth::ForwardDead, 20_000), Action::Tunnel);
        // 冷却内(<45s)再 forward_dead 不重建
        assert_eq!(wd.decide(GatewayHealth::ForwardDead, 40_000), Action::None);
        // 过冷却 → 再自愈
        assert_eq!(wd.decide(GatewayHealth::ForwardDead, 80_000), Action::Tunnel);
    }

    #[test]
    fn escalates_tunnel_to_restart_then_gives_up() {
        // 持续 forward_dead、每 20s 一拍:防抖后窗口内重建转发 3 次(各隔冷却),
        // 仍坏 → 升级重启 mihomo 一次,再坏 → 放弃。一拍 = 一次 decide。
        let mut wd = Watchdog::default();
        let (mut restarts, mut tunnels) = (0, 0);
        for i in 0..200u64 {
            match wd.decide(GatewayHealth::ForwardDead, i * 20_000) {
                Action::Restart => restarts += 1,
                Action::Tunnel => tunnels += 1,
                Action::None => {}
            }
            if wd.gave_up() {
                break;
            }
        }
        assert_eq!(tunnels, FLAP_GIVEUP_COUNT, "一级重建转发窗口内最多试 3 次");
        assert_eq!(restarts, 1, "二级重启 mihomo 只试一次");
        assert!(wd.gave_up());
        // 放弃后即便再 forward_dead 也不动
        assert_eq!(wd.decide(GatewayHealth::ForwardDead, 9_000_000), Action::None);
    }

    #[test]
    fn vm_and_container_down_never_heal() {
        let mut wd = Watchdog::default();
        assert_eq!(wd.decide(GatewayHealth::VmDown, 0), Action::None);
        assert_eq!(wd.decide(GatewayHealth::VmDown, 20_000), Action::None);
        assert_eq!(wd.decide(GatewayHealth::ContainerDown, 40_000), Action::None);
        assert!(!wd.gave_up());
    }

    #[test]
    fn transport_dead_shares_the_same_ladder() {
        // 转发链的两种死法(分流口死 / docker 也不可达)现在同一条梯子:
        // 先重建 SSH 转发(廉价、不动容器),反复无效才升级重启 mihomo。
        let mut wd = Watchdog::default();
        assert_eq!(wd.decide(GatewayHealth::TransportDead, 0), Action::None, "防抖第一拍不动");
        assert_eq!(wd.decide(GatewayHealth::TransportDead, 20_000), Action::Tunnel);
        assert_eq!(wd.decide(GatewayHealth::TransportDead, 40_000), Action::None, "冷却内不重复");
        // 两种状态互相切换不打断梯子:额度是共享的
        assert_eq!(wd.decide(GatewayHealth::ForwardDead, 90_000), Action::Tunnel);
        assert!(!wd.gave_up());
    }

    /// 把梯子跑到底(重建 ×3 → 重启 ×1 → 放弃),返回动作计数。
    fn exhaust_ladder(wd: &mut Watchdog, health: GatewayHealth) -> (usize, usize) {
        let (mut tunnels, mut restarts) = (0, 0);
        for i in 0..200u64 {
            match wd.decide(health, i * 20_000) {
                Action::Tunnel => tunnels += 1,
                Action::Restart => restarts += 1,
                Action::None => {}
            }
            if wd.gave_up() {
                break;
            }
        }
        (tunnels, restarts)
    }

    #[test]
    fn transport_degraded_is_stable_and_clears_gave_up() {
        // 降级稳态:分流口活着、docker 不可达 → 不折腾,且清 gave_up
        // (否则 gave_up 只有 Healthy 能清、docker 不通时永远到不了 → 永久错误红横幅)。
        let mut wd = Watchdog::default();
        assert!(exhaust_ladder(&mut wd, GatewayHealth::TransportDead).0 > 0);
        assert!(wd.gave_up());
        assert_eq!(wd.decide(GatewayHealth::TransportDegraded, 9_000_000), Action::None);
        assert!(!wd.gave_up());
        // 持续降级不再动作
        assert_eq!(wd.decide(GatewayHealth::TransportDegraded, 9_100_000), Action::None);
    }

    #[test]
    fn transport_states_serialize_snake_case() {
        // 前端横幅按字符串分支,序列化名是契约的一部分。
        assert_eq!(serde_json::to_string(&GatewayHealth::TransportDead).unwrap(), "\"transport_dead\"");
        assert_eq!(
            serde_json::to_string(&GatewayHealth::TransportDegraded).unwrap(),
            "\"transport_degraded\""
        );
    }

    #[test]
    fn transport_dead_reports_healing() {
        let wd = Watchdog::default();
        assert!(wd.healing(GatewayHealth::TransportDead));
        assert!(!wd.healing(GatewayHealth::TransportDegraded));
        assert!(!wd.healing(GatewayHealth::VmDown));
    }

    #[test]
    fn force_heal_bypasses_debounce_and_giveup() {
        // 睡醒:上一轮已放弃、也没到防抖阈值,force_heal 后下一拍必须立刻动手。
        let mut wd = Watchdog { gave_up: true, last_heal_ms: Some(0), ..Default::default() };
        wd.force_heal();
        assert!(!wd.gave_up(), "醒来是新世界,旧的放弃不该压住这次自愈");
        assert_eq!(wd.decide(GatewayHealth::ForwardDead, 1_000), Action::Tunnel);
    }

    #[test]
    fn outage_recovery_reports_duration_heals_and_final_action() {
        let mut wd = Watchdog::default();
        assert_eq!(wd.decide(GatewayHealth::ForwardDead, 1_000), Action::None);
        assert_eq!(wd.decide(GatewayHealth::ForwardDead, 21_000), Action::Tunnel);
        assert_eq!(wd.decide(GatewayHealth::Healthy, 51_000), Action::None);
        assert_eq!(wd.take_recovery(), Some(Recovery {
            outage_ms: 50_000, heal_count: 1, final_action: Some(Action::Tunnel),
        }));
        assert_eq!(wd.take_recovery(), None, "恢复记录只消费一次");
    }

    #[test]
    fn recovery_after_giveup_keeps_the_whole_outage() {
        let mut wd = Watchdog::default();
        let (tunnels, restarts) = exhaust_ladder(&mut wd, GatewayHealth::ForwardDead);
        assert!(wd.gave_up());
        assert_eq!((tunnels, restarts), (3, 1));
        assert_eq!(wd.decide(GatewayHealth::Healthy, 10_000_000), Action::None);
        let recovery = wd.take_recovery().unwrap();
        assert_eq!(recovery.outage_ms, 10_000_000);
        assert_eq!(recovery.heal_count, 4);
        assert_eq!(recovery.final_action, Some(Action::Restart));
    }

    #[tokio::test]
    async fn unserved_port_is_false() {
        // 0 / 空 → false;无监听 → false(connect 拒绝)
        assert!(!proxy_serves("0").await);
        assert!(!proxy_serves("").await);
        assert!(!proxy_serves("1").await); // 1 号端口几乎不可能有监听
    }

    #[tokio::test]
    async fn accepting_but_silent_port_is_false() {
        // 2026-08-04 事故的核心回归:僵死转发照常 accept、永不回字节。
        // 旧判据(TCP connect 成功即健康)在这里恒真,连续数小时误报 healthy。
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            // 只 accept、不回应答,连接一直挂着(模拟 lima gRPC 半死转发)
            let _held = l.accept().await;
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        assert!(!proxy_serves(&port.to_string()).await, "能连上但不过数据 ≠ 健康");
    }

    #[tokio::test]
    async fn probe_distinguishes_failure_stages() {
        // 无人监听 → connect_failed(与「连上但不回话」是不同的病,必须分开)。
        assert_eq!(probe_proxy("1").await, ProxyProbe::ConnectFailed);
        assert_eq!(probe_proxy("0").await, ProxyProbe::BadPort);
        assert_eq!(probe_proxy("").await, ProxyProbe::BadPort);
        // 僵死转发形态:accept 后无字节 → stalled。
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _held = l.accept().await;
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        assert_eq!(probe_proxy(&port.to_string()).await, ProxyProbe::Stalled);
    }

    #[tokio::test]
    async fn probe_flags_bad_reply() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut s, _)) = l.accept().await {
                let mut req = [0u8; 3];
                let _ = s.read_exact(&mut req).await;
                let _ = s.write_all(b"HT").await; // 不是 SOCKS5(比如误连到 HTTP 服务)
            }
        });
        assert_eq!(probe_proxy(&port.to_string()).await, ProxyProbe::BadReply);
    }

    #[tokio::test]
    async fn socks5_responder_is_true() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut s, _)) = l.accept().await {
                let mut req = [0u8; 3];
                let _ = s.read_exact(&mut req).await;
                let _ = s.write_all(&[0x05, 0x00]).await; // 无需认证
            }
        });
        assert!(proxy_serves(&port.to_string()).await);
    }
}
