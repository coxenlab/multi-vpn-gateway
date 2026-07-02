//! 分流口健康看门狗。命门 #1 不破:登录判据仍是 SOCKS5 探活(`manager::probe`,经 docker.sock
//! 走 VM 内网),本模块只额外看**宿主→mihomo 分流口**的可达性——那是用户 Clash 真正拨的口,
//! 也是 SOCKS5 探活摸不到的盲区。
//!
//! 背景病:mihomo 分流口经 lima 端口转发暴露到宿主 127.0.0.1;SSH 主连接重建(睡醒/网络抖动)后,
//! 重建前就一直监听的长命端口转发会被 **静默丢失**(lima 不重建、日志只有 Forwarding 无 Stopping),
//! 于是容器全绿、宿主却连不上分流口 → 浏览器 ERR_EMPTY_RESPONSE。详见 auto-memory
//! `desktop-app-colima-ops-gotchas`。
//!
//! 自愈是两级梯子:
//! 1. `docker restart mihomo` —— 让分流口"重新出现"→ 逼 lima 在当前活的主连接上重建转发
//!    (端口映射 restart 保留,vpn-router 节点不用改)。前提是 hostagent 活着、会响应端口事件。
//! 2. restart 连续无效(= hostagent 僵死,端口事件发出来没人听;实测 2026-06-10:睡醒后
//!    hostagent 日志/事件流彻底停摆,ssh.sock 不再重建)→ 经 [`crate::vm::spawn_port_tunnel`]
//!    拉备援 SSH 隧道直转分流口 + 控制口(与 colima 自身 docker.sock 隧道同款,不依赖 hostagent)。
//!
//! 两级都失败才放弃。带防抖 + 冷却,避免反复重启断连。状态吐给 /api/system 供前端横幅。
//!
//! 盲区 #3(实测 2026-07-02,auto-memory `watchdog-transport-dead-misdiagnosed-vmdown`):
//! docker.sock 转发与分流口转发共享同一 SSH 主连接(mux)故障域——mux 死透时 docker ping
//! 也挂,只看它会把「VM 活着、仅传输层断」误诊成 vm_down,短路整个梯子;而此时备援隧道
//! (独立连接)明明能当场救活分流口。故 docker 不可达时先做鉴别诊断([`crate::vm::ssh_reachable`],
//! 与隧道同款独立 SSH):可达 = 传输层故障(`TransportDead`,直上二级隧道;一级 restart 需要
//! docker.sock,此态下不可用),不可达才是真 `VmDown`。隧道救活分流口后 docker 仍不可达,
//! 落 `TransportDegraded` 降级稳态(分流可用、容器管理不可用),重开 app 由 boot 自愈收尾。

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
    pub proxy_port_reachable: bool,
    /// 正在尝试自愈(已确认 forward_dead、未放弃)。
    pub healing: bool,
    /// 自动自愈已放弃(两级梯子都无效),需手动修复/查诊断。
    pub gave_up: bool,
    /// 分流口已由备援 SSH 隧道接管(hostagent 僵死的降级态:现有口可用,
    /// 但新端口——如新建通道的 noVNC——不会再被 lima 转发,前端据此提示)。
    ///
    /// 已知限制(评审坐实、按「不为罕见场景加架构」不修,记录在案):
    /// 1. 只置位不清除——隧道是 `-f` 守护化孤儿进程,无法廉价判断「端口现在由谁伺服」;
    ///    用户手动重启 VM 真恢复后,本进程内提示残留(文案是「可能」级,危害低);app 重启后标志丢失。
    /// 2. 隧道活着时 `proxy_port_reachable` 必通(ssh 本地 listener 先 accept),对「隧道在、
    ///    端到端断」失明;窗口窄:容器死走 ContainerDown(查 docker 不查端口),VM ssh 断后
    ///    隧道靠 ServerAlive ~2min 自杀、端口释放、探测恢复(容忍放宽:高负载下 30s 会误杀活隧道)。
    pub tunnel_fallback: bool,
}

impl Default for HealthSnapshot {
    fn default() -> Self {
        // 首次 tick 前的乐观默认;看门狗启动即跑首检覆盖它。
        Self {
            gateway_health: GatewayHealth::Healthy,
            proxy_port_reachable: true,
            healing: false,
            gave_up: false,
            tunnel_fallback: false,
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
const HEAL_COOLDOWN_MS: u64 = 90_000; // 自愈后冷却,避免连击
const FLAP_WINDOW_MS: u64 = 15 * 60_000; // 抖动统计窗口
const FLAP_GIVEUP_COUNT: usize = 3; // 窗口内 restart ≥3 次仍坏 → 升级二级(隧道)

/// 看门狗内部状态。
#[derive(Debug, Default)]
pub struct Watchdog {
    fail_streak: u32,
    last_heal_ms: Option<u64>,
    heal_times_ms: Vec<u64>,
    /// 已升级到二级(备援隧道)。再失败即放弃。
    tunneled: bool,
    gave_up: bool,
}

/// 一次决策的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    /// 一级:`docker restart mihomo`(hostagent 活着时逼 lima 重建转发)。
    Restart,
    /// 二级:拉备援 SSH 隧道(restart 救不回 = hostagent 僵死,端口事件没人听)。
    Tunnel,
}

impl Watchdog {
    /// 纯决策:吃当前 health + 单调毫秒时钟,推进内部状态,返回动作。
    pub fn decide(&mut self, health: GatewayHealth, now_ms: u64) -> Action {
        match health {
            GatewayHealth::Healthy => {
                // 恢复:清零,允许将来再自愈(梯子从一级重新开始)。
                self.fail_streak = 0;
                self.gave_up = false;
                self.tunneled = false;
                self.heal_times_ms.clear();
                Action::None
            }
            GatewayHealth::ForwardDead => {
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
                    // 一级连续无效 = hostagent 多半僵死 → 升级备援隧道(只试一次;
                    // 若传输层阶段已消耗过隧道额度,则到此放弃)。tunneled 的判定放在
                    // restart 次数用尽之后:TransportDead→ForwardDead 转换(docker 恢复)
                    // 时一级 restart 可用且对症,不能因为隧道试过就直接缴械。
                    if self.tunneled {
                        self.gave_up = true;
                        return Action::None;
                    }
                    self.tunneled = true;
                    self.last_heal_ms = Some(now_ms);
                    return Action::Tunnel;
                }
                self.heal_times_ms.push(now_ms);
                self.last_heal_ms = Some(now_ms);
                Action::Restart
            }
            GatewayHealth::TransportDead => {
                // 一级 restart 需要 docker.sock(此态下正是死的那条),直上二级隧道。
                self.fail_streak += 1;
                if self.gave_up {
                    return Action::None;
                }
                if self.fail_streak < FAIL_STREAK_BEFORE_HEAL {
                    return Action::None; // 防抖:再观察一拍
                }
                if self.tunneled {
                    // 隧道已试过:给它一个冷却窗稳定;过窗口仍不通 → 放弃自动、亮横幅。
                    if let Some(t) = self.last_heal_ms {
                        if now_ms.saturating_sub(t) < HEAL_COOLDOWN_MS {
                            return Action::None;
                        }
                    }
                    self.gave_up = true;
                    return Action::None;
                }
                // 首次隧道不受冷却限制(冷却是给 restart 防连击的;隧道不断连接、
                // 与刚做过的 Restart 是不同手段,共享 last_heal_ms 不该白压它 90s)。
                self.tunneled = true;
                self.last_heal_ms = Some(now_ms);
                Action::Tunnel
            }
            GatewayHealth::TransportDegraded => {
                // 降级稳态:分流口活着(通常靠备援隧道)= 梯子目的已达,清 gave_up
                // (否则 gave_up 只有 Healthy 能清、mux 死时永远到不了 → 永久错误红横幅)。
                // 防重试由 tunneled 独自承担:口再死(隧道自杀)走 TransportDead 的
                // tunneled→放弃路径,不会无限拉隧道。
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

    /// healing = 正在尝试自愈(forward_dead / transport_dead 且未放弃)。
    pub fn healing(&self, health: GatewayHealth) -> bool {
        matches!(health, GatewayHealth::ForwardDead | GatewayHealth::TransportDead) && !self.gave_up
    }
}

// ── 检测(I/O)─────────────────────────────────────────────────────────────

/// 宿主侧探分流口 TCP 可达性(命门 #4:127.0.0.1)。这是用户 Clash 真正拨的口。
pub async fn proxy_port_reachable(host_port: &str) -> bool {
    let port: u16 = match host_port.parse() {
        Ok(p) if p != 0 => p,
        _ => return false,
    };
    let addr = format!("127.0.0.1:{port}");
    matches!(
        tokio::time::timeout(Duration::from_millis(500), tokio::net::TcpStream::connect(&addr)).await,
        Ok(Ok(_))
    )
}

/// 备援隧道要转发的宿主口:分流口 + 控制口(都经 lima 转发,死法相同、一起救)。
pub fn tunnel_ports(cfg: &crate::config::Config) -> Vec<u16> {
    let mut v = Vec::new();
    if let Ok(p) = cfg.mihomo_host_port.parse::<u16>() {
        if p != 0 {
            v.push(p);
        }
    }
    if let Some(p) = cfg.mihomo_ctrl_port.as_ref().and_then(|s| s.parse::<u16>().ok()) {
        if p != 0 {
            v.push(p);
        }
    }
    v
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
        return if proxy_port_reachable(&state.cfg.mihomo_host_port).await {
            GatewayHealth::TransportDegraded
        } else {
            GatewayHealth::TransportDead
        };
    };
    if !docker::is_running(&docker, infra::MIHOMO_CONTAINER).await {
        return GatewayHealth::ContainerDown;
    }
    if proxy_port_reachable(&state.cfg.mihomo_host_port).await {
        GatewayHealth::Healthy
    } else {
        GatewayHealth::ForwardDead
    }
}

/// 传输层自愈(看门狗二级 / 手动修复共用):备援隧道转发分流口 + 控制口 + 各通道 noVNC 口,
/// 再把 docker.sock 也经隧道救回、换上新连接 → 下一拍 ping 通,状态收敛回 Healthy。
/// 端口隧道 spawn 失败但分流口实际可达(旧隧道占口)时不算失败,继续救 docker。
pub async fn heal_transport(state: &AppState) -> anyhow::Result<()> {
    let mut ports = tunnel_ports(&state.cfg);
    if let Ok(chs) = crate::store::list_channels(&state.cfg.db_path()) {
        ports.extend(extra_tunnel_ports(&chs));
    }
    ports.sort_unstable();
    ports.dedup();
    if let Err(e) = crate::vm::spawn_port_tunnel(crate::vm::PROFILE, &ports).await {
        if !proxy_port_reachable(&state.cfg.mihomo_host_port).await {
            return Err(e);
        }
        eprintln!("[watchdog] 端口隧道未新建(可能旧隧道已占口):{e};继续救 docker.sock");
    }
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

/// 纯:通道 noVNC 宿主口(hostagent 死时 lima 不再转发,登录窗打不开;隧道一并救)。
pub fn extra_tunnel_ports(chs: &[crate::store::ChannelPublic]) -> Vec<u16> {
    chs.iter()
        .filter_map(|c| c.novnc_port)
        .filter_map(|p| u16::try_from(p).ok())
        .filter(|&p| p != 0)
        .collect()
}

// ── 看门狗循环 ───────────────────────────────────────────────────────────

const TICK_SECS: u64 = 20;

/// 后台看门狗:定时探分流口,forward_dead 自动重启 mihomo 重建 lima 转发,状态写入快照。
/// 在 `app::serve` 起头 spawn(bin 与 Tauri 壳共用,单处接入)。docker 不可用时只如实报 vm_down、不自愈。
pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        let mut wd = Watchdog::default();
        let started = std::time::Instant::now();
        let mut tick = tokio::time::interval(Duration::from_secs(TICK_SECS));
        loop {
            tick.tick().await;
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
                Action::Restart => {
                    if let Some(d) = state.docker().as_ref() {
                        eprintln!("[watchdog] 分流口不可达,自动 restart {} 重建转发", infra::MIHOMO_CONTAINER);
                        if let Err(e) = docker::restart(d, infra::MIHOMO_CONTAINER).await {
                            eprintln!("[watchdog] restart {} 失败: {e}", infra::MIHOMO_CONTAINER);
                        }
                    }
                }
                Action::Tunnel => {
                    eprintln!("[watchdog] restart 无效或不可用(hostagent/传输层疑似僵死),拉备援 SSH 隧道");
                    match heal_transport(&state).await {
                        Ok(()) => {
                            if let Ok(mut snap) = state.health.lock() {
                                snap.tunnel_fallback = true;
                            }
                        }
                        Err(e) => eprintln!("[watchdog] 备援隧道失败: {e}"),
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
        // 先制造一次放弃(且已升级过隧道)
        let mut wd = Watchdog {
            gave_up: true,
            tunneled: true,
            fail_streak: 5,
            heal_times_ms: vec![1, 2, 3],
            ..Default::default()
        };
        assert_eq!(wd.decide(GatewayHealth::Healthy, 100), Action::None);
        assert!(!wd.gave_up());
        assert!(!wd.tunneled, "恢复后梯子复位,下次事故从一级重新开始");
        assert_eq!(wd.fail_streak, 0);
        assert!(wd.heal_times_ms.is_empty());
    }

    #[test]
    fn debounces_then_heals() {
        let mut wd = Watchdog::default();
        // 第 1 拍:仅 streak=1,不动(防瞬时)
        assert_eq!(wd.decide(GatewayHealth::ForwardDead, 0), Action::None);
        // 第 2 拍:确认 → 一级自愈
        assert_eq!(wd.decide(GatewayHealth::ForwardDead, 20_000), Action::Restart);
    }

    #[test]
    fn cooldown_blocks_back_to_back_heals() {
        let mut wd = Watchdog::default();
        wd.decide(GatewayHealth::ForwardDead, 0);
        assert_eq!(wd.decide(GatewayHealth::ForwardDead, 20_000), Action::Restart);
        // 冷却内(<90s)再 forward_dead 不重启
        assert_eq!(wd.decide(GatewayHealth::ForwardDead, 40_000), Action::None);
        assert_eq!(wd.decide(GatewayHealth::ForwardDead, 80_000), Action::None);
        // 过冷却 → 再自愈
        assert_eq!(wd.decide(GatewayHealth::ForwardDead, 120_000), Action::Restart);
    }

    #[test]
    fn escalates_restart_to_tunnel_then_gives_up() {
        // 持续 forward_dead、每 20s 一拍:防抖后窗口内 Restart 3 次(各隔冷却),
        // 仍坏 → 升级 Tunnel 一次,再坏 → 放弃。一拍 = 一次 decide。
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
        assert_eq!(restarts, FLAP_GIVEUP_COUNT, "一级 restart 窗口内最多试 3 次");
        assert_eq!(tunnels, 1, "二级隧道只试一次");
        assert!(wd.gave_up());
        // 放弃后即便再 forward_dead 也不动
        assert_eq!(wd.decide(GatewayHealth::ForwardDead, 9_000_000), Action::None);
    }

    #[test]
    fn tunnel_ports_reads_both_ports() {
        let m: std::collections::HashMap<&str, &str> =
            [("MIHOMO_HOST_PORT", "37473"), ("MIHOMO_CTRL_PORT", "48020")].into_iter().collect();
        let cfg = crate::config::Config::from_getter(|k| m.get(k).map(|s| s.to_string()));
        assert_eq!(tunnel_ports(&cfg), vec![37473, 48020]);
        // 缺省(未配置)→ 空,不会去转 0 号口
        let empty = crate::config::Config::from_getter(|_| None);
        assert!(tunnel_ports(&empty).is_empty());
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
    fn transport_dead_debounces_then_tunnels_directly() {
        // 盲区 #3:传输层死时一级 restart 不可用,防抖后直上二级隧道。
        let mut wd = Watchdog::default();
        assert_eq!(wd.decide(GatewayHealth::TransportDead, 0), Action::None, "防抖第一拍不动");
        assert_eq!(wd.decide(GatewayHealth::TransportDead, 20_000), Action::Tunnel, "确认后直上二级");
        // 冷却内不重复
        assert_eq!(wd.decide(GatewayHealth::TransportDead, 40_000), Action::None);
        assert!(!wd.gave_up(), "冷却内不算失败");
        // 过冷却仍 transport_dead(隧道没救回)→ 放弃
        assert_eq!(wd.decide(GatewayHealth::TransportDead, 200_000), Action::None);
        assert!(wd.gave_up(), "隧道只试一次,再坏即放弃");
    }

    #[test]
    fn transport_degraded_is_stable_and_keeps_ladder_state() {
        // 隧道救活分流口后 docker 仍不可达 → 降级稳态:不折腾,但梯子状态保留,
        // 口再死(隧道自杀)时不会无限重试隧道。
        let mut wd = Watchdog::default();
        wd.decide(GatewayHealth::TransportDead, 0);
        assert_eq!(wd.decide(GatewayHealth::TransportDead, 20_000), Action::Tunnel);
        // 隧道生效 → 降级稳态,持续 None 且不放弃
        assert_eq!(wd.decide(GatewayHealth::TransportDegraded, 40_000), Action::None);
        assert_eq!(wd.decide(GatewayHealth::TransportDegraded, 400_000), Action::None);
        assert!(!wd.gave_up());
        // 隧道死了口又不通 → 已 tunneled,防抖+冷却后放弃(不无限拉隧道)
        assert_eq!(wd.decide(GatewayHealth::TransportDead, 420_000), Action::None, "防抖");
        assert_eq!(wd.decide(GatewayHealth::TransportDead, 440_000), Action::None);
        assert!(wd.gave_up());
        // 彻底恢复(boot 自愈/手动)→ 梯子复位
        wd.decide(GatewayHealth::Healthy, 500_000);
        assert!(!wd.gave_up());
    }

    #[test]
    fn transport_degraded_clears_gave_up() {
        // 评审确认项#1:gave_up 只有 Healthy 能清、mux 死时永远到不了 Healthy →
        // 手动修复成功后(下一拍落 Degraded)必须在 Degraded 里清 gave_up,
        // 否则前端 gave_up 优先渲染,降级稳态挂着「通道都打不开」的永久错误红横幅。
        let mut wd = Watchdog::default();
        wd.decide(GatewayHealth::TransportDead, 0);
        assert_eq!(wd.decide(GatewayHealth::TransportDead, 20_000), Action::Tunnel);
        wd.decide(GatewayHealth::TransportDead, 200_000); // 过冷却仍死 → 放弃
        assert!(wd.gave_up());
        // 手动修复把口救活 → 下一拍 Degraded:清 gave_up,横幅回到正确的降级文案
        assert_eq!(wd.decide(GatewayHealth::TransportDegraded, 220_000), Action::None);
        assert!(!wd.gave_up());
    }

    #[test]
    fn forward_dead_after_transport_tunnel_still_restarts() {
        // 评审确认项#2:传输层阶段消耗过隧道额度后 docker 恢复(TransportDead→ForwardDead),
        // 一级 restart 此刻可用且对症,不能因 tunneled 直接缴械。
        let mut wd = Watchdog::default();
        wd.decide(GatewayHealth::TransportDead, 0);
        assert_eq!(wd.decide(GatewayHealth::TransportDead, 20_000), Action::Tunnel);
        // docker 恢复但口仍死 → ForwardDead;冷却内先等
        assert_eq!(wd.decide(GatewayHealth::ForwardDead, 40_000), Action::None);
        // 过冷却 → 必须给一级 Restart 机会(而非 gave_up)
        assert_eq!(wd.decide(GatewayHealth::ForwardDead, 120_000), Action::Restart);
        assert!(!wd.gave_up());
    }

    #[test]
    fn transport_dead_tunnel_not_blocked_by_restart_cooldown() {
        // 评审确认项#4:刚做过一级 Restart(共享 last_heal_ms)就转 TransportDead 时,
        // 首次隧道不该被 90s 冷却白压——隧道不断连接,冷却是给 restart 防连击的。
        let mut wd = Watchdog::default();
        wd.decide(GatewayHealth::ForwardDead, 0);
        assert_eq!(wd.decide(GatewayHealth::ForwardDead, 20_000), Action::Restart);
        // 20s 后 mux 死 → TransportDead:冷却未过,但隧道立即发
        assert_eq!(wd.decide(GatewayHealth::TransportDead, 40_000), Action::Tunnel);
    }

    #[test]
    fn extra_tunnel_ports_filters_invalid() {
        fn ch(novnc: Option<i64>) -> crate::store::ChannelPublic {
            crate::store::ChannelPublic {
                id: "x".into(), name: "n".into(), vpn_type: "wireguard".into(),
                server: "s".into(), ec_ver: None, login_method: "headless".into(),
                username: "u".into(), vnc_password: None, mac: None, novnc_port: novnc,
                probe_url: "".into(), status: "running".into(), container_id: None,
                latency_ms: None, config: serde_json::json!({}),
            }
        }
        let chs = vec![ch(Some(60283)), ch(None), ch(Some(0)), ch(Some(70000))];
        // 无口 / 0 号口 / 超 u16 的都过滤;只留合法 noVNC 宿主口
        assert_eq!(extra_tunnel_ports(&chs), vec![60283]);
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

    #[tokio::test]
    async fn unreachable_port_is_false() {
        // 0 / 空 → false;占不到的端口 → false(无监听,connect 拒绝)
        assert!(!proxy_port_reachable("0").await);
        assert!(!proxy_port_reachable("").await);
        assert!(!proxy_port_reachable("1").await); // 1 号端口几乎不可能有监听
    }

    #[tokio::test]
    async fn reachable_port_is_true() {
        // 起一个临时监听,确认能探到
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        assert!(proxy_port_reachable(&port.to_string()).await);
    }
}
