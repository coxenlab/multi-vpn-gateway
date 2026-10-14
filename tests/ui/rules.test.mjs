// 规则页（U5）的纯函数：「现在」一列、搜索、不生效说明（T-05）、流向图分组（R-16 的数据一侧）、
// 测试地址结果（R-12 的界面一侧）、添加规则的预览与结果（R-10、R-11 的界面一侧）。
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { allRules, filterRows, fateFor, takeoverNote, NO_PROXY_APPS } from '../../app/ui/js/screens/rules.js';
import { flowGroups, flowState } from '../../app/ui/js/screens/rules-flow.js';
import { testView } from '../../app/ui/js/screens/rules-test.js';
import { fateOf } from '../../app/ui/js/screens/channel-rules.js';
import { splitTokens, classifyToken, preview, previewText, summarize } from '../../app/ui/js/screens/rule-add.js';
import { UNAVAILABLE, ApiError } from '../../app/ui/js/api.js';
import { FORBIDDEN, FATE } from '../../app/ui/js/terms.js';

const FX = new URL('./fixtures/rules/', import.meta.url);
const json = async p => JSON.parse(await readFile(new URL(p, FX), 'utf8'));

async function scenario(name) {
  const channels = await json(`${name}/api/channels.json`);
  const health = {};
  for (const ch of channels) {
    try { health[ch.id] = await json(`${name}/api/channels/${ch.id}/health.json`); } catch { /* 没有运行的通道不采样 */ }
  }
  return { channels, health, system: await json(`${name}/api/system.json`), takeover: await json(`${name}/api/takeover.json`) };
}

const byId = (channels, id) => channels.find(c => c.id === id);

test('IPv6 在不支持的接管方式下四处都不宣称经过通道', () => {
  const rule = { id: 1, kind: 'ip', pattern: 'fd00::/64', enabled: 1 };
  const ch = { id: 'v6', name: '测试通道', status: 'logged_in', domains: [], ips: [rule] };
  for (const effective of ['tun', 'system_proxy']) {
    const takeover = { effective, domains_via: effective };
    assert.equal(fateFor(rule, ch, null, null, takeover), 'unsupported');
    assert.equal(fateOf(rule, ch, null, null, takeover), 'unsupported');
    assert.equal(flowGroups([ch], { takeover })[0].rules[0].fate, 'unsupported');
    for (const result of ['channel', 'blocked']) {
      const view = testView({ kind: 'ip', result, rule, channel: ch, reachable: { ok: true } },
        { q: 'fd00::1', channels: [ch], takeover });
      assert.equal(view.sub.text, FATE.unsupported.text);
      assert.ok(!JSON.stringify(view).includes('能通'));
    }
    assert.equal(fateFor({ ...rule, enabled: 0 }, ch, null, null, takeover), 'disabled');
    assert.equal(fateFor(rule, { ...ch, status: 'stopped' }, null, null, takeover), 'direct-stopped');
    assert.equal(fateFor(rule, { ...ch, routing_enabled: false }, null, null, takeover), 'direct-unrouted');
  }
  assert.equal(fateFor(rule, ch, null, null, { effective: 'clash' }), 'channel');
  assert.equal(fateFor(rule, ch, null, null, { effective: 'tun', allowed: false }), 'channel');
});

test('未接管时四处规则去向一致，已停用和已停止的规则仍放行', async () => {
  const s = await scenario('tun-install');
  const ch = { id: 'fixture', name: '测试通道', status: 'logged_in',
    domains: [{ id: 1, kind: 'domain', pattern: 'corp.example', enabled: 1 }],
    ips: [{ id: 2, kind: 'ip', pattern: '192.0.2.1/32', enabled: 1 }] };
  assert.match(takeoverNote(s.takeover).text, /^规则现在还没有生效/);
  for (const rule of [...ch.domains, ...ch.ips]) {
    assert.equal(fateFor(rule, ch, null, null, s.takeover), 'inactive');
    assert.equal(fateOf(rule, ch, null, null, s.takeover), 'inactive');
    const view = testView({ kind: rule.kind, result: 'channel', rule, channel: ch, reachable: { ok: true, latency_ms: 1 } },
      { q: rule.pattern, channels: [ch], takeover: s.takeover });
    assert.equal(view.sub.text, '规则现在还没有生效');
    assert.ok(!JSON.stringify(view).includes('能通'));
    assert.equal(fateFor({ ...rule, enabled: 0 }, ch, null, null, s.takeover), 'disabled');
    assert.equal(fateFor(rule, { ...ch, status: 'stopped' }, null, null, s.takeover), 'direct-stopped');
    assert.equal(fateFor(rule, { ...ch, routing_enabled: false }, null, null, s.takeover), 'direct-unrouted');
    assert.equal(fateFor(rule, { ...ch, blocked: true }, null, null, s.takeover), 'inactive');
    for (const t of [null, UNAVAILABLE, { ...s.takeover, allowed: false },
      { effective: 'clash', domains_via: 'clash' }, { effective: 'system_proxy', domains_via: 'system_proxy' }]) {
      assert.equal(fateFor(rule, ch, null, null, t), 'channel');
    }
  }
  const groups = flowGroups([ch], { takeover: s.takeover });
  assert.equal(groups[0].state.text, '规则还没生效');
  assert.deepEqual(groups[0].rules.map(r => r.fate), ['inactive', 'inactive']);
  const mixed = flowGroups([ch], { takeover: { effective: 'tun', domains_via: 'none' } });
  assert.deepEqual(mixed[0].rules.map(r => r.fate), ['inactive', 'channel']);
});

test('「现在」一列：按 DESIGN 4.3，待登录不算阻止，enabled 为 0 时是已停用', async () => {
  const { channels, health, system } = await scenario('tun');
  const fate = pattern => {
    const row = allRules(channels).find(r => r.rule.pattern === pattern);
    return fateFor(row.rule, row.ch, health[row.ch.id], system);
  };
  assert.equal(fate('git.corp.example'), 'channel');
  assert.equal(fate('10.99.0.0/24'), 'waiting');
  assert.equal(fate('erp.vendor.example'), 'direct-stopped');
  assert.equal(fate('10.30.0.0/16'), 'blocked');
  assert.equal(fate('old.cloud.example'), 'disabled');
  // 分流总开关关着：全部走原网络；停用的规则仍显示已停用
  const off = { ...system, routing_off: true };
  const row = allRules(channels).find(r => r.rule.pattern === 'git.corp.example');
  assert.equal(fateFor(row.rule, row.ch, health[row.ch.id], off), 'direct');
  // 待登录的通道连续掉线被阻止
  const park = byId(channels, 'demo-park');
  assert.equal(fateFor(park.ips[0], park, { ...health['demo-park'], blocked: true }, system), 'blocked');
});

test('全部规则按通道顺序、每条通道先域名后网段；搜索地址、备注、通道名，不分大小写', async () => {
  const { channels } = await scenario('tun');
  const rows = allRules(channels);
  assert.equal(rows.length, 10);
  assert.deepEqual(rows.slice(1, 3).map(r => r.rule.pattern), ['git.corp.example', '10.20.16.0/24']);
  assert.deepEqual(filterRows(rows, 'GIT').map(r => r.rule.pattern), ['git.corp.example']);
  assert.deepEqual(filterRows(rows, '构建机').map(r => r.rule.pattern), ['10.20.16.8/32']);
  assert.equal(filterRows(rows, '云平台').length, 4);
  assert.equal(filterRows(rows, '  ').length, 10);
  assert.equal(filterRows(rows, 'nothing.example').length, 0);
});

test('不生效说明：虚拟网卡时按 domains_via 写两种；其他方式和接口不存在时不写（T-05）', async () => {
  // 5.13 没落地（K12 没执行）：系统代理没开（rules/tun）和开着指向自己（rules/proxy）两种情况，说法不同
  const offNote = takeoverNote((await scenario('tun')).takeover);
  const onNote = takeoverNote((await scenario('proxy')).takeover);
  assert.notEqual(offNote.text, onNote.text);
  assert.match(offNote.text, /^域名规则现在还没有生效/);
  assert.doesNotMatch(offNote.text, /由系统代理处理/);
  const tun = takeoverNote({ effective: 'tun', domains_via: 'tun' });
  assert.match(tun.text, /dig、nslookup 和开了安全 DNS 的浏览器不走这里/);
  const proxy = takeoverNote((await scenario('proxy')).takeover);
  assert.match(proxy.text, /域名规则由系统代理处理，不读系统代理的程序不生效/);
  for (const app of ['curl', 'git', 'ssh']) assert.ok(proxy.text.includes(app), app);
  assert.deepEqual([...proxy.apps], [...NO_PROXY_APPS]);
  assert.equal(takeoverNote({ effective: 'clash', domains_via: 'clash' }), null);
  assert.equal(takeoverNote({ effective: 'system_proxy', domains_via: 'system_proxy' }), null);
  assert.equal(takeoverNote({ effective: 'tun', domains_via: 'clash' }), null);
  assert.equal(takeoverNote(UNAVAILABLE), null);
  assert.equal(takeoverNote(null), null);
  for (const n of [tun, proxy, offNote]) assert.ok(!FORBIDDEN.test(n.text));
});

test('域名规则没有生效：写明，并给能让它生效的动作（F4 第 1 条）', async () => {
  const live = { mode: 'auto', allowed: true, other_app: { tun: false, system_proxy: false }, tun: { installed: true, enabled: true } };
  const off = sp => ({ enabled: false, ours: false, ...sp });
  // 虚拟网卡开着、系统代理还没确认：用系统代理接管
  const tun = takeoverNote((await scenario('tun')).takeover);
  assert.deepEqual([tun.off, tun.fix], [true, 'system-proxy-ok']);
  assert.match(tun.text, /用系统代理接管以后才生效/);
  // 接管执行过但没生效：重试；系统代理指向别人的地址时说清楚
  const retry = takeoverNote({ ...live, effective: 'tun', system_proxy_ok: true, system_proxy: off(), domains_via: 'none', action: 'retry_takeover' });
  assert.equal(retry.fix, 'retry');
  const foreign = takeoverNote({ ...live, effective: 'tun', system_proxy_ok: true, system_proxy: { enabled: true, ours: false }, domains_via: 'none', action: 'retry_takeover' });
  assert.equal(foreign.fix, 'retry');
  assert.match(foreign.text, /系统代理现在指向别的地址/);
  // 没有接管：没装虚拟网卡也没确认系统代理 → 用系统代理接管；确认过但还没执行 → 去设置
  assert.equal(takeoverNote({ ...live, effective: 'none', tun: { installed: false, enabled: false }, system_proxy_ok: false, system_proxy: off(), domains_via: 'none', action: 'install_tun' }).fix, 'system-proxy-ok');
  assert.equal(takeoverNote({ ...live, effective: 'none', tun: { installed: false, enabled: false }, system_proxy_ok: true, system_proxy: off(), domains_via: 'none', action: 'install_tun' }).fix, 'settings');
  // 另一个应用在接管：没有按钮
  const other = takeoverNote({ ...live, effective: 'none', other_app: { tun: true, system_proxy: false }, system_proxy: off(), domains_via: 'none', action: 'close_other_app' });
  assert.equal(other.fix, null);
  assert.match(other.text, /另一个应用/);
  // 核心还报 system_proxy、系统代理却没指向自己时，界面同样按没有生效算
  assert.equal(takeoverNote({ ...live, effective: 'tun', system_proxy: off(), domains_via: 'system_proxy', action: null }).off, true);
  // 开发版不检测，不写
  assert.equal(takeoverNote({ ...live, allowed: false, effective: 'none', system_proxy: off(), domains_via: 'system_proxy', action: null }), null);
  for (const n of [tun, retry, foreign, other]) assert.ok(!FORBIDDEN.test(n.text), n.text);
});

test('「现在」一列：域名规则没有生效时，本该进通道的域名规则写「还没生效」，地址规则不受影响', async () => {
  const { channels, health, system, takeover } = await scenario('tun');
  const p = await scenario('proxy');
  const fate = (pattern, t) => {
    const row = allRules(channels).find(r => r.rule.pattern === pattern);
    return fateFor(row.rule, row.ch, health[row.ch.id], system, t);
  };
  assert.equal(fate('git.corp.example', takeover), 'inactive');
  assert.equal(fate('git.corp.example', p.takeover), 'channel');
  assert.equal(fate('git.corp.example', UNAVAILABLE), 'channel');
  assert.equal(fate('10.20.16.0/24', takeover), 'channel');
  // 停用的、走原网络的照旧
  assert.equal(fate('old.cloud.example', takeover), 'disabled');
  assert.equal(fate('erp.vendor.example', takeover), 'direct-stopped');
  assert.equal(FATE.inactive.text, '还没生效');
});

test('流向图：只列有规则的通道，规则和列表一条不差；通道状态写清对规则的影响', async () => {
  const { channels, health, system } = await scenario('tun');
  const groups = flowGroups(channels, { health, system });
  assert.deepEqual(groups.map(g => g.ch.id), ['demo-lab', 'demo-rd', 'demo-cloud', 'demo-park', 'demo-vendor', 'demo-dc']);
  const flat = groups.flatMap(g => g.rules.map(r => r.rule.id)).sort((a, b) => a - b);
  assert.deepEqual(flat, allRules(channels).map(r => r.rule.id).sort((a, b) => a - b));
  const text = id => groups.find(g => g.ch.id === id).state;
  assert.deepEqual(text('demo-rd'), { kind: 'ok', text: '已连接' });
  assert.deepEqual(text('demo-park'), { kind: 'wait', text: '待登录，流量仍进通道' });
  assert.deepEqual(text('demo-vendor'), { kind: 'off', text: '已停止，走原网络' });
  assert.deepEqual(text('demo-dc'), { kind: 'bad', text: '出错，暂时阻止' });
  assert.equal(groups.find(g => g.ch.id === 'demo-vendor').idle, true);
  assert.equal(groups.find(g => g.ch.id === 'demo-cloud').rules.find(r => r.rule.pattern === 'old.cloud.example').on, false);
  assert.deepEqual(flowState(byId(channels, 'demo-park'), { blocked: true }, system), { kind: 'wait', text: '待登录，暂时阻止' });
  // 出错且被阻止写「暂时阻止」；库里是运行态、运行环境里找不到（down，核心报 blocked:false）时流量仍进通道
  assert.deepEqual(flowState({ id: 'e', status: 'error', blocked: true }, null, system), { kind: 'bad', text: '出错，暂时阻止' });
  assert.deepEqual(flowState({ id: 'd', status: 'down', blocked: false }, null, system), { kind: 'bad', text: '出错，流量仍进通道' });
  assert.deepEqual(flowState(byId(channels, 'demo-rd'), health['demo-rd'], { routing_off: true }), { kind: 'off', text: '分流已关闭，走原网络' });
  // 没有参与分流的通道：已连接、出错被阻止都写走原网络（同规则列表的 direct-unrouted）
  assert.deepEqual(flowState({ ...byId(channels, 'demo-rd'), routing_enabled: false }, health['demo-rd'], system), { kind: 'off', text: '不参与分流，走原网络' });
  assert.deepEqual(flowState({ id: 'e', status: 'error', blocked: true, routing_enabled: false }, null, system), { kind: 'off', text: '不参与分流，走原网络' });
  // 搜索时只留有命中的通道
  assert.deepEqual(flowGroups(channels, { health, system, q: '10.20.16' }).map(g => g.ch.id), ['demo-lab', 'demo-rd']);
  assert.deepEqual(flowGroups([{ id: 'x', name: '空', status: 'logged_in', domains: [], ips: [] }]), []);
});

test('测试地址：命中、未命中、命中已停止通道、命中出错通道四种结果（R-12 的界面一侧）', async () => {
  const s = await scenario('tun');
  const ctx = { q: 'x', channels: s.channels, health: s.health, system: s.system };
  const hit = testView(await json('tun/api/rules/test.json'), ctx);
  assert.equal(hit.state, 'result');
  assert.equal(hit.title, '走 华东研发');
  assert.deepEqual(hit.sub, { kind: 'ok', text: '能通，18 毫秒' });
  assert.equal(hit.hit, 'demo-rd');
  assert.equal(hit.addr, 'git.corp.example');
  assert.deepEqual(hit.hops.map(x => x.title), ['命中规则', '华东研发']);

  const miss = testView({ q: 'a.example', kind: 'domain', result: 'direct', rule: null, channel: null, reachable: null }, ctx);
  assert.equal(miss.title, '走原网络');
  assert.equal(miss.hit, 'direct');
  assert.equal(miss.sub.text, '没有命中生效的规则');

  // 命中已停止通道：核心按生效的规则算，已停止通道的规则不算命中，rule、channel 都是 null（K7 实测形状）
  const stopped = testView(await json('proxy/api/rules/test.json'), ctx);
  assert.equal(stopped.title, '走原网络');
  assert.equal(stopped.hit, 'direct');
  assert.equal(stopped.sub.text, '没有命中生效的规则');
  assert.deepEqual(stopped.hops.map(x => x.title), ['没有命中生效的规则', '原网络']);
  assert.equal(testView({ q: 'a', kind: 'domain', result: 'direct', rule: null, channel: null }, { ...ctx, system: { ...ctx.system, routing_off: true } }).sub.text, '分流已关闭');

  const blocked = testView({ q: '10.30.1.5', kind: 'ip', result: 'blocked', rule: { id: 10, kind: 'ip', pattern: '10.30.0.0/16' },
    channel: { id: 'demo-dc', name: '数据中心', status: 'error' }, reachable: null }, ctx);
  assert.equal(blocked.title, '暂时阻止');
  assert.equal(blocked.sub.kind, 'bad');
  assert.equal(blocked.hit, 'demo-dc');
  assert.equal(blocked.hops[1].tone, 'bad');

  // 通道连着但地址不通；还没连上时不假装知道能不能通
  assert.equal(testView({ result: 'channel', kind: 'domain', rule: { pattern: 'git.corp.example' }, channel: { id: 'demo-rd', name: '华东研发' }, reachable: { ok: false, latency_ms: null } }, ctx).sub.kind, 'bad');
  assert.deepEqual(testView({ result: 'channel', kind: 'ip', rule: { pattern: '10.99.0.0/24' }, channel: { id: 'demo-park', name: '园区办公' }, reachable: null }, ctx).sub,
    { kind: 'wait', text: '待登录，流量仍进通道' });
  // 结果不写「一定」
  for (const v of [hit, miss, stopped, blocked]) assert.ok(!JSON.stringify(v).includes('一定'));
});

test('测试地址：空输入、接口不存在、400、其他错误；系统代理退路时给域名加一句说明', async () => {
  const s = await scenario('proxy');
  assert.equal(testView(null, { q: '  ' }).state, 'empty');
  assert.equal(testView(UNAVAILABLE, { q: 'a.example' }).state, 'unavailable');
  const bad = testView(null, { q: '10.0.0.0/8', error: new ApiError({ status: 400, reason: 'bad' }) });
  assert.equal(bad.state, 'bad-input');
  assert.equal(testView(null, { q: 'a', error: new ApiError({ status: 500, reason: 'boom' }) }).state, 'error');
  const ctx = { q: 'erp.vendor.example', channels: s.channels, health: s.health, system: s.system, takeover: s.takeover };
  const withNote = testView(await json('proxy/api/rules/test.json'), ctx);
  assert.match(withNote.note, /系统代理/);
  const ip = testView({ kind: 'ip', result: 'direct', rule: null, channel: null }, ctx);
  assert.equal(ip.note, null);
  assert.equal(testView(await json('tun/api/rules/test.json'), { ...ctx, takeover: { effective: 'tun', domains_via: 'tun' } }).note, null);
});

test('测试地址：域名规则没有生效时，域名的结果不说「能通」，写明没有生效并给动作（F4 第 1 条）', async () => {
  const s = await scenario('tun');
  const ctx = { q: 'git.corp.example', channels: s.channels, health: s.health, system: s.system, takeover: s.takeover };
  const hit = testView(await json('tun/api/rules/test.json'), ctx);
  assert.equal(hit.title, '规则指向 华东研发');
  assert.deepEqual(hit.sub, { kind: 'wait', text: '域名规则现在还没有生效' });
  assert.ok(!JSON.stringify(hit).includes('能通'));
  assert.match(hit.note, /^域名规则现在还没有生效/);
  assert.equal(hit.fix, 'system-proxy-ok');
  assert.equal(hit.hit, 'demo-rd');
  // 命中出错通道的域名：同样说没有生效，不说暂时阻止
  const blocked = testView({ kind: 'domain', result: 'blocked', rule: { id: 10, kind: 'domain', pattern: 'dc.corp.example' },
    channel: { id: 'demo-dc', name: '数据中心', status: 'error' }, reachable: null }, ctx);
  assert.equal(blocked.sub.text, '域名规则现在还没有生效');
  // 地址规则由虚拟网卡处理，不受影响
  const ip = testView({ kind: 'ip', result: 'channel', rule: { pattern: '10.20.16.0/24' }, channel: { id: 'demo-rd', name: '华东研发' }, reachable: { ok: true, latency_ms: 18 } }, ctx);
  assert.deepEqual([ip.title, ip.sub.text, ip.note, ip.fix], ['走 华东研发', '能通，18 毫秒', null, null]);
  // 系统代理开着、指向自己时照常
  const p = await scenario('proxy');
  const on = testView(await json('tun/api/rules/test.json'), { ...ctx, takeover: p.takeover });
  assert.equal(on.title, '走 华东研发');
  assert.equal(on.fix, null);
});

test('添加规则的预览：域名、IPv4、IPv6、网段、不带掩码、大写、结尾带点；含逗号等的拆开或认不出（R-10 的界面一侧）', () => {
  assert.deepEqual(classifyToken('Wiki.Corp.Example.'), { kind: 'domain', pattern: 'wiki.corp.example' });
  assert.deepEqual(classifyToken('https://oa.example:8443/path'), { kind: 'domain', pattern: 'oa.example' });
  assert.deepEqual(classifyToken('*.corp.example'), { kind: 'domain', pattern: 'corp.example' });
  assert.deepEqual(classifyToken('10.20.16.40'), { kind: 'ip', pattern: '10.20.16.40/32' });
  assert.deepEqual(classifyToken('10.20.18.0/24'), { kind: 'ip', pattern: '10.20.18.0/24' });
  assert.deepEqual(classifyToken('fd00::1'), { kind: 'ip', pattern: 'fd00::1/128' });
  assert.deepEqual(classifyToken('fd00:20::/48'), { kind: 'ip', pattern: 'fd00:20::/48' });
  for (const t of ['10.0.0.0/40', 'fd00::/129', '10.0.0.1/24/5', 'fe80::1%en0', 'a..example', 'a"b.example', '', '10.0.0.0/255.0.0.0']) {
    assert.equal(classifyToken(t), null, t);
  }
  assert.deepEqual(splitTokens('wiki.corp.example\n10.20.18.0/24, 10.20.16.40;\n\n'), ['wiki.corp.example', '10.20.18.0/24', '10.20.16.40']);
  const p = preview('wiki.corp.example\n10.20.18.0/24\n10.20.16.40\nWIKI.corp.example\n10.0.0.0/40');
  assert.deepEqual({ d: p.domains, i: p.ips, x: p.invalid }, { d: 1, i: 2, x: 1 });
  assert.equal(previewText(p), '识别到 1 个域名、2 个网段，1 条认不出');
  assert.equal(previewText(preview('wiki.corp.example\n10.20.18.0/24\n10.20.16.40')), '识别到 1 个域名、2 个网段');
  assert.equal(previewText(preview('')), '');
  assert.equal(previewText(preview('10.0.0.0/40')), '没有认出域名或网段，1 条认不出');
});

test('添加规则的结果：加了几条、已经有了几条、哪几条没加上（R-11 的界面一侧）', async () => {
  const ok = summarize(await json('tun/api/channels/demo-rd/rules.POST.json'), 3);
  assert.deepEqual({ a: ok.added, e: ok.existed, r: ok.rejected }, { a: 3, e: 0, r: [] });
  assert.equal(ok.text, '已添加 3 条');
  const mixed = summarize(await json('tun/api/channels/demo-cloud/rules.POST.json'), 3);
  assert.deepEqual(mixed.rejected, ['10.0.0.0/40']);
  assert.equal(mixed.text, '已添加 1 条，1 条已经有了，1 条没加上');
  assert.equal(summarize({ added: { domain: 0, ip: 0 }, rejected: [] }, 2).text, '2 条已经有了');
  assert.equal(summarize({}, 0).text, '没有添加规则');
});
