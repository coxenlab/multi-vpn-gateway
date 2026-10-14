import test from 'node:test';
import assert from 'node:assert/strict';
import { displayState, isActive, channelRules, ruleFate, speed, pushPoint, chipState, runtimeFailed, summary, domainsVia, takeoverTodo, domainsOff } from '../../app/ui/js/derive.js';
import { UNAVAILABLE } from '../../app/ui/js/api.js';
import { pickView, headStatus } from '../../app/ui/js/screens/channels.js';
import { lastCheck, canVerifyTarget } from '../../app/ui/js/screens/channel-overview.js';
import { workspaceTab } from '../../app/ui/js/screens/channel-login.js';

test('无法确认的检测结果不显示为不通', () => {
  assert.equal(lastCheck({ checked_at: 1000, connected: null, probe_failed: true }, 2000), '状态待确认');
  assert.equal(lastCheck({ checked_at: 1000, connected: null }, 2000), '状态待确认');
  assert.match(lastCheck({ checked_at: 1000, connected: false }, 2000), /不通/);
});

test('窗口通道默认可见概览；旧测活入口归入概览，规则和日志深链接保留', () => {
  for (const tab of [undefined, 'login', 'target', 'overview', 'invalid']) assert.equal(workspaceTab({ tab }), 'overview');
  assert.equal(workspaceTab({ tab: 'rules' }), 'rules');
  assert.equal(workspaceTab({ tab: 'logs' }), 'logs');
});

test('检测地址只对稳定运行的客户端开放，停止中和状态未知时不允许提交', () => {
  const ch = { status: 'running' };
  assert.equal(canVerifyTarget(ch), true);
  assert.equal(canVerifyTarget({ ...ch, replacement: { phase: 'awaiting_login' } }), true);
  for (const patch of [{ stop_pending: true }, { container_state: 'unknown' }, { status: 'stopped' }, { replacement: { phase: 'provisioning' } }, { replacement: { phase: 'queued' } }]) {
    assert.equal(canVerifyTarget({ ...ch, ...patch }), false);
  }
});

test('运行中的交互通道始终留在统一登录工作台，显式分页只切右侧工具', () => {
  const ch = { status: 'running', login_method: 'interactive', blocked: false };
  assert.equal(pickView(ch, null, null, 'channel-overview'), 'channel-login');
  assert.equal(pickView(ch, null, null), 'channel-login');
  assert.equal(pickView(ch, null, 'login', 'channel-overview'), 'channel-login');
  assert.equal(pickView(ch, null, 'overview', 'channel-login'), 'channel-login');
  assert.equal(pickView(ch, null, 'rules', 'channel-login'), 'channel-login');
  assert.equal(pickView(ch, null, 'logs', 'channel-login'), 'channel-login');
  assert.equal(pickView({ ...ch, blocked: true }, null, null, 'channel-overview'), 'channel-login');
  assert.equal(pickView(ch, { blocked: true }, null, 'channel-overview'), 'channel-login');
  assert.equal(pickView({ ...ch, status: 'error' }, null, null, 'channel-overview'), 'channel-error');
  assert.equal(pickView({ ...ch, status: 'error' }, null, 'logs', 'channel-error'), 'channel-logs');
  assert.equal(pickView({ ...ch, container_state: 'unknown' }, null, 'rules', 'channel-overview'), 'channel-rules');
  assert.equal(pickView({ ...ch, status: 'logged_in' }, null, null, 'channel-login'), 'channel-login');
  assert.equal(pickView(ch, { awaiting_target: true }, null), 'channel-login');
});

test('显示状态按 DESIGN 4.3 的顺序取第一个成立的', () => {
  const cases = [
    [{ status: 'logged_in', container_state: 'unknown' }, { connected: true }, 'unknown'],
    [{ status: 'running', container_state: 'unknown' }, null, 'unknown'],
    [{ status: 'stopped', container_state: 'unknown' }, null, 'stopped'],
    [{ status: 'error', container_state: 'unknown' }, null, 'unknown'],
    [{ status: 'logged_in' }, { probe_failed: true }, 'unknown'],
    [{ status: 'error' }, { probe_failed: true }, 'unknown'],
    [{ status: 'stopped' }, { probe_failed: true }, 'stopped'],
    [{ status: 'stopped' }, null, 'stopped'],
    [{ status: 'logged_in', stop_pending: true }, null, 'stopped'],
    [{ status: 'error' }, null, 'error'],
    [{ status: 'down' }, null, 'error'],
    [{ status: 'logged_in', replacement: { phase: 'provisioning' } }, null, 'error'],
    [{ status: 'running', replacement: { phase: 'awaiting_login' } }, null, 'waiting'],
    [{ status: 'logged_in', replacement: { phase: 'committed' } }, null, 'connected'],
    [{ status: 'logged_in', replacement: { phase: 'rolled_back' } }, null, 'connected'],
    [{ status: 'running', replacement: null }, { awaiting_target: true }, 'unverified'],
    [{ status: 'logged_in' }, { awaiting_target: true }, 'unverified'],
    [{ status: 'error' }, { awaiting_target: true }, 'error'],
    [{ status: 'stopped' }, { awaiting_target: true }, 'stopped'],
    [{ status: 'running' }, { awaiting_target: false }, 'waiting'],
    [{ status: 'creating' }, null, 'waiting'],
    [{ status: 'logged_in' }, null, 'connected'],
  ];
  for (const [ch, health, want] of cases) assert.equal(displayState(ch, health), want, JSON.stringify([ch, health]));
});

test('运行状态未知时不进重建或登录页，也不计入在线通道', () => {
  const ch = { id: 'unknown', status: 'logged_in', container_state: 'unknown', login_method: 'gui' };
  assert.equal(pickView(ch, { connected: true }, null), 'channel-overview');
  assert.equal(headStatus(ch).text, '状态待确认');
  assert.equal(summary([ch]).online, 0);
  assert.equal(ruleFate({ enabled: 1, kind: 'ip' }, ch), 'unknown');
});

test('运行中的通道才采样 /health', () => {
  assert.equal(isActive({ status: 'running' }), true);
  assert.equal(isActive({ status: 'logged_in' }), true);
  for (const status of ['stopped', 'error', 'down', 'creating']) assert.equal(isActive({ status }), false, status);
  assert.equal(isActive(null), false);
});

test('通道规则：域名在前，地址在后', () => {
  assert.deepEqual(channelRules({ domains: [{ id: 1 }], ips: [{ id: 2 }] }).map(r => r.id), [1, 2]);
  assert.deepEqual(channelRules({}), []);
});

test('规则「现在」一列：待登录不画成阻止（5.2 的「3 次」由核心判定）', () => {
  const on = { enabled: true };
  assert.equal(ruleFate({ enabled: false }, { status: 'logged_in' }), 'disabled');
  assert.equal(ruleFate({ enabled: 0 }, { status: 'logged_in' }), 'disabled');
  assert.equal(ruleFate({ enabled: 1 }, { status: 'logged_in' }), 'channel');
  assert.equal(ruleFate(on, { status: 'logged_in' }, { routingOff: true }), 'direct');
  assert.equal(ruleFate(on, { status: 'stopped' }), 'direct-stopped');
  assert.equal(ruleFate(on, { status: 'error', blocked: true }), 'blocked');
  assert.equal(ruleFate(on, { status: 'running' }, { blocked: true }), 'blocked');
  assert.equal(ruleFate(on, { status: 'logged_in' }), 'channel');
  assert.equal(ruleFate(on, { status: 'running' }), 'waiting');
  assert.equal(ruleFate(on, { status: 'error' }), 'waiting');
  assert.equal(ruleFate(on, { status: 'running' }, { state: 'unverified' }), 'waiting');
});

test('规则「现在」一列：没有参与分流的通道走原网络（F5 第 3 条）', () => {
  const on = { enabled: true };
  assert.equal(ruleFate(on, { status: 'logged_in', routing_enabled: false }), 'direct-unrouted');
  assert.equal(ruleFate(on, { status: 'error', blocked: true, routing_enabled: false }), 'direct-unrouted');
  assert.equal(ruleFate(on, { status: 'stopped', routing_enabled: false }), 'direct-unrouted');
  assert.equal(ruleFate({ enabled: 0 }, { status: 'logged_in', routing_enabled: false }), 'disabled');
  assert.equal(ruleFate(on, { status: 'logged_in', routing_enabled: false }, { routingOff: true }), 'direct');
  assert.equal(ruleFate(on, { status: 'logged_in', routing_enabled: true }), 'channel');
});

test('O-04：速度用累计值差分，计数归零时记 0，不出现负数', () => {
  const a = speed(null, { uploadTotal: 1000, downloadTotal: 5000 }, 1000);
  assert.equal(a.up, null);
  assert.equal(a.down, null);
  const b = speed(a.next, { uploadTotal: 3000, downloadTotal: 9000 }, 3000);
  assert.equal(b.up, 1000);
  assert.equal(b.down, 2000);
  const reset = speed(b.next, { uploadTotal: 64, downloadTotal: 128 }, 4000);
  assert.equal(reset.up, 0);
  assert.equal(reset.down, 0);
  const after = speed(reset.next, { uploadTotal: 1064, downloadTotal: 128 }, 5000);
  assert.equal(after.up, 1000);
  assert.equal(after.down, 0);
  const same = speed(after.next, { uploadTotal: 2000, downloadTotal: 200 }, 5000);
  assert.equal(same.up, null);
  const bad = speed(after.next, {}, 6000);
  assert.equal(bad.up, null);
  assert.equal(bad.next, null);
  assert.equal(speed(after.next, null, 6000).next, null);
  assert.equal(speed(after.next, { unavailable: true }, 6000).up, null);
  assert.equal(speed(bad.next, { uploadTotal: 90000, downloadTotal: 100000 }, 7000).up, null);
});

test('折线点只留最近 max 个', () => {
  assert.deepEqual(pushPoint([1, 2, 3], 4, 3), [2, 3, 4]);
  assert.deepEqual(pushPoint([], 1, 3), [1]);
});

test('顶栏分流标记五种状态', () => {
  const ok = { routing_off: false, gateway_health: 'healthy', gave_up: false, runtime: null };
  assert.equal(chipState(ok), 'on');
  assert.equal(chipState({ ...ok, config_application: { pending: true } }), 'pending');
  assert.equal(chipState({ ...ok, routing_off: true }), 'off');
  assert.equal(chipState({ ...ok, gateway_health: 'forward_dead' }), 'bad');
  assert.equal(chipState({ ...ok, gave_up: true }), 'bad');
  assert.equal(chipState({ ...ok, runtime: { phase: 'starting' } }), 'starting');
  assert.equal(chipState({ ...ok, runtime: { phase: 'dormant' } }), 'dormant');
  for (const phase of ['failed', 'releasing', 'closing']) assert.equal(chipState({ ...ok, runtime: { phase } }), 'runtime', phase);
  for (const phase of ['ready', 'waiting']) assert.equal(chipState({ ...ok, runtime: { phase } }), 'on', phase);
  assert.equal(chipState(null), null);
  assert.equal(runtimeFailed({ runtime: { phase: 'failed' } }), true);
  assert.equal(runtimeFailed({ runtime: null }), false);
});

test('在线数、规则总数、平均延迟只算已连接的', () => {
  const channels = [
    { id: 'a', status: 'logged_in', latency_ms: 30, domains: [{}], ips: [{}] },
    { id: 'b', status: 'logged_in', latency_ms: 90, domains: [{}] },
    { id: 'c', status: 'running', latency_ms: 10, domains: [] },
    { id: 'd', status: 'logged_in', latency_ms: null },
  ];
  assert.deepEqual(summary(channels, { b: { latency_ms: 50 }, d: { awaiting_target: true } }), { online: 2, rules: 3, avgLatency: 40 });
  assert.deepEqual(summary([]), { online: 0, rules: 0, avgLatency: null });
});

// /api/takeover 的一组真实字段（F1 之后 effective、domains_via 是实际状态）
const tk = over => ({ mode: 'auto', allowed: true, effective: 'tun', system_proxy_ok: false, other_app: { tun: false, system_proxy: false },
  tun: { installed: true, enabled: true }, system_proxy: { enabled: false, ours: false }, domains_via: 'none', action: null, ...over });

test('域名规则由谁处理：按 domains_via 和系统代理的实际状态；开发版、接口不存在时不判断', () => {
  assert.equal(domainsVia(tk({ domains_via: 'none' })), 'none');
  assert.equal(domainsVia(tk({ domains_via: 'system_proxy', system_proxy: { enabled: true, ours: true } })), 'system_proxy');
  assert.equal(domainsVia(tk({ domains_via: 'system_proxy' })), 'none');
  assert.equal(domainsVia(tk({ domains_via: 'system_proxy', system_proxy: { enabled: true, ours: false } })), 'none');
  assert.equal(domainsVia(tk({ domains_via: 'clash', effective: 'clash' })), 'clash');
  assert.equal(domainsVia(tk({ domains_via: 'tun' })), 'tun');
  assert.equal(domainsVia(tk({ allowed: false, domains_via: 'system_proxy' })), null);
  for (const v of [UNAVAILABLE, null, undefined]) assert.equal(domainsVia(v), null);
  assert.equal(domainsOff(tk({ domains_via: 'clash', effective: 'clash' })), null);
  assert.equal(domainsOff(UNAVAILABLE), null);
});

test('接管网络的一条：按实际状态说；已经接管时启用虚拟网卡只是建议；执行失败给重试', () => {
  const pick = t => { const x = takeoverTodo(t); return x && [x.title, x.fix, x.tip]; };
  assert.deepEqual(pick(tk({ effective: 'none', tun: { installed: false, enabled: false }, action: 'install_tun' })), ['网络还没有接管', 'settings', false]);
  assert.deepEqual(pick(tk({ effective: 'system_proxy', tun: { installed: false, enabled: false }, system_proxy_ok: true, system_proxy: { enabled: true, ours: true }, domains_via: 'system_proxy', action: 'install_tun' })), ['可以启用虚拟网卡', 'settings', true]);
  assert.deepEqual(pick(tk({ action: 'enable_system_proxy' })), ['域名规则还没有生效', 'system-proxy-ok', false]);
  assert.deepEqual(pick(tk({ effective: 'none', action: 'enable_system_proxy' })), ['网络还没有接管', 'system-proxy-ok', false]);
  assert.deepEqual(pick(tk({ effective: 'none', action: 'retry_takeover' })), ['接管没有生效', 'retry', false]);
  assert.deepEqual(pick(tk({ effective: 'none', action: 'close_other_app' })), ['网络还没有接管', null, false]);
  assert.equal(takeoverTodo(tk({ action: null })), null);
  assert.equal(takeoverTodo(tk({ allowed: false, action: 'install_tun' })), null);
  assert.equal(takeoverTodo(UNAVAILABLE), null);
});

test('只开虚拟网卡、助手没装：域名规则不说「启用虚拟网卡以后才生效」（虚拟网卡只接地址），指到设置', () => {
  const t = tk({ mode: 'tun', effective: 'none', tun: { installed: false, enabled: false }, system_proxy_ok: false, domains_via: 'none', action: 'install_tun' });
  const off = domainsOff(t);
  assert.equal(off.fix, 'settings');
  assert.match(off.text, /打开系统代理，或者配合 Clash/);
  assert.equal(takeoverTodo(t).fix, 'settings');
  // 早先的自动方式下同一组合照旧给「用系统代理接管」
  assert.equal(domainsOff({ ...t, mode: 'auto' }).fix, 'system-proxy-ok');
});
