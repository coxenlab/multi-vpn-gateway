// 从接口数据推导界面状态的纯函数（DESIGN 4.3）。顶层不碰 document / window。
import { TAKEOVER_ACTION, TAKEOVER_TITLE, DOMAINS_OFF, RULES_OFF } from './terms.js';

const SETTLED = new Set(['awaiting_login', 'committed', 'rolled_back']);

// 通道的显示状态：stopped / error / unverified / waiting / connected。
// 按顺序取第一个成立的：已停止 → 出错 → 还没验证 → 待登录 → 已连接。
// health 是该通道 /health（或 /status）的返回，可以没有。
export function displayState(ch, health) {
  if (!ch) return 'error';
  const status = ch.status;
  if (status === 'stopped' || ch.stop_pending === true) return 'stopped';
  if (ch.container_state === 'unknown' || health?.probe_failed === true) return 'unknown';
  const phase = ch.replacement && ch.replacement.phase;
  if (status === 'error' || status === 'down' || (phase != null && !SETTLED.has(phase))) return 'error';
  if (health && health.awaiting_target === true) return 'unverified';
  if (status === 'running' || status === 'creating') return 'waiting';
  if (status === 'logged_in') return 'connected';
  return 'waiting';
}

// 需要采样 /health 的通道：库里是运行态的
export const isActive = ch => !!ch && (ch.status === 'running' || ch.status === 'logged_in');

// 通道的全部规则：domains 在前，ips 在后
export const channelRules = ch => [...((ch && ch.domains) || []), ...((ch && ch.ips) || [])];

// 规则「现在」一列：disabled / direct / direct-unrouted / direct-stopped / blocked / channel / waiting。
// state 是 displayState 的结果；blocked 取通道（或 health）的 blocked 字段。
// 核心的 enabled 是整数 0/1（store.rs），也兼容布尔。
// 通道没有参与分流（routing_enabled 为假，从旧应用导入时原样带过来）：核心把它的规则全部折叠掉，走原网络
export function ruleFate(rule, ch, { routingOff = false, state, blocked, takeover = null } = {}) {
  if (rule && (rule.enabled === false || rule.enabled === 0)) return 'disabled';
  if (routingOff) return 'direct';
  if (ch && ch.routing_enabled === false) return 'direct-unrouted';
  const s = state || displayState(ch);
  if (s === 'stopped') return 'direct-stopped';
  if (ruleUnsupported(rule, takeover)) return 'unsupported';
  if (ruleInactive(rule, takeover)) return 'inactive';
  if (blocked === true || (ch && ch.blocked === true)) return 'blocked';
  if (s === 'unknown') return 'unknown';
  if (s === 'connected') return 'channel';
  return 'waiting';
}

// 速度：/api/connections 的累计值差分。差值为负（入口重载后归零）记 0，第一帧没有速度。
// prev: {up, down, at} 或 null；返回 {up, down, next}，up/down 为每秒字节数或 null
export function speed(prev, conn, now) {
  if (!conn || conn.unavailable === true) return { up: null, down: null, next: null };
  const up = Number(conn && conn.uploadTotal);
  const down = Number(conn && conn.downloadTotal);
  if (conn.uploadTotal == null || conn.downloadTotal == null || !Number.isFinite(up) || !Number.isFinite(down)) return { up: null, down: null, next: null };
  const next = { up, down, at: now };
  if (!prev || !(now > prev.at)) return { up: null, down: null, next };
  const dt = (now - prev.at) / 1000;
  return { up: Math.max(0, up - prev.up) / dt, down: Math.max(0, down - prev.down) / dt, next };
}

// 追加一个点，只留最近 max 个
export function pushPoint(points, v, max = 120) {
  const out = points.length >= max ? points.slice(points.length - max + 1) : points.slice();
  out.push(v);
  return out;
}

// 顶栏分流标记：on / off / bad / runtime / starting；拿不到系统状态时为 null（隐藏）
export function chipState(system) {
  if (!system || typeof system !== 'object') return null;
  const phase = system.runtime && system.runtime.phase;
  if (phase === 'starting' || (phase === 'releasing' && system.runtime.detail?.includes('重启'))) return 'starting';
  if (phase === 'dormant') return 'dormant';
  if (phase && phase !== 'ready' && phase !== 'waiting') return 'runtime';
  if (system.config_application?.pending === true) return 'pending';
  if (system.routing_off === true) return 'off';
  if (system.gave_up === true || (system.gateway_health && system.gateway_health !== 'healthy')) return 'bad';
  return 'on';
}

// 运行环境是不是起不来了：顶栏提示条要给「启动」按钮
export const runtimeFailed = system => !!(system && system.runtime && system.runtime.phase === 'failed');

// ---- 接管网络（DESIGN 5.8）：/api/takeover 的 effective、domains_via 按实际状态报，不按计划 ----

const takeoverOk = t => !!t && typeof t === 'object' && t.allowed !== false;
// 系统代理开着，但指向别人的地址
const proxyForeign = t => !!(t.system_proxy && t.system_proxy.enabled && !t.system_proxy.ours);

// 域名规则现在实际由谁处理：tun / system_proxy / clash / none。接口不可用、不允许接管（开发版不检测）时为 null。
// 说由系统代理处理、系统代理却没有指向自己时，同样算 none
export function domainsVia(t) {
  if (!takeoverOk(t) || typeof t.domains_via !== 'string') return null;
  const sp = t.system_proxy;
  if (t.domains_via === 'system_proxy' && sp && typeof sp === 'object' && !(sp.enabled && sp.ours)) return 'none';
  return t.domains_via;
}

// 接管网络在「需要处理」里的一条，或者只是一句建议（tip 为真，不计入需要处理的数量）。没有时为 null。
// 返回 {title, text, button, fix, tip}；fix：system-proxy-ok 用系统代理接管 / retry 重试 / settings 启用虚拟网卡 / null 没有按钮
export function takeoverTodo(t) {
  if (!takeoverOk(t) || !t.action) return null;
  const none = (t.effective || 'none') === 'none';
  const A = TAKEOVER_ACTION;
  switch (t.action) {
    case 'close_other_app':
      return { title: TAKEOVER_TITLE.none, text: A.close_other_app.text, button: '', fix: null, tip: false };
    case 'retry_takeover':
      return { title: TAKEOVER_TITLE.failed, text: proxyForeign(t) ? A.retry_takeover.foreign : A.retry_takeover.text, button: A.retry_takeover.button, fix: 'retry', tip: false };
    case 'enable_system_proxy':
      return none
        ? { title: TAKEOVER_TITLE.none, text: A.enable_system_proxy.text, button: A.enable_system_proxy.button, fix: 'system-proxy-ok', tip: false }
        : { title: TAKEOVER_TITLE.domains, text: A.enable_system_proxy.domains, button: A.enable_system_proxy.button, fix: 'system-proxy-ok', tip: false };
    case 'install_tun':
      // 已经用系统代理接管了：启用虚拟网卡只是建议
      return none
        ? { title: TAKEOVER_TITLE.none, text: A.install_tun.text, button: A.install_tun.button, fix: 'settings', tip: false }
        : { title: TAKEOVER_TITLE.tip, text: A.install_tun.tip, button: A.install_tun.button, fix: 'settings', tip: true };
    default:
      return null;
  }
}

// 域名规则没有生效时的说明和能让它生效的动作：{text, fix}；生效了、或者看不出来时为 null。
// fix 同 takeoverTodo，另有 settings 表示到设置里处理
export function domainsOff(t) {
  if (domainsVia(t) !== 'none') return null;
  const A = TAKEOVER_ACTION;
  let why, fix;
  if (t.action === 'close_other_app' || (t.other_app && (t.other_app.tun || t.other_app.system_proxy))) {
    why = A.close_other_app.text; fix = null;
  } else if (t.action === 'retry_takeover') {
    why = proxyForeign(t) ? A.retry_takeover.foreign : A.retry_takeover.text; fix = 'retry';
  } else if (t.action === 'enable_system_proxy' || (t.action === 'install_tun' && t.mode === 'auto' && !t.system_proxy_ok)) {
    // 早先的自动方式：确认系统代理后核心才会打开它（DESIGN 5.8 的表）
    why = '用系统代理接管以后才生效'; fix = 'system-proxy-ok';
  } else {
    // 虚拟网卡只接地址；域名规则要靠系统代理或者 Clash
    why = '到设置里打开系统代理，或者配合 Clash'; fix = 'settings';
  }
  return { text: `${DOMAINS_OFF}：${why}。`, fix };
}

// 接口明确报告没有接管时，地址规则也没生效。开发版和未知状态不推断。
export function rulesOff(t) {
  if (takeoverOk(t) && t.effective === 'none') {
    const note = domainsOff({ ...t, domains_via: 'none' });
    return { ...note, text: note.text.replace(DOMAINS_OFF, RULES_OFF) };
  }
  return domainsOff(t);
}

export function ruleInactive(rule, takeover) {
  if (!takeoverOk(takeover)) return false;
  return takeover.effective === 'none' || (rule?.kind === 'domain' && !!domainsOff(takeover));
}

export function ruleUnsupported(rule, takeover) {
  return takeoverOk(takeover) && ['tun', 'system_proxy'].includes(takeover.effective)
    && rule?.kind === 'ip' && String(rule.pattern || '').includes(':');
}

// 在线数、规则总数、平均延迟（只算已连接且有延迟的）
export function summary(channels, healthMap = {}) {
  let online = 0, rules = 0, sum = 0, n = 0;
  for (const ch of channels || []) {
    const hl = healthMap[ch.id];
    rules += channelRules(ch).length;
    if (displayState(ch, hl) === 'connected') {
      online++;
      const ms = hl && hl.latency_ms != null ? hl.latency_ms : ch.latency_ms;
      if (Number.isFinite(Number(ms)) && ms != null) { sum += Number(ms); n++; }
    }
  }
  return { online, rules, avgLatency: n ? Math.round(sum / n) : null };
}
