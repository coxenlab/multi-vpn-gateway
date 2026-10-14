// 总览的纯推导（U1）：需要处理（O-02）、已自动处理（O-03）、通道卡片与标题数字（O-01）。
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile, readdir } from 'node:fs/promises';
import { needs, takeoverTip, autoHandled, rulesReordered, channelRows, headline, when, AUTO_EVENTS, REORDERED } from '../../app/ui/js/screens/home.js';
import { takeoverView, proxyAddr, clashEntryView, netNow, modeOf, wants, flowGraph, flowSvg } from '../../app/ui/js/screens/settings.js';
import { UNAVAILABLE } from '../../app/ui/js/api.js';
import { summary } from '../../app/ui/js/derive.js';
import { FORBIDDEN } from '../../app/ui/js/terms.js';

const FX = new URL('./fixtures/', import.meta.url);
test('规则待同步单列重试，不重复添加规则', () => {
  const items = needs({ system: { config_application: { pending: true } } });
  assert.equal(items.length, 1);
  assert.equal(items[0].title, '规则还没同步');
  assert.equal(items[0].action.run, 'config');
});
const json = async p => JSON.parse(await readFile(new URL(p, FX), 'utf8'));

async function scenario(name) {
  const channels = await json(`${name}/api/channels.json`);
  const health = {};
  for (const ch of channels) {
    try { health[ch.id] = await json(`${name}/api/channels/${ch.id}/health.json`); } catch { /* 没有夹具的通道不采样 */ }
  }
  return { channels, health, system: await json(`${name}/api/system.json`) };
}

const sys = (over = {}) => ({ routing_off: false, gave_up: false, vm_egress_dead: false, gateway_health: 'healthy', runtime: { phase: 'ready' }, ...over });
const rule = (id, pattern, enabled = 1) => ({ id, pattern, enabled, kind: 'domain' });

test('需要处理：待登录、出错、还没验证各一条，按钮各一个；已连接和已停止不出现', () => {
  const channels = [
    { id: 'a', name: '甲', status: 'running' },
    { id: 'b', name: '乙', status: 'error' },
    { id: 'c', name: '丙', status: 'running', login_method: 'headless' },
    { id: 'd', name: '丁', status: 'logged_in' },
    { id: 'e', name: '戊', status: 'stopped' },
  ];
  const out = needs({ channels, health: { c: { awaiting_target: true } }, system: sys() });
  assert.deepEqual(out.map(x => x.key), ['error:b', 'wait:a', 'unverified:c']);
  assert.equal(out[0].title, '乙 连不上');
  assert.equal(out[0].text, '原因还不清楚');
  assert.deepEqual(out[0].action, { label: '查看日志', go: '#/channels/b?tab=logs' });
  assert.equal(out[1].title, '甲 需要登录');
  assert.deepEqual(out[1].action, { label: '去登录', go: '#/channels/a' });
  assert.deepEqual(out[2].action, { label: '添加规则', go: '?sheet=rule-add&channel=c' });
  for (const x of out) assert.ok(x.action && x.action.label, x.key);
});

test('需要处理：还没验证时，只有不需要登录窗口的通道给「添加规则」，其余的去填内网地址（F2）', () => {
  const health = { h: { awaiting_target: true }, g: { awaiting_target: true } };
  const channels = [
    { id: 'h', name: '甲', status: 'running', login_method: 'headless' },
    { id: 'g', name: '乙', status: 'running', login_method: 'interactive' },
  ];
  const [byRule, byUrl] = needs({ channels, health, system: sys() });
  assert.equal(byRule.text, '给它加一条域名或单独的地址，我们会用它来确认');
  assert.deepEqual(byRule.action, { label: '添加规则', go: '?sheet=rule-add&channel=h' });
  assert.equal(byUrl.title, '乙 还没验证');
  assert.equal(byUrl.text, '登录以后，填一个连上才能打开的内网地址来确认');
  assert.deepEqual(byUrl.action, { label: '填写地址', go: '#/channels/g' });
  assert.ok(!/规则/.test(byUrl.text));
});

test('需要处理：处理完就消失，全都没有时返回空数组', () => {
  assert.deepEqual(needs({ channels: [{ id: 'a', name: '甲', status: 'logged_in' }], system: sys() }), []);
  assert.deepEqual(needs({}), []);
  assert.equal(headline(2, 0), '2 条通道在线，没有需要处理的事');
  assert.equal(headline(3, 2), '3 条通道在线，2 件事需要你处理');
});

test('需要处理：出错原因来自 /cause，动作按 action 分派；接口不可用时按原因不清楚处理', () => {
  const ch = [{ id: 'x/1', name: '甲', status: 'error', blocked: true, domains: [rule(1, 'a.example.test')], ips: [rule(2, '10.1.0.0/16', 0)] }];
  const go = (cause) => needs({ channels: ch, system: sys(), causes: { 'x/1': cause } })[0];
  assert.equal(go({ code: 'crash_loop', action: 'retry' }).text, '客户端启动后崩溃，1 条规则暂时被阻止');
  assert.deepEqual(go({ code: 'crash_loop', action: 'retry' }).action, { label: '重试', run: 'start', id: 'x/1' });
  assert.deepEqual(go({ code: 'runtime_down', action: 'restart_runtime' }).action, { label: '重新启动运行环境', run: 'runtime' });
  assert.deepEqual(go({ code: 'kicked', action: 'relogin' }).action, { label: '重新登录', go: '#/channels/x%2F1?tab=login' });
  assert.equal(go(UNAVAILABLE).text, '原因还不清楚，1 条规则暂时被阻止');
  assert.equal(go(UNAVAILABLE).action.label, '查看日志');
  assert.equal(go({ code: null, action: null }).text, '原因还不清楚，1 条规则暂时被阻止');
  assert.equal(go({ code: 'unknown', action: 'view_logs' }).text, '原因还不清楚，1 条规则暂时被阻止');
});

test('需要处理：运行环境放弃修复给「修复」；出口断了只提示重新打开，不给「修复」；运行环境没启动时不重复', () => {
  const heal = needs({ system: sys({ gave_up: true }) });
  assert.equal(heal.length, 1);
  assert.equal(heal[0].title, '运行环境出问题了');
  assert.deepEqual(heal[0].action, { label: '修复', run: 'heal' });
  const egress = needs({ system: sys({ vm_egress_dead: true }) });
  assert.equal(egress.length, 1);
  assert.equal(egress[0].key, 'runtime');
  assert.equal(egress[0].action, null);
  assert.match(egress[0].text, /重新打开/);
  // 两种同时出现时按放弃修复处理，给「修复」
  assert.deepEqual(needs({ system: sys({ gave_up: true, vm_egress_dead: true }) })[0].action, { label: '修复', run: 'heal' });
  assert.deepEqual(needs({ system: sys({ vm_egress_dead: true, runtime: { phase: 'dormant' } }) }), []);
  assert.deepEqual(needs({ system: sys({ gave_up: true, runtime: { phase: 'dormant' } }) }), []);
  assert.deepEqual(needs({ system: sys({ gave_up: true, runtime: { phase: 'starting' } }) }), []);
});

test('需要处理：接管网络的 action 对应一句话；系统代理那条带按钮；接口不存在时不出现', () => {
  const base = { mode: 'auto', effective: 'none' };
  const sp = needs({ system: sys(), takeover: { ...base, action: 'enable_system_proxy' } });
  assert.equal(sp.length, 1);
  assert.equal(sp[0].text, '可以改用系统代理接管，需要你确认一次');
  assert.deepEqual(sp[0].action, { label: '用系统代理接管', run: 'system-proxy-ok' });
  assert.deepEqual(needs({ system: sys(), takeover: { ...base, action: 'install_tun' } })[0].action, { label: '启用虚拟网卡', go: '#/settings' });
  assert.equal(needs({ system: sys(), takeover: { ...base, action: 'close_other_app' } })[0].action, null);
  assert.deepEqual(needs({ system: sys(), takeover: { ...base, action: null } }), []);
  assert.deepEqual(needs({ system: sys(), takeover: UNAVAILABLE }), []);
});

test('需要处理：已经用系统代理接管时不说「网络还没有接管」，启用虚拟网卡只是建议、不计数（F4 第 2 条）', async () => {
  // 自动模式、没装虚拟网卡、已确认系统代理并且开着（核心的真实组合，settings/proxy 夹具）
  const t = await json('settings/proxy/api/takeover.json');
  assert.deepEqual([t.effective, t.action], ['system_proxy', 'install_tun']);
  assert.deepEqual(needs({ system: sys(), takeover: t }), []);
  const tip = takeoverTip(t);
  assert.equal(tip.tone, 'tip');
  assert.doesNotMatch(tip.title + tip.text, /还没有接管/);
  assert.deepEqual(tip.action, { label: '启用虚拟网卡', go: '#/settings' });
  assert.equal(headline(2, needs({ system: sys(), takeover: t }).length), '2 条通道在线，没有需要处理的事');
  const v = takeoverView(t);
  assert.deepEqual(v.now.map(r => r.state), ['ok', 'ok', 'idle']);
  assert.match(v.now[0].how, /系统代理接住/);
  assert.equal(v.tip, true);
  assert.deepEqual(v.actions, ['install_tun']);
  assert.doesNotMatch(v.hint, /才能接管网络/);
  // 还没接管时照旧：计入需要处理，设置页同时给两个按钮
  const none = await json('settings/install/api/takeover.json');
  assert.equal(takeoverTip(none), null);
  assert.equal(needs({ system: sys(), takeover: none })[0].title, '网络还没有接管');
  assert.deepEqual(takeoverView(none).actions, ['install_tun', 'enable_system_proxy']);
  assert.equal(takeoverView(none).tip, false);
  // 只开虚拟网卡、助手没装：两个开关是独立的，同样可以改开系统代理
  assert.deepEqual(takeoverView({ ...none, mode: 'tun' }).actions, ['install_tun', 'enable_system_proxy']);
  // 虚拟网卡开着、域名规则要确认系统代理：不说「网络还没有接管」
  const tun = await json('home/api/takeover.json');
  assert.equal(needs({ system: sys(), takeover: tun })[0].title, '域名规则还没有生效');
  assert.match(takeoverView(tun).now[0].how, /虚拟网卡接住/);
  assert.equal(takeoverView(tun).now[1].state, 'off');
});

test('需要处理：接管执行失败出现一条带「重试」；设置页同样给「重试」（F4 第 3 条）', async () => {
  const t = await json('settings/retry/api/takeover.json');
  const out = needs({ system: sys(), takeover: t });
  assert.equal(out.length, 1);
  assert.equal(out[0].key, 'takeover:retry_takeover');
  assert.equal(out[0].title, '接管没有生效');
  assert.deepEqual(out[0].action, { label: '重试', run: 'retry-takeover', mode: 'auto' });
  assert.equal(takeoverTip(t), null);
  const v = takeoverView(t);
  assert.deepEqual(v.actions, ['retry']);
  assert.deepEqual(v.now.map(r => r.state), ['off', 'off', 'idle']);
  assert.equal(v.hint, out[0].text);
  // 系统代理指向别的地址：重试不会覆盖它，说清楚先去关掉
  const foreign = { ...t, system_proxy: { enabled: true, ours: false } };
  assert.match(needs({ system: sys(), takeover: foreign })[0].text, /系统代理现在指向别的地址/);
  assert.match(takeoverView(foreign).hint, /系统代理现在指向别的地址/);
  for (const x of [out[0], needs({ system: sys(), takeover: foreign })[0]]) assert.ok(!FORBIDDEN.test(x.title + x.text));
});

test('需要处理：分流总开关关着、或通道不参与分流时，不写「N 条规则暂时被阻止」（F4 第 4 条）', () => {
  const ch = { id: 'x', name: '甲', status: 'error', blocked: true, domains: [rule(1, 'a.example.test')], ips: [] };
  assert.equal(needs({ channels: [ch], system: sys() })[0].text, '原因还不清楚，1 条规则暂时被阻止');
  assert.equal(needs({ channels: [ch], system: sys({ routing_off: true }) })[0].text, '原因还不清楚');
  assert.equal(needs({ channels: [{ ...ch, routing_enabled: false }], system: sys() })[0].text, '原因还不清楚');
  const wait = { ...ch, status: 'running', login_method: 'interactive' };
  assert.equal(needs({ channels: [wait], system: sys({ routing_off: true }) })[0].text, '登录以后才能访问内网');
  assert.equal(needs({ channels: [{ ...wait, routing_enabled: false }], system: sys() })[0].text, '登录以后才能访问内网');
  assert.equal(needs({ channels: [wait], system: sys() })[0].text, '1 条规则暂时被阻止');
});

test('需要处理：不需要登录窗口的通道待登录时给「查看」，不给「去登录」（F4 第 5 条）', async () => {
  const out = needs({ channels: [{ id: 'w', name: '甲', status: 'running', login_method: 'headless' }], system: sys() });
  assert.equal(out[0].title, '甲 还没连通');
  assert.deepEqual(out[0].action, { label: '查看', go: '#/channels/w' });
  for (const m of ['interactive', 'byo', 'password']) {
    assert.equal(needs({ channels: [{ id: 'w', name: '甲', status: 'running', login_method: m }], system: sys() })[0].action.label, '去登录', m);
  }
  const s = await scenario('default');
  const labels = Object.fromEntries(needs(s).map(x => [x.key, x.action && x.action.label]));
  assert.equal(labels['wait:demo-wh1'], '查看');
});

test('设置：任何接管方式下都显示通用代理地址（F4 第 6 条）', async () => {
  for (const d of ['auto', 'clash', 'dev', 'fallback', 'install', 'retry', 'proxy']) {
    assert.equal(proxyAddr(await json(`settings/${d}/api/entry/setup-commands.json`)), '127.0.0.1:47231', d);
  }
  assert.equal(proxyAddr({ port: '?' }), '');
  assert.equal(proxyAddr(null), '');
  // 配合 Clash 的片段按真实输出，最后的说明不含内部用词
  for (const d of ['auto', 'clash', 'dev', 'fallback', 'install']) {
    const snip = await json(`settings/${d}/api/clash-snippet.json`);
    assert.match(snip, /# ③ 无 Clash 时/);
    for (const line of snip.split('\n').filter(l => l.startsWith('#'))) assert.ok(!FORBIDDEN.test(line), line);
  }
});

test('设置：两个开关和四种方式一一对应；想开的和真开着的分开', () => {
  assert.deepEqual([modeOf(false, false), modeOf(true, false), modeOf(false, true), modeOf(true, true)],
    ['clash', 'tun', 'system_proxy', 'tun_system_proxy']);
  assert.deepEqual(wants({ mode: 'auto', want: { tun: true, system_proxy: false } }), { tun: true, proxy: false });
  assert.deepEqual(wants({ mode: 'tun_system_proxy' }), { tun: true, proxy: true });
  assert.deepEqual(wants({ mode: 'manual' }), { tun: false, proxy: false });
  // 想开虚拟网卡但助手没装：开关显示关，小字说明要输一次密码
  const v = takeoverView({ mode: 'tun', allowed: true, effective: 'none', want: { tun: true, system_proxy: false },
    tun: { installed: false, enabled: false }, domains_via: 'none', action: 'install_tun' });
  assert.equal(v.tun.on, false);
  assert.match(v.tun.sub, /管理员密码/);
  assert.deepEqual(v.actions, ['install_tun', 'enable_system_proxy']);
});

test('设置：现在的网络按实际状态说三类流量各自由谁接；Clash 在跑时提醒系统代理会让它失效', async () => {
  // 虚拟网卡开着、Clash 在跑：地址归虚拟网卡，域名归 Clash（要用户自己确认），其余不碰
  const t = await json('settings/clash/api/takeover.json');
  const now = netNow(t);
  assert.deepEqual(now.map(r => [r.key, r.state]), [['ip', 'ok'], ['domain', 'wait'], ['rest', 'idle']]);
  assert.match(now[0].how, /虚拟网卡接住/);
  assert.match(now[1].how, /Clash/);
  assert.match(now[2].how, /照旧走 Clash/);
  // 流向图：每条链从「你的程序」出发，画现在实际经过的环节
  assert.deepEqual(now.map(r => r.path.join('>')), ['你的程序>虚拟网卡>入口>通道>内网', '你的程序>Clash>入口>通道>内网', '你的程序>Clash>外网']);
  const v = takeoverView(t);
  assert.deepEqual([v.tun.on, v.proxy.on], [true, false]);
  assert.match(v.proxy.warn, /用 Clash 的话不要开/);
  // 系统代理开着、Clash 也在跑：其余流量走不到 Clash，说出来
  const both = { ...t, mode: 'tun_system_proxy', want: { tun: true, system_proxy: true }, system_proxy: { enabled: true, ours: true }, domains_via: 'system_proxy' };
  assert.deepEqual(netNow(both).map(r => r.state), ['ok', 'ok', 'wait']);
  assert.match(netNow(both)[2].how, /Clash 接不到/);
  assert.deepEqual(netNow(both)[2].path, ['你的程序', '原来的网络']);
  assert.match(takeoverView(both).proxy.warn, /会打不开/);
  // 都没开、只有 Clash：内网地址多半不通
  const bare = { ...t, mode: 'clash', effective: 'clash', want: { tun: false, system_proxy: false }, tun: { installed: true, enabled: false } };
  assert.equal(netNow(bare)[0].state, 'wait');
  assert.match(netNow(bare)[0].how, /打开虚拟网卡/);
  assert.deepEqual(netNow(bare)[0].path, ['你的程序', 'Clash', '原来的网络']);
  // 节点连线图：入口并进通道一个节点；同一段上的两类流量错开画；没接住的线不带流动的光点
  const g = flowGraph(now);
  assert.deepEqual(g.nodes.map(n => [n.id, n.col]), [['你的程序', 0], ['虚拟网卡', 1], ['Clash', 1], ['通道', 2], ['内网', 3], ['外网', 3]]);
  assert.equal(g.edges.length, 8);
  assert.equal(new Set(g.edges.map(e => e.d)).size, 8);
  const svg = flowSvg(netNow({ ...bare, clash_running: false, domains_via: 'none' }));
  // 都没接住：只有其余流量那一条照旧在走
  assert.equal((svg.match(/set-map-run/g) || []).length, 1);
  assert.match(svg, /原来的网络/);
  assert.ok((flowSvg(now).match(/set-map-run/g) || []).length === 8);
  // 没有 Clash：不提醒
  assert.equal(takeoverView({ ...t, clash_running: false }).proxy.warn, '');
  for (const r of [...now, ...netNow(both), ...netNow(bare)]) assert.ok(!FORBIDDEN.test(r.what + r.how + r.path.join('')));
});

test('设置：Clash 运行与配置接入严格区分', () => {
  const running = clashEntryView({ clash_running: true, effective: 'tun', domains_via: 'clash' }, { running: true });
  assert.equal(running.state, 'wait');
  assert.match(running.text, /需要你在 Clash 里确认/);
  assert.doesNotMatch(running.title + running.text, /已接入|已同步/);
  const shadowed = clashEntryView({ clash_running: true, system_proxy: { enabled: true, ours: true } }, { running: true });
  assert.match(shadowed.text, /流量到不了它/);
  assert.equal(clashEntryView({ clash_running: false }, null).state, 'off');
});

test('需要处理：default 夹具里出错、待登录、还没验证都列出来，已连接和已停止不列', async () => {
  const s = await scenario('default');
  assert.deepEqual(needs(s).map(x => x.key), ['error:demo-factory1', 'wait:demo-branch1', 'wait:demo-wh1', 'unverified:demo-new1']);
  assert.deepEqual(needs(await scenario('empty')), []);
});

test('已自动处理：只认白名单里的事件，用户操作和失败事件不算；新的在前，同一句话只留一条', () => {
  const channels = [{ id: 'c1', name: '甲', status: 'logged_in' }];
  const now = new Date(2026, 8, 29, 12, 0).getTime();
  const at = (h, m) => new Date(2026, 8, 29, h, m).getTime();
  const events = [
    { seq: 1, ts_ms: at(9, 0), src: 'watchdog', event: 'heal_done', detail: {} },
    { seq: 2, ts_ms: at(9, 30), src: 'watchdog', event: 'heal_done', detail: {} },
    { seq: 3, ts_ms: at(10, 0), src: 'audit', event: 'channel_start', detail: {} },
    { seq: 4, ts_ms: at(10, 5), src: 'watchdog', event: 'heal_failed', detail: {} },
    { seq: 5, ts_ms: at(10, 10), src: 'api', event: 'heal_done', detail: {} },
    { seq: 6, ts_ms: at(11, 18), src: 'manager', event: 'dns_cache_recovered', detail: { cid: 'c1' } },
    { seq: 0, ts_ms: new Date(2026, 8, 27, 7, 31).getTime(), src: 'vm', event: 'egress_guard_applied', detail: {} },
  ];
  const out = autoHandled({ events, channels, now });
  assert.deepEqual(out.map(x => [x.time, x.text]), [
    ['11:18', '甲 的域名访问已自动恢复'],
    ['09:30', '分流中断过一次，已自动修复'],
    ['09-27', '内网访问防护已自动生效'],
  ]);
  assert.equal(autoHandled({ events, channels, now, limit: 1 }).length, 1);
});

test('已自动处理：一条都没有时返回空数组（整块不显示）', async () => {
  assert.deepEqual(autoHandled({ events: [], channels: [] }), []);
  assert.deepEqual(autoHandled({ events: [{ seq: 1, src: 'audit', event: 'channel_start' }] }), []);
  const empty = await scenario('empty');
  assert.deepEqual(autoHandled({ events: (await json('empty/api/events.json')).events, channels: empty.channels }), []);
});

test('已自动处理：白名单的每一句都不含内部用词，通道名按 detail.cid 取', () => {
  for (const [name, fn] of Object.entries(AUTO_EVENTS)) {
    for (const text of [fn(null), fn('甲')]) assert.ok(!FORBIDDEN.test(text), name + ': ' + text);
  }
  assert.equal(AUTO_EVENTS.recovered('甲'), '甲 已自动恢复');
  assert.equal(AUTO_EVENTS.recovered(null), '分流已自动恢复');
  assert.ok(!FORBIDDEN.test(REORDERED));
});

test('规则排序：宽的规则先插入、具体的后插入且属于另一条通道时，记一条「已自动排好顺序」', () => {
  const A = { id: 'A', status: 'logged_in', domains: [rule(1, 'example.test')] };
  const B = { id: 'B', status: 'logged_in', domains: [rule(2, 'corp.example.test')] };
  assert.equal(rulesReordered([A, B]), true);
  assert.equal(autoHandled({ channels: [A, B] })[0].text, REORDERED);
  // 推导出来的这一条没有发生时间，时间一栏写「当前」，和其他行对齐
  assert.equal(autoHandled({ channels: [A, B] })[0].time, '当前');
  // 具体的先插入：两种顺序结果一样
  assert.equal(rulesReordered([{ ...A, domains: [rule(3, 'example.test')] }, { ...B, domains: [rule(2, 'corp.example.test')] }]), false);
  // 同一条通道里重叠不算
  assert.equal(rulesReordered([{ id: 'A', status: 'logged_in', domains: [rule(1, 'example.test'), rule(2, 'corp.example.test')] }]), false);
  // 停用的规则、已停止的通道不参与
  assert.equal(rulesReordered([{ ...A, domains: [rule(1, 'example.test', 0)] }, B]), false);
  assert.equal(rulesReordered([{ ...A, status: 'stopped' }, B]), false);
  // *. 和 +. 前缀按同一个域名算
  assert.equal(rulesReordered([{ ...A, domains: [rule(1, '*.example.test')] }, { ...B, domains: [rule(2, '+.corp.example.test')] }]), true);
  // 不相关的后缀不算命中
  assert.equal(rulesReordered([{ ...A, domains: [rule(1, 'ample.test')] }, { ...B, domains: [rule(2, 'corp.example.test')] }]), false);
  assert.equal(rulesReordered([]), false);
});

test('通道卡片：已停止的不列，已连接的按今天流量排在前面，流量接口不可用时不显示流量', async () => {
  const s = await scenario('default');
  const traffic = { available: true, channels: { 'demo-hq1': { up: 10, down: 90 }, 'demo-hq2': { up: 100, down: 300 } } };
  const rows = channelRows({ ...s, traffic });
  assert.deepEqual(rows.map(r => r.id), ['demo-hq2', 'demo-hq1', 'demo-branch1', 'demo-new1', 'demo-factory1', 'demo-wh1']);
  assert.deepEqual(rows.slice(0, 2).map(r => [r.total, r.pct, r.latency]), [[400, 100, 65], [100, 25, 42]]);
  assert.deepEqual(rows.map(r => r.state).slice(2), ['waiting', 'unverified', 'error', 'waiting']);
  for (const t of [null, UNAVAILABLE, { available: false, channels: {} }]) {
    const plain = channelRows({ ...s, traffic: t });
    assert.deepEqual(plain.slice(0, 2).map(r => [r.id, r.total, r.pct]), [['demo-hq1', null, 0], ['demo-hq2', null, 0]]);
  }
  // 流量表里没有这条通道：记 0，不当成不可用
  assert.deepEqual(channelRows({ ...s, traffic: { available: true, channels: {} } }).slice(0, 2).map(r => r.total), [0, 0]);
});

test('标题数字和通道卡片同源（O-01）：在线数等于卡片里已连接的行数', async () => {
  for (const name of ['default', 'home', 'runtime-down', 'empty']) {
    const s = await scenario(name);
    const sum = summary(s.channels, s.health);
    assert.equal(sum.online, channelRows(s).filter(r => r.state === 'connected').length, name);
  }
});

test('时间：当天显示时分，更早的显示月日；没有时间为空', () => {
  const now = new Date(2026, 8, 29, 12, 0).getTime();
  assert.equal(when(new Date(2026, 8, 29, 7, 5).getTime(), now), '07:05');
  assert.equal(when(new Date(2026, 8, 28, 23, 59).getTime(), now), '09-28');
  assert.equal(when(null, now), '');
  assert.equal(when(0, now), '');
});

test('home 夹具：给截图用的场景字段齐全', async () => {
  const s = await scenario('home');
  const out = needs({ ...s, causes: { 'demo-factory1': await json('home/api/channels/demo-factory1/cause.json') }, takeover: await json('home/api/takeover.json') });
  assert.deepEqual(out.map(x => x.key), ['error:demo-factory1', 'wait:demo-branch1', 'wait:demo-wh1', 'unverified:demo-new1', 'takeover:enable_system_proxy']);
  const auto = autoHandled({ events: (await json('home/api/events.json')).events, channels: s.channels, health: s.health });
  assert.equal(auto[0].text, REORDERED);
  assert.ok(auto.some(a => a.text === '总部主线路(示例) 的域名访问已自动恢复'));
  const seq = (await json('home/api/connections.json')).__sequence__;
  for (let i = 1; i < seq.length; i++) assert.ok(seq[i].downloadTotal > seq[i - 1].downloadTotal);
  assert.ok((await readdir(new URL('home/api/traffic/', FX))).includes('today.json'));
});
