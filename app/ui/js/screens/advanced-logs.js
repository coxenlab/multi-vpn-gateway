// 设置 · 高级 · 查看日志（界面 22 的二级页）：运行和操作的经过，每 5 秒增量取一次，最多留 1000 条。
// 功能和原来一样：按级别、来源、关键字筛选，只看操作记录，暂停，点开看详情，复制，按日期导出。
// 日志正文由后台写，原样显示。推导是纯函数，顶层不碰 document / window。
import { h, fill, card, btn, seg, search, pop, pageState } from '../ui.js';
import { api, isAbort } from '../api.js';
import { poll } from '../poll.js';
import { day, count } from '../format.js';
import { FORBIDDEN } from '../terms.js';
import { showError, notify } from '../feedback.js';
import { pageHead, openExport } from './settings-advanced.js';

const EVERY = 5000;
export const KEEP = 1000;
const RANK = Object.freeze({ debug: 0, info: 1, warn: 2, error: 3 });

export const LEVEL = Object.freeze({
  debug: { text: '调试', cls: '' },
  info: { text: '信息', cls: '' },
  warn: { text: '提醒', cls: 'w' },
  error: { text: '出错', cls: 'e' },
});
export const levelView = l => LEVEL[l] || LEVEL.info;

const SRC = Object.freeze({
  audit: '操作记录',
  api: '操作',
  boot: '启动',
  entry: '接管网络',
  infra: '分流入口',
  manager: '通道',
  novnc: '登录窗口',
  replacement: '通道恢复',
  runtime: '运行环境',
  shutdown: '退出',
  tunnel: '转发',
  vm: '运行环境',
  watchdog: '自动修复',
});
// 来源的中文名；不认识的原样显示，含内部用词的归到「其他」
export function srcLabel(src) {
  const s = String(src || '');
  if (Object.hasOwn(SRC, s)) return SRC[s];
  return !s || FORBIDDEN.test(s) ? '其他' : s;
}

// 按 seq 合并去重，只留最新的 max 条
export function mergeEvents(prev, items, max = KEEP) {
  const m = new Map((prev || []).map(e => [e.seq, e]));
  for (const e of items || []) if (e && e.seq != null) m.set(e.seq, e);
  return [...m.values()].sort((a, b) => a.seq - b.seq).slice(-max);
}

// 操作记录一眼能看到对象：对象名加规则
export function auditTail(e) {
  if (!e || e.src !== 'audit' || !e.detail || typeof e.detail !== 'object') return '';
  const d = e.detail;
  const bits = [];
  if (d.target_name) bits.push(String(d.target_name));
  const pat = (d.after && d.after.pattern) || (d.before && d.before.pattern);
  if (pat) bits.push(String(pat));
  if (d.result === 'failed') bits.push('失败');
  return bits.join(' · ');
}

// 筛选：最低级别、来源、关键字（正文、事件名、操作对象），只看操作记录
export function filterEvents(events, { level = 'debug', src = '', q = '', audit = false } = {}) {
  const min = RANK[level] ?? 0;
  const s = String(q || '').trim().toLowerCase();
  return (events || []).filter(e => {
    if ((RANK[e.level] ?? RANK.info) < min) return false;
    if (audit && e.src !== 'audit') return false;
    if (src && e.src !== src) return false;
    if (s && ![e.msg, e.event, auditTail(e)].some(v => String(v || '').toLowerCase().includes(s))) return false;
    return true;
  });
}

const pad = (n, w = 2) => String(n).padStart(w, '0');
// 时间：今天的只显示时分秒，别的日子带上月日
export function eventTime(e, now = Date.now()) {
  const d = new Date(e && (e.ts_ms || e.ts));
  if (Number.isNaN(d.getTime())) return '—';
  const t = `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
  return day(d.getTime()) === day(now) ? t : `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${t}`;
}

// 列表上方的一行说明
export function metaText({ shown, total, paused, enabled, dropped, heals }) {
  const parts = [];
  if (enabled === false) parts.push('日志记录已关闭');
  if (paused) parts.push('已暂停');
  parts.push(shown === total ? `共 ${count(total)} 条` : `显示 ${count(shown)} / ${count(total)} 条`);
  if (heals) parts.push(`最近一天自动修复 ${heals} 次`);
  if (dropped) parts.push(`${dropped} 条没能写进文件`);
  return parts.join(' · ');
}

export const healsIn24h = (events, now = Date.now()) =>
  (events || []).filter(e => e.src === 'watchdog' && e.event === 'heal_start' && Number(e.ts_ms) >= now - 86400000).length;

export function mount(root, ctx) {
  const { signal } = ctx;
  let events = [];
  let seq = 0;
  let loaded = false;
  let failed = false;
  let enabled = null;
  let dropped = 0;
  let paused = false;
  const f = { level: 'debug', src: '', q: '', audit: false };
  const open = new Set();
  let sig = '';

  const srcSlot = h('span', { class: 'adv-slot' });
  const meta = h('span', { class: 'dim adv-count' });
  const pauseBtn = btn('暂停', { cls: 'sm', icon: 'pause', onClick: togglePause });
  const list = h('div', { class: 'adv-ev-rows' });
  const scroller = h('div', { class: 'adv-ev-scroll' },
    h('div', { class: 'tr th adv-ev-row' }, h('div', null, '时间'), h('div', null, '级别'), h('div', null, '来源'), h('div', null, '内容')),
    list);
  const body = h('div', { class: 'adv-body adv-ev-body' });

  fill(root, h('div', { class: 'adv adv-fixed' },
    pageHead('高级', '#/settings/advanced', '查看日志', meta),
    h('div', { class: 'hrow adv-tools' },
      seg([{ key: 'all', label: '全部' }, { key: 'audit', label: '只看操作记录' }], { value: 'all', label: '范围', onChange: k => { f.audit = k === 'audit'; render(); } }),
      pop([{ value: 'debug', label: '全部级别' }, { value: 'info', label: '信息以上' }, { value: 'warn', label: '提醒以上' }, { value: 'error', label: '只看出错' }],
        { value: 'debug', label: '级别', onChange: v => { f.level = v; render(); } }),
      srcSlot,
      h('span', { class: 'sp' }),
      search({ placeholder: '搜索日志', onInput: v => { f.q = v; render(); } }),
      pauseBtn,
      btn('复制', { cls: 'sm', icon: 'copy', onClick: copy }),
      btn('导出', { cls: 'sm', icon: 'export', onClick: () => openExport({ signal }) })),
    body));

  let srcSig = '';
  function renderSources() {
    const srcs = [...new Set(events.map(e => e.src).filter(Boolean))].sort();
    if (f.src && !srcs.includes(f.src)) f.src = '';
    const next = JSON.stringify([srcs, f.src]);
    if (next === srcSig) return;
    srcSig = next;
    fill(srcSlot, pop([{ value: '', label: '全部来源' }, ...srcs.map(s => ({ value: s, label: srcLabel(s) }))],
      { value: f.src, label: '来源', onChange: v => { f.src = v; render(); } }));
  }

  function row(e) {
    const lv = levelView(e.level);
    const tail = auditTail(e);
    const expanded = open.has(e.seq);
    const msg = h('button', { type: 'button', class: 'adv-ev-msg cell', 'aria-expanded': String(expanded), title: e.msg || e.event || '' },
      e.msg || e.event || '—', tail ? h('span', { class: 'dim' }, ' · ' + tail) : null);
    msg.addEventListener('click', () => { if (open.has(e.seq)) open.delete(e.seq); else open.add(e.seq); sig = ''; render(); });
    const r = h('div', { class: 'tr adv-ev-row' + (lv.cls ? ' ' + lv.cls : '') },
      h('div', { class: 'num dim' }, eventTime(e)),
      h('div', { class: 'adv-lv' }, lv.text),
      h('div', { class: 'cell', title: srcLabel(e.src) }, srcLabel(e.src)),
      msg);
    if (!expanded) return r;
    return [r, h('pre', { class: 'logs adv-ev-detail' }, JSON.stringify(e.detail ?? {}, null, 2))];
  }

  function render() {
    renderSources();
    const shown = filterEvents(events, f);
    meta.textContent = failed ? '暂时读不到，恢复后自动更新' : loaded ? metaText({ shown: shown.length, total: events.length, paused, enabled, dropped, heals: healsIn24h(events) }) : '';
    const next = JSON.stringify([loaded, failed, seq, events.length, f, [...open]]);
    if (next === sig) return;
    sig = next;
    if (!loaded) { fill(body, failed ? pageState('error', { onRetry: () => poller.kick() }) : pageState('loading')); return; }
    if (!body.contains(scroller)) fill(body, card(scroller, { cls: 'tcard adv-ev' }));
    const follow = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 48;
    fill(list, shown.length ? shown.map(row) : h('div', { class: 'adv-none dim' }, events.length ? '没有符合条件的记录' : '还没有记录'));
    if (follow) scroller.scrollTop = scroller.scrollHeight;
  }

  async function load(s) {
    if (paused) return;
    try {
      let r = await api.runtimeEvents({ since_seq: seq, limit: KEEP }, { signal: s });
      // 序号变小说明后台重启过，从头取
      if (r && Number(r.seq || 0) < seq) {
        events = [];
        seq = 0;
        r = await api.runtimeEvents({ limit: KEEP }, { signal: s });
      }
      events = mergeEvents(events, r && r.events);
      seq = Number((r && r.seq) || seq);
      if (typeof (r && r.enabled) === 'boolean') enabled = r.enabled;
      dropped = Number((r && r.dropped) || 0);
      loaded = true;
      failed = false;
    } catch (e) {
      if (isAbort(e)) return;
      if (!failed) showError(e);
      failed = true;
    }
    render();
  }

  function togglePause() {
    paused = !paused;
    fill(pauseBtn, ...btn(paused ? '继续' : '暂停', { cls: 'sm', icon: paused ? 'play' : 'pause' }).childNodes);
    pauseBtn.setAttribute('aria-pressed', String(paused));
    render();
    if (!paused) poller.kick();
  }

  async function copy() {
    const text = filterEvents(events, f).map(e => JSON.stringify(e)).join('\n');
    if (!text) { notify('没有可以复制的记录', { kind: 'info' }); return; }
    try {
      await navigator.clipboard.writeText(text);
      notify(`已复制 ${filterEvents(events, f).length} 条`, { kind: 'ok' });
    } catch {
      notify('没能复制，可以用「导出」存成文件', { kind: 'bad' });
    }
  }

  render();
  const poller = poll(load, EVERY, { signal });
  return { update() {}, unmount() {} };
}
