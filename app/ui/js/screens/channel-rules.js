// 通道 · 规则（界面 04）：这条通道负责的地址。和「规则」页是同一份数据（通道的 domains + ips）。
// 「现在」一列按 DESIGN 4.3：停用 → 已停用；分流关着或通道停了 → 走原网络；被阻止 → 暂时阻止；已连接 → 走通道；其余 → 等待登录。
// 「添加规则」打开添加规则弹层（界面 19）。
// 通道没有参与分流（routing_enabled 为假）时，表格上方说明原因，给「恢复分流」（PATCH {routing_enabled: true}）。
import { h, fill, btn, cell, tog, ic, empty } from '../ui.js';
import { api, isAbort } from '../api.js';
import { displayState, channelRules, ruleFate } from '../derive.js';
import { FATE } from '../terms.js';
import { showError, showDone } from '../feedback.js';
import { watchTakeover, takeoverNote, fixButton } from './rules.js';

const FATE_ICON = { channel: 'check', blocked: 'block' };

// 一行规则的「现在」。库里的 enabled 是 0/1
export function fateOf(rule, ch, health, system, takeover = null) {
  return ruleFate({ ...rule, enabled: !!rule.enabled }, ch, {
    routingOff: !!(system && system.routing_off),
    state: displayState(ch, health),
    blocked: health && health.blocked,
    takeover,
  });
}

export function mount(root, ctx) {
  const { signal, navigate, data } = ctx;
  const id = ctx.params.id;
  let ch = ctx.channel;
  let health = ctx.health;
  let system = ctx.system;
  let takeover = null;
  const pending = new Set();   // 正在改的规则 id
  let armed = null;            // 点过一次删除、等确认的规则 id
  let armTimer = null;

  const table = h('div', { class: 'ch-rules-rows' });
  const noteSlot = h('div', { class: 'ch-unrouted-slot' });
  let reviving = false;
  const addBtn = btn('添加规则', { cls: 'soft', icon: 'plus', onClick: () => navigate(`?sheet=rule-add&channel=${encodeURIComponent(id)}`) });
  fill(root, h('div', { class: 'card tcard ch-rules' },
    noteSlot,
    h('div', { class: 'tr th' }, h('div', null, '地址'), h('div', null, '现在'), h('div', { class: 'cell r' }, '启用'), h('div')),
    table,
    h('div', { class: 'ch-rules-foot' }, addBtn)));

  async function toggle(rule, on, input) {
    if (pending.has(rule.id)) return;
    pending.add(rule.id);
    input.disabled = true;
    try {
      const r = await api.toggleRule(id, rule.id, on, { signal });
      showDone(r, on ? '已启用' : '已停用');
    } catch (e) {
      if (isAbort(e)) return;
      input.checked = !on;
      showError(e);
    }
    pending.delete(rule.id);
    await data.refresh('core');
    if (!signal.aborted) render();
  }

  async function remove(rule) {
    if (armed !== rule.id) {
      armed = rule.id;
      clearTimeout(armTimer);
      armTimer = setTimeout(() => { armed = null; render(); }, 4000);
      render();
      return;
    }
    clearTimeout(armTimer);
    armed = null;
    pending.add(rule.id);
    render();
    try {
      const r = await api.delRule(id, rule.id, { signal });
      showDone(r, '已删除');
    } catch (e) {
      if (isAbort(e)) return;
      showError(e);
    }
    pending.delete(rule.id);
    await data.refresh('core');
    if (!signal.aborted) render();
  }

  function row(rule) {
    const fate = fateOf(rule, ch, health, system, takeover);
    const f = FATE[fate] || FATE.waiting;
    const busy = pending.has(rule.id);
    const input = tog({ checked: !!rule.enabled, disabled: busy, label: '启用 ' + rule.pattern, onChange: on => toggle(rule, on, input) });
    const del = armed === rule.id
      ? btn('删除', { cls: 'sm danger', disabled: busy, title: '再点一次删除这条规则', onClick: () => remove(rule) })
      : h('button', { type: 'button', class: 'ch-del', title: '删除', 'aria-label': '删除 ' + rule.pattern, disabled: busy, onClick: () => remove(rule) }, ic('trash', 14, 1.5));
    return h('div', { class: 'tr' },
      cell(rule.pattern),
      h('div', { class: 'cell' }, h('span', { class: 'fate ' + f.cls, title: f.text }, FATE_ICON[fate] ? ic(FATE_ICON[fate], 12, 2) : null, f.text)),
      h('div', { class: 'cell r' }, input),
      h('div', { class: 'cell r' }, del));
  }

  // 恢复分流：只改这条通道的 routing_enabled，不动规则本身
  async function revive() {
    if (reviving) return;
    reviving = true;
    renderNote();
    try {
      const r = await api.update(id, { routing_enabled: true }, { signal });
      showDone(r, '已恢复分流');
    } catch (e) {
      if (isAbort(e)) return;
      showError(e, { fallback: '没能恢复分流' });
    }
    reviving = false;
    await data.refresh('core');
    if (!signal.aborted) render();
  }

  function renderNote() {
    const note = takeoverNote(takeover);
    fill(noteSlot, ch && ch.routing_enabled === false
      ? h('div', { class: 'rl-warn', role: 'note' }, ic('info', 14, 1.6),
        h('span', null, '这条通道没有参与分流，它的规则现在都走原网络。'),
        btn(reviving ? '正在恢复' : '恢复分流', { cls: 'sm', disabled: reviving, onClick: revive }))
      : note?.off ? h('div', { class: 'rl-warn', role: 'note' }, ic('info', 14, 1.6),
        h('span', null, note.text), fixButton(note.fix, { takeover, signal, navigate,
          onDone: t => { takeover = t; render(); } })) : null);
  }

  function render() {
    renderNote();
    const rules = channelRules(ch);
    if (!rules.length) {
      fill(table, empty({ title: '还没有规则', text: '加一条这个通道负责的地址，比如 oa.example 或 10.0.0.0/8。' }));
      return;
    }
    fill(table, rules.map(row));
  }

  watchTakeover(signal, t => { takeover = t; render(); });
  render();

  return {
    update() {},
    channel(next, nextHealth, nextSystem) {
      ch = next;
      health = nextHealth;
      if (nextSystem) system = nextSystem;
      render();
    },
    unmount() { clearTimeout(armTimer); },
  };
}
