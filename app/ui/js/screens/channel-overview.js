// 通道 · 概览（界面 03）：延迟、今天的流量、在用规则数、最近一小时的延迟、连接信息、备忘录。
// 由 channels.js 挂在详情头和分页下面；通道或 /health 有更新时它调 view.channel(ch, health)。
// 延迟曲线和今天的流量是新接口，返回 404 时整块隐藏。备忘录保存时带上原文，别处先改过就返回 409：提示冲突，草稿保留。
import { h, fill, card, lb, kv, btn, field, st } from '../ui.js';
import { api, isAbort, isUnavailable, ApiError } from '../api.js';
import { area } from '../chart.js';
import { poll } from '../poll.js';
import { displayState, channelRules, isActive } from '../derive.js';
import { bytesParts, ago } from '../format.js';
import { showError, humanize } from '../feedback.js';
import { toTargetUrl, tryTarget, targetFailureText } from './channel-unverified.js';

const LATENCY_MS = 30000;
const NOTE_MAX = 20000;

// 切换通道会整页重挂：没保存的备忘录草稿按通道留在这里，回来时还在
const drafts = new Map(); // id → {text, base}
const targetDrafts = new Map();

// 运行中的稳定客户端才可检测；检测失败本身不禁止再次尝试。
export const canVerifyTarget = ch => isActive(ch) && ch.stop_pending !== true && ch.container_state !== 'unknown' &&
  (!ch.replacement?.phase || ['awaiting_login', 'committed', 'rolled_back'].includes(ch.replacement.phase));

// /latency 的 points [[时间戳, 延迟|null], …] → 折线图的值和三段时间文字（以最后一个点为「现在」）
export function latencyChart(points) {
  const pts = (Array.isArray(points) ? points : []).filter(p => Array.isArray(p) && Number.isFinite(Number(p[0])));
  const values = pts.map(p => (p[1] == null || !Number.isFinite(Number(p[1])) ? null : Number(p[1])));
  if (pts.length < 2) return { values, labels: null };
  const span = Math.round((Number(pts[pts.length - 1][0]) - Number(pts[0][0])) / 60000);
  const before = m => (m >= 60 && m % 60 === 0 ? `${m / 60} 小时前` : m >= 1 ? `${m} 分钟前` : '刚才');
  const half = Math.round(span / 2);
  return { values, labels: [before(span), half >= 1 && half < span ? before(half) : '', '现在'] };
}

// 在用的规则数
export const enabledRules = ch => channelRules(ch).filter(r => !!r.enabled).length;

// 今天的流量：接口不可用或 available 为假时返回 null（整块隐藏）
export function todayBytes(traffic, id) {
  if (!traffic || isUnavailable(traffic) || traffic.available === false) return null;
  const t = traffic.channels && traffic.channels[id];
  return t ? (Number(t.up) || 0) + (Number(t.down) || 0) : 0;
}

// 上次检测
export function lastCheck(health, now = Date.now()) {
  const uncertain = health?.probe_failed === true || (health?.checked_at != null && health.connected == null);
  if (uncertain) return '状态待确认';
  if (!health || health.checked_at == null) return '还没有检测';
  return `${ago(health.checked_at, now)}，${health.connected ? '正常' : '不通'}`;
}

const bigNum = (num, unit) => h('div', { class: 'big' }, num, unit ? h('small', null, unit) : null);

export function mount(root, ctx) {
  const { signal, data } = ctx;
  const id = ctx.params.id;
  let ch = ctx.channel;
  let health = ctx.health;
  let traffic = null;
  let latency = null; // null 还没取到；UNAVAILABLE 接口不存在；否则 {points}
  let targetBusy = false;
  let targetBase = ch.probe_url || '';

  const latCard = card([], { cls: 'ch-latency' });
  const statBox = h('div', { class: 'ch-stats' });
  const infoCard = card([], { cls: 'list' });
  const memoCard = card([], { cls: 'ch-memo' });
  const targetInput = field({ label: '内网验证地址', value: targetDrafts.get(id) ?? targetBase, placeholder: '例如 https://oa.example.com', onEnter: checkTarget });
  const targetBtn = btn('检测并保存', { cls: 'sm', onClick: checkTarget });
  const targetResult = h('div', { class: 'cs-result', role: 'status', hidden: true });
  const targetSummary = h('span', { class: 'ch-target-summary' });
  const targetDetails = h('details', { class: 'ch-target-editor', open: targetDrafts.has(id) || !targetBase || health?.awaiting_target === true },
    h('summary', null, h('b', null, '连通性检测'), targetSummary, h('span', { class: 'ch-target-edit' }, '修改地址')),
    h('div', { class: 'ch-target-form' }, h('div', { class: 'cs-url' }, targetInput, targetBtn),
      h('p', { class: 'dim cs-foot' }, '填写连上通道后才能访问的内网地址，检测通过后保存。'), targetResult));
  const overview = h('div', { class: 'ch-overview' }, statBox, latCard,
    h('div', { class: 'ch-overview-details' }, targetDetails, infoCard, memoCard));
  fill(root, overview);
  latCard.hidden = true;

  // ---- 数字 ----

  function renderStats() {
    const s = displayState(ch, health);
    const lat = s === 'connected' ? (health && health.latency_ms != null ? health.latency_ms : ch.latency_ms) : null;
    const cards = [h('div', { class: 'ch-metric' }, lb('延迟'), lat == null ? bigNum('—') : bigNum(String(Math.round(lat)), '毫秒'))];
    const bytes = todayBytes(traffic, id);
    if (bytes != null) {
      const p = bytesParts(bytes);
      cards.push(h('div', { class: 'ch-metric' }, lb('今天的流量'), bigNum(p.num, p.unit)));
    }
    cards.push(h('div', { class: 'ch-metric' }, lb('启用规则'), bigNum(String(enabledRules(ch)), '条')));
    fill(statBox, cards);
  }

  function renderInfo() {
    const rows = [];
    if (ch.server) rows.push(kv('服务器', ch.server));
    if (ch.username) rows.push(kv('账号', ch.username));
    fill(infoCard, rows);
    infoCard.hidden = rows.length === 0;
    const active = isActive(ch);
    targetBtn.disabled = targetBusy || !canVerifyTarget(ch);
    targetBtn.textContent = targetBusy ? '正在检测' : '检测并保存';
    const addr = ch.probe_url || health?.target || '';
    targetSummary.textContent = addr ? `${addr} · ${active ? lastCheck(health) : '通道已停止'}` : '还没有验证地址';
    targetSummary.title = targetSummary.textContent;
    if (targetInput.value === targetBase) targetInput.value = ch.probe_url || '';
    targetBase = ch.probe_url || '';
  }

  function renderLatency() {
    if (!latency || isUnavailable(latency)) { latCard.hidden = true; return; }
    const c = latencyChart(latency.points);
    latCard.hidden = c.values.filter(v => v != null).length < 2;
    if (latCard.hidden) return;
    const chart = h('div', { class: 'ch-chart' });
    // 按实际栏宽绘图，避免侧栏里的坐标文字随整张 SVG 缩成几像素。
    chart.innerHTML = area(c.values, { w: Math.max(240, latCard.clientWidth - 32), h: 110, labels: c.labels || undefined, empty: '最近一小时还没有检测结果' });
    fill(latCard, lb('最近一小时的延迟'), chart);
  }

  async function checkTarget() {
    if (targetBusy || !canVerifyTarget(ch)) return;
    const target = toTargetUrl(targetInput.value);
    const resultText = (kind, text) => { fill(targetResult, st(kind, text)); targetResult.hidden = false; };
    if (!target) { resultText('bad', '地址写得不对，例如 https://oa.example.com/'); return; }
    targetBusy = true;
    targetResult.hidden = true;
    renderInfo();
    try {
      const result = await tryTarget(api, id, target, { signal });
      if (signal.aborted) return;
      if (result.connected) {
        ch = { ...ch, probe_url: result.target };
        targetInput.value = result.target;
        targetBase = result.target;
        targetDrafts.delete(id);
        health = { ...(health || {}), connected: true, awaiting_target: false, probe_failed: false, target: result.target, checked_at: Date.now() };
        ctx.onVerified?.(result);
        resultText('ok', '检测通过，地址已保存');
        renderStats();
      } else resultText('bad', targetFailureText());
      data.refresh('core');
      data.refresh('health');
    } catch (e) {
      if (isAbort(e)) return;
      const m = humanize(e, { fallback: '检测没有完成' });
      if (m) resultText('bad', m.text ? `${m.title}。${m.text}` : m.title);
    } finally {
      targetBusy = false;
      if (!signal.aborted) renderInfo();
    }
  }

  // ---- 备忘录 ----

  const memo = { loaded: false, base: '', latest: null, busy: false, failed: false };
  const saved = drafts.get(id);
  const ta = h('textarea', {
    class: 'memo ch-memo-text', 'aria-label': '备忘录', spellcheck: 'false', maxlength: String(NOTE_MAX),
    placeholder: '写下登录要用的提示，比如验证码发到哪台手机',
  });
  ta.addEventListener('input', () => { keepDraft(); renderMemo(); });
  // 高度随内容：至少三行，最多十二行，再多就在框里滚动
  const fit = () => { ta.rows = Math.min(12, Math.max(3, ta.value.split('\n').length + 1)); };
  const memoState = h('span', { class: 'dim ch-memo-state' });
  const saveBtn = btn('保存', { cls: 'more', onClick: () => saveNote() });
  const conflict = h('div', { class: 'ch-conflict', role: 'alert' });
  fill(memoCard, lb('备忘录', h('span', { class: 'ch-memo-acts' }, memoState, saveBtn)), conflict, ta);

  const dirty = () => memo.loaded && ta.value !== memo.base;

  function keepDraft() {
    if (dirty() || memo.latest !== null) drafts.set(id, { text: ta.value, base: memo.base });
    else drafts.delete(id);
  }

  function renderMemo() {
    fit();
    ta.disabled = !memo.loaded || memo.busy;
    saveBtn.hidden = !dirty() || memo.latest !== null;
    saveBtn.disabled = memo.busy;
    memoState.textContent = memo.busy ? '正在保存' : memo.failed ? '没能读取' : dirty() ? '还没保存' : '';
    conflict.hidden = memo.latest === null;
    if (memo.latest !== null) {
      fill(conflict,
        h('span', null, '备忘录在别处改过了，你写的草稿还在。'),
        h('span', { class: 'ch-conflict-acts' },
          btn('用我的覆盖', { cls: 'sm', disabled: memo.busy, onClick: () => { memo.base = memo.latest; memo.latest = null; saveNote(); } }),
          btn('看最新的', { cls: 'sm ghost', disabled: memo.busy, onClick: () => {
            ta.value = memo.latest; memo.base = memo.latest; memo.latest = null; keepDraft(); renderMemo();
          } })));
    }
  }

  async function loadNote() {
    let note;
    try { note = String((await api.noteGet(id, { signal })).note || ''); } catch (e) {
      if (isAbort(e)) return;
      memo.failed = true;
      renderMemo();
      return;
    }
    memo.loaded = true;
    if (saved && saved.text !== note) {
      // 回来时草稿还在：原文没变就接着编辑；原文变了按冲突处理
      ta.value = saved.text;
      memo.base = saved.base;
      if (note !== saved.base) memo.latest = note;
    } else {
      ta.value = note;
      memo.base = note;
    }
    keepDraft();
    renderMemo();
  }

  async function saveNote() {
    if (memo.busy || !memo.loaded || memo.latest !== null) return;
    const text = ta.value;
    memo.busy = true;
    renderMemo();
    try {
      await api.noteSet(id, text, memo.base, { signal });
      memo.base = text;
    } catch (e) {
      if (isAbort(e)) return;
      if (e instanceof ApiError && e.status === 409) {
        try { memo.latest = String((await api.noteGet(id, { signal })).note || ''); } catch (e2) {
          if (isAbort(e2)) return;
          memo.latest = '';
        }
      } else showError(e);
    }
    memo.busy = false;
    keepDraft();
    renderMemo();
  }

  // ---- 取数 ----

  renderStats();
  renderInfo();
  renderMemo();
  loadNote();

  let chartWidth = 0;
  const chartResize = new ResizeObserver(() => {
    const width = latCard.clientWidth;
    if (width !== chartWidth) { chartWidth = width; renderLatency(); }
  });
  chartResize.observe(latCard);

  data.subscribe('traffic', snap => {
    if (snap.value === undefined) return;
    traffic = snap.value;
    renderStats();
  }, { signal });

  const latCtrl = new AbortController();
  signal.addEventListener('abort', () => latCtrl.abort(), { once: true });
  poll(async s => {
    let r;
    try { r = await api.latency(id, { signal: s }); } catch (e) {
      if (!isAbort(e)) console.warn('[latency]', e);
      return;
    }
    latency = r;
    renderLatency();
    if (isUnavailable(r)) latCtrl.abort();
  }, LATENCY_MS, { signal: latCtrl.signal });

  return {
    update() {},
    channel(next, nextHealth) {
      ch = next;
      health = nextHealth;
      renderStats();
      renderInfo();
    },
    unmount() {
      keepDraft();
      if (targetInput.value !== targetBase) targetDrafts.set(id, targetInput.value);
      else targetDrafts.delete(id);
      chartResize.disconnect();
      latCtrl.abort();
    },
  };
}
