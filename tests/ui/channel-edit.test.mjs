// F5：通道管理的入口（修改连接信息、删除通道、恢复上一次设置、恢复分流）和「还没验证」页检测失败改回原值。
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { editInputs, editBody, reconnects, restoreView, deleteText } from '../../app/ui/js/screens/channel-edit.js';
import { tryTarget, ruleCanVerify, targetFailureText } from '../../app/ui/js/screens/channel-unverified.js';
import { fateOf } from '../../app/ui/js/screens/channel-rules.js';
import { FATE, FORBIDDEN } from '../../app/ui/js/terms.js';

const TYPES = JSON.parse(await readFile(new URL('./fixtures/add/api/vpn-types.json', import.meta.url), 'utf8'));
const type = key => TYPES.find(a => a.key === key);

const ch = {
  id: 'demo-edit', name: '华东研发', vpn_type: 'easyconnect', server: 'vpn.rd.example', username: 'zhang.wei',
  probe_url: '', status: 'logged_in', domains: [{ id: 1, pattern: 'git.corp.example', enabled: 1 }], ips: [{ id: 2, pattern: '10.20.16.0/24', enabled: 1 }],
};
const inputsOf = key => editInputs(type(key), ch);
const values = (over = {}) => ({ name: ch.name, server: ch.server, username: ch.username, password: '', target: ch.probe_url, ...over });

test('修改连接信息：显示这种类型要填、核心能改的项；文件项不显示；拿不到类型清单时按通道现有的项', () => {
  assert.deepEqual(inputsOf('easyconnect').map(i => i.key), ['server', 'username', 'password']);
  assert.deepEqual(inputsOf('atrust').map(i => i.key), ['server']);
  assert.deepEqual(inputsOf('wireguard'), []);
  assert.deepEqual(inputsOf('openvpn'), []);
  assert.deepEqual(inputsOf('hillstone'), []);
  assert.deepEqual(editInputs(null, ch).map(i => i.key), ['server', 'username']);
  assert.deepEqual(editInputs(null, { name: 'x' }), []);
});

test('修改连接信息：只提交改过的字段，密码留空表示不改', () => {
  const inputs = inputsOf('easyconnect');
  assert.deepEqual(editBody(ch, inputs, values()), { body: {} });
  assert.deepEqual(editBody(ch, inputs, values({ name: '  华东研发二部 ' })), { body: { name: '华东研发二部' } });
  assert.deepEqual(editBody(ch, inputs, values({ server: 'vpn2.rd.example' })), { body: { server: 'vpn2.rd.example' } });
  assert.deepEqual(editBody(ch, inputs, values({ password: ' 新 密码 ' })), { body: { password: ' 新 密码 ' } });
  assert.equal(reconnects({ name: 'x' }), false);
  assert.equal(reconnects({ password: 'x' }), true);
  assert.equal(reconnects({ probe_url: 'http://oa.corp.example/' }), false); // wording-allow
});

test('修改连接信息：名字和标了必填的项不能空；验证地址补上协议，清空就是不用', () => {
  assert.deepEqual(editBody(ch, inputsOf('easyconnect'), values({ name: '  ' })), { error: '给它起个名字' });
  const oc = { ...ch, vpn_type: 'anyconnect' };
  assert.deepEqual(editBody(oc, editInputs(type('anyconnect'), oc), values({ server: '' })), { error: '请填写服务器' });
  // EasyConnect 的服务器没有标必填：清空也照样提交
  assert.deepEqual(editBody(ch, inputsOf('easyconnect'), values({ server: '' })), { body: { server: '' } });
  assert.deepEqual(editBody(ch, [], values({ target: 'oa.corp.example' })).body, { probe_url: 'http://oa.corp.example/' }); // wording-allow
  assert.deepEqual(editBody(ch, [], values({ target: 'oa corp' })), { error: '验证地址写得不对，例如 oa.corp.example' });
  // 原来存的是没写协议的地址：没动就不提交；清空提交空串
  const raw = { ...ch, probe_url: 'oa.corp.example' }; // wording-allow
  assert.deepEqual(editBody(raw, [], values({ target: 'oa.corp.example' })), { body: {} });
  assert.deepEqual(editBody(raw, [], values({ target: '' })).body, { probe_url: '' }); // wording-allow
});

test('恢复上一次设置：只在核心说可以恢复时出现，按阶段给说法', () => {
  assert.equal(restoreView(ch), null);
  assert.equal(restoreView({ ...ch, replacement: null }), null);
  assert.equal(restoreView({ ...ch, replacement: { phase: 'committed', can_restore: false } }), null);
  assert.match(restoreView({ ...ch, replacement: { phase: 'awaiting_login', can_restore: true } }).text, /还在等确认/);
  assert.match(restoreView({ ...ch, replacement: { phase: 'queued', can_restore: true } }).text, /启动通道时才会用/);
  assert.match(restoreView({ ...ch, replacement: { phase: 'validating', can_restore: true } }).text, /没有用上/);
});

test('删除通道：确认框写清楚规则会一起删掉', () => {
  assert.match(deleteText(ch), /2 条规则、连接信息和备忘录会一起删掉/);
  assert.doesNotMatch(deleteText({ ...ch, domains: [], ips: [] }), /规则/);
});

test('没有参与分流的通道：规则分页的「现在」写走原网络和原因（F5 第 3 条）', () => {
  const off = { ...ch, routing_enabled: false };
  const rule = ch.domains[0];
  assert.equal(fateOf(rule, off, { connected: true }, { routing_off: false }), 'direct-unrouted');
  assert.equal(FATE[fateOf(rule, off, null, null)].text, '走原网络（不参与分流）');
  assert.equal(fateOf(rule, ch, null, null), 'channel');
});

// 记下请求的替身
function fakeClient({ connected = false, probeFailed = false, throws = false } = {}) {
  const calls = [];
  return {
    calls,
    async verifyTarget(id, url) {
      calls.push(['verifyTarget', id, url]);
      if (throws) throw new Error('boom');
      if (probeFailed) return { connected: null, probe_failed: true, probe_reason: '探针执行失败', target: url };
      return { connected, target: url };
    },
  };
}

test('验证地址由原子接口探测：成功才保存，不通、执行失败和取消都不发 PATCH', async () => {
  const bad = fakeClient();
  const r = await tryTarget(bad, 'demo-edit', 'http://oa.corp.example/');
  assert.deepEqual(r, { connected: false, target: 'http://oa.corp.example/', saved: false });
  assert.deepEqual(bad.calls, [['verifyTarget', 'demo-edit', 'http://oa.corp.example/']]);

  const good = fakeClient({ connected: true });
  assert.deepEqual(await tryTarget(good, 'demo-edit', 'http://oa.corp.example/'), {
    connected: true, target: 'http://oa.corp.example/', saved: true,
  });

  const failed = fakeClient({ probeFailed: true });
  await assert.rejects(tryTarget(failed, 'demo-edit', 'http://oa.corp.example/'), /探针执行失败/);
  const broken = fakeClient({ throws: true });
  await assert.rejects(tryTarget(broken, 'demo-edit', 'http://oa.corp.example/'), /boom/);
  for (const client of [bad, good, failed, broken]) assert.ok(client.calls.every(c => c[0] !== 'update'));
});

test('「还没验证」页：只有无头类型能用规则里的地址来确认', () => {
  assert.equal(ruleCanVerify({ login_method: 'headless' }), true);
  assert.equal(ruleCanVerify({ login_method: 'interactive' }), false);
  assert.equal(ruleCanVerify({ login_method: 'byo' }), false);
});

test('验证失败说明同时覆盖隧道未连通和目标不可达', () => {
  assert.match(targetFailureText(), /隧道还没连通/);
  assert.match(targetFailureText(), /地址无法从内网访问/);
  assert.match(targetFailureText(), /没有保存/);
  assert.doesNotMatch(targetFailureText(), /恢复/);
});

test('新加的界面文字不含内部用词', () => {
  const texts = [
    ...['awaiting_login', 'queued', 'validating'].flatMap(p => Object.values(restoreView({ ...ch, replacement: { phase: p, can_restore: true } }))),
    deleteText(ch), deleteText({}), FATE['direct-unrouted'].text,
  ];
  for (const t of texts) assert.ok(!FORBIDDEN.test(t), t);
});
