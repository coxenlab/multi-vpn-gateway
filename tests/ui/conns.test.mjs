// 连接页的纯推导（U6）：表格行、搜索、分段渲染、选中行固定（O-06）、计数归零（O-04）、没有「已阻止」（O-05）。
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import {
  channelIdOf, hms, flowOf, connRows, filterConns, rowTexts, countText, pickAnchor, nextLimit, CHUNK, POLL_MS,
} from '../../app/ui/js/screens/conns.js';
import { createData } from '../../app/ui/js/data.js';
import { FORBIDDEN } from '../../app/ui/js/terms.js';

const FX = new URL('./fixtures/', import.meta.url);
const json = async p => JSON.parse(await readFile(new URL(p, FX), 'utf8'));

const CH = [{ id: 'a', name: '研发' }, { id: 'b', name: '实验室' }];
const conn = (id, start, over = {}) => ({
  id, start, chains: ['ch-a', 'DIRECT'],
  metadata: { host: 'git.corp.example.test', destinationIP: '10.20.3.9' },
  upload: 100, download: 900, ...over,
});

test('通道：取链路里的 ch-<id>，没有时是原网络', () => {
  assert.equal(channelIdOf(['ch-a', 'DIRECT']), 'a');
  assert.equal(channelIdOf(['group', 'ch-b']), 'b');
  assert.equal(channelIdOf(['DIRECT']), null);
  assert.equal(channelIdOf(['ch-']), null);
  assert.equal(channelIdOf(undefined), null);
});

test('时间和流量：缺字段显示「—」，负数不算', () => {
  assert.match(hms(Date.parse('2026-05-28T20:26:37.000Z')), /^\d\d:\d\d:37$/);
  assert.equal(hms(null), '—');
  assert.equal(hms(NaN), '—');
  assert.equal(flowOf({ upload: 100, download: 900 }), 1000);
  assert.equal(flowOf({ download: 900 }), 900);
  assert.equal(flowOf({}), null);
  assert.equal(flowOf({ upload: -5, download: null }), null);
  assert.equal(rowTexts({ addr: 'x', ip: '', start: null, flow: null, dur: null, cid: null }).flow, '—');
});

test('表格行：新的在前，同一时刻按 id，地址优先用域名，找不到的通道写「未知通道」', () => {
  const now = Date.parse('2026-05-28T20:30:00.000Z');
  const rows = connRows({
    connections: [
      conn('c2', '2026-05-28T20:26:00.000Z'),
      conn('c1', '2026-05-28T20:29:00.000Z', { metadata: { host: '', destinationIP: '10.30.1.2' }, chains: ['ch-b'] }),
      conn('c3', '2026-05-28T20:26:00.000Z', { chains: ['ch-gone'] }),
      conn('c0', 'bad', { chains: ['DIRECT'] }),
      { metadata: {} },
    ],
  }, CH, now);
  assert.deepEqual(rows.map(r => r.id), ['c1', 'c2', 'c3', 'c0']);
  assert.equal(rows[0].addr, '10.30.1.2');
  assert.equal(rows[0].ip, '');
  assert.equal(rows[0].name, '实验室');
  assert.equal(rows[0].dur, 60);
  assert.equal(rows[1].addr, 'git.corp.example.test');
  assert.equal(rows[1].ip, '10.20.3.9');
  assert.equal(rows[2].name, '未知通道');
  assert.equal(rows[3].cid, null);
  assert.equal(rows[3].dur, null);
  const t = rowTexts(rows[3]);
  assert.equal(t.ch, '原网络');
  assert.equal(t.time, '—');
  assert.equal(rowTexts(rows[1]).title, 'git.corp.example.test（10.20.3.9）');
});

test('持续时间不为负：开始时间比现在晚也记 0', () => {
  const now = Date.parse('2026-05-28T20:00:00.000Z');
  const [r] = connRows({ connections: [conn('x', '2026-05-28T20:00:05.000Z')] }, CH, now);
  assert.equal(r.dur, 0);
  assert.equal(rowTexts(r).dur, '0 秒');
});

test('搜索：地址、目标 IP、通道名，不分大小写', () => {
  const rows = connRows({
    connections: [
      conn('1', '2026-05-28T20:00:00Z'),
      conn('2', '2026-05-28T20:00:01Z', { metadata: { host: 'API.cloud.example.test' }, chains: ['ch-b'] }),
      conn('3', '2026-05-28T20:00:02Z', { chains: ['DIRECT'], metadata: { destinationIP: '192.0.2.8' } }),
    ],
  }, CH, Date.parse('2026-05-28T20:01:00Z'));
  assert.deepEqual(filterConns(rows, 'api.CLOUD').map(r => r.id), ['2']);
  assert.deepEqual(filterConns(rows, '10.20.3').map(r => r.id), ['1']);
  assert.deepEqual(filterConns(rows, '实验').map(r => r.id), ['2']);
  assert.deepEqual(filterConns(rows, '原网络').map(r => r.id), ['3']);
  assert.equal(filterConns(rows, '  ').length, 3);
  assert.equal(filterConns(rows, 'nothing').length, 0);
});

test('计数：搜索时写找到几条，暂停时标出来', () => {
  assert.equal(countText({ total: 42, shown: 42, q: '', paused: false }), '共 42 条');
  assert.equal(countText({ total: 42, shown: 3, q: 'git', paused: false }), '找到 3 条');
  assert.equal(countText({ total: 42, shown: 42, q: '', paused: true }), '共 42 条 · 已暂停');
});

test('选中的行不跳：更新前后都在时固定它；没选中时只在往下滚过后固定最上面一行', () => {
  const before = new Set(['a', 'b', 'c']);
  const after = new Set(['new', 'a', 'b', 'c']);
  assert.equal(pickAnchor({ selected: 'b', before, after, scrollTop: 0, firstVisible: 'a' }), 'b');
  assert.equal(pickAnchor({ selected: null, before, after, scrollTop: 0, firstVisible: 'a' }), null);
  assert.equal(pickAnchor({ selected: null, before, after, scrollTop: 120, firstVisible: 'b' }), 'b');
  // 选中的那条这一轮刚结束：退回固定最上面一行
  assert.equal(pickAnchor({ selected: 'x', before: new Set(['x', 'b']), after: new Set(['b']), scrollTop: 40, firstVisible: 'b' }), 'b');
  // 选中的还没画出来（在分段渲染的后面几段里）
  assert.equal(pickAnchor({ selected: 'z', before, after: new Set(['z', ...after]), scrollTop: 0, firstVisible: 'a' }), null);
});

test('1000 条（many 场景）：顺序稳定，分段渲染五帧画完，新连接只加在最前面', async () => {
  const [channels, c] = await Promise.all([json('many/api/channels.json'), json('many/api/connections.json')]);
  const now = Date.parse('2026-09-28T09:00:00.000Z');
  const rows = connRows(c, channels, now);
  assert.equal(rows.length, 1000);
  assert.ok(rows.every(r => r.name && r.name !== '未知通道'));
  let limit = CHUNK, frames = 1;
  while (nextLimit(limit, rows.length) !== limit) { limit = nextLimit(limit, rows.length); frames++; }
  assert.equal(frames, 5);
  assert.ok(limit >= rows.length);
  assert.equal(nextLimit(CHUNK, 10), CHUNK);

  // 再来一轮：多一条更新的连接，原有 1000 条的相对顺序不变
  const fresh = { ...c.connections[0], id: 'conn-new', start: '2026-09-28T08:59:59.000Z' };
  const again = connRows({ ...c, connections: [...c.connections, fresh] }, channels, now + POLL_MS);
  assert.equal(again[0].id, 'conn-new');
  assert.deepEqual(again.slice(1).map(r => r.id), rows.map(r => r.id));
});

test('计数归零（reset 场景，O-04）：速度不出负数、不出尖峰，行里没有负值', async () => {
  const seq = (await json('reset/api/connections.json')).__sequence__;
  const channels = await json('reset/api/channels.json');
  let i = 0;
  let t = Date.parse('2026-05-28T20:20:00.000Z');
  const d = createData({ api: { connections: async () => seq[Math.min(i++, seq.length - 1)] }, doc: { hidden: false }, now: () => t });
  const speeds = [];
  for (let k = 0; k < 5; k++) {
    t += POLL_MS;
    await d.refresh('connections');
    const v = d.get('connections').value;
    speeds.push([v.up, v.down]);
    for (const r of connRows(v.conn, channels, t)) {
      assert.ok(r.flow == null || r.flow >= 0);
      assert.ok(r.dur == null || r.dur >= 0);
      const tx = rowTexts(r);
      assert.ok(!tx.flow.startsWith('-') && !tx.dur.startsWith('-'));
    }
  }
  const before = speeds[1];
  assert.ok(before[0] > 0 && before[1] > 0, '归零前有速度');
  for (const [up, down] of speeds.slice(2)) {
    assert.equal(up, 0);
    assert.equal(down, 0);
  }
});

test('连接采样失败不画假零，恢复第一帧不把累计流量当瞬时速度', async () => {
  let t = 0, fail = false, total = 100;
  const d = createData({ api: { connections: async () => {
    if (fail) throw new Error('fixture 503');
    return { connections: [], uploadTotal: total, downloadTotal: total };
  } }, doc: { hidden: false }, now: () => t });
  await d.refresh('connections');
  t = 1000; total = 200; await d.refresh('connections');
  assert.equal(d.get('connections').value.down, 100);
  fail = true; t = 2000; await d.refresh('connections');
  assert.ok(d.get('connections').error);
  assert.equal(d.get('connections').value.down, null);
  assert.equal(d.get('connections').value.last, null);
  fail = false; total = 1e9; t = 3000; await d.refresh('connections');
  assert.equal(d.get('connections').value.down, null);
  total += 100; t = 4000; await d.refresh('connections');
  assert.equal(d.get('connections').value.down, 100);
});

test('界面文字：没有内部用词，也没有「已阻止」', async () => {
  const src = await readFile(new URL('../../app/ui/js/screens/conns.js', import.meta.url), 'utf8');
  const code = src.split('\n').filter(l => !l.trim().startsWith('//')).join('\n');
  assert.ok(!code.includes('已阻止'));
  assert.ok(!/block/.test(code));
  const texts = ['原网络', '未知通道', countText({ total: 1, shown: 1, q: 'x', paused: true })];
  for (const s of texts) assert.ok(!FORBIDDEN.test(s), s);
});
