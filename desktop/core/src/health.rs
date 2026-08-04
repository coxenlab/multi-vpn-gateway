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
    /// 容器在跑但宿主分流口连不上 = lima 转发静默丢失(本工具自愈目标态)。
    ForwardDead,
    /// mihomo 容器没在跑(不自动重启——交给 ensure/手动,避免盖住更深的问题)。
    ContainerDown,
    /// docker 不可达但 VM 的 sshd 可达且分流口也不通 = 传输层(mux/转发)坏死。
    /// 可自愈:直上二级备援隧道(一级 restart 需要 docker.sock,此态下不可用)。
    TransportDead,
    /// docker 不可达但分流口可达(通常 = 备援隧道已接管)= 降级稳态:
    /// 分流可用、容器管理不可用。不折腾,横幅引导重开 app(boot 自愈重建底座)。
    TransportDegraded,
    /// docker 与 VM sshd 都不可达(真·VM 死,不在本模块自愈范围,弹横幅引导重开 app)。
    VmDown,
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
}

impl Default for HealthSnapshot {
    fn default() -> Self {
        // 首次 tick 前的乐观默认;看门狗启动即跑首检覆盖它。
        Self {
            gateway_health: GatewayHealth::Healthy,
            proxy_port_reachable: true,
            healing: false,
            gave_up: false,
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

impl Watchdog {
    /// 纯决策:吃当前 health + 单调毫秒时钟,推进内部状态,返回动作。
    pub fn decide(&mut self, health: GatewayHealth, now_ms: u64) -> Action {
        match health {
            GatewayHealth::Healthy => {
                // 恢复:清零,允许将来再自愈(梯子从一级重新开始)。
                self.fail_streak = 0;
                self.gave_up = false;
                self.restarted = false;
                self.heal_times_ms.clear();
                Action::None
            }
            // 分流口不过数据:先重建自持隧道(廉价、不动容器);连续无效才升级重启 mihomo。
            GatewayHealth::ForwardDead | GatewayHealth::TransportDead => {
                self.fail_streak += 1;
                if self.gave_up {
                    return Action::None;
                }
                if self.fail_streak < FAIL_STREAK_BEFORE_HEAL {
                    return Action::None; // 防抖:再观察一拍
                }
                if let Some(t) = self.last_heal_ms {
                    if now_ms.saturating_sub(t) < HEAL_COOLDOWN_MS {
                        return Action::None; // 冷却中
                    }
                }
                self.heal_times_ms.retain(|&t| now_ms.saturating_sub(t) < FLAP_WINDOW_MS);
                if self.heal_times_ms.len() >= FLAP_GIVEUP_COUNT {
                    // 隧道反复重建仍不过数据 = 问题不在转发链,升级重启 mihomo 容器(只试一次)。
                    if self.restarted {
                        self.gave_up = true;
                        return Action::None;
                    }
                    self.restarted = true;
                    self.last_heal_ms = Some(now_ms);
                    return Action::Restart;
                }
                self.heal_times_ms.push(now_ms);
                self.last_heal_ms = Some(now_ms);
                Action::Tunnel
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
        }
    }

    pub fn gave_up(&self) -> bool {
        self.gave_up
    }

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

/// 宿主侧探分流口是否**真的过数据**(命门 #4:127.0.0.1)。这是用户 Clash 真正拨的口。
///
/// ⚠️ 判据必须端到端,不能只看 TCP 能否建连:2026-08-04 事故里 lima 的转发僵死后
/// listener 照常 accept、数据永不转发,旧的「connect 成功即健康」判据因此连续数小时
/// 误报 healthy——看门狗一次没触发、手动修复还回「已修复」。这里发一次真实 SOCKS5
/// 握手(`05 01 00` → 期望 `05 00`),只与 mihomo 本地交互、不产生上游流量。
pub async fn proxy_serves(host_port: &str) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let port: u16 = match host_port.parse() {
        Ok(p) if p != 0 => p,
        _ => return false,
    };
    let addr = format!("127.0.0.1:{port}");
    let handshake = async {
        let mut s = tokio::net::TcpStream::connect(&addr).await.ok()?;
        s.write_all(&[0x05, 0x01, 0x00]).await.ok()?;
        let mut resp = [0u8; 2];
        s.read_exact(&mut resp).await.ok()?;
        Some(resp == [0x05, 0x00])
    };
    // 3s:僵死转发的典型表现是 connect 成功后永远无字节,必须靠超时判死。
    matches!(tokio::time::timeout(Duration::from_secs(3), handshake).await, Ok(Some(true)))
}

/// 综合判网关健康。
pub async fn check(state: &AppState) -> GatewayHealth {
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
        if let Ok(Ok(d)) = tokio::time::timeout(Duration::from_secs(5), docker::connect()).await {
            state.set_docker(Some(d.clone()));
            docker = Some(d);
        }
    }
    let Some(docker) = docker else {
        // 鉴别诊断(盲区 #3):docker.sock 与被检转发共享 mux 故障域,ping 挂 ≠ VM 死。
        // 独立 SSH 探针可达 = 仅传输层断(可自愈);再按分流口死活分「待救」vs「降级稳态」。
        if !crate::vm::ssh_reachable(crate::vm::PROFILE).await {
            return GatewayHealth::VmDown;
        }
        return if proxy_serves(&state.cfg.mihomo_host_port).await {
            GatewayHealth::TransportDegraded
        } else {
            GatewayHealth::TransportDead
        };
    };
    if !docker::is_running(&docker, infra::MIHOMO_CONTAINER).await {
        return GatewayHealth::ContainerDown;
    }
    if proxy_serves(&state.cfg.mihomo_host_port).await {
        GatewayHealth::Healthy
    } else {
        GatewayHealth::ForwardDead
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
    match crate::vm::spawn_docker_sock_tunnel(crate::vm::PROFILE, &sock).await {
        Ok(()) => match crate::docker::connect_at(&sock.display().to_string()).await {
            Ok(d) => {
                state.set_docker(Some(d));
                eprintln!("[watchdog] docker 已经隧道 sock 重连,容器管理恢复");
            }
            Err(e) => eprintln!("[watchdog] 隧道 sock 连接失败: {e}"),
        },
        Err(e) => eprintln!("[watchdog] docker.sock 隧道失败: {e}"),
    }
    Ok(())
}

// ── 看门狗循环 ───────────────────────────────────────────────────────────

const TICK_SECS: u64 = 20;
/// 墙钟比单调钟多走这么多 = 中间睡过(Mac 合盖冻结 VM)。取 3 拍,躲开调度抖动。
const WAKE_GAP_SECS: u64 = TICK_SECS * 3;

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
        let mut last_wall = std::time::SystemTime::now();
        loop {
            tick.tick().await;
            let wall_gap = std::time::SystemTime::now()
                .duration_since(last_wall)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            last_wall = std::time::SystemTime::now();
            if wall_gap >= WAKE_GAP_SECS {
                eprintln!("[watchdog] 墙钟跳变 {wall_gap}s(疑似刚从睡眠唤醒),立即体检自愈");
                wd.force_heal();
            }
            let now_ms = started.elapsed().as_millis() as u64;
            let health = check(&state).await;
            let action = wd.decide(health, now_ms);
            if let Ok(mut snap) = state.health.lock() {
                snap.gateway_health = health;
                snap.proxy_port_reachable =
                    matches!(health, GatewayHealth::Healthy | GatewayHealth::TransportDegraded);
                snap.healing = wd.healing(health);
                snap.gave_up = wd.gave_up();
            }
            match action {
                Action::Tunnel => {
                    eprintln!("[watchdog] 分流口不过数据,重建 SSH 转发");
                    if let Err(e) = heal_transport(&state).await {
                        eprintln!("[watchdog] 重建 SSH 转发失败: {e}");
                    }
                }
                Action::Restart => {
                    if let Some(d) = state.docker().as_ref() {
                        eprintln!("[watchdog] 重建转发无效,升级 restart {}", infra::MIHOMO_CONTAINER);
                        if let Err(e) = docker::restart(d, infra::MIHOMO_CONTAINER).await {
                            eprintln!("[watchdog] restart {} 失败: {e}", infra::MIHOMO_CONTAINER);
                        }
                        // 容器换了 IP,转发目标随之失效 → 立刻按新 IP 重建。
                        if let Err(e) = crate::tunnel::ensure(&state).await {
                            eprintln!("[watchdog] restart 后重建 SSH 转发失败: {e}");
                        }
                    }
                }
                Action::None => {}
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

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
