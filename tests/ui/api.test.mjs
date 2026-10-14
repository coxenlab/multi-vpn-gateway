import test from 'node:test';
import assert from 'node:assert/strict';
import { createApi, ApiError, UNAVAILABLE, isUnavailable, isAbort, LONG_TIMEOUT, TIMEOUT } from '../../app/ui/js/api.js';
import { createLatest, isStale } from '../../app/ui/js/latest.js';

const json = (body, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });
const text = (body, status = 200) => new Response(body, { status, headers: { 'content-type': 'text/plain' } });

// 替身 fetch：按「方法 路径」回应，记下每次请求。handler 可以是函数或 Response 工厂
function fake(routes = {}) {
  const calls = [];
  const fetch = async (url, init = {}) => {
    const method = init.method || 'GET';
    calls.push({ url, method, init, body: init.body });
    const h = routes[`${method} ${url}`] ?? routes[url] ?? routes['*'];
    if (!h) return text('', 404);
    return typeof h === 'function' ? h(url, init) : h();
  };
  return { fetch, calls };
}

// 在 signal 取消之前不回应
const hang = (url, init) => new Promise((_, reject) => {
  init.signal.addEventListener('abort', () => reject(Object.assign(new Error('aborted'), { name: 'AbortError' })));
});
const later = (ms, res) => (url, init) => new Promise((resolve, reject) => {
  const t = setTimeout(() => resolve(res()), ms);
  init.signal.addEventListener('abort', () => { clearTimeout(t); reject(Object.assign(new Error('aborted'), { name: 'AbortError' })); });
});

test('HTTP ≥ 400 抛 ApiError，reason 取 error，再取 detail', async () => {
  const f = fake({
    'GET /api/channels': () => json({ error: 'not found', detail: 'x' }, 404),
    'GET /api/system': () => json({ detail: '请先启动通道' }, 409),
    'GET /api/routing': () => text('Internal', 500),
  });
  const api = createApi({ fetch: f.fetch });
  await assert.rejects(api.channels(), e => e instanceof ApiError && e.status === 404 && e.reason === 'not found' && e.kind === 'http');
  await assert.rejects(api.system(), e => e.status === 409 && e.reason === '请先启动通道' && e.body.detail === '请先启动通道');
  await assert.rejects(api.routing(), e => e.status === 500 && e.reason === '' && e.raw === 'Internal');
});

test('503 标为运行环境没准备好', async () => {
  const api = createApi({ fetch: fake({ '*': () => json({ error: 'docker unavailable' }, 503) }).fetch });
  await assert.rejects(api.start('c1'), e => e.kind === 'runtime' && e.status === 503);
});

test('V-09：2xx 但体里 ok === false 按失败处理', async () => {
  const api = createApi({ fetch: fake({ 'POST /api/system/heal-proxy': () => json({ ok: false, reachable: false, error: '分流口仍不可达' }) }).fetch });
  await assert.rejects(api.healProxy(), e => e instanceof ApiError && e.kind === 'failed' && e.status === 200 && e.reason === '分流口仍不可达');
});

test('202 或 deferred === true：照常返回并带 deferred', async () => {
  const f = fake({
    'POST /api/channels/c1/rules': () => json({ added: ['a.example'], rejected: [], deferred: true, message: '已保存，连接后生效' }, 202),
    'PATCH /api/channels/c1': () => json({ id: 'c1', status: 'stopped', deferred: true }),
    'POST /api/channels/c1/start': () => json({ id: 'c1', status: 'running' }, 202),
    'POST /api/channels/c1/stop': () => json({ id: 'c1', status: 'stopped', deferred: false }),
  });
  const api = createApi({ fetch: f.fetch });
  assert.equal((await api.addRules('c1', ['a.example'])).deferred, true);
  assert.equal((await api.update('c1', { name: 'x' })).deferred, true);
  assert.equal((await api.start('c1')).deferred, true);
  assert.equal((await api.stop('c1')).deferred, false);
});

test('新接口 404 返回 UNAVAILABLE；老接口 404 照常报错', async () => {
  const api = createApi({ fetch: fake({}).fetch });
  for (const call of [
    () => api.rulesTest('a.example'), () => api.cause('c1'), () => api.setConfigFile('c1', 'x', 'example.ovpn'),
    () => api.trafficToday(), () => api.latency('c1'), () => api.appSettings(), () => api.setAppSettings({ auto_connect: true }),
    () => api.takeover(), () => api.setTakeover({ mode: 'auto' }),
  ]) {
    const r = await call();
    assert.equal(r, UNAVAILABLE);
    assert.ok(isUnavailable(r));
  }
  await assert.rejects(api.channels(), e => e.status === 404);
});

test('新接口的方法、路径和请求体（DESIGN 5.0）', async () => {
  const f = fake({ '*': () => json({}) });
  const api = createApi({ fetch: f.fetch });
  await api.rulesTest('a b.example');
  await api.cause('c/1');
  await api.setConfigFile('c1', 'client\n', 'example.ovpn');
  await api.trafficToday();
  await api.latency('c1');
  await api.appSettings();
  await api.setAppSettings({ resume_channels: [] });
  await api.takeover();
  await api.setTakeover({ system_proxy_ok: true });
  assert.deepEqual(f.calls.map(c => `${c.method} ${c.url}`), [
    'GET /api/rules/test?q=a+b.example',
    'GET /api/channels/c%2F1/cause',
    'PUT /api/channels/c1/config-file',
    'GET /api/traffic/today',
    'GET /api/channels/c1/latency',
    'GET /api/app-settings',
    'PUT /api/app-settings',
    'GET /api/takeover',
    'PUT /api/takeover',
  ]);
  assert.deepEqual(JSON.parse(f.calls[2].body), { content: 'client\n', filename: 'example.ovpn' });
  assert.deepEqual(JSON.parse(f.calls[6].body), { resume_channels: [] });
  assert.deepEqual(JSON.parse(f.calls[8].body), { system_proxy_ok: true });
  assert.equal(f.calls[2].init.headers['Content-Type'], 'application/json');
});

test('验证地址使用原子接口，不经过通道 PATCH', async () => {
  const f = fake({ '*': () => json({ connected: false }) });
  const api = createApi({ fetch: f.fetch });
  await api.verifyTarget('c/1', 'https://oa.example/');
  assert.equal(f.calls.length, 1);
  assert.equal(`${f.calls[0].method} ${f.calls[0].url}`, 'POST /api/channels/c%2F1/verify-target');
  assert.deepEqual(JSON.parse(f.calls[0].body), { probe_url: 'https://oa.example/' });
});

test('C-10：同一地址的 GET 在途时合并，写请求不合并', async () => {
  const f = fake({
    'GET /api/channels/c1/status': later(30, () => json({ status: 'logged_in', connected: true })),
    'POST /api/channels/c1/start': later(10, () => json({ id: 'c1' })),
  });
  const api = createApi({ fetch: f.fetch });
  const [a, b, c] = await Promise.all([api.status('c1'), api.status('c1'), api.status('c1')]);
  assert.equal(f.calls.length, 1);
  assert.equal(a, b);
  assert.equal(b, c);
  await api.status('c1'); // 上一次结束后再请求会重新发
  assert.equal(f.calls.length, 2);
  await Promise.all([api.start('c1'), api.start('c1')]);
  assert.equal(f.calls.filter(x => x.method === 'POST').length, 2);
});

test('V-08：合并的请求里一方取消只影响它自己，全部取消才中止', async () => {
  const f = fake({ 'GET /api/channels/c1/health': later(30, () => json({ connected: true })) });
  const api = createApi({ fetch: f.fetch });
  const ca = new AbortController();
  const pa = api.channelHealth('c1', { signal: ca.signal });
  const pb = api.channelHealth('c1');
  ca.abort();
  await assert.rejects(pa, e => isAbort(e));
  assert.deepEqual(await pb, { connected: true });
  assert.equal(f.calls.length, 1);

  const cx = new AbortController(), cy = new AbortController();
  const px = api.channelHealth('c1', { signal: cx.signal });
  const py = api.channelHealth('c1', { signal: cy.signal });
  cx.abort(); cy.abort();
  await assert.rejects(px, isAbort);
  await assert.rejects(py, isAbort);
  assert.equal(f.calls.at(-1).init.signal.aborted, true);
  // 中止后同一地址重新请求，不复用已中止的那次
  assert.deepEqual(await api.channelHealth('c1'), { connected: true });
  assert.equal(f.calls.length, 3);
});

test('V-08：每个函数都接受 {signal}，已取消时不发请求', async () => {
  const f = fake({ '*': () => json({ ok: true }) });
  const api = createApi({ fetch: f.fetch });
  const ctrl = new AbortController();
  ctrl.abort();
  const o = { signal: ctrl.signal };
  const file = new Blob(['x']);
  file.name = 'example.bin';
  const calls = {
    channels: [o], create: [{}, o], update: ['c1', {}, o], remove: ['c1', false, o], start: ['c1', o], stop: ['c1', o],
    restore: ['c1', o], status: ['c1', o], verifyTarget: ['c1', 'https://oa.example/', o], channelHealth: ['c1', o], logs: ['c1', 200, o], login: ['c1', {}, o],
    renewLoginViewer: ['c1', 'v1', o], releaseLoginViewer: ['c1', 'v1', o], noteGet: ['c1', o], noteSet: ['c1', 'n', undefined, o],
    upload: ['c1', file, o], installStatus: ['c1', o], startupStatus: ['c1', o], addRules: ['c1', ['a.example'], undefined, o], delRule: ['c1', 1, o], toggleRule: ['c1', 1, true, o],
    updateRule: ['c1', 1, {}, o], toggleRules: [[1], true, o], system: [o], runtimeStart: [o], startRuntime: [o], healProxy: [o],
    selfHeal: [true, o], routing: [o], setRouting: [true, o], diag: [o], preflight: [null, null, 'full', o],
    preflightFix: ['create_network', {}, o], preflightFixStatus: ['t1', o], containers: [o], containerLogs: ['n', 200, o],
    containerRemove: ['n', o], vpnTypes: [o], vpnVersions: ['easyconnect', o], images: [o], imageImport: [file, o],
    imageImportStatus: ['i1', o], imageImportDiscard: ['i1', o], imageImportConfirm: ['i1', o], mirrors: [o], addMirror: ['m.example', o],
    patchMirror: [1, {}, o], delMirror: [1, o], testMirror: ['m.example', o], connections: [o], proxies: [o],
    runtimeEvents: [{}, o], runtimeEventsEnabled: [o], runtimeEventsSetEnabled: [true, o], runtimeEventsExport: ['2026-09-01', '2026-09-02', o],
    entrySetup: [o], snippet: [o], clashDetect: [o], mergeProfile: [o], systemProxyGet: [o], systemProxySet: [true, o], tunGet: [o],
    tunSet: [true, o], tunInstall: [o], tunUninstall: [o], exportConfig: [o], importConfig: [{}, o], retryConfig: [o],
    rulesTest: ['a.example', o], cause: ['c1', o], setConfigFile: ['c1', 'x', 'example.ovpn', o], trafficToday: [o], latency: ['c1', o],
    appSettings: [o], setAppSettings: [{}, o], takeover: [o], setTakeover: [{}, o],
  };
  const skip = new Set(['runtimeEventsExportUrl']);
  for (const name of Object.keys(api)) {
    if (skip.has(name)) continue;
    assert.ok(calls[name], `测试没覆盖 ${name}`);
    await assert.rejects(api[name](...calls[name]), e => isAbort(e), name);
  }
  assert.equal(f.calls.length, 0);
  assert.equal(api.runtimeEventsExportUrl('2026-09-01', '2026-09-02'), '/api/events/export?from=2026-09-01&to=2026-09-02');
});

test('超时：常规 25 秒，建、启、停、改、重建、换配置文件 20 分钟', async () => {
  assert.equal(TIMEOUT, 25000);
  assert.equal(LONG_TIMEOUT, 20 * 60 * 1000);
  const slow = later(60, () => json({ ok: true }));
  const api = createApi({ fetch: fake({ '*': slow }).fetch, timeout: 20, longTimeout: 2000, slowTimeout: 20 });
  await assert.rejects(api.channels(), e => e instanceof ApiError && e.kind === 'timeout');
  for (const call of [
    () => api.create({}), () => api.start('c1'), () => api.stop('c1'), () => api.update('c1', {}),
    () => api.restore('c1'), () => api.setConfigFile('c1', 'x', 'example.ovpn'), () => api.remove('c1'),
  ]) assert.deepEqual(await call(), { ok: true });
});

test('网络失败标为 network；取消标为 AbortError', async () => {
  const api = createApi({ fetch: async () => { throw new TypeError('Failed to fetch'); } });
  await assert.rejects(api.system(), e => e.kind === 'network');
  const api2 = createApi({ fetch: fake({ '*': hang }).fetch });
  const ctrl = new AbortController();
  const p = api2.stop('c1', { signal: ctrl.signal });
  ctrl.abort();
  await assert.rejects(p, isAbort);
});

test('文本响应原样返回；备忘录的原文参数只在给了时才带', async () => {
  const f = fake({ 'GET /api/clash-snippet': () => text('proxies: []'), '*': () => json({ note: 'x' }) });
  const api = createApi({ fetch: f.fetch });
  assert.equal(await api.snippet(), 'proxies: []');
  await api.noteSet('c1', '新', undefined);
  await api.noteSet('c1', '新', '旧');
  assert.deepEqual(JSON.parse(f.calls[1].body), { note: '新' });
  assert.deepEqual(JSON.parse(f.calls[2].body), { note: '新', expected_note: '旧' });
});

test('上传：空文件和超过 1 GiB 在发请求前拒绝；没确认 ok 按失败', async () => {
  const f = fake({ 'POST /api/channels/c1/upload': () => json({ package: 'x' }) });
  const api = createApi({ fetch: f.fetch });
  await assert.rejects(api.upload('c1', new Blob([])), e => e.status === 400);
  await assert.rejects(api.upload('c1', { size: 1024 ** 3 + 1, name: 'big.bin' }), e => e.status === 400);
  assert.equal(f.calls.length, 0);
  const file = new Blob(['x']);
  await assert.rejects(api.upload('c1', file), e => e.kind === 'failed');
  assert.ok(f.calls[0].body instanceof FormData);
});

test('V-08：只认最新，晚到的旧响应丢弃', async () => {
  const latest = createLatest();
  const slowA = latest.run(signal => new Promise(r => setTimeout(() => r('A'), 30)));
  const fastB = latest.run(async () => 'B');
  assert.equal(await fastB, 'B');
  await assert.rejects(slowA, e => isStale(e));

  // 上一代的 signal 在开新一代时被取消
  const t1 = latest.next();
  const t2 = latest.next();
  assert.equal(t1.signal.aborted, true);
  assert.equal(t1.current(), false);
  assert.equal(t2.current(), true);
  latest.cancel();
  assert.equal(t2.signal.aborted, true);

  // 父 signal 取消时当前一代也取消
  const parent = new AbortController();
  const child = createLatest(parent.signal).next();
  parent.abort();
  assert.equal(child.signal.aborted, true);
});

test('V-08：换通道时前一条的请求被取消', async () => {
  const f = fake({ '*': later(30, () => json({ connected: true })) });
  const api = createApi({ fetch: f.fetch });
  const latest = createLatest();
  const a = latest.run(signal => api.channelHealth('c1', { signal }));
  const b = latest.run(signal => api.channelHealth('c2', { signal }));
  await assert.rejects(a, e => isStale(e));
  assert.deepEqual(await b, { connected: true });
  assert.equal(f.calls[0].init.signal.aborted, true);
});
