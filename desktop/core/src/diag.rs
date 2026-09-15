//! 通道诊断(只读):容器级重启循环 + 隧道级探针,驱动「分流总表」穿透图。
//! 桌面版 host-only 路由(web 版无此端点,前端 feature-detect 降级)。
//!
//! ⚠️ 命门 #1 不动摇:这里的信号全是**补充诊断**(死循环 / 踢线 / 握手中断的"为什么"),
//! 登录成功与否的唯一判据仍是 SOCKS5 探活(/status)。实测 EC 在登录页时 svpnservice
//! 也活着、tun0 也有 IP——所以 EC 不出 tunnel_up,只出 ec_client_alive + ec_kick_age。

use axum::{extract::State, response::IntoResponse, Json};
use bollard::Docker;
use chrono::{DateTime, NaiveDateTime, Utc};
use serde_json::{json, Value};

use crate::AppState;

const EC_LOG: &str = "/usr/share/sangfor/EasyConnect/resources/logs/EasyConnect.log";

/// 死循环判定:docker 正在 restarting,或(运行中且)重启次数 ≥3、本次存活 <10 分钟。
/// (unless-stopped 下崩溃即拉起;长存活后的偶发重启不算循环;手动 stop 后残留的
/// 高 restart_count 也不算——循环已被停止终结。)
pub fn is_crash_loop(state: &str, restart_count: i64, uptime_secs: Option<i64>) -> bool {
    state == "restarting"
        || (state == "running" && restart_count >= 3 && uptime_secs.is_none_or(|s| s < 600))
}

/// 解析 `wg show <if> latest-handshakes`(每 peer 一行 `<pubkey>\t<epoch>`)→ 最近一次握手距今秒数。
/// epoch 0 = 从未握手 → None。
pub fn parse_wg_handshake_age(out: &str, now_epoch: i64) -> Option<i64> {
    let latest = out
        .lines()
        .filter_map(|l| l.split_whitespace().last()?.parse::<i64>().ok())
        .filter(|e| *e > 0)
        .max()?;
    Some((now_epoch - latest).max(0))
}

/// 解析 EasyConnect.log 踢线行的时间戳(容器时区 UTC)→ 距今秒数。
/// 行形如 `[2026-07-08 06:54:43] [INFO] [...]-The user logged out, logged-out code: 0`
/// 或 `[...]-querryLogout: The user logged out!`(两种都以 "The user logged out" 匹配)。
pub fn parse_ec_kick_age(line: &str, now: DateTime<Utc>) -> Option<i64> {
    let start = line.find('[')? + 1;
    let end = line[start..].find(']')? + start;
    let ts = NaiveDateTime::parse_from_str(&line[start..end], "%Y-%m-%d %H:%M:%S").ok()?;
    Some((now - ts.and_utc()).num_seconds().max(0))
}

/// 隧道接口探针:容器内 eth0/lo 之外第一个带 IPv4 的接口名(wg0/ppp0/tun0…)。
/// 适用 oss 家族(接口 = 隧道已建立);EC 例外(登录前 tun0 就有 IP),勿用。
async fn tunnel_iface(docker: &Docker, name: &str) -> Option<String> {
    let out = crate::docker::exec_capture(
        docker,
        name,
        vec!["sh", "-c", "ip -o -4 addr show 2>/dev/null | awk '{print $2}' | grep -vx -e eth0 -e lo | head -1"],
    )
    .await
    .ok()?;
    let s = out.trim();
    if s.is_empty() { None } else { Some(s.to_string()) }
}

async fn wg_handshake_age(docker: &Docker, name: &str) -> Option<i64> {
    let out = crate::docker::exec_capture(
        docker,
        name,
        vec!["sh", "-c", "wg show wg0 latest-handshakes 2>/dev/null"],
    )
    .await
    .ok()?;
    parse_wg_handshake_age(&out, Utc::now().timestamp())
}

/// EC 客户端栈是否活着(镜像无 pgrep/procps,扫 /proc/*/comm)。
async fn ec_client_alive(docker: &Docker, name: &str) -> Option<bool> {
    let out = crate::docker::exec_capture(
        docker,
        name,
        vec!["sh", "-c", "grep -l '^svpnservice$' /proc/[0-9]*/comm 2>/dev/null | head -1"],
    )
    .await
    .ok()?;
    Some(!out.trim().is_empty())
}

async fn ec_kick_age(docker: &Docker, name: &str) -> Option<i64> {
    let cmd = format!("tail -c 131072 {EC_LOG} 2>/dev/null | grep -a 'The user logged out' | tail -1");
    let out = crate::docker::exec_capture(docker, name, vec!["sh", "-c", &cmd]).await.ok()?;
    let line = out.trim();
    if line.is_empty() {
        return None;
    }
    parse_ec_kick_age(line, Utc::now())
}

/// 单通道诊断。容器缺失 → `{state:"missing"}`;探针失败一律 null(不猜)。
pub async fn diag_one(docker: &Docker, ch: &crate::store::ChannelPublic) -> Value {
    let name = format!("vpn-{}", ch.id);
    let info = match docker.inspect_container(&name, None).await {
        Ok(i) => i,
        Err(_) => return json!({ "id": ch.id, "state": "missing" }),
    };
    let restart_count = info.restart_count.unwrap_or(0);
    let state_obj = info.state.as_ref();
    let state = state_obj
        .and_then(|s| s.status.as_ref())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "unknown".into());
    let running = state_obj.and_then(|s| s.running).unwrap_or(false);
    let uptime_secs = state_obj
        .and_then(|s| s.started_at.clone())
        .and_then(|t| DateTime::parse_from_rfc3339(&t).ok())
        .map(|t| (Utc::now() - t.with_timezone(&Utc)).num_seconds().max(0))
        .filter(|_| running);
    let exit_code = state_obj.and_then(|s| s.exit_code).filter(|_| !running);
    let crash_loop = is_crash_loop(&state, restart_count, uptime_secs);

    let mut v = json!({
        "id": ch.id,
        "state": state,
        "restart_count": restart_count,
        "uptime_secs": uptime_secs,
        "exit_code": exit_code,
        "crash_loop": crash_loop,
        "tunnel_up": Value::Null,
        "tunnel_iface": Value::Null,
        "wg_handshake_age": Value::Null,
        "ec_client_alive": Value::Null,
        "ec_kick_age": Value::Null,
    });
    if !running {
        return v;
    }
    // safe: json! 宏上面构造的一定是 object
    let o = v.as_object_mut().unwrap();
    let runtime = crate::registry::get(&ch.vpn_type).map(|s| s.runtime).unwrap_or_default();
    match ch.vpn_type.as_str() {
        "easyconnect" => {
            o.insert("ec_client_alive".into(), json!(ec_client_alive(docker, &name).await));
            o.insert("ec_kick_age".into(), json!(ec_kick_age(docker, &name).await));
        }
        "wireguard" => {
            let iface = tunnel_iface(docker, &name).await;
            o.insert("tunnel_up".into(), json!(iface.is_some()));
            o.insert("tunnel_iface".into(), json!(iface));
            o.insert("wg_handshake_age".into(), json!(wg_handshake_age(docker, &name).await));
        }
        // 其余 oss 无头家族:隧道接口存在 = 隧道已建立(entrypoint 等它起来才 exec danted)
        _ if runtime == "oss" => {
            let iface = tunnel_iface(docker, &name).await;
            o.insert("tunnel_up".into(), json!(iface.is_some()));
            o.insert("tunnel_iface".into(), json!(iface));
        }
        // atrust / byo / 未知:无已验证的隧道级探针,只出容器级信号(诚实 null)
        _ => {}
    }
    v
}

/// GET /api/diag —— 全通道并发诊断,单通道 6s 超时(exec 卡死不拖全表)。
pub async fn diag(State(st): State<AppState>) -> axum::response::Response {
    let db = st.cfg.db_path();
    // 前端 allSettled 已优雅降级,放心 5xx(而非空表掩盖 db 故障)。
    let chans = match crate::store::list_channels(&db) {
        Ok(c) => c,
        Err(e) => return crate::api::err_detail(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            &format!("list_channels: {e}"),
        ),
    };
    let channels: Vec<Value> = match st.docker() {
        Some(d) => {
            let futs = chans.iter().map(|c| {
                let d = &d;
                async move {
                    match tokio::time::timeout(std::time::Duration::from_secs(6), diag_one(d, c)).await {
                        Ok(v) => v,
                        Err(_) => json!({ "id": c.id, "state": "timeout" }),
                    }
                }
            });
            futures_util::future::join_all(futs).await
        }
        None => chans.iter().map(|c| json!({ "id": c.id, "state": "unknown" })).collect(),
    };
    Json(json!({ "channels": channels })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crash_loop_rules() {
        // 实测样本:173 次重启、单次存活 ~88s → 死循环
        assert!(is_crash_loop("running", 173, Some(88)));
        // docker 报 restarting 直接算
        assert!(is_crash_loop("restarting", 1, None));
        // 长存活的偶发重启不算
        assert!(!is_crash_loop("running", 5, Some(86400)));
        // 少量重启不算
        assert!(!is_crash_loop("running", 2, Some(30)));
        // 稳定运行
        assert!(!is_crash_loop("running", 0, Some(3600)));
        // 手动停止后残留高 restart_count:循环已终结,不算
        assert!(!is_crash_loop("exited", 173, None));
    }

    #[test]
    fn wg_handshake_parse() {
        // 实测输出格式:pubkey\tepoch
        let out = "me1IyrSpwZfCh7Rb0Ehi7k4squwpLtMKPMeD470DByE=\t1783495553\n";
        assert_eq!(parse_wg_handshake_age(out, 1783495560), Some(7));
        // epoch 0 = 从未握手
        assert_eq!(parse_wg_handshake_age("k=\t0\n", 100), None);
        // 多 peer 取最新
        let multi = "a=\t100\nb=\t200\n";
        assert_eq!(parse_wg_handshake_age(multi, 260), Some(60));
        // 垃圾输入
        assert_eq!(parse_wg_handshake_age("", 100), None);
        assert_eq!(parse_wg_handshake_age("error: no such device", 100), None);
        // 时钟偏差不出负数
        assert_eq!(parse_wg_handshake_age("k=\t200\n", 100), Some(0));
    }

    #[test]
    fn ec_kick_parse() {
        let now = DateTime::parse_from_rfc3339("2026-07-08T07:04:43Z").unwrap().with_timezone(&Utc);
        // 实测两种行都以 "The user logged out" 匹配,时间戳格式一致
        let l1 = "[2026-07-08 06:54:43] [INFO] [https_com_controller]-The user logged out, logged-out code: 0";
        assert_eq!(parse_ec_kick_age(l1, now), Some(600));
        let l2 = "[2026-07-08 06:54:43] [INFO] [https_com_controller]-querryLogout: The user logged out!";
        assert_eq!(parse_ec_kick_age(l2, now), Some(600));
        // 垃圾输入
        assert_eq!(parse_ec_kick_age("no timestamp here", now), None);
        assert_eq!(parse_ec_kick_age("[not-a-date] x", now), None);
    }
}
