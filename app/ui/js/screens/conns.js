// 连接（界面 20）：正在经过通道的连接。搜索、暂停；1000 条时分段渲染，轮询更新时按连接 id 就地改，选中的行不跳（O-06）。
// 没有「已阻止」分页（DESIGN 5.7，O-05）。速度差分在 data.js，差值为负记 0（O-04），本页不显示速度。
// 纯函数顶层不碰 document / window，node 直接测试。
import { h, fill, btn, chip, search, ic, pageState } from '../ui.js';
import { chipState } from '../derive.js';
import { bytes, duration } from '../format.js';

export const CHUNK = 200;       // 每一帧最多新建的行数
export const POLL_MS = 1500;    // 连接页的取数间隔（DESIGN 4.3）

const str = v => (v == null ? '' : String(v));
const nameOf = ch => (ch && ch.name) || '未命名通道';
const pad = n => String(n).padStart(2, '0');

// 连接经过哪条通道：链路里 ch-<id> 那一项；没有时是原网络
export function channelIdOf(chains) {
  for (const c of Array.isArray(chains) ? chains : []) {
    if (typeof c === 'string' && c.startsWith('ch-') && c.length > 3) return c.slice(3);
  }
  return null;
}

// 毫秒时间戳 → 本地时间 11:42:08
export function hms(ts) {
  const d = new Date(Number(ts));
  return ts == null || Number.isNaN(d.getTime()) ? '—' : `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}

// 单条连接的流量：上下行之和。两个字段都没有（或不是非负数）时为 null，显示「—」
export function flowOf(c) {
  let sum = null;
  for (const raw of [c && c.upload, c && c.download]) {
    if (raw == null || raw === '') continue;
    const v = Number(raw);
    if (Number.isFinite(v) && v >= 0) sum = (sum ?? 0) + v;
  }
  return sum;
}

// /api/connections 的结果 → 表格行，新的在前，同一时刻按 id 排，顺序稳定。
// [{id, start, addr, ip, cid, name, flow, dur}]；dur 是秒，不小于 0
export function connRows(conn, channels, now = Date.now()) {
  const list = conn && Array.isArray(conn.connections) ? conn.connections : [];
  const names = new Map((channels || []).map(c => [str(c.id), nameOf(c)]));
  const rows = [];
  for (const c of list) {
    if (!c || c.id == null || c.id === '') continue;
    const m = c.metadata || {};
    const host = str(m.host).trim();
    const ip = str(m.destinationIP).trim();
    const t = Date.parse(c.start);
    const start = Number.isFinite(t) ? t : null;
    const cid = channelIdOf(c.chains);
    rows.push({
      id: str(c.id),
      start,
      addr: host || ip || '—',
      ip: host && ip && host !== ip ? ip : '',
      cid,
      name: cid ? (names.get(cid) ?? '未知通道') : null,
      flow: flowOf(c),
      dur: start == null ? null : Math.max(0, (now - start) / 1000),
    });
  }
  rows.sort((a, b) => {
    if (a.start !== b.start) {
      if (a.start == null) return 1;
      if (b.start == null) return -1;
      return b.start - a.start;
    }
    return a.id < b.id ? -1 : a.id > b.id ? 1 : 0;
  });
  return rows;
}

// 搜索：地址、目标 IP、通道名，不分大小写
export function filterConns(rows, q) {
  const s = str(q).trim().toLowerCase();
  if (!s) return rows;
  return rows.filter(r => [r.addr, r.ip, r.name ?? '原网络'].some(v => str(v).toLowerCase().includes(s)));
}

// 一行里各格显示的文字
export function rowTexts(r) {
  return {
    time: hms(r.start),
    addr: r.addr,
    title: r.ip ? `${r.addr}（${r.ip}）` : r.addr,
    ch: r.cid ? r.name : '原网络',
    flow: r.flow == null ? '—' : bytes(r.flow),
    dur: r.dur == null ? '—' : duration(r.dur),
  };
}

// 标题旁的计数
export function countText({ total, shown, q, paused }) {
  const base = str(q).trim() ? `找到 ${shown} 条` : `共 ${total} 条`;
  return paused ? base + ' · 已暂停' : base;
}

// 更新前后要保持位置不变的那一行：选中的行（更新前后都在表里）优先；
// 没选中时，只在已经往下滚过的情况下固定最上面那一行，停在顶部时新连接照常从上面出现。
export function pickAnchor({ selected, before, after, scrollTop, firstVisible }) {
  const both = id => id != null && before.has(id) && after.has(id);
  if (both(selected)) return selected;
  if (scrollTop > 0 && both(firstVisible)) return firstVisible;
  return null;
}

// 分段渲染：还有没画出来的行时，下一帧多画一段
export const nextLimit = (limit, total, chunk = CHUNK) => (total > limit ? limit + chunk : limit);

// 运行环境没起来（顶栏已经有「启动」提示条）
const runtimeDown = system => {
  const s = chipState(system);
  return s === 'runtime' || s === 'starting';
};

export function mount(root, ctx) {
  const { signal, data } = ctx;
  let q = '';
  let paused = false;
  let pausedAt = 0;
  let selected = null;
  let limit = CHUNK;
  let raf = 0;
  let channels = [];
  let system = null;
  let latest = null;   // 最新一次取到的快照
  let shown = null;    // 正在显示的快照（暂停时停在这一份）
  let rows = [];       // 过滤后的全部行
  const nodes = new Map(); // id → {el, t, cells}

  const countEl = h('span', { class: 'dim num cn-count', 'aria-live': 'polite' });
  const staleEl = h('span', { class: 'dim3 cn-stale', hidden: true }, '暂时读不到新数据，稍后自动重试');
  const pauseBtn = btn('暂停', { icon: 'pause', onClick: togglePause });
  pauseBtn.setAttribute('aria-pressed', 'false');
  const searchEl = search({
    placeholder: '搜索地址',
    onInput: v => { q = v; limit = CHUNK; scrollEl.scrollTop = 0; render(); },
  });
  const rowsEl = h('div', { class: 'cn-rows', role: 'rowgroup' });
  const noneEl = h('div', { class: 'cn-none', hidden: true }, '没有找到这个地址的连接');
  // 表头放在滚动区里（吸顶），出现滚动条时和各行一起变窄，列不会错开
  const scrollEl = h('div', { class: 'cn-scroll' },
    h('div', { class: 'tr th', role: 'row' },
      h('div', null, '时间'), h('div', null, '地址'), h('div', null, '通道'), h('div', { class: 'cell r' }, '流量'), h('div', null, '持续')),
    rowsEl, noneEl);
  const table = h('div', { class: 'card tcard cn-list', role: 'table', 'aria-label': '连接' }, scrollEl);
  const body = h('div', { class: 'cn-body' });
  let bodyKind = '';

  fill(root, h('div', { class: 'hrow cn-head' },
    h('h1', { class: 'h1' }, '连接'), countEl, staleEl,
    h('span', { class: 'sp' }), searchEl, pauseBtn), body);

  rowsEl.addEventListener('click', e => {
    const el = e.target.closest('.cn-row');
    if (el) pick(el.dataset.id);
  });
  rowsEl.addEventListener('keydown', e => {
    const el = e.target.closest('.cn-row');
    if (el && e.target === el && (e.key === ' ' || e.key === 'Enter')) { e.preventDefault(); pick(el.dataset.id); }
  });

  function pick(id) {
    const prev = selected != null && nodes.get(selected);
    if (prev) { prev.el.classList.remove('sel'); prev.el.setAttribute('aria-selected', 'false'); }
    selected = selected === id ? null : id;
    const next = selected != null && nodes.get(selected);
    if (next) { next.el.classList.add('sel'); next.el.setAttribute('aria-selected', 'true'); }
  }

  function togglePause() {
    paused = !paused;
    if (paused) pausedAt = Date.now();
    else if (latest) shown = latest;
    fill(pauseBtn, ic(paused ? 'play' : 'pause', 14, 1.6), paused ? '继续' : '暂停');
    pauseBtn.setAttribute('aria-pressed', String(paused));
    render();
  }

  function setBody(kind, el) {
    if (bodyKind === kind) return;
    bodyKind = kind;
    fill(body, el);
  }

  function chCell(t, r) {
    return r.cid ? chip(t.ch) : h('span', { class: 'dim' }, t.ch);
  }

  function makeRow(r) {
    const t = rowTexts(r);
    const cells = {
      time: h('div', { class: 'num dim' }, t.time),
      addr: h('div', { class: 'cell mono', title: t.title }, t.addr),
      ch: h('div', { class: 'cell' }, chCell(t, r)),
      flow: h('div', { class: 'cell r num' }, t.flow),
      dur: h('div', { class: 'cell dim num' }, t.dur),
    };
    const on = r.id === selected;
    const el = h('div', { class: 'tr cn-row' + (on ? ' sel' : ''), role: 'row', tabindex: '0', 'aria-selected': on ? 'true' : 'false', 'data-id': r.id },
      cells.time, cells.addr, cells.ch, cells.flow, cells.dur);
    return { el, t, cells, r };
  }

  // 只改变了的格子
  function patchRow(n, r) {
    const t = rowTexts(r);
    for (const k of ['time', 'addr', 'flow', 'dur']) if (n.t[k] !== t[k]) n.cells[k].textContent = t[k];
    if (n.t.title !== t.title) n.cells.addr.title = t.title;
    if (n.t.ch !== t.ch || !n.r.cid !== !r.cid) fill(n.cells.ch, chCell(t, r));
    n.t = t;
    n.r = r;
  }

  function firstVisible() {
    const top = scrollEl.scrollTop + rowsEl.offsetTop;
    for (const el of rowsEl.children) if (el.offsetTop + el.offsetHeight > top) return el.dataset.id;
    return null;
  }

  function patchRows() {
    const want = rows.slice(0, limit);
    const after = new Set(want.map(r => r.id));
    const anchor = pickAnchor({ selected, before: nodes, after, scrollTop: scrollEl.scrollTop, firstVisible: firstVisible() });
    const offset = anchor != null ? nodes.get(anchor).el.offsetTop - scrollEl.scrollTop : 0;

    for (const [id, n] of nodes) if (!after.has(id)) { n.el.remove(); nodes.delete(id); }
    let cur = rowsEl.firstChild;
    for (const r of want) {
      let n = nodes.get(r.id);
      if (n) patchRow(n, r); else { n = makeRow(r); nodes.set(r.id, n); }
      if (cur === n.el) cur = cur.nextSibling;
      else rowsEl.insertBefore(n.el, cur);
    }
    if (anchor != null) scrollEl.scrollTop = nodes.get(anchor).el.offsetTop - offset;

    if (!raf && rows.length > limit) {
      raf = requestAnimationFrame(() => {
        raf = 0;
        if (signal.aborted) return;
        limit = nextLimit(limit, rows.length);
        patchRows();
      });
    }
  }

  const runtimeEmpty = () => pageState('empty', { title: '运行环境没启动', text: '启动以后，经过通道的连接会出现在这里。' });

  function render() {
    if (signal.aborted) return;
    staleEl.hidden = !(latest && latest.error && shown && shown.value);
    const v = shown && shown.value;
    if (v?.unavailable && !v.conn?.connections?.length) {
      countEl.textContent = '';
      setBody('unavailable', pageState('error', { title: '暂时读不到新数据', onRetry: () => data.refresh('connections') }));
      return;
    }
    if (!v) {
      countEl.textContent = '';
      if (!shown || !shown.error) setBody('loading', pageState('loading'));
      else if (runtimeDown(system)) setBody('runtime', runtimeEmpty());
      else setBody('error', pageState('error', { title: '没能读取连接', onRetry: () => data.refresh('connections') }));
      return;
    }
    const all = connRows(v.conn, channels, paused ? pausedAt : Date.now());
    if (selected != null && !paused && !all.some(r => r.id === selected)) selected = null;
    rows = filterConns(all, q);
    countEl.textContent = countText({ total: all.length, shown: rows.length, q, paused });
    if (!all.length) {
      if (runtimeDown(system)) { countEl.textContent = ''; setBody('runtime', runtimeEmpty()); } else setBody('empty', pageState('empty', { title: '现在没有连接', text: '有程序访问通道负责的地址时，这里会列出来。' }));
      return;
    }
    setBody('table', table);
    noneEl.hidden = rows.length > 0;
    patchRows();
  }

  data.subscribe('core', snap => {
    const cv = snap.value || {};
    if (Array.isArray(cv.channels)) channels = cv.channels;
    system = cv.system ?? system;
    render();
  }, { signal });
  data.subscribe('connections', snap => {
    latest = snap;
    if (!paused || !(shown && shown.value)) shown = snap;
    render();
  }, { signal, ms: POLL_MS });

  render();

  const stop = () => { if (raf) cancelAnimationFrame(raf); raf = 0; };
  signal.addEventListener('abort', stop, { once: true });
  return { update() {}, unmount: stop };
}
