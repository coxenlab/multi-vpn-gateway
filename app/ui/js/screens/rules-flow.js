// 流向图（界面 17）：同一份规则的另一种看法，按通道分组的看板，末尾一张「原网络」卡。只用来看，不用来配置。
// 测试地址（18）复用这里导出的 board()：命中的卡片和地址高亮，其余变淡。
// flowGroups 是纯函数，顶层不碰 document / window。
import { h, fill, card, st, bul, ic } from '../ui.js';
import { displayState, ruleFate } from '../derive.js';
import { stateText, stateKind, FATE } from '../terms.js';
import { rulesHead, watchRules, watchTakeover, takeoverNote, fixButton, stateView, filterRows, ruleOn, nameOf } from './rules.js';

// 卡片上通道的状态和它对规则的影响
export function flowState(ch, health, system) {
  const state = displayState(ch, health);
  if (system && system.routing_off) return { kind: 'off', text: '分流已关闭，走原网络' };
  // 没有参与分流的通道：规则全部走原网络（同 derive.ruleFate 的 direct-unrouted）
  if (ch.routing_enabled === false) return { kind: 'off', text: '不参与分流，走原网络' };
  if (state === 'stopped') return { kind: 'off', text: '已停止，走原网络' };
  const blocked = ch.blocked === true || (health && health.blocked === true);
  // 库里是运行态、只是运行环境里找不到它（down）时核心不阻止，流量仍进通道
  if (state === 'error') return { kind: 'bad', text: blocked ? '出错，暂时阻止' : '出错，流量仍进通道' };
  if (blocked) return { kind: stateKind(state), text: `${stateText(state)}，暂时阻止` };
  if (state === 'waiting') return { kind: 'wait', text: '待登录，流量仍进通道' };
  return { kind: stateKind(state), text: stateText(state) };
}

// 有规则的通道，每条一组：[{ch, state: {kind, text}, idle, rules: [{rule, on}]}]。q 按地址、备注、通道名筛选
export function flowGroups(channels, { health = {}, system = null, takeover = null, q = '' } = {}) {
  const out = [];
  for (const ch of channels || []) {
    const all = [...(ch.domains || []), ...(ch.ips || [])];
    if (!all.length) continue;
    const rules = filterRows(all.map(rule => ({ rule, ch })), q).map(({ rule }) => ({ rule, on: ruleOn(rule),
      fate: ruleFate(rule, ch, { takeover, routingOff: !!system?.routing_off,
        state: displayState(ch, health[ch.id]), blocked: health[ch.id]?.blocked }) }));
    if (q && !rules.length) continue;
    const hl = health[ch.id];
    const active = rules.filter(r => r.on);
    const inactive = active.length > 0 && active.every(r => r.fate === 'inactive');
    out.push({ ch, state: inactive ? { kind: 'wait', text: '规则还没生效' } : flowState(ch, hl, system), idle: displayState(ch, hl) === 'stopped', rules });
  }
  return out;
}

// 看板。hit：命中的通道 id，或 'direct'（原网络）；addr：命中的规则地址；cols：列数
export function board(groups, { hit = null, addr = null, cols = 3 } = {}) {
  const cellOf = g => card([
    h('div', { class: 'rl-fhead' }, bul(nameOf(g.ch), { size: 'md', idle: g.idle }),
      h('div', { class: 'two' }, h('span', { class: 'cell', title: nameOf(g.ch) }, nameOf(g.ch)), h('small', null, st(g.state.kind, g.state.text)))),
    h('div', { class: 'rows' }, g.rules.map(({ rule, on, fate }) =>
      h('div', { class: 'row slim' + (addr != null && hit === g.ch.id && addr === rule.pattern ? ' hitrow' : '') + (on ? '' : ' rl-off') },
        h('span', { class: 'cell mono', title: rule.pattern }, rule.pattern),
        rule.note || !on ? h('span', { class: 'sp' }) : null,
        rule.note ? h('span', { class: 'dim3 rl-rnote cell', title: rule.note }, rule.note) : null,
        !on ? h('span', { class: 'dim3 rl-rnote' }, '已停用') : ['inactive', 'unsupported'].includes(fate) ? h('span', { class: 'dim3 rl-rnote' }, FATE[fate].text) : null))),
  ], { cls: hit ? (hit === g.ch.id ? 'hit' : 'dimmed') : '' });
  return h('div', { class: 'rl-board' },
    h('div', { class: 'grid rl-grid', style: `grid-template-columns:repeat(${cols},minmax(0,1fr))` }, groups.map(cellOf)),
    h('div', { class: 'card flat rl-direct' + (hit ? (hit === 'direct' ? ' hit' : ' dimmed') : '') },
      h('span', { class: 'bul md idle', 'aria-hidden': 'true' }, ic('globe', 16)),
      h('div', { class: 'two' }, h('span', null, '原网络'), h('small', null, '其他所有地址，不经过任何通道'))));
}

export function mount(root, ctx) {
  const { signal, navigate, data } = ctx;
  let q = '';
  let sig = '';
  let takeover = null;
  const body = h('div', { class: 'rl-body rl-scroll' });
  fill(root, rulesHead({ view: 'flow', navigate, onSearch: v => { q = v; render(true); } }), body);
  const st0 = watchRules(data, signal, () => render());
  watchTakeover(signal, t => { takeover = t; render(); });

  function render(force) {
    if (signal.aborted) return;
    const state = stateView(st0, { onRetry: () => data.refresh('core'), navigate });
    const groups = state ? [] : flowGroups(st0.channels, { health: st0.health, system: st0.system, takeover, q });
    // 看板上只写域名规则没有生效这一种说明（按程序区分的那段在列表页）
    const note = takeoverNote(takeover);
    const off = note && note.off ? note : null;
    const next = JSON.stringify([state ? (st0.channels ? st0.channels.length : -1) + ':' + (st0.loading ? 1 : 0) : 0,
      groups.map(g => [g.ch.id, g.ch.name, g.state, g.idle, g.rules.map(r => [r.rule.id, r.rule.pattern, r.rule.note, r.on, r.fate])]), off && off.text, off && off.fix]);
    if (!force && next === sig) return;
    sig = next;
    if (state) { fill(body, state); return; }
    if (!groups.length) { fill(body, h('div', { class: 'rl-none' }, '没有找到这个地址的规则')); return; }
    fill(body, off ? h('div', { class: 'rl-warn', role: 'note' }, ic('info', 14, 1.6), h('span', null, off.text),
      fixButton(off.fix, { takeover, signal, navigate, onDone: t => { takeover = t; render(true); } })) : null, board(groups));
  }

  render();
  return { update() {}, unmount() {} };
}
