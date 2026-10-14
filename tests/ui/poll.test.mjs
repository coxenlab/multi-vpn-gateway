import test from 'node:test';
import assert from 'node:assert/strict';
import { poll } from '../../app/ui/js/poll.js';
import { createData, resumeChannels, INTERVAL } from '../../app/ui/js/data.js';
import { UNAVAILABLE, ApiError } from '../../app/ui/js/api.js';

const wait = ms => new Promise(r => setTimeout(r, ms));

// 替身 document：外壳关窗时改写 hidden 并发 visibilitychange
function fakeDoc(hidden = false) {
  const listeners = new Set();
  return {
    hidden,
    addEventListener: (t, f) => { if (t === 'visibilitychange') listeners.add(f); },
    removeEventListener: (t, f) => { if (t === 'visibilitychange') listeners.delete(f); },
    setVisible(v) { const before = this.hidden; this.hidden = !v; if (before !== this.hidden) for (const f of [...listeners]) f(); },
    get listeners() { return listeners.size; },
  };
}

test('上一轮结束后才开始计时，不叠加', async () => {
  const doc = fakeDoc();
  const ctrl = new AbortController();
  let running = 0, maxRunning = 0, runs = 0;
  poll(async () => {
    runs++; running++; maxRunning = Math.max(maxRunning, running);
    await wait(30);
    running--;
  }, 5, { signal: ctrl.signal, doc });
  await wait(120);
  ctrl.abort();
  assert.equal(maxRunning, 1);
  assert.ok(runs >= 2 && runs <= 4, `runs=${runs}`);
});

test('O-08：隐藏时暂停，重新可见时立刻补一轮', async () => {
  const doc = fakeDoc();
  const ctrl = new AbortController();
  let runs = 0;
  poll(() => { runs++; }, 10, { signal: ctrl.signal, doc });
  await wait(35);
  assert.ok(runs >= 2);
  doc.setVisible(false);
  await wait(15);
  const paused = runs;
  await wait(60);
  assert.equal(runs, paused, '隐藏期间不应再请求');
  doc.setVisible(true);
  await wait(1);
  assert.equal(runs, paused + 1);
  ctrl.abort();
});

test('一开始就隐藏时不发请求；signal 取消后停止并移除监听', async () => {
  const doc = fakeDoc(true);
  const ctrl = new AbortController();
  let runs = 0;
  poll(() => { runs++; }, 5, { signal: ctrl.signal, doc });
  await wait(30);
  assert.equal(runs, 0);
  assert.equal(doc.listeners, 1);
  ctrl.abort();
  assert.equal(doc.listeners, 0);
  doc.setVisible(true);
  await wait(20);
  assert.equal(runs, 0);
});

test('kick：立刻刷新；本轮进行中时等本轮结束后紧接着再跑一轮', async () => {
  const doc = fakeDoc();
  const ctrl = new AbortController();
  let runs = 0;
  const p = poll(async () => { runs++; await wait(20); }, 1000, { signal: ctrl.signal, doc });
  await wait(5);
  assert.equal(runs, 1);
  await p.kick();
  assert.equal(runs, 2);
  await p.kick();
  assert.equal(runs, 3);
  ctrl.abort();
});

test('出错不停：交给 onError，下一轮照常', async () => {
  const doc = fakeDoc();
  const ctrl = new AbortController();
  let runs = 0;
  const errors = [];
  poll(() => { runs++; throw new Error('x' + runs); }, 5, { signal: ctrl.signal, doc, onError: e => errors.push(e.message) });
  await wait(40);
  ctrl.abort();
  assert.ok(runs >= 2);
  assert.equal(errors[0], 'x1');
});

// 替身 api：记下每个请求
function fakeApi(over = {}) {
  const calls = [];
  const rec = (name, v) => (...args) => { calls.push(name + (typeof args[0] === 'string' ? ':' + args[0] : '')); return Promise.resolve(typeof v === 'function' ? v(...args) : v); };
  const api = {
    channels: rec('channels', [{ id: 'c1', status: 'logged_in' }, { id: 'c2', status: 'running' }, { id: 'c3', status: 'stopped' }]),
    system: rec('system', { routing_off: false }),
    channelHealth: rec('health', id => ({ status: 'logged_in', connected: true, id })),
    connections: rec('connections', { uploadTotal: 0, downloadTotal: 0, connections: [] }),
    runtimeEvents: rec('events', { events: [], seq: 0 }),
    trafficToday: rec('traffic', UNAVAILABLE),
    ...over,
  };
  for (const [k, f] of Object.entries(over)) api[k] = (...a) => { calls.push(k); return f(...a); };
  return { api, calls };
}

test('共享快照：只在有订阅者时轮询；最后一个退订后停止', async () => {
  const { api, calls } = fakeApi();
  const data = createData({ api, doc: fakeDoc() });
  await wait(10);
  assert.deepEqual(calls, []);
  const c1 = new AbortController();
  const seen = [];
  data.subscribe('core', s => seen.push(s), { signal: c1.signal });
  const off = data.subscribe('core', () => {});
  await wait(10);
  assert.deepEqual(calls.sort(), ['channels', 'system']);
  assert.equal(seen.length, 1);
  assert.equal(seen[0].value.channels.length, 3);
  assert.equal(seen[0].value.system.routing_off, false);
  c1.abort();
  assert.equal(data.polling('core'), true);
  off();
  assert.equal(data.polling('core'), false);
  // 后来的订阅者立即拿到已有快照
  let got = null;
  const c2 = new AbortController();
  data.subscribe('core', s => { got = s; }, { signal: c2.signal });
  assert.equal(got.value.channels.length, 3);
  c2.abort();
});

test('health 只查运行中的通道，并带上通道列表的订阅', async () => {
  const { api, calls } = fakeApi();
  const data = createData({ api, doc: fakeDoc() });
  const ctrl = new AbortController();
  let snap = null;
  data.subscribe('health', s => { snap = s; }, { signal: ctrl.signal });
  await wait(20);
  assert.deepEqual(Object.keys(snap.value).sort(), ['c1', 'c2']);
  assert.deepEqual(calls.filter(c => c.startsWith('health')).sort(), ['health:c1', 'health:c2']);
  assert.equal(data.polling('core'), true);
  ctrl.abort();
  assert.equal(data.polling('health'), false);
  assert.equal(data.polling('core'), false);
});

test('connections 取订阅者要的最短间隔；速度差分、入口重启后不出现负数', async () => {
  const totals = [[1000, 1000], [3000, 5000], [10, 20], [1010, 20]];
  let i = 0;
  let t = 0;
  const { api } = fakeApi({ connections: () => { const [u, d] = totals[Math.min(i++, totals.length - 1)]; return Promise.resolve({ uploadTotal: u, downloadTotal: d }); } });
  const data = createData({ api, doc: fakeDoc(), now: () => (t += 1000) });
  const ctrl = new AbortController();
  const snaps = [];
  data.subscribe('connections', s => snaps.push(s.value), { signal: ctrl.signal, ms: 5 });
  await wait(60);
  ctrl.abort();
  assert.ok(snaps.length >= 4, `snaps=${snaps.length}`);
  assert.equal(snaps[0].up, null);
  assert.equal(snaps[1].up, 2000);
  assert.equal(snaps[1].down, 4000);
  assert.equal(snaps[2].up, 0);
  assert.equal(snaps[2].down, 0);
  assert.equal(snaps[3].up, 1000);
  for (const s of snaps) for (const p of s.history) assert.ok(p.up >= 0 && p.down >= 0);
  assert.equal(INTERVAL.connections, 3000);
});

test('events 第一次整段取，之后按 since_seq 增量取', async () => {
  const params = [];
  const pages = [
    { events: [{ seq: 1 }, { seq: 2 }], seq: 2 },
    { events: [{ seq: 3 }], seq: 3 },
    { events: [{ seq: 1 }], seq: 1 }, // 核心重启，seq 变小
  ];
  let i = 0;
  const { api } = fakeApi({ runtimeEvents: p => { params.push(p); return Promise.resolve(pages[Math.min(i++, pages.length - 1)]); } });
  const data = createData({ api, doc: fakeDoc() });
  const ctrl = new AbortController();
  data.subscribe('events', () => {}, { signal: ctrl.signal });
  await wait(5);
  await data.refresh('events');
  assert.deepEqual(data.get('events').value.events.map(e => e.seq), [1, 2, 3]);
  await data.refresh('events');
  assert.deepEqual(data.get('events').value.events.map(e => e.seq), [1]);
  ctrl.abort();
  assert.equal(params[0].since_seq, undefined);
  assert.equal(params[1].since_seq, 2);
  assert.equal(params[2].since_seq, 3);
  assert.ok(params.every(p => p.level === 'info'));
});

test('连接读取失败断开曲线，恢复第一帧只重建基线，不制造尖峰', async () => {
  let i = 0, t = 0;
  const { api } = fakeApi({ connections: async () => {
    const frame = i++;
    if (frame === 2) throw new ApiError({ status: 503 });
    const total = [1000, 2000, 0, 900000, 901000][Math.min(frame, 4)];
    return { uploadTotal: total, downloadTotal: total, connections: [] };
  } });
  const data = createData({ api, doc: fakeDoc(), now: () => (t += 1000) });
  const ctrl = new AbortController();
  data.subscribe('connections', () => {}, { signal: ctrl.signal });
  await wait(10);
  await data.refresh('connections');
  assert.equal(data.get('connections').value.up, 1000);
  await data.refresh('connections');
  const lost = data.get('connections').value;
  assert.equal(lost.unavailable, true);
  assert.equal(lost.last, null);
  assert.equal(lost.history.at(-1).up, null);
  await data.refresh('connections');
  assert.equal(data.get('connections').value.up, null);
  assert.equal(data.get('connections').value.unavailable, false);
  await data.refresh('connections');
  assert.equal(data.get('connections').value.up, 1000);
  ctrl.abort();
});

test('traffic 接口不存在时值为 UNAVAILABLE，不再轮询', async () => {
  const { api, calls } = fakeApi();
  const data = createData({ api, doc: fakeDoc() });
  const ctrl = new AbortController();
  let snap = null;
  data.subscribe('traffic', s => { snap = s; }, { signal: ctrl.signal, ms: 5 });
  await wait(40);
  assert.equal(snap.value, UNAVAILABLE);
  assert.equal(calls.filter(c => c === 'traffic').length, 1);
  assert.equal(data.polling('traffic'), false);
  ctrl.abort();
});

test('窗口隐藏时共享快照也不轮询', async () => {
  const { api, calls } = fakeApi();
  const doc = fakeDoc(true);
  const data = createData({ api, doc });
  const ctrl = new AbortController();
  data.subscribe('core', () => {}, { signal: ctrl.signal });
  await wait(20);
  assert.deepEqual(calls, []);
  doc.setVisible(true);
  await wait(5);
  assert.deepEqual(calls.sort(), ['channels', 'system']);
  ctrl.abort();
});

test('请求失败时保留上一份数据并带上错误', async () => {
  let fail = false;
  const { api } = fakeApi({ system: () => (fail ? Promise.reject(new ApiError({ status: 503, kind: 'runtime' })) : Promise.resolve({ routing_off: true })) });
  const data = createData({ api, doc: fakeDoc() });
  await data.refresh('core');
  fail = true;
  await data.refresh('core');
  const s = data.get('core');
  assert.equal(s.value.system.routing_off, true);
  assert.equal(s.error.kind, 'runtime');
});

test('S-02：启动时恢复在用通道，先清空记录再逐条启动', async () => {
  const order = [];
  const api = {
    appSettings: async () => ({ auto_connect: true, resume_channels: ['c1', 'c2'] }),
    setAppSettings: async body => { order.push('put:' + JSON.stringify(body)); return { ok: true }; },
    start: async id => { order.push('start:' + id); if (id === 'c2') throw new ApiError({ status: 409 }); return {}; },
  };
  const out = await resumeChannels(api);
  assert.deepEqual(order, ['put:{"resume_channels":[]}', 'start:c1', 'start:c2']);
  assert.deepEqual(out.map(r => [r.id, r.ok]), [['c1', true], ['c2', false]]);
});

test('S-02：没开自动连接、没有记录或接口不存在时什么都不做', async () => {
  for (const settings of [UNAVAILABLE, { auto_connect: false, resume_channels: ['c1'] }, { auto_connect: true, resume_channels: [] }]) {
    const order = [];
    const api = {
      appSettings: async () => settings,
      setAppSettings: async () => { order.push('put'); return {}; },
      start: async () => { order.push('start'); },
    };
    assert.deepEqual(await resumeChannels(api), []);
    assert.deepEqual(order, []);
  }
});
