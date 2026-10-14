//! 接管方式(方案 5.8)。界面给的是两个独立开关:虚拟网卡、系统代理,四种组合各对应一个方式——
//! `clash`(都不开)、`tun`(只开虚拟网卡)、`system_proxy`(只开系统代理)、`tun_system_proxy`(都开);
//! 开关是什么就是什么,不看 Clash 在不在跑。`auto` 是早先的方式,界面不再给,存着的照旧按它执行。设置存在 `<data_dir>/takeover.json`
//! (0600,原子写);默认值通道管家是 `auto`,旧身份是 `manual`(后台不做任何事,旧应用的行为不变)。
//! 后台循环每 20 秒一次,只在通道管家身份、允许宿主集成、运行环境在服务这三个条件都满足时执行;
//! 开发版、隔离实例、旧身份不起循环,GET 也不做任何检测。对方接管连续确认后只关闭自己的入口。
//! 只和上一次自己应用的计划比(边沿触发):计划变了才开关虚拟网卡或系统代理;实时状态和计划不一致时
//! 不重写,用户在系统设置里改成别的地址后不会被写回去。要打开系统代理时,当前服务的自动代理已启用、
//! 地址不是我们的,就不动它,只在接口里报告。后台永远不装助手、不弹管理员密码框;自动模式下系统代理要
//! 用户确认一次(`system_proxy_ok`)才开。「Clash 在跑」只探 `127.0.0.1:9090/version`
//! ([`crate::entry::detect_clash`]):设了密钥、改了端口、只开 unix socket 的会漏报；后台计划连续三次一致才执行。
//! 执行失败的计划不算已应用:同一计划失败后至少隔 5 分钟再试,计划变了或用户 PUT 时立即再试。
//! 接口里的 `effective`、`domains_via` 按检测到的实际状态报,不按计划。
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use crate::api::{err500, err_detail};
use crate::config::Config;
use crate::entry::OtherTakeover;
use crate::identity::Identity;
use crate::AppState;

const EVERY: Duration = Duration::from_secs(20);
/// 同一计划执行失败后,至少隔这么久再试。
const RETRY_AFTER: Duration = Duration::from_secs(5 * 60);
const FILE: &str = "takeover.json";
const OTHER_APP: &str = "旧版应用正在接管网络，请先在旧版里关掉";

/// 上一次自己执行过的计划,只在内存里；循环经连续观测后应用，PUT 立即执行。二者串行。
static LAST: tokio::sync::Mutex<Option<Last>> = tokio::sync::Mutex::const_new(None);
static OBSERVATIONS: std::sync::Mutex<Vec<Observation>> = std::sync::Mutex::new(Vec::new());
static CACHE: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<PathBuf, Cached>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));
static DETECT_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static BACKGROUND: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[derive(Clone)]
struct Cached { detected: Detected, at: i64, refreshed: Instant, tried: Option<Plan> }

fn cache_detection(path: &Path, detected: Detected, tried: Option<Plan>) -> Cached {
    let mut cache = CACHE.lock().unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    let at = cache.get(path).map_or(now, |old| now.max(old.at.saturating_add(1)));
    let value = Cached { detected, at, refreshed: Instant::now(), tried };
    cache.insert(path.into(), value.clone());
    value
}

async fn cached_detection(path: &Path, background: bool,
    detect: impl std::future::Future<Output = Detected>) -> Cached {
    let read = || CACHE.lock().unwrap().get(path)
        .filter(|value| background || value.refreshed.elapsed() < EVERY).cloned();
    if let Some(value) = read() { return value; }
    let _refresh = DETECT_LOCK.lock().await;
    if let Some(value) = read() { return value; }
    cache_detection(path, detect.await, None)
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Observation { settings: Settings, plan: Plan, other: bool }

fn observation(settings: &Settings, d: &Detected) -> Observation {
    let other = d.other_app.is_some();
    let plan = if other {
        Plan { tun: false, system_proxy: false, effective: "none", domains_via: "none", action: Some("close_other_app") }
    } else { decide(settings.mode, d.clash_running, d.tun_installed, d.tun_handles_domains, settings.system_proxy_ok) };
    Observation { settings: *settings, plan, other }
}

fn settle(observations: &[Observation]) -> Option<Plan> {
    let last = observations.last()?;
    let count = if last.other { 2 } else { 3 };
    (observations.len() >= count && observations.iter().rev().take(count).all(|item| item == last)).then_some(last.plan)
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// 早先的默认值,界面不再给:有 Clash 就配合它,没有就虚拟网卡,系统代理要确认一次。
    Auto,
    /// 只开虚拟网卡。
    Tun,
    /// 都不开,全交给 Clash。
    Clash,
    /// 只开系统代理。
    SystemProxy,
    /// 虚拟网卡和系统代理都开。
    TunSystemProxy,
    /// 旧身份的默认值:后台不管接管,沿用手动开关。PUT 不接受。
    Manual,
}

#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub mode: Mode,
    /// 用户确认过可以打开系统代理(选了「系统代理」方式,或点了「用系统代理接管」)。
    pub system_proxy_ok: bool,
}

fn is_tongdao(id: &Identity) -> bool {
    std::ptr::eq(id, &crate::identity::TONGDAO)
}

/// 只在通道管家身份、允许宿主集成(正式档,非开发模式)时接管;其余什么都不做。
fn runs_for(id: &Identity, host_integrations_allowed: bool) -> bool {
    host_integrations_allowed && is_tongdao(id)
}

pub fn allowed(cfg: &Config) -> bool {
    runs_for(cfg.identity(), cfg.host_integrations_allowed())
}

pub fn file_path(data_dir: &Path) -> PathBuf {
    data_dir.join(FILE)
}

/// 读回设置;文件不在、读不懂时用本身份的默认值。
pub fn load(path: &Path, id: &Identity) -> Settings {
    #[derive(Deserialize, Default)]
    struct Stored {
        mode: Option<Mode>,
        #[serde(default)]
        system_proxy_ok: bool,
    }
    let stored: Stored = std::fs::read(path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
    let default_mode = if is_tongdao(id) { Mode::Tun } else { Mode::Manual };
    Settings { mode: stored.mode.unwrap_or(default_mode), system_proxy_ok: stored.system_proxy_ok }
}

pub fn save(path: &Path, settings: &Settings) -> anyhow::Result<()> {
    crate::manager::atomic_write_0600(path, &serde_json::to_vec_pretty(settings)?)
}

/// 纯函数:把 PUT 的字段合并进当前设置。`mode` 不收 `manual`;选了带系统代理的方式就等于确认过系统代理。
pub fn merge(current: &Settings, patch: &Value) -> Result<Settings, String> {
    let patch = patch.as_object().ok_or("请求体须为对象")?;
    if !patch.contains_key("mode") && !patch.contains_key("system_proxy_ok") {
        return Err("需要 mode 或 system_proxy_ok".into());
    }
    let mut next = *current;
    if let Some(value) = patch.get("system_proxy_ok") {
        next.system_proxy_ok = value.as_bool().ok_or("system_proxy_ok 须为布尔值")?;
    }
    if let Some(value) = patch.get("mode") {
        next.mode = match serde_json::from_value(value.clone()) {
            Ok(Mode::Manual) | Err(_) => return Err("mode 须为 tun、clash、system_proxy、tun_system_proxy、auto 之一".into()),
            Ok(mode) => mode,
        };
        if matches!(next.mode, Mode::SystemProxy | Mode::TunSystemProxy) {
            next.system_proxy_ok = true;
        }
    }
    Ok(next)
}

/// [`decide`] 的结果。`effective`:`tun`/`clash`/`system_proxy`/`none`;`domains_via`:`tun`/`system_proxy`/`clash`/`none`;
/// `action`:`install_tun`/`enable_system_proxy`(`close_other_app`、`retry_takeover` 由检测结果另给,见 [`view`])。
/// 这些都是计划值,接口按实际状态报的见 [`view`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    pub tun: bool,
    pub system_proxy: bool,
    pub effective: &'static str,
    pub domains_via: &'static str,
    pub action: Option<&'static str>,
}

/// 上一次自己执行过的计划。执行失败的计划不算已应用(见 [`loop_round`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Last {
    pub plan: Plan,
    /// 执行失败的时间;成功时为 None。
    pub failed_at: Option<Instant>,
}

/// 纯函数(T-01):按方式和检测结果定虚拟网卡、系统代理开不开。`tun`、`clash`、`system_proxy`、
/// `tun_system_proxy` 四种就是两个开关的四种组合,照开关执行,不看 Clash 在不在跑;要开虚拟网卡而助手没装时
/// 不开,报 `install_tun`。虚拟网卡只给规则里的地址加路由,别的流量不动,所以和 Clash 能同时用;系统代理写的是
/// 自动代理,优先于 Clash 设的手动代理,PAC 没命中的又是直连,Clash 在跑时开它会把 Clash 架空——这由界面提醒,
/// 这里不拦。`tun_handles_domains` 在 5.13 落地前恒为假,这时虚拟网卡只管地址:没开系统代理时域名规则只能靠
/// Clash(`domains_via` 在 Clash 在跑时报 `clash`,否则 `none`)。
/// `auto`(界面不再给):Clash 在跑时不开系统代理、助手装了就开虚拟网卡;没有 Clash 时开虚拟网卡,系统代理要
/// 确认过(`system_proxy_ok`)才开。
pub fn decide(mode: Mode, clash_running: bool, tun_installed: bool, tun_handles_domains: bool, system_proxy_ok: bool) -> Plan {
    let wanted = |proxy: bool| (tun_installed, proxy, (!tun_installed).then_some("install_tun"));
    let (tun, system_proxy, action) = match mode {
        Mode::Auto if clash_running => (tun_installed, false, None),
        Mode::Auto if tun_installed => {
            let needs_proxy = !tun_handles_domains;
            (true, needs_proxy && system_proxy_ok, (needs_proxy && !system_proxy_ok).then_some("enable_system_proxy"))
        }
        Mode::Auto => (false, system_proxy_ok, Some("install_tun")),
        Mode::Tun => wanted(false),
        Mode::TunSystemProxy => wanted(true),
        Mode::SystemProxy => (false, true, None),
        Mode::Clash | Mode::Manual => (false, false, None),
    };
    let clash = mode == Mode::Clash || (mode == Mode::Auto && clash_running);
    let effective = if clash { "clash" } else if tun { "tun" } else if system_proxy { "system_proxy" } else { "none" };
    let domains_via = if clash {
        "clash"
    } else if tun && tun_handles_domains {
        "tun"
    } else if mode == Mode::Tun {
        if clash_running { "clash" } else { "none" }
    } else {
        "system_proxy"
    };
    Plan { tun, system_proxy, effective, domains_via, action }
}

/// 当前服务的自动代理,相对自己而言。
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Proxy {
    /// 没开,或开着但没有地址。
    #[default]
    Off,
    /// 指向自己当前的 PAC。
    Current,
    /// 指向自己端口范围内的另一个端口(异常退出后残留的自己的旧 PAC)。
    Stale,
    /// 指向别的地址:用户自己设的,或另一个应用的。
    Foreign,
}

pub fn classify_proxy(enabled: bool, url: Option<&str>, current_pac: &str, own_range: &RangeInclusive<u16>) -> Proxy {
    match url {
        Some(url) if enabled && url == current_pac => Proxy::Current,
        Some(url) if enabled && crate::entry::is_own_pac(url, own_range) => Proxy::Stale,
        Some(_) if enabled => Proxy::Foreign,
        _ => Proxy::Off,
    }
}

/// 一次检测读到的现状。不允许接管时取默认值,不执行任何检测。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Detected {
    pub other_app: Option<OtherTakeover>,
    pub clash_running: bool,
    pub tun_installed: bool,
    /// 启用标记(`tun-entry.json`)在。关虚拟网卡按它判断:标记在就关,否则 `tun_sync`、空闲恢复会按标记把网卡再开起来。
    pub tun_flag: bool,
    /// 虚拟网卡真的开着:启用标记在,且自己的助手回报引擎在跑(见 [`tun_running`])。开虚拟网卡和接口上报按它。
    pub tun_enabled: bool,
    pub tun_handles_domains: bool,
    pub proxy: Proxy,
}

/// 纯函数:虚拟网卡是不是真的开着——启用标记在,且助手回报引擎要跑、进程也活着。助手连不上当作没开。
pub fn tun_running(flag: bool, helper_status: Option<&Value>) -> bool {
    flag && helper_status.is_some_and(|status| status["running"] == true && status["alive"] == true)
}

/// Capability/version alone is insufficient: compare against today's effective
/// rules so an acknowledged older generation cannot turn off domain proxying.
pub fn tun_domains_confirmed(flag: bool, helper: Option<&Value>, rules: &[crate::store::Rule]) -> bool {
    let Some(status) = helper.filter(|s| tun_running(flag, Some(s))) else { return false };
    let expected = crate::entry::tun_domains(rules);
    let generation = status["generation"].as_u64();
    expected.skipped.is_empty() && !expected.domains.is_empty()
        && status["version"] == crate::entry::HELPER_VERSION
        && status["dns_capable"] == true && status["domains_ready"] == true
        && status["pending"] == false && status["resolver_taken"] == 0
        && status["resolver_error"].is_null()
        && generation.is_some_and(|n| n > 0 && status["applied_generation"].as_u64() == Some(n))
        && status["resolver_files"].as_u64() == Some(expected.domains.len() as u64)
        && status["domains"] == json!(expected.domains)
}

#[cfg(test)]
#[test]
fn domain_confirmation_rejects_partial_stale_conflicting_or_old_helper_state() {
    let mut rules = vec![crate::store::Rule { id: 1, channel_id:"a".into(), kind:"domain".into(),
        pattern:"corp.test".into(), enabled:1, note:String::new(), locked:0 }];
    let good = json!({"running":true,"alive":true,"version":crate::entry::HELPER_VERSION,
        "dns_capable":true,"domains_ready":true,"pending":false,"resolver_taken":0,
        "resolver_error":null,"generation":3,"applied_generation":3,"resolver_files":1,"domains":["corp.test"]});
    assert!(tun_domains_confirmed(true, Some(&good), &rules));
    assert!(!tun_domains_confirmed(false, Some(&good), &rules));
    for (field, value) in [("version",json!("0.4.0")),("dns_capable",json!(false)),
        ("domains_ready",json!(false)),("pending",json!(true)),("resolver_taken",json!(1)),
        ("resolver_error",json!("failure")),("applied_generation",json!(2)),("resolver_files",json!(0)),
        ("domains",json!(["old.test"])),("alive",json!(false)),("generation",Value::Null)] {
        let mut bad = good.clone(); bad[field] = value;
        assert!(!tun_domains_confirmed(true, Some(&bad), &rules), "{field}");
    }
    rules[0].pattern = "new.test".into(); assert!(!tun_domains_confirmed(true, Some(&good), &rules));
    rules[0].pattern = "host.local".into(); assert!(!tun_domains_confirmed(true, Some(&good), &rules));
}

/// 读现状(只读):对方在不在接管、Clash 在不在跑、助手装没装、虚拟网卡是不是真的开着、当前服务的自动代理。
pub async fn detect(cfg: &Config) -> Detected {
    let id = cfg.identity();
    let ui_port = cfg.ui_port.to_string();
    let proxy = crate::entry::system_proxy_status(&ui_port).await;
    let tun_flag = crate::entry::tun_enabled(&cfg.data_dir);
    // 启用标记在时才问一次自己的助手(只连本身份的套接字)。
    let helper = if tun_flag {
        crate::entry::helper_call(id.helper_sock, json!({"cmd": "status"})).await.ok()
    } else {
        None
    };
    let tun_handles_domains = !id.tun_config_frozen && helper.is_some()
        && crate::store::effective_rules(&cfg.db_path()).is_ok_and(|rules|
            tun_domains_confirmed(tun_flag, helper.as_ref(), &rules));
    Detected {
        other_app: crate::entry::other_app_takeover(cfg).await,
        clash_running: crate::entry::detect_clash().await.running,
        tun_installed: Path::new(id.helper_plist).exists(),
        tun_flag,
        tun_enabled: tun_running(tun_flag, helper.as_ref()),
        tun_handles_domains,
        proxy: classify_proxy(proxy.enabled, proxy.url.as_deref(), &crate::entry::pac_url(&ui_port), &id.port_range),
    }
}

/// 一轮要执行的开关:`Some(true)` 打开、`Some(false)` 关闭、`None` 不动。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Steps {
    pub tun: Option<bool>,
    pub system_proxy: Option<bool>,
}

/// 纯函数:这一轮要不要动作。`None` = 整轮不动(`manual`、对方在接管),上一次的计划保持不变;
/// 否则返回新计划(调用方记为上一次)和要执行的开关。只和上一次自己应用的计划比:计划没变就什么都不做,
/// 哪怕实时状态已经被用户改掉;计划变了的那一项,实时状态已经一致就不执行。要打开系统代理时,当前服务的
/// 自动代理已启用、地址不是我们的,就不动它。`last` 为 None(进程刚启动、刚改过设置)时按实时状态应用一次。
/// 虚拟网卡要开时看它是不是真的开着;要关时看启用标记,标记在就关(引擎没在跑也关,清掉标记)。
pub fn round(settings: &Settings, d: &Detected, last: Option<&Plan>) -> Option<(Plan, Steps)> {
    if settings.mode == Mode::Manual || d.other_app.is_some() {
        return None;
    }
    let plan = decide(settings.mode, d.clash_running, d.tun_installed, d.tun_handles_domains, settings.system_proxy_ok);
    if last == Some(&plan) {
        return Some((plan, Steps::default()));
    }
    let tun_differs = if plan.tun { !d.tun_enabled } else { d.tun_flag };
    let tun = (last.is_none_or(|l| l.tun != plan.tun) && tun_differs).then_some(plan.tun);
    let system_proxy = match (plan.system_proxy, d.proxy) {
        _ if last.is_some_and(|l| l.system_proxy == plan.system_proxy) => None,
        (true, Proxy::Off | Proxy::Stale) => Some(true),
        (false, Proxy::Current | Proxy::Stale) => Some(false),
        _ => None,
    };
    Some((plan, Steps { tun, system_proxy }))
}

/// 纯函数:后台这一轮要执行什么,在 [`round`] 上加失败后的重试间隔。执行失败的计划不算已应用,按实时状态
/// 重新比;同一计划失败后 [`RETRY_AFTER`] 内不再执行(`None`,上一次保持不变),计划变了立即执行
/// (PUT 会清掉上一次,也立即执行)。等待期间实时状态已经对上的,不用执行,调用方记为已应用。
pub fn loop_round(settings: &Settings, d: &Detected, last: Option<&Last>, now: Instant) -> Option<(Plan, Steps)> {
    let applied = last.filter(|l| l.failed_at.is_none()).map(|l| &l.plan);
    let (plan, steps) = round(settings, d, applied)?;
    let waiting = last.is_some_and(|l| l.plan == plan
        && l.failed_at.is_some_and(|at| now.saturating_duration_since(at) < RETRY_AFTER));
    if waiting && steps != Steps::default() {
        return None;
    }
    Some((plan, steps))
}

/// 执行一轮的开关并留一条审计;全部成功返回真。两个开关写之前都会再检测一次对方([`crate::entry`])。
async fn apply(cfg: &Config, settings: &Settings, d: &Detected, plan: &Plan, steps: Steps, trigger: &str) -> bool {
    if steps == Steps::default() {
        return true;
    }
    let mut errors = Vec::new();
    if let Some(on) = steps.tun {
        if let Err(e) = crate::entry::tun_apply(cfg, on).await {
            errors.push(format!("虚拟网卡: {e}"));
        }
    }
    if let Some(on) = steps.system_proxy {
        if let Err(e) = crate::entry::system_proxy_apply(cfg, on).await {
            errors.push(format!("系统代理: {e}"));
        }
    }
    let ok = errors.is_empty();
    let mut detail = json!({
        "target_kind": "entry", "target_name": "takeover", "trigger": trigger, "mode": settings.mode,
        "before": {"tun": d.tun_enabled, "system_proxy": d.proxy},
        "after": {"tun": steps.tun, "system_proxy": steps.system_proxy, "effective": plan.effective},
        "result": if ok { "ok" } else { "failed" },
    });
    if ok {
        crate::events::audit("takeover_apply", "接管方式已按计划应用", detail);
    } else {
        detail["error"] = json!(errors.join("; "));
        crate::events::audit_failed("takeover_apply", "接管方式应用失败", detail);
    }
    ok
}

/// 由 [`crate::app::serve`] 启动;不允许接管时不起循环。
pub fn spawn(state: AppState) {
    if !allowed(&state.cfg) {
        return;
    }
    BACKGROUND.store(true, std::sync::atomic::Ordering::SeqCst);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            if state.lifecycle.is_closing() { break; }
            if !crate::runtime::serving(&state) { OBSERVATIONS.lock().unwrap().clear(); continue; }
            let cfg = &state.cfg;
            let mut last = LAST.lock().await;
            let settings = load(&file_path(&cfg.data_dir), cfg.identity());
            if settings.mode == Mode::Manual { continue; }
            let d = {
                let _detect = DETECT_LOCK.lock().await;
                let d = detect(cfg).await;
                cache_detection(&cfg.data_dir, d.clone(), last.map(|value| value.plan));
                d
            };
            let now = Instant::now();
            let stable = {
                let mut history = OBSERVATIONS.lock().unwrap();
                history.push(observation(&settings, &d));
                if history.len() > 3 { history.remove(0); }
                settle(&history)
            };
            let Some(stable) = stable else { continue; };
            let next = if d.other_app.is_some() {
                let waiting = last.as_ref().is_some_and(|value| value.plan == stable
                    && value.failed_at.is_some_and(|at| now.saturating_duration_since(at) < RETRY_AFTER));
                if waiting { None } else { Some((stable, Steps {
                    tun: d.tun_flag.then_some(false),
                    system_proxy: matches!(d.proxy, Proxy::Current | Proxy::Stale).then_some(false),
                })) }
            } else { loop_round(&settings, &d, last.as_ref(), now) };
            let Some((plan, steps)) = next else { continue; };
            let mut ok = true;
            if steps != Steps::default() {
                // 执行期间占住运行环境:空闲释放和退出清理都等这一轮做完;正在释放或退出时这一轮不动,下一轮再比。
                let Some(_activity) = state.lifecycle.runtime().maintenance() else { continue };
                ok = apply(cfg, &settings, &d, &plan, steps, "loop").await;
            }
            *last = Some(Last { plan, failed_at: (!ok).then_some(now) });
            if let Some(value) = CACHE.lock().unwrap().get_mut(&cfg.data_dir) { value.tried = Some(plan); }
        }
        BACKGROUND.store(false, std::sync::atomic::Ordering::SeqCst);
    });
}

/// 纯函数:实际生效的接管方式,按检测到的状态,不按计划。配合 Clash 时报 `clash`(用户有没有导入片段,
/// 这里看不到);否则虚拟网卡真的开着是 `tun`,当前服务的自动代理指向自己当前的 PAC 是 `system_proxy`,都不是则 `none`。
fn live_effective(plan: &Plan, d: &Detected) -> &'static str {
    if plan.effective == "clash" {
        "clash"
    } else if d.tun_enabled {
        "tun"
    } else if d.proxy == Proxy::Current {
        "system_proxy"
    } else {
        "none"
    }
}

/// 纯函数:域名规则现在实际由谁处理。本该处理的那种方式没有真的生效时是 `none`(比如该由系统代理处理,
/// 系统代理却没有指向自己);配合 Clash 时仍报 `clash`。
fn live_domains_via(plan: &Plan, d: &Detected) -> &'static str {
    match plan.domains_via {
        "tun" if !d.tun_enabled => "none",
        "system_proxy" if d.proxy != Proxy::Current => "none",
        via => via,
    }
}

/// 纯函数:计划要开的没有真的开着。
fn falls_short(plan: &Plan, d: &Detected) -> bool {
    (plan.tun && !d.tun_enabled) || (plan.system_proxy && d.proxy != Proxy::Current)
}

/// `GET/PUT /api/takeover` 的返回。`effective`、`domains_via` 按检测到的实际状态报(见 [`live_effective`]、
/// [`live_domains_via`]),`tun.enabled` 也是实际状态。`tried` 是后台或 PUT 已经执行过的计划(运行环境在服务时
/// 才给):执行过、计划要开的却没真开时 `action` 是 `retry_takeover`;还没执行过的计划不算失败,照常报它自己的
/// `action`。不允许接管时不做任何检测:`effective` 为 `none`、`domains_via` 取计划值、`action` 为 null;
/// 对方在接管时 `effective` 为 `none`、`action` 为 `close_other_app`(这时后台不动)。
pub fn view(settings: &Settings, allowed: bool, d: &Detected, tried: Option<&Plan>) -> Value {
    let plan = decide(settings.mode, d.clash_running, d.tun_installed, d.tun_handles_domains, settings.system_proxy_ok);
    let (effective, domains_via, action) = match (allowed, &d.other_app) {
        (false, _) => ("none", plan.domains_via, None),
        (true, Some(_)) => ("none", live_domains_via(&plan, d), Some("close_other_app")),
        (true, None) if tried == Some(&plan) && falls_short(&plan, d) =>
            (live_effective(&plan, d), live_domains_via(&plan, d), Some("retry_takeover")),
        (true, None) => (live_effective(&plan, d), live_domains_via(&plan, d), plan.action),
    };
    json!({
        "mode": settings.mode,
        "effective": effective,
        "allowed": allowed,
        "clash_running": d.clash_running,
        "system_proxy_ok": settings.system_proxy_ok,
        "other_app": {
            "tun": matches!(d.other_app, Some(OtherTakeover::Tun { .. })),
            "system_proxy": matches!(d.other_app, Some(OtherTakeover::SystemProxy { .. })),
        },
        "want": {"tun": plan.tun || plan.action == Some("install_tun"), "system_proxy": plan.system_proxy},
        "tun": {"installed": d.tun_installed, "enabled": d.tun_enabled},
        "system_proxy": {"enabled": d.proxy != Proxy::Off, "ours": d.proxy == Proxy::Current},
        "domains_via": domains_via,
        "action": action,
    })
}

/// PUT 的主体(接口和单测共用):不允许接管 → 403;请求体不对 → 400;对方在接管 → 409,不保存;否则保存。
/// `detect` 只在前两项通过后才等待(接口传真实检测,单测传替身)。返回新设置和检测结果。
pub async fn update(data_dir: &Path, id: &Identity, allowed: bool, patch: &Value,
    detect: impl std::future::Future<Output = Detected>) -> Result<(Settings, Detected), (StatusCode, String)>
{
    if !allowed {
        return Err((StatusCode::FORBIDDEN, "开发版、隔离实例和旧版应用不自动接管网络".into()));
    }
    let path = file_path(data_dir);
    let next = merge(&load(&path, id), patch).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let detected = detect.await;
    if detected.other_app.is_some() {
        return Err((StatusCode::CONFLICT, OTHER_APP.into()));
    }
    save(&path, &next).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("设置保存失败: {e}")))?;
    Ok((next, detected))
}

/// `GET /api/takeover`。不允许接管时不做任何检测(不跑系统命令,也不探 Clash)。
pub async fn get(State(st): State<AppState>) -> Json<Value> {
    let cfg = &st.cfg;
    let allowed = allowed(cfg);
    let settings = load(&file_path(&cfg.data_dir), cfg.identity());
    if !allowed {
        let mut value = view(&settings, false, &Detected::default(), None);
        value["detected_at"] = Value::Null;
        return Json(value);
    }
    let serving = crate::runtime::serving(&st);
    let cached = cached_detection(&cfg.data_dir, serving && settings.mode != Mode::Manual
        && BACKGROUND.load(std::sync::atomic::Ordering::SeqCst), detect(cfg)).await;
    let mut value = view(&settings, true, &cached.detected, if serving { cached.tried.as_ref() } else { None });
    value["detected_at"] = json!(cached.at);
    Json(value)
}

/// `PUT /api/takeover`:保存后立即按实时状态应用一次(失败后的重试间隔也一并清掉),返回执行之后重新检测到的
/// 状态;运行环境不在服务时只保存,由后台循环在服务后应用。
pub async fn put(State(st): State<AppState>, Json(b): Json<Value>) -> axum::response::Response {
    let cfg = &st.cfg;
    let mut last = LAST.lock().await;
    let _detect = DETECT_LOCK.lock().await;
    let before = load(&file_path(&cfg.data_dir), cfg.identity());
    let (settings, mut d) = match update(&cfg.data_dir, cfg.identity(), allowed(cfg), &b, detect(cfg)).await {
        Ok(saved) => saved,
        Err((status, msg)) if status == StatusCode::INTERNAL_SERVER_ERROR => {
            crate::events::audit_failed("takeover", "接管方式设置失败", json!({
                "target_kind": "entry", "target_name": "takeover",
                "before": before, "result": "failed", "error": msg.as_str(),
            }));
            return err500(&msg);
        }
        Err((status, msg)) => return err_detail(status, &msg),
    };
    if settings != before {
        crate::events::audit("takeover", "接管方式设置已更改", json!({
            "target_kind": "entry", "target_name": "takeover",
            "before": before, "after": settings, "result": "ok",
        }));
    }
    // 忘掉上一次的计划:这一次和实时状态对一遍,同进程启动后的第一轮。
    *last = None;
    OBSERVATIONS.lock().unwrap().clear();
    if crate::runtime::serving(&st) {
        if let Some((plan, steps)) = round(&settings, &d, None) {
            let ok = apply(cfg, &settings, &d, &plan, steps, "put").await;
            *last = Some(Last { plan, failed_at: (!ok).then(Instant::now) });
            if steps != Steps::default() {
                d = detect(cfg).await;
            }
        }
    }
    let tried = last.map(|l| l.plan);
    let cached = cache_detection(&cfg.data_dir, d.clone(), tried);
    let mut value = view(&settings, true, &d, tried.as_ref());
    value["detected_at"] = json!(cached.at);
    Json(value).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{TONGDAO, VPNMGR};

    const PAC: &str = "http://127.0.0.1:12345/entry/proxy.pac";

    #[test]
    fn settle_requires_three_matching_plans_or_two_other_takeover_observations() {
        let settings = settings(Mode::Auto, true);
        let a = observation(&settings, &Detected { clash_running: true, ..Default::default() });
        let b = observation(&settings, &Detected { tun_installed: true, ..Default::default() });
        assert_eq!(settle(&[a, b, a]), None);
        assert_eq!(settle(&[b, b]), None);
        assert_eq!(settle(&[b, b, b]), Some(b.plan));
        let other = observation(&settings, &Detected {
            other_app: Some(OtherTakeover::Tun { device: "utun225" }), ..Default::default()
        });
        assert_eq!(settle(&[b, other]), None);
        assert_eq!(settle(&[b, other, other]), Some(other.plan));
        assert!(!other.plan.tun && !other.plan.system_proxy);
        let changed_settings = observation(&Settings { mode: Mode::Tun, system_proxy_ok: true },
            &Detected { tun_installed: true, ..Default::default() });
        assert_eq!(settle(&[b, b, changed_settings]), None);
    }

    #[tokio::test]
    async fn cached_get_reuses_background_and_coalesces_fallback_with_monotonic_timestamps() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let calls = AtomicUsize::new(0);
        let detect = || async {
            calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(10)).await;
            Detected { clash_running: true, ..Default::default() }
        };
        let (first, same) = tokio::join!(cached_detection(dir.path(), false, detect()),
            cached_detection(dir.path(), false, detect()));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(first.at, same.at);
        CACHE.lock().unwrap().get_mut(dir.path()).unwrap().refreshed = Instant::now() - Duration::from_secs(60);
        let background = cached_detection(dir.path(), true, detect()).await;
        assert_eq!(background.at, first.at);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let refreshed = cached_detection(dir.path(), false, detect()).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(refreshed.at > first.at);
        assert!(refreshed.detected.clash_running);
        CACHE.lock().unwrap().remove(dir.path());
    }

    fn settings(mode: Mode, system_proxy_ok: bool) -> Settings {
        Settings { mode, system_proxy_ok }
    }

    #[test]
    fn decide_follows_the_table() {
        use Mode::*;
        // (方式, Clash 在跑, 助手已装, 虚拟网卡管域名, 已确认系统代理) → (虚拟网卡, 系统代理, effective, domains_via, action)
        type Input = (Mode, bool, bool, bool, bool);
        type Expected = (bool, bool, &'static str, &'static str, Option<&'static str>);
        let rows: &[(Input, Expected)] = &[
            // 自动 · Clash 在跑:不开系统代理(确认过也一样),界面给接入说明;助手装了就开虚拟网卡接规则里的地址
            ((Auto, true, false, false, false), (false, false, "clash", "clash", None)),
            ((Auto, true, true, false, true), (true, false, "clash", "clash", None)),
            // 自动 · 没有 Clash · 已装:开虚拟网卡;5.13 前系统代理要确认一次才开,没确认时报 enable_system_proxy
            ((Auto, false, true, false, false), (true, false, "tun", "system_proxy", Some("enable_system_proxy"))),
            ((Auto, false, true, false, true), (true, true, "tun", "system_proxy", None)),
            // 5.13 后:虚拟网卡管域名,不开系统代理
            ((Auto, false, true, true, false), (true, false, "tun", "tun", None)),
            ((Auto, false, true, true, true), (true, false, "tun", "tun", None)),
            // 自动 · 没有 Clash · 没装:不开虚拟网卡,报 install_tun;确认过就开系统代理
            ((Auto, false, false, false, false), (false, false, "none", "system_proxy", Some("install_tun"))),
            ((Auto, false, false, false, true), (false, true, "system_proxy", "system_proxy", Some("install_tun"))),
            // 只开虚拟网卡:不开系统代理(确认过也一样);域名规则 Clash 在跑时归它,否则没人接;5.13 后归虚拟网卡
            ((Tun, false, true, false, true), (true, false, "tun", "none", None)),
            ((Tun, true, true, false, true), (true, false, "tun", "clash", None)),
            ((Tun, true, true, true, false), (true, false, "tun", "tun", None)),
            // 只开虚拟网卡 · 没装
            ((Tun, false, false, false, true), (false, false, "none", "none", Some("install_tun"))),
            ((Tun, true, false, false, false), (false, false, "none", "clash", Some("install_tun"))),
            // 都开:照开,Clash 在跑也不拦
            ((TunSystemProxy, false, true, false, false), (true, true, "tun", "system_proxy", None)),
            ((TunSystemProxy, true, true, false, false), (true, true, "tun", "system_proxy", None)),
            // 都开 · 没装:系统代理照开
            ((TunSystemProxy, false, false, false, false), (false, true, "system_proxy", "system_proxy", Some("install_tun"))),
            // 都不开(配合 Clash):装了助手也不开虚拟网卡
            ((Clash, false, true, false, true), (false, false, "clash", "clash", None)),
            ((Clash, true, false, false, false), (false, false, "clash", "clash", None)),
            // 只开系统代理
            ((SystemProxy, true, true, false, false), (false, true, "system_proxy", "system_proxy", None)),
            // 旧身份的 manual:后台不管
            ((Manual, false, true, false, true), (false, false, "none", "system_proxy", None)),
        ];
        for &((mode, clash, installed, handles, ok), (tun, proxy, effective, via, action)) in rows {
            let plan = decide(mode, clash, installed, handles, ok);
            assert_eq!(plan, Plan { tun, system_proxy: proxy, effective, domains_via: via, action },
                "{mode:?} clash={clash} installed={installed} handles={handles} ok={ok}");
        }
    }

    #[test]
    fn settings_default_by_identity_and_saved_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = file_path(dir.path());
        assert_eq!(load(&path, &TONGDAO), settings(Mode::Tun, false));
        assert_eq!(load(&path, &VPNMGR), settings(Mode::Manual, false));
        std::fs::write(&path, b"{").unwrap();
        assert_eq!(load(&path, &TONGDAO), settings(Mode::Tun, false));
        std::fs::write(&path, br#"{"system_proxy_ok": true}"#).unwrap();
        assert_eq!(load(&path, &TONGDAO), settings(Mode::Tun, true));
        // 早先存下的 auto 照旧读回。
        std::fs::write(&path, br#"{"mode": "auto"}"#).unwrap();
        assert_eq!(load(&path, &TONGDAO), settings(Mode::Auto, false));
        save(&path, &settings(Mode::SystemProxy, true)).unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&std::fs::read(&path).unwrap()).unwrap(),
            json!({"mode": "system_proxy", "system_proxy_ok": true}));
        assert_eq!(load(&path, &VPNMGR), settings(Mode::SystemProxy, true));
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn merge_accepts_modes_and_confirmation() {
        let current = settings(Mode::Auto, false);
        assert_eq!(merge(&current, &json!({"mode": "tun"})).unwrap(), settings(Mode::Tun, false));
        assert_eq!(merge(&current, &json!({"mode": "clash"})).unwrap(), settings(Mode::Clash, false));
        assert_eq!(merge(&current, &json!({"mode": "tun_system_proxy"})).unwrap(), settings(Mode::TunSystemProxy, true));
        assert_eq!(merge(&current, &json!({"system_proxy_ok": true})).unwrap(), settings(Mode::Auto, true));
        assert_eq!(merge(&current, &json!({"mode": "auto", "system_proxy_ok": true})).unwrap(), settings(Mode::Auto, true));
        // 选「系统代理」方式就等于确认过。
        assert_eq!(merge(&current, &json!({"mode": "system_proxy"})).unwrap(), settings(Mode::SystemProxy, true));
        for bad in [json!({"mode": "manual"}), json!({"mode": "surge"}), json!({"mode": 1}), json!({"system_proxy_ok": "yes"}),
            json!({}), json!({"other": true}), json!([])] {
            assert!(merge(&current, &bad).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn update_forbids_rejects_then_detects() {
        let dir = tempfile::tempdir().unwrap();
        let path = file_path(dir.path());
        let never = || async { panic!("不允许或请求体不对时不该检测") };
        // 不允许接管(开发版、隔离实例、旧身份):403,不检测也不写。
        let err = update(dir.path(), &TONGDAO, false, &json!({"mode": "tun"}), never()).await.unwrap_err();
        assert_eq!(err.0, StatusCode::FORBIDDEN);
        let err = update(dir.path(), &TONGDAO, true, &json!({"mode": "manual"}), never()).await.unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        // 对方在接管:409,不保存。
        for other in [OtherTakeover::Tun { device: "utun225" }, OtherTakeover::SystemProxy { service: "Wi-Fi".into() }] {
            let d = Detected { other_app: Some(other), ..Default::default() };
            let err = update(dir.path(), &TONGDAO, true, &json!({"system_proxy_ok": true}), async { d }).await.unwrap_err();
            assert_eq!(err.0, StatusCode::CONFLICT);
            assert!(!path.exists());
        }
        let d = Detected { clash_running: true, ..Default::default() };
        let (saved, detected) = update(dir.path(), &TONGDAO, true, &json!({"mode": "clash"}), async { d.clone() }).await.unwrap();
        assert_eq!((saved, detected), (settings(Mode::Clash, false), d));
        assert_eq!(load(&path, &TONGDAO), settings(Mode::Clash, false));
    }

    #[test]
    fn proxy_judged_against_current_pac_and_own_range() {
        let range = &TONGDAO.port_range;
        assert_eq!(classify_proxy(true, Some(PAC), PAC, range), Proxy::Current);
        // 自己范围内的旧端口:残留,不算别人的。
        assert_eq!(classify_proxy(true, Some("http://127.0.0.1:10000/entry/proxy.pac"), PAC, range), Proxy::Stale);
        // 旧应用范围的端口、用户自己的 PAC:别人的。
        assert_eq!(classify_proxy(true, Some("http://127.0.0.1:45000/entry/proxy.pac"), PAC, range), Proxy::Foreign);
        assert_eq!(classify_proxy(true, Some("http://192.0.2.10/proxy.pac"), PAC, range), Proxy::Foreign);
        assert_eq!(classify_proxy(false, Some(PAC), PAC, range), Proxy::Off);
        assert_eq!(classify_proxy(false, Some("http://192.0.2.10/proxy.pac"), PAC, range), Proxy::Off);
        assert_eq!(classify_proxy(true, None, PAC, range), Proxy::Off);
    }

    #[test]
    fn round_acts_only_when_own_plan_changes() {
        let auto = settings(Mode::Auto, false);
        let installed = Detected { tun_installed: true, ..Default::default() };
        // 进程刚启动:按实时状态应用一次。
        let (plan, steps) = round(&auto, &installed, None).unwrap();
        assert_eq!((plan.tun, plan.system_proxy, plan.action), (true, false, Some("enable_system_proxy")));
        assert_eq!(steps, Steps { tun: Some(true), system_proxy: None });
        // 计划不变,实时状态和计划不一致(用户手动关了虚拟网卡、自己开了系统代理):不动作。
        for live in [installed.clone(), Detected { proxy: Proxy::Current, ..installed.clone() }] {
            assert_eq!(round(&auto, &live, Some(&plan)), Some((plan, Steps::default())));
        }
        // Clash 起来了:计划变了,但虚拟网卡照开,系统代理本来就没开,不用动作。
        let running = Detected { tun_flag: true, tun_enabled: true, ..installed.clone() };
        let clash = Detected { clash_running: true, ..running.clone() };
        let (clash_plan, steps) = round(&auto, &clash, Some(&plan)).unwrap();
        assert_eq!((clash_plan.tun, clash_plan.system_proxy, clash_plan.domains_via), (true, false, "clash"));
        assert_eq!(steps, Steps::default());
        // 确认过系统代理、PAC 开着时 Clash 起来:只关系统代理,把它还给 Clash。
        let confirmed = settings(Mode::Auto, true);
        let proxied = Detected { proxy: Proxy::Current, ..running.clone() };
        let (proxy_plan, _) = round(&confirmed, &proxied, None).unwrap();
        let clash_proxied = Detected { clash_running: true, ..proxied.clone() };
        assert_eq!(round(&confirmed, &clash_proxied, Some(&proxy_plan)).unwrap().1, Steps { tun: None, system_proxy: Some(false) });
        // Clash 退了:计划变回来,虚拟网卡一直开着,不重复执行。
        assert_eq!(round(&auto, &running, Some(&clash_plan)).unwrap().1, Steps::default());
        // 确认了系统代理:计划变了才开。
        let (confirmed, steps) = round(&settings(Mode::Auto, true), &running, Some(&plan)).unwrap();
        assert!(confirmed.system_proxy);
        assert_eq!(steps, Steps { tun: None, system_proxy: Some(true) });
    }

    #[test]
    fn tun_off_follows_flag_even_when_engine_is_down() {
        // 启用标记在、引擎没在跑(开机下发失败、助手被卸掉):要开时按实际状态,没在跑就开。
        let auto = settings(Mode::Auto, false);
        let flag_only = Detected { tun_installed: true, tun_flag: true, ..Default::default() };
        let (on_plan, steps) = round(&auto, &flag_only, None).unwrap();
        assert!(on_plan.tun);
        assert_eq!(steps.tun, Some(true));
        // 计划转为关:标记在就关(清掉标记),否则规则一变 tun_sync 就按标记把网卡再开起来。
        // 用户 PUT 系统代理(上一次已清空)、助手被卸掉。
        let system_proxy = settings(Mode::SystemProxy, true);
        assert_eq!(round(&system_proxy, &flag_only, None).unwrap().1.tun, Some(false));
        let uninstalled = Detected { tun_installed: false, ..flag_only.clone() };
        assert_eq!(round(&auto, &uninstalled, Some(&on_plan)).unwrap().1.tun, Some(false));
        // 后台循环同样下发:第一轮没关就会把关的计划记为已应用,之后边沿触发再也不纠正。
        assert_eq!(loop_round(&system_proxy, &flag_only, None, Instant::now()).unwrap().1.tun, Some(false));
        // 标记不在:不用关。
        let off = Detected { tun_installed: true, ..Default::default() };
        assert_eq!(round(&system_proxy, &off, None).unwrap().1.tun, None);
    }

    #[test]
    fn round_never_overrides_someone_elses_proxy() {
        let on = settings(Mode::SystemProxy, true);
        let off_plan = decide(Mode::Clash, false, false, false, false);
        // 要打开:当前服务的自动代理已启用且地址不是我们的就不动;没开或是自己的残留就写。
        for (proxy, step) in [(Proxy::Foreign, None), (Proxy::Current, None), (Proxy::Off, Some(true)), (Proxy::Stale, Some(true))] {
            let d = Detected { proxy, ..Default::default() };
            assert_eq!(round(&on, &d, Some(&off_plan)).unwrap().1.system_proxy, step, "{proxy:?}");
            assert_eq!(round(&on, &d, None).unwrap().1.system_proxy, step, "{proxy:?}");
        }
        // 要关:只关自己的(当前的或残留的),别人的不动。
        let on_plan = decide(Mode::SystemProxy, false, false, false, true);
        let clash = settings(Mode::Clash, false);
        for (proxy, step) in [(Proxy::Current, Some(false)), (Proxy::Stale, Some(false)), (Proxy::Foreign, None), (Proxy::Off, None)] {
            let d = Detected { proxy, ..Default::default() };
            assert_eq!(round(&clash, &d, Some(&on_plan)).unwrap().1.system_proxy, step, "{proxy:?}");
        }
    }

    #[test]
    fn round_does_nothing_for_manual_or_while_other_app_holds() {
        let installed = Detected { tun_installed: true, ..Default::default() };
        assert_eq!(round(&settings(Mode::Manual, true), &installed, None), None);
        for other in [OtherTakeover::Tun { device: "utun225" }, OtherTakeover::SystemProxy { service: "Wi-Fi".into() }] {
            let d = Detected { other_app: Some(other), ..installed.clone() };
            assert_eq!(round(&settings(Mode::Tun, true), &d, None), None);
        }
    }

    #[test]
    fn only_tongdao_with_host_integrations_runs() {
        assert!(runs_for(&TONGDAO, true));
        assert!(!runs_for(&TONGDAO, false));
        // 旧身份即使允许宿主集成,后台也什么都不做。
        assert!(!runs_for(&VPNMGR, true));
        assert!(!runs_for(&VPNMGR, false));
    }

    #[test]
    fn view_reports_plan_other_app_and_live_state() {
        // 不允许接管:不给动作。
        assert_eq!(view(&settings(Mode::Auto, false), false, &Detected::default(), None), json!({
            "mode": "auto", "effective": "none", "allowed": false, "clash_running": false, "system_proxy_ok": false,
            "other_app": {"tun": false, "system_proxy": false},
            "want": {"tun": true, "system_proxy": false}, "tun": {"installed": false, "enabled": false},
            "system_proxy": {"enabled": false, "ours": false}, "domains_via": "system_proxy", "action": null,
        }));
        // 系统代理指向别人的地址:域名规则没有生效(F1 起报 none,原来报 system_proxy)。
        let live = Detected { tun_installed: true, tun_enabled: true, proxy: Proxy::Foreign, ..Default::default() };
        assert_eq!(view(&settings(Mode::Auto, false), true, &live, None), json!({
            "mode": "auto", "effective": "tun", "allowed": true, "clash_running": false, "system_proxy_ok": false,
            "other_app": {"tun": false, "system_proxy": false},
            "want": {"tun": true, "system_proxy": false}, "tun": {"installed": true, "enabled": true},
            "system_proxy": {"enabled": true, "ours": false}, "domains_via": "none", "action": "enable_system_proxy",
        }));
        // 两个开关:想开的(want)和真开着的分开报。只开虚拟网卡、Clash 在跑:域名归 Clash。
        let clash = Detected { clash_running: true, tun_installed: true, tun_enabled: true, ..Default::default() };
        let v = view(&settings(Mode::Tun, false), true, &clash, None);
        assert_eq!((&v["want"], &v["effective"], &v["domains_via"]),
            (&json!({"tun": true, "system_proxy": false}), &json!("tun"), &json!("clash")));
        let v = view(&settings(Mode::TunSystemProxy, true), true, &Detected::default(), None);
        assert_eq!((&v["want"], &v["action"]), (&json!({"tun": true, "system_proxy": true}), &json!("install_tun")));
        let v = view(&settings(Mode::Tun, true), true, &Detected { other_app: Some(OtherTakeover::Tun { device: "utun225" }), ..live.clone() }, None);
        assert_eq!((&v["effective"], &v["action"], &v["other_app"]), (&json!("none"), &json!("close_other_app"), &json!({"tun": true, "system_proxy": false})));
        let v = view(&settings(Mode::SystemProxy, true), true,
            &Detected { other_app: Some(OtherTakeover::SystemProxy { service: "Wi-Fi".into() }), ..Default::default() }, None);
        assert_eq!(v["other_app"], json!({"tun": false, "system_proxy": true}));
        let v = view(&settings(Mode::SystemProxy, true), true, &Detected { proxy: Proxy::Current, ..Default::default() }, None);
        assert_eq!((&v["effective"], &v["system_proxy"]), (&json!("system_proxy"), &json!({"enabled": true, "ours": true})));
    }

    #[test]
    fn tun_counts_as_on_only_with_flag_and_live_engine() {
        let on = json!({"ok": true, "running": true, "alive": true});
        assert!(tun_running(true, Some(&on)));
        assert!(!tun_running(false, Some(&on)), "没有启用标记");
        assert!(!tun_running(true, None), "助手连不上");
        assert!(!tun_running(true, Some(&json!({"running": true, "alive": false}))), "引擎进程没活着");
        assert!(!tun_running(true, Some(&json!({"running": false, "alive": false}))), "已停");
    }

    #[test]
    fn failed_apply_reports_live_state_and_retry_action() {
        // 自动、没有 Clash、已装、已确认系统代理:计划开虚拟网卡和系统代理。
        let s = settings(Mode::Auto, true);
        let plan = decide(Mode::Auto, false, true, false, true);
        assert_eq!((plan.tun, plan.system_proxy, plan.effective), (true, true, "tun"));
        // 执行过、两样都没开成:effective 不是计划值,action 是 retry_takeover。
        let failed = Detected { tun_installed: true, ..Default::default() };
        let v = view(&s, true, &failed, Some(&plan));
        assert_eq!((&v["effective"], &v["domains_via"], &v["action"]), (&json!("none"), &json!("none"), &json!("retry_takeover")));
        assert_eq!(v["tun"], json!({"installed": true, "enabled": false}));
        // 还没执行过(进程刚启动、刚保存、运行环境不在服务):不算失败,照常报计划自己的动作。
        assert_eq!(view(&s, true, &failed, None)["action"], json!(null));
        // 执行过的是另一个计划(计划刚变,这一轮还没到):同样不算失败。
        let other = decide(Mode::Auto, true, true, false, true);
        assert_eq!(view(&s, true, &failed, Some(&other))["action"], json!(null));
        // 虚拟网卡开成了、系统代理没开成。
        let half = Detected { tun_enabled: true, ..failed.clone() };
        let v = view(&s, true, &half, Some(&plan));
        assert_eq!((&v["effective"], &v["domains_via"], &v["action"]), (&json!("tun"), &json!("none"), &json!("retry_takeover")));
        // 都开成了。
        let done = Detected { proxy: Proxy::Current, ..half.clone() };
        let v = view(&s, true, &done, Some(&plan));
        assert_eq!((&v["effective"], &v["domains_via"], &v["action"]), (&json!("tun"), &json!("system_proxy"), &json!(null)));
        // 系统代理方式没开成:不报 system_proxy。计划要开的动作(install_tun)被重试盖过。
        let sp_plan = decide(Mode::SystemProxy, false, false, false, true);
        let v = view(&settings(Mode::SystemProxy, true), true, &Detected::default(), Some(&sp_plan));
        assert_eq!((&v["effective"], &v["domains_via"], &v["action"]), (&json!("none"), &json!("none"), &json!("retry_takeover")));
        let auto_plan = decide(Mode::Auto, false, false, false, true);
        assert_eq!(auto_plan.action, Some("install_tun"));
        assert_eq!(view(&s, true, &Detected::default(), Some(&auto_plan))["action"], json!("retry_takeover"));
        assert_eq!(view(&s, true, &Detected { proxy: Proxy::Current, ..Default::default() }, Some(&auto_plan))["action"], json!("install_tun"));
    }

    #[test]
    fn failed_plan_retried_after_five_minutes_or_when_plan_changes() {
        let s = settings(Mode::Auto, true);
        let off = Detected { tun_installed: true, ..Default::default() };
        let t0 = Instant::now();
        // 进程刚启动:按实时状态执行一次。
        let (plan, steps) = loop_round(&s, &off, None, t0).unwrap();
        assert_eq!(steps, Steps { tun: Some(true), system_proxy: Some(true) });
        // 执行失败:不算已应用;5 分钟内同一计划不重试,上一次保持不变。
        let failed = Last { plan, failed_at: Some(t0) };
        for later in [Duration::ZERO, Duration::from_secs(60), RETRY_AFTER - Duration::from_secs(1)] {
            assert_eq!(loop_round(&s, &off, Some(&failed), t0 + later), None, "{later:?}");
        }
        // 满 5 分钟:按实时状态再试一次。
        assert_eq!(loop_round(&s, &off, Some(&failed), t0 + RETRY_AFTER), Some((plan, steps)));
        // 计划变了(换成系统代理方式):立即执行。
        let (sp_plan, sp_steps) = loop_round(&settings(Mode::SystemProxy, true), &off, Some(&failed), t0 + Duration::from_secs(1)).unwrap();
        assert_ne!(sp_plan, plan);
        assert_eq!(sp_steps, Steps { tun: None, system_proxy: Some(true) });
        // 等待期间实时状态已经对上:不用执行(调用方记为已应用)。
        let live = Detected { tun_enabled: true, proxy: Proxy::Current, ..off.clone() };
        assert_eq!(loop_round(&s, &live, Some(&failed), t0 + Duration::from_secs(60)), Some((plan, Steps::default())));
        // 成功应用过的计划仍按边沿触发:计划不变就不动,哪怕实时状态和计划不一致。
        let applied = Last { plan, failed_at: None };
        assert_eq!(loop_round(&s, &off, Some(&applied), t0 + RETRY_AFTER * 2), Some((plan, Steps::default())));
        // 旧身份的 manual、对方在接管:整轮不动。
        assert_eq!(loop_round(&settings(Mode::Manual, true), &off, Some(&failed), t0 + RETRY_AFTER), None);
    }

    #[test]
    fn domains_via_none_when_the_handling_way_is_not_live() {
        // 自动、没有 Clash、已装、还没确认系统代理(5.13 前新装应用的默认状态):虚拟网卡开着,系统代理没指向自己。
        let tun_on = Detected { tun_installed: true, tun_enabled: true, ..Default::default() };
        for proxy in [Proxy::Off, Proxy::Stale, Proxy::Foreign] {
            let v = view(&settings(Mode::Auto, false), true, &Detected { proxy, ..tun_on.clone() }, None);
            assert_eq!((&v["effective"], &v["domains_via"]), (&json!("tun"), &json!("none")), "{proxy:?}");
        }
        let v = view(&settings(Mode::Auto, true), true, &Detected { proxy: Proxy::Current, ..tun_on.clone() }, None);
        assert_eq!(v["domains_via"], json!("system_proxy"));
        // 5.13 后由虚拟网卡处理:网卡没真开时同样是 none。
        let handles = Detected { tun_installed: true, tun_handles_domains: true, ..Default::default() };
        assert_eq!(view(&settings(Mode::Auto, false), true, &handles, None)["domains_via"], json!("none"));
        assert_eq!(view(&settings(Mode::Auto, false), true, &Detected { tun_enabled: true, ..handles }, None)["domains_via"], json!("tun"));
        // 配合 Clash:仍报 clash(用户有没有导入片段看不到)。
        let v = view(&settings(Mode::Clash, false), true, &Detected::default(), None);
        assert_eq!((&v["effective"], &v["domains_via"]), (&json!("clash"), &json!("clash")));
        let v = view(&settings(Mode::Auto, false), true, &Detected { clash_running: true, ..Default::default() }, None);
        assert_eq!((&v["effective"], &v["domains_via"]), (&json!("clash"), &json!("clash")));
    }
}
