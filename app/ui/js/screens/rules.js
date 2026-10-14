// 规则（界面 16）：一张表，哪个地址走哪条通道、现在通不通。批量启停、搜索、「现在」一列（DESIGN 4.3）。
// 流向图（17）、测试地址（18）复用这里导出的页头和纯函数。
// 纯函数顶层不碰 document / window，node 直接测试。
import { h, fill, card, btn, cell, chip, tog, seg, search, ic, empty, pageState } from '../ui.js';
import { api, isUnavailable, isAbort } from '../api.js';
import { displayState, ruleFate, domainsVia, rulesOff } from '../derive.js';
import { FATE, TAKEOVER_ACTION, RETRY_FAILED, PHRASE } from '../terms.js';
import { poll } from '../poll.js';
import { showError, showDone, notify } from '../feedback.js';

export const ruleOn = r => !!r && r.enabled !== false && r.enabled !== 0;
export const nameOf = ch => (ch && ch.name) || '未命名通道';

// 全部规则，按通道顺序、每条通道先域名后网段：[{rule, ch}]
export function allRules(channels) {
  const out = [];
  for (const ch of channels || []) {
    for (const rule of [...(ch.domains || []), ...(ch.ips || [])]) out.push({ rule, ch });
  }
  return out;
}

// 搜索：地址、备注、通道名，不分大小写
export function filterRows(rows, q) {
  const s = String(q ?? '').trim().toLowerCase();
  if (!s) return rows;
  return rows.filter(({ rule, ch }) =>
    [rule.pattern, rule.note, ch && ch.name].some(v => String(v ?? '').toLowerCase().includes(s)));
}

// 一条规则「现在」怎么走。库里的 enabled 是 0/1，先转成布尔。
// takeover（/api/takeover）说域名规则没有生效时，本该进通道的域名规则写「还没生效」
export function fateFor(rule, ch, health, system, takeover = null) {
  return ruleFate({ ...rule, enabled: ruleOn(rule) }, ch, {
    routingOff: !!(system && system.routing_off),
    state: displayState(ch, health),
    blocked: health && health.blocked,
    takeover,
  });
}

// 不读系统代理的程序（UP1 E4 的对照项，V2 按实测结论改）
export const NO_PROXY_APPS = Object.freeze(['curl', 'git', 'ssh']);

// 域名规则的说明。takeover 是 /api/takeover 的结果，按实际状态说（DESIGN 5.8）：
// 域名规则没有生效时写明，并给能让它生效的动作（fix，见 derive.domainsOff）；
// 虚拟网卡模式下写域名规则对哪些程序不生效（PLAN 5.13 风险表、TESTS T-05）。其余情况、接口不存在时返回 null
export function takeoverNote(takeover) {
  if (!takeover || isUnavailable(takeover)) return null;
  const off = rulesOff(takeover);
  if (off) return { text: off.text, fix: off.fix, off: true };
  if (takeover.effective !== 'tun') return null;
  const via = domainsVia(takeover);
  if (via === 'tun') {
    return { text: 'dig、nslookup 和开了安全 DNS 的浏览器不走这里，域名规则对它们不生效。' };
  }
  if (via === 'system_proxy') {
    return { text: `域名规则由系统代理处理，不读系统代理的程序不生效，比如 ${NO_PROXY_APPS.join('、')}。`, apps: NO_PROXY_APPS };
  }
  return null;
}

// 让域名规则生效的按钮（fix 见 derive.domainsOff）。onDone(PUT 返回的新状态)
export function fixButton(fix, { takeover, signal, navigate, onDone }) {
  if (!fix) return null;
  const label = fix === 'retry' ? TAKEOVER_ACTION.retry_takeover.button
    : fix === 'system-proxy-ok' ? TAKEOVER_ACTION.enable_system_proxy.button : '去设置';
  const b = btn(label, {
    cls: 'sm', onClick: async () => {
      if (fix === 'settings') { navigate('#/settings'); return; }
      b.disabled = true;
      try {
        const body = fix === 'retry' ? { mode: (takeover && takeover.mode) || 'auto' } : { system_proxy_ok: true };
        const r = await api.setTakeover(body, { signal });
        if (isUnavailable(r)) notify(PHRASE.unavailable);
        else {
          if (r && r.action === 'retry_takeover') notify(RETRY_FAILED, { kind: 'bad' });
          else showDone(r, '已处理');
          if (r && typeof r === 'object') onDone(r);
        }
      } catch (e) {
        if (!isAbort(e)) showError(e);
      }
      if (!signal.aborted) b.disabled = false;
    },
  });
  return b;
}

// 规则页的页头：标题、列表 / 流向图、搜索、测试地址、添加规则
export function rulesHead({ view, navigate, query, onSearch }) {
  return h('div', { class: 'hrow rl-head' },
    h('h1', { class: 'h1' }, '规则'),
    seg([{ key: 'list', label: '列表', icon: 'list' }, { key: 'flow', label: '流向图', icon: 'map' }], {
      value: view, label: '看法',
      onChange: k => navigate(k === 'flow' ? '#/rules?view=flow' : '#/rules'),
    }),
    h('span', { class: 'sp' }),
    search({ placeholder: '搜索地址', value: query || '', onInput: onSearch }),
    btn('测试地址', { icon: 'target', onClick: () => navigate('#/rules/test') }),
    btn('添加规则', { cls: 'soft', icon: 'plus', onClick: () => navigate('?sheet=rule-add') }));
}

// 取 /api/takeover（探测）：挂载时一次，之后 30 秒一次。fn(结果)，接口不存在时是 UNAVAILABLE
export function watchTakeover(signal, fn) {
  let done = false;
  poll(async s => {
    if (done) return;
    try {
      const t = await api.takeover({ signal: s });
      if (isUnavailable(t)) done = true;
      fn(t);
    } catch (e) { if (!isAbort(e)) fn(null); }
  }, 30000, { signal });
}

// 通道列表 + 健康数据的共同订阅。onChange({channels, system, health, error, loading})
export function watchRules(data, signal, onChange) {
  const st = { channels: null, system: null, health: {}, error: null, loading: true };
  data.subscribe('core', snap => {
    const v = snap.value || {};
    st.channels = v.channels ?? st.channels;
    st.system = v.system ?? st.system;
    st.error = snap.error;
    st.loading = false;
    onChange(st);
  }, { signal });
  data.subscribe('health', snap => {
    st.health = snap.value || {};
    if (!st.loading) onChange(st);
  }, { signal });
  return st;
}

// 页面状态：首次加载、读不到、没有通道、没有规则；都不是时返回 null
export function stateView(st, { onRetry, navigate }) {
  if (st.loading && !st.channels) return pageState('loading');
  if (!st.channels) return pageState('error', { onRetry });
  if (!st.channels.length) {
    return pageState('empty', {
      title: '还没有通道', text: '先添加一条通道，再给它加规则。',
      action: btn('添加通道', { cls: 'pri lg', icon: 'plus', onClick: () => navigate('?sheet=add') }),
    });
  }
  if (!allRules(st.channels).length) {
    return pageState('empty', {
      title: '还没有规则', text: '加一条通道负责的地址，比如 oa.example 或 10.0.0.0/8。',
      action: btn('添加规则', { cls: 'pri lg', icon: 'plus', onClick: () => navigate('?sheet=rule-add') }),
    });
  }
  return null;
}

const FATE_ICON = { channel: 'check', blocked: 'block' };

export function fateTag(fate) {
  const f = FATE[fate] || FATE.waiting;
  return h('span', { class: 'fate ' + f.cls, title: f.text }, FATE_ICON[fate] ? ic(FATE_ICON[fate], 12, 2) : null, f.text);
}

export function mount(root, ctx) {
  const { signal, navigate, data } = ctx;
  let q = '';
  let takeover = null;
  const selected = new Set();   // 选中的规则 id
  const pending = new Set();    // 正在改的规则 id
  let sig = '';
  let refocus = null;           // 开关改完以后要回到的控件 {rid, part}：改的时候开关被禁用，焦点会先丢

  const body = h('div', { class: 'rl-body' });
  fill(root, rulesHead({ view: 'list', navigate, onSearch: v => { q = v; render(true); } }), body);

  const st = watchRules(data, signal, () => render());
  watchTakeover(signal, t => { takeover = t; render(); });

  async function toggleOne(rule, ch, on, input) {
    if (pending.has(rule.id)) return;
    pending.add(rule.id);
    if (document.activeElement === input) refocus = { rid: String(rule.id), part: 'tog' };
    input.disabled = true;
    try {
      const r = await api.toggleRule(ch.id, rule.id, on, { signal });
      showDone(r, on ? '已启用' : '已停用');
    } catch (e) {
      if (isAbort(e)) return;
      input.checked = !on;
      showError(e);
    }
    pending.delete(rule.id);
    await data.refresh('core');
    render(true);
  }

  async function toggleMany(on) {
    const ids = [...selected].filter(id => !pending.has(id));
    if (!ids.length) return;
    ids.forEach(id => pending.add(id));
    render(true);
    try {
      const r = await api.toggleRules(ids, on, { signal });
      showDone(r, `${on ? '已启用' : '已停用'} ${ids.length} 条`);
      selected.clear();
    } catch (e) {
      if (isAbort(e)) return;
      showError(e);
    }
    ids.forEach(id => pending.delete(id));
    await data.refresh('core');
    render(true);
  }

  function pick(id) {
    if (selected.has(id)) selected.delete(id); else selected.add(id);
    render(true);
  }

  function row({ rule, ch }) {
    const busy = pending.has(rule.id);
    const on = selected.has(rule.id);
    const input = tog({ checked: ruleOn(rule), disabled: busy, label: '启用 ' + rule.pattern, onChange: v => toggleOne(rule, ch, v, input) });
    const el = h('div', {
      class: 'tr rl-row' + (on ? ' sel' : ''), tabindex: '0', role: 'row', 'aria-selected': on ? 'true' : 'false', 'data-rid': String(rule.id),
      onClick: e => { if (!e.target.closest('input')) pick(rule.id); },
      onKeydown: e => { if (e.target === el && (e.key === ' ' || e.key === 'Enter')) { e.preventDefault(); pick(rule.id); } },
    },
      h('div', { class: 'rl-addr' }, cell(rule.pattern, { cls: 'mono' }), rule.note ? h('span', { class: 'dim cell rl-note', title: rule.note }, rule.note) : null),
      h('div', { class: 'cell' }, chip(nameOf(ch), { idle: displayState(ch, st.health[ch.id]) === 'stopped' })),
      h('div', { class: 'cell' }, fateTag(fateFor(rule, ch, st.health[ch.id], st.system, takeover))),
      h('div', { class: 'cell r' }, input));
    return el;
  }

  function selBar(rows) {
    if (!selected.size) return null;
    const all = rows.length && rows.every(r => selected.has(r.rule.id));
    const busy = [...selected].some(id => pending.has(id));
    return h('div', { class: 'rl-sel', role: 'toolbar', 'aria-label': '批量操作' },
      h('b', null, `已选 ${selected.size} 条`),
      all ? null : h('button', { type: 'button', class: 'link', onClick: () => { rows.forEach(r => selected.add(r.rule.id)); render(true); } }, `全选这 ${rows.length} 条`),
      h('span', { class: 'sp' }),
      btn('启用', { cls: 'sm', disabled: busy, onClick: () => toggleMany(true) }),
      btn('停用', { cls: 'sm', disabled: busy, onClick: () => toggleMany(false) }),
      btn('取消选择', { cls: 'sm ghost', onClick: () => { selected.clear(); render(true); } }));
  }

  function render(force) {
    if (signal.aborted) return;
    const state = stateView(st, { onRetry: () => data.refresh('core'), navigate });
    const all = state ? [] : allRules(st.channels);
    // 已经不在的规则不再算选中
    const alive = new Set(all.map(r => r.rule.id));
    for (const id of selected) if (!alive.has(id)) selected.delete(id);
    const note = takeoverNote(takeover);
    const next = JSON.stringify([state ? (st.channels ? st.channels.length : -1) + ':' + (st.loading ? 1 : 0) : 0, all.map(({ rule, ch }) =>
      [rule.id, rule.pattern, rule.note, ruleOn(rule), ch.id, ch.name, fateFor(rule, ch, st.health[ch.id], st.system, takeover), displayState(ch, st.health[ch.id])]), note && note.text, note && note.fix]);
    if (!force && next === sig) return;
    sig = next;
    if (state) { fill(body, state); refocus = null; return; }
    const keep = focusedRow();
    const top = (body.querySelector('.rl-rows') || {}).scrollTop || 0;
    const rows = filterRows(all, q);
    const table = h('div', { class: 'rl-rows' });
    if (rows.length) fill(table, rows.map(row));
    else fill(table, h('div', { class: 'rl-none' }, '没有找到这个地址的规则'));
    fill(body, h('div', { class: 'card tcard rl-list' },
      selBar(rows),
      h('div', { class: 'tr th' }, h('div', null, '地址'), h('div', null, '走哪条通道'), h('div', null, '现在'), h('div', { class: 'cell r' }, '启用')),
      table,
      h('div', { class: 'rl-foot' },
        note ? h('div', { class: 'rl-warn', role: 'note' }, ic('info', 14, 1.6), h('span', null, note.text),
          fixButton(note.fix, { takeover, signal, navigate, onDone: t => { takeover = t; render(true); } })) : null,
        h('div', { class: 'dim' }, '顺序自动安排：范围更小的地址优先。没有命中任何规则的地址走原网络。'))));
    table.scrollTop = top;
    if (keep) restoreFocus(table, keep);
  }

  // 整表重建前记下焦点在哪一行的哪个控件（行本身或启用开关）。
  // 焦点不在表里时，只在开关改动中丢了焦点（落到 body）的情况下用 refocus 找回
  function focusedRow() {
    const a = document.activeElement;
    const row = a && body.contains(a) ? a.closest('.rl-row') : null;
    const lost = !a || a === document.body || a === document.documentElement;
    let keep = row ? { rid: row.dataset.rid, part: a.matches('input.tog') ? 'tog' : 'row' } : null;
    if (refocus && (lost || (keep && keep.rid === refocus.rid))) keep = refocus;
    else if (!lost) refocus = null;
    // 开关改完（不在 pending 里）以后，这次用过就清掉
    if (refocus && !pending.has(Number(refocus.rid)) && !pending.has(refocus.rid)) refocus = null;
    return keep;
  }

  function restoreFocus(table, { rid, part }) {
    const row = [...table.querySelectorAll('.rl-row')].find(r => r.dataset.rid === rid);
    if (!row) return;
    const target = part === 'tog' ? row.querySelector('input.tog') : row;
    if (target && !target.disabled) target.focus({ preventScroll: true });
    else if (target && part === 'tog') row.focus({ preventScroll: true });
  }

  render();
  return { update() {}, unmount() {} };
}
