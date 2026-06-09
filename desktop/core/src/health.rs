//! 分流口健康看门狗。命门 #1 不破:登录判据仍是 SOCKS5 探活(`manager::probe`,经 docker.sock
//! 走 VM 内网),本模块只额外看**宿主→mihomo 分流口**的可达性——那是用户 Clash 真正拨的口,
//! 也是 SOCKS5 探活摸不到的盲区。
//!
//! 背景病:mihomo 分流口经 lima 端口转发暴露到宿主 127.0.0.1;SSH 主连接重建(睡醒/网络抖动)后,
//! 重建前就一直监听的长命端口转发会被 **静默丢失**(lima 不重建、日志只有 Forwarding 无 Stopping),
//! 于是容器全绿、宿主却连不上分流口 → 浏览器 ERR_EMPTY_RESPONSE。详见 auto-memory
//! `desktop-app-colima-ops-gotchas`。
//!
//! 自愈:确认 `forward_dead` 后 `docker restart mihomo`,让分流口"重新出现"→ 逼 lima 在当前活的
//! 主连接上重建转发(端口映射 restart 保留,vpn-router 节点不用改)。带防抖 + 冷却 + 抖动放弃,
//! 避免反复重启断连。状态吐给 /api/system 供前端横幅。

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
    /// docker/VM 整体不可达(不在本模块自愈范围,弹横幅引导重开 app)。
    VmDown,
}

/// 吐给 /api/system 的快照(看门狗每 tick 刷新)。
#[derive(Debug, Clone, Serialize)]
pub struct HealthSnapshot {
    pub gateway_health: GatewayHealth,
    pub proxy_port_reachable: bool,
    /// 正在尝试自愈(已确认 forward_dead、未放弃)。
    pub healing: bool,
    /// 自动自愈已放弃(抖动过频),需手动修复/查诊断。
    pub gave_up: bool,
}

impl Default for HealthSnapshot {
    fn default() -> Self {
        // 首次 tick 前的乐观默认;看门狗启动即跑首检覆盖它。
        Self { gateway_health: GatewayHealth::Healthy, proxy_port_reachable: true, healing: false, gave_up: false }
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
const FLAP_GIVEUP_COUNT: usize = 3; // 窗口内自愈 ≥3 次 → 放弃自动、改横幅

/// 看门狗内部状态。
#[derive(Debug, Default)]
pub struct Watchdog {
    fail_streak: u32,
    last_heal_ms: Option<u64>,
    heal_times_ms: Vec<u64>,
    gave_up: bool,
}

/// 一次决策的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    /// 触发 `docker restart mihomo`。
    Heal,
}

impl Watchdog {
    /// 纯决策:吃当前 health + 单调毫秒时钟,推进内部状态,返回动作。
    pub fn decide(&mut self, health: GatewayHealth, now_ms: u64) -> Action {
        match health {
            GatewayHealth::Healthy => {
                // 恢复:清零,允许将来再自愈。
                self.fail_streak = 0;
                self.gave_up = false;
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
                    self.gave_up = true; // 抖动过频 → 放弃自动
                    return Action::None;
                }
                self.heal_times_ms.push(now_ms);
                self.last_heal_ms = Some(now_ms);
                Action::Heal
            }
            // VM/容器层面的问题不在本模块自愈范围。
            GatewayHealth::ContainerDown | GatewayHealth::VmDown => Action::None,
        }
    }

    pub fn gave_up(&self) -> bool {
        self.gave_up
    }

    /// healing = 正在尝试自愈(forward_dead 且未放弃)。
    pub fn healing(&self, health: GatewayHealth) -> bool {
        health == GatewayHealth::ForwardDead && !self.gave_up
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

/// 综合判网关健康。
pub async fn check(state: &AppState) -> GatewayHealth {
    let docker = match state.docker.as_ref() {
        Some(d) => d,
        None => return GatewayHealth::VmDown,
    };
    if docker::ping(docker).await.is_err() {
        return GatewayHealth::VmDown;
    }
    if !docker::is_running(docker, infra::MIHOMO_CONTAINER).await {
        return GatewayHealth::ContainerDown;
    }
    if proxy_port_reachable(&state.cfg.mihomo_host_port).await {
        GatewayHealth::Healthy
    } else {
        GatewayHealth::ForwardDead
    }
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
                snap.proxy_port_reachable = health == GatewayHealth::Healthy;
                snap.healing = wd.healing(health);
                snap.gave_up = wd.gave_up();
            }
            if action == Action::Heal {
                if let Some(d) = state.docker.as_ref() {
                    eprintln!("[watchdog] 分流口不可达,自动 restart {} 重建转发", infra::MIHOMO_CONTAINER);
                    if let Err(e) = docker::restart(d, infra::MIHOMO_CONTAINER).await {
                        eprintln!("[watchdog] restart {} 失败: {e}", infra::MIHOMO_CONTAINER);
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn healthy_resets_and_clears_giveup() {
        let mut wd = Watchdog::default();
        // 先制造一次放弃
        wd.gave_up = true;
        wd.fail_streak = 5;
        wd.heal_times_ms = vec![1, 2, 3];
        assert_eq!(wd.decide(GatewayHealth::Healthy, 100), Action::None);
        assert!(!wd.gave_up());
        assert_eq!(wd.fail_streak, 0);
        assert!(wd.heal_times_ms.is_empty());
    }

    #[test]
    fn debounces_then_heals() {
        let mut wd = Watchdog::default();
        // 第 1 拍:仅 streak=1,不动(防瞬时)
        assert_eq!(wd.decide(GatewayHealth::ForwardDead, 0), Action::None);
        // 第 2 拍:确认 → 自愈
        assert_eq!(wd.decide(GatewayHealth::ForwardDead, 20_000), Action::Heal);
    }

    #[test]
    fn cooldown_blocks_back_to_back_heals() {
        let mut wd = Watchdog::default();
        wd.decide(GatewayHealth::ForwardDead, 0);
        assert_eq!(wd.decide(GatewayHealth::ForwardDead, 20_000), Action::Heal);
        // 冷却内(<90s)再 forward_dead 不重启
        assert_eq!(wd.decide(GatewayHealth::ForwardDead, 40_000), Action::None);
        assert_eq!(wd.decide(GatewayHealth::ForwardDead, 80_000), Action::None);
        // 过冷却 → 再自愈
        assert_eq!(wd.decide(GatewayHealth::ForwardDead, 120_000), Action::Heal);
    }

    #[test]
    fn gives_up_after_flapping() {
        // 每 20s 一拍、持续 forward_dead:防抖后首次自愈,之后每过冷却(90s)再自愈一次,
        // 同一 15min 窗口内累计 FLAP_GIVEUP_COUNT 次即放弃。一拍 = 一次 decide。
        let mut wd = Watchdog::default();
        let mut heals = 0;
        for i in 0..200u64 {
            if wd.decide(GatewayHealth::ForwardDead, i * 20_000) == Action::Heal {
                heals += 1;
            }
            if wd.gave_up() {
                break;
            }
        }
        assert_eq!(heals, FLAP_GIVEUP_COUNT, "窗口内最多自愈 3 次后放弃");
        assert!(wd.gave_up());
        // 放弃后即便再 forward_dead 也不动
        assert_eq!(wd.decide(GatewayHealth::ForwardDead, 5_000_000), Action::None);
    }

    #[test]
    fn vm_and_container_down_never_heal() {
        let mut wd = Watchdog::default();
        assert_eq!(wd.decide(GatewayHealth::VmDown, 0), Action::None);
        assert_eq!(wd.decide(GatewayHealth::VmDown, 20_000), Action::None);
        assert_eq!(wd.decide(GatewayHealth::ContainerDown, 40_000), Action::None);
        assert!(!wd.gave_up());
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
