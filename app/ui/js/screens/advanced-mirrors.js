// 设置 · 高级 · 下载源（界面 22 的二级页）：下载客户端时按顺序尝试的地址。
// 功能和原来一样：启用停用、测速、上移、删除、添加。推导是纯函数，顶层不碰 document / window。
import { h, fill, card, btn, tog, field, pageState } from '../ui.js';
import { api, isAbort } from '../api.js';
import { poll } from '../poll.js';
import { ms as msText } from '../format.js';
import { showError, notify } from '../feedback.js';
import { pageHead, confirmSheet } from './settings-advanced.js';

const EVERY = 20000;
const on = v => v === true || v === 1;

// 按优先级排好，数字小的在前；同优先级按编号
export function mirrorRows(list) {
  return (Array.isArray(list) ? list : [])
    .filter(m => m && m.id != null)
    .slice()
    .sort((a, b) => (Number(a.priority) - Number(b.priority)) || (Number(a.id) - Number(b.id)));
}

// 地址只能有字母、数字和 . _ : / -（后台同一条规则）
export const validHost = s => /^[A-Za-z0-9._:/-]+$/.test(String(s ?? '').trim());

// 测速结果 → 一句话
export function speedText(r) {
  if (!r) return '';
  if (r.testing) return '正在测速';
  if (r.failed) return '没能测速';
  return r.reachable ? `能连上，${msText(r.latency_ms)}` : '连不上';
}

export function mirrorSub(m, index, speed) {
  const parts = [on(m.enabled) ? `第 ${index + 1} 个尝试` : '已停用'];
  const s = speedText(speed);
  if (s) parts.push(s);
  return parts.join(' · ');
}

export function mount(root, ctx) {
  const { signal } = ctx;
  let list = null;      // null 读取中
  let failed = false;
  let busy = '';        // 正在处理的下载源编号，或 add
  const speed = new Map();
  let sig = '';

  const listEl = h('div', { class: 'adv-list' });
  const staleEl = h('p', { class: 'dim', hidden: true }, '暂时读不到，恢复后自动更新');
  const input = field({ placeholder: '例如 mirror.example.com', label: '下载源地址', cls: 'adv-host', onEnter: add });
  const addBtn = btn('添加', { cls: 'pri sm', onClick: () => add(input.value) });

  fill(root, h('div', { class: 'adv' },
    pageHead('高级', '#/settings/advanced', '下载源'),
    h('p', { class: 'h1sub adv-lead' }, '下载客户端时按顺序尝试这些地址，前面的优先；都不行时直接从官方地址下载。'),
    staleEl, listEl,
    card(h('div', { class: 'rows' }, h('div', { class: 'row' }, input, addBtn)), { cls: 'adv-add' })));

  function render() {
    staleEl.hidden = !failed;
    const next = JSON.stringify([list, failed, busy, [...speed]]);
    if (next === sig) return;
    sig = next;
    if (list === null) { fill(listEl, failed ? pageState('error', { onRetry: () => poller.kick() }) : pageState('loading')); return; }
    const rows = mirrorRows(list);
    if (!rows.length) { fill(listEl, card(h('div', { class: 'adv-none dim' }, '还没有下载源，客户端会直接从官方地址下载。'))); return; }
    fill(listEl, card(h('div', { class: 'rows' }, rows.map((m, i) => {
      const id = String(m.id);
      const dis = !!busy;
      return h('div', { class: 'row' },
        h('div', { class: 'two sp' }, h('span', { class: 'adv-t num', title: m.host }, m.host), h('small', null, mirrorSub(m, i, speed.get(id)))),
        h('div', { class: 'adv-acts' },
          btn('测速', { cls: 'sm', disabled: dis || (speed.get(id) || {}).testing, onClick: () => test(m) }),
          btn('上移', { cls: 'sm', disabled: dis || i === 0, onClick: () => up(rows, i) }),
          btn('删除', { cls: 'sm danger', disabled: dis, onClick: () => del(m) })),
        tog({ checked: on(m.enabled), label: `启用 ${m.host}`, disabled: dis, onChange: v => setEnabled(m, v) }));
    })), { cls: 'list' }));
  }

  async function load(s) {
    try {
      list = await api.mirrors({ signal: s });
      if (!Array.isArray(list)) list = [];
      failed = false;
    } catch (e) {
      if (isAbort(e)) return;
      if (!failed) showError(e);
      failed = true;
    }
    render();
  }

  async function act(key, fn) {
    if (busy) return;
    busy = key;
    render();
    try { await fn(); } catch (e) { if (!isAbort(e)) showError(e); }
    if (signal.aborted) return;
    busy = '';
    sig = '';
    await poller.kick();
  }

  const setEnabled = (m, v) => act(String(m.id), async () => {
    await api.patchMirror(m.id, { enabled: v }, { signal });
    notify(v ? '已启用' : '已停用', { kind: 'ok' });
  });

  // 和前一个交换优先级
  const up = (rows, i) => act(String(rows[i].id), async () => {
    const a = rows[i], b = rows[i - 1];
    const pa = Number(a.priority), pb = Number(b.priority);
    await Promise.all([
      api.patchMirror(a.id, { priority: pa === pb ? pb - 1 : pb }, { signal }),
      api.patchMirror(b.id, { priority: pa }, { signal }),
    ]);
  });

  async function del(m) {
    if (busy) return;
    const ok = await confirmSheet({ title: '删除下载源', ok: '删除', danger: true, text: `删除「${m.host}」以后，下载客户端时不再尝试这个地址。` });
    if (ok) act(String(m.id), async () => { await api.delMirror(m.id, { signal }); notify('已删除', { kind: 'ok' }); });
  }

  async function test(m) {
    const id = String(m.id);
    speed.set(id, { testing: true });
    render();
    try {
      const r = await api.testMirror(m.host, { signal });
      speed.set(id, { reachable: !!(r && r.reachable), latency_ms: r && r.latency_ms });
    } catch (e) {
      if (isAbort(e)) return;
      speed.set(id, { failed: true });
    }
    render();
  }

  function add(value) {
    const host = String(value ?? '').trim();
    if (!host) { input.focus(); return; }
    if (!validHost(host)) { notify('地址里只能有字母、数字和 . _ : / -', { kind: 'bad' }); return; }
    act('add', async () => {
      await api.addMirror(host, { signal });
      input.value = '';
      notify('已添加', { kind: 'ok' });
    });
  }

  render();
  const poller = poll(load, EVERY, { signal });
  return { update() {}, unmount() {} };
}
