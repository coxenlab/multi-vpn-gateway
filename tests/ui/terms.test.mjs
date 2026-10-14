import test from 'node:test';
import assert from 'node:assert/strict';
import * as terms from '../../app/ui/js/terms.js';

const { FORBIDDEN, STATE, FATE, stateText, stateKind, causeText, actionText, typeLabel } = terms;

// 把导出的全部字符串收集起来
function strings(v, out = []) {
  if (typeof v === 'string') out.push(v);
  else if (v && typeof v === 'object' && !(v instanceof RegExp)) for (const x of Object.values(v)) strings(x, out);
  return out;
}

test('状态用词：已连接 / 待登录 / 已启动，还没验证 / 出错 / 已停止', () => {
  assert.deepEqual(Object.fromEntries(Object.entries(STATE).map(([k, v]) => [k, v.text])), {
    connected: '已连接', waiting: '待登录', unverified: '已启动，还没验证', error: '出错', stopped: '已停止', unknown: '状态待确认',
  });
  assert.equal(stateKind('connected'), 'ok');
  assert.equal(stateKind('error'), 'bad');
  assert.equal(stateKind('stopped'), 'off');
  assert.equal(stateText('nope'), '待登录');
});

test('规则「现在」一列的用词覆盖 derive.ruleFate 的全部结果', () => {
  for (const k of ['disabled', 'direct', 'direct-unrouted', 'direct-stopped', 'blocked', 'channel', 'waiting']) assert.ok(FATE[k] && FATE[k].text, k);
  assert.equal(FATE['direct-unrouted'].text, '走原网络（不参与分流）');
  assert.equal(FATE.waiting.text, '等待登录（流量仍进通道）');
  assert.equal(FATE.blocked.text, '暂时阻止');
});

test('出错原因和动作覆盖 5.0 的全部取值；拿不到原因时说原因还不清楚', () => {
  for (const code of ['runtime_down', 'image_pull_failed', 'crash_loop', 'kicked']) assert.ok(causeText(code), code);
  assert.equal(causeText(null), '连不上，原因还不清楚');
  assert.equal(causeText('whatever'), '连不上，原因还不清楚');
  for (const a of ['restart_runtime', 'retry', 'relogin', 'view_logs']) assert.ok(actionText(a), a);
  assert.equal(actionText(null), '');
  for (const a of ['install_tun', 'enable_system_proxy', 'close_other_app', 'retry_takeover']) assert.ok(terms.TAKEOVER_ACTION[a].text, a);
  assert.equal(terms.TAKEOVER_ACTION.retry_takeover.button, '重试');
  assert.equal(terms.TAKEOVER_ACTION.enable_system_proxy.button, '用系统代理接管');
});

test('类型显示名：表里没有的用类型自己的名称', () => {
  assert.equal(typeLabel('easyconnect'), '深信服 EasyConnect');
  assert.equal(typeLabel('example-vpn', '示例客户端'), '示例客户端');
  assert.equal(typeLabel('example-vpn'), 'example-vpn');
});

test('禁词表不分大小写', () => {
  for (const w of ['mihomo', 'Docker', 'SOCKS5', 'probe_url', 'PROBE', '容器']) assert.ok(FORBIDDEN.test(w), w);
  for (const w of ['通道', '运行环境', '内网验证地址', 'Clash']) assert.ok(!FORBIDDEN.test(w), w);
});

test('用词表里的每一句都不含内部用词', () => {
  const all = strings(Object.fromEntries(Object.entries(terms).filter(([, v]) => typeof v !== 'function')));
  assert.ok(all.length > 30);
  for (const s of all) assert.ok(!FORBIDDEN.test(s), s);
});
