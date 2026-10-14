// 通道管理的入口：详情头的「更多」菜单（修改连接信息、删除通道），和「恢复上一次设置」提示条。
// - 修改连接信息：弹层沿用添加通道「填写连接信息」的样子（界面 13）。能改名字、这种类型要填的项、验证地址；
//   密码留空表示不改。只提交改过的字段，走 PATCH /api/channels/:cid。
// - 删除通道：先确认，写明这条通道的规则会一起删掉。走 DELETE /api/channels/:cid；
//   运行环境没起来、核心要求先启动时（409 且 runtime_required），按钮换成「启动运行环境并删除」。
// - 恢复上一次设置：通道改了设置、还在等确认（replacement.can_restore）时出现，走 POST /restore。
// 详情头每次刷新都会重画，菜单开着的状态记在模块里，重画以后仍然开着。
// channels.js 挂载时调 bind(ctx) 交来 navigate、data；纯函数在顶层不碰 document / window。
import { h, fill, btn, ic } from '../ui.js';
import { openSheet } from '../sheet.js';
import { api, isAbort } from '../api.js';
import { humanize, showError, showDone, notify } from '../feedback.js';
import { typeLabel } from '../terms.js';
import { channelRules } from '../derive.js';
import { formInputs, fieldLabel } from './add.js';
import { toTargetUrl } from './channel-unverified.js';

// ---- 纯函数（node 可直接导入测试） ----

// 核心能改的连接信息（store::update_channel）
const EDITABLE = new Set(['server', 'username', 'password']);

// 修改弹层里显示哪些项：这种类型要填的项里核心能改的那几个。拿不到类型清单时，按通道现有的服务器、账号给
export function editInputs(adapter, ch) {
  if (adapter) return formInputs(adapter).main.filter(i => i.type !== 'file' && EDITABLE.has(i.key));
  const out = [];
  if (ch && ch.server) out.push({ key: 'server', label: '服务器', type: 'text' });
  if (ch && ch.username) out.push({ key: 'username', label: '账号', type: 'text' });
  return out;
}

// PATCH 的请求体：只带改过的字段；密码留空表示不改。返回 {body} 或 {error}
export function editBody(ch, inputs, values = {}) {
  const body = {};
  const name = String(values.name ?? '').trim();
  if (!name) return { error: '给它起个名字' };
  if (name !== ch.name) body.name = name;
  for (const input of inputs || []) {
    const v = values[input.key] ?? '';
    if (input.type === 'password') { if (v !== '') body[input.key] = String(v); continue; }
    const t = String(v).trim();
    if (input.required && !t) return { error: `请填写${fieldLabel(input)}` };
    if (t !== String(ch[input.key] ?? '')) body[input.key] = t;
  }
  const old = String(ch.probe_url ?? '').trim(); // wording-allow
  const t = String(values.target ?? '').trim();
  if (t !== old) {
    const url = t ? toTargetUrl(t) : '';
    if (url == null) return { error: '验证地址写得不对，例如 oa.corp.example' };
    if (url !== old) body.probe_url = url; // wording-allow
  }
  return { body };
}

// 改了这几项，核心会用新的设置重新连接
export const reconnects = body => ['server', 'username', 'password'].some(k => k in (body || {}));

// 「恢复上一次设置」提示条的文字；没有可恢复的设置时为 null
export function restoreView(ch) {
  const r = ch && ch.replacement;
  if (!r || r.can_restore !== true) return null;
  if (r.phase === 'queued') return { text: '改过的连接设置还没用上，启动通道时才会用。不想要这次修改，可以恢复原来的设置。', done: '已恢复原来的设置' };
  if (r.phase === 'awaiting_login') return { text: '改过的连接设置还在等确认，登录成功以后自动确认。不想要这次修改，可以恢复原来的设置。', done: '已恢复上一次设置，连没连上请再检测一次' };
  return { text: '改过的连接设置没有用上。可以恢复原来的设置。', done: '已恢复上一次设置，连没连上请再检测一次' };
}

// 删除确认框里的一句话
export function deleteText(ch) {
  const n = channelRules(ch).length;
  return n
    ? `这条通道的 ${n} 条规则、连接信息和备忘录会一起删掉，删掉以后找不回来。`
    : '这条通道的连接信息和备忘录会一起删掉，删掉以后找不回来。';
}

// ---- 和页面的连接 ----

let env = { navigate: () => {}, data: null };
export function bind(ctx) {
  env = { navigate: ctx.navigate, data: ctx.data };
}

const refresh = () => { if (env.data) env.data.refresh('core'); };
const message = (e, fallback) => {
  const m = humanize(e, { fallback });
  return m ? (m.text ? `${m.title}。${m.text}` : m.title) : '';
};

// ---- 「更多」菜单 ----

let openId = null;   // 菜单开着的通道 id
let listening = false;

function closeMenu({ focus } = {}) {
  if (openId == null) return;
  openId = null;
  for (const m of document.querySelectorAll('.ch-more')) {
    m.querySelector('.ch-menu').hidden = true;
    m.firstChild.setAttribute('aria-expanded', 'false');
    if (focus) m.firstChild.focus();
  }
}

function listen() {
  if (listening) return;
  listening = true;
  document.addEventListener('pointerdown', e => { if (openId != null && !e.target.closest('.ch-more')) closeMenu(); }, true);
  document.addEventListener('keydown', e => {
    if (openId == null) return;
    if (e.key === 'Escape' && !e.isComposing) { e.preventDefault(); closeMenu({ focus: true }); return; }
    if (e.key !== 'ArrowDown' && e.key !== 'ArrowUp') return;
    const items = Array.from(document.querySelectorAll('.ch-menu:not([hidden]) button'));
    if (!items.length) return;
    e.preventDefault();
    const at = items.indexOf(document.activeElement);
    items[(at + (e.key === 'ArrowDown' ? 1 : -1) + items.length) % items.length].focus();
  });
}

export function moreMenu(ch) {
  const open = openId === ch.id;
  // 重画前焦点在菜单里的第几项，重画后放回去
  const old = document.querySelector('.ch-menu:not([hidden])');
  const focusAt = old ? Array.from(old.querySelectorAll('button')).indexOf(document.activeElement) : -1;
  const pick = fn => () => { closeMenu({ focus: true }); fn(ch); };
  const items = [
    btn('修改连接信息', { cls: 'ghost', onClick: pick(openEdit) }),
    btn('删除通道', { cls: 'ghost danger', onClick: pick(confirmDelete) }),
  ];
  for (const b of items) b.setAttribute('role', 'menuitem');
  const menu = h('div', { class: 'ch-menu', role: 'menu', 'aria-label': '更多', hidden: !open }, items);
  const toggle = h('button', {
    type: 'button', class: 'btn', 'aria-haspopup': 'menu', 'aria-expanded': String(open),
    onClick: () => {
      if (openId === ch.id) { closeMenu(); return; }
      closeMenu();
      listen();
      openId = ch.id;
      menu.hidden = false;
      toggle.setAttribute('aria-expanded', 'true');
      items[0].focus();
    },
  }, '更多', ic('down', 12, 1.8));
  if (open && focusAt >= 0) queueMicrotask(() => { if (items[focusAt].isConnected) items[focusAt].focus(); });
  return h('span', { class: 'ch-more' }, toggle, menu);
}

// ---- 恢复上一次设置 ----

const restoring = new Set();

export function restoreBar(ch) {
  const v = restoreView(ch);
  if (!v) return null;
  const busy = restoring.has(ch.id);
  const b = btn(busy ? '正在恢复' : '恢复上一次设置', {
    cls: 'sm', disabled: busy, onClick: async () => {
      if (restoring.has(ch.id)) return;
      restoring.add(ch.id);
      b.disabled = true;
      b.lastChild.textContent = '正在恢复';
      try {
        showDone(await api.restore(ch.id), v.done);
      } catch (e) {
        if (!isAbort(e)) showError(e, { fallback: '没能恢复上一次设置' });
      }
      restoring.delete(ch.id);
      refresh();
    },
  });
  return h('div', { class: 'rl-warn ch-restore', role: 'note' }, ic('info', 14, 1.6), h('span', null, v.text), b);
}

// ---- 修改连接信息 ----

let types = null; // /api/vpn-types，读到一次就留着

async function openEdit(ch) {
  if (!types) {
    try { const list = await api.vpnTypes(); if (Array.isArray(list)) types = list; } catch (e) {
      if (isAbort(e)) return;
    }
  }
  const adapter = (types || []).find(a => a.key === ch.vpn_type) || null;
  const inputs = editInputs(adapter, ch);
  const values = { name: ch.name, target: ch.probe_url || '' }; // wording-allow
  for (const i of inputs) values[i.key] = i.type === 'password' ? '' : String(ch[i.key] ?? '');
  let busy = false;

  const input = (key, { type = 'text', placeholder = '', label }) => {
    const el = h('input', { class: 'grow', type, placeholder, 'aria-label': label, autocomplete: 'off', spellcheck: 'false' });
    el.value = values[key] ?? '';
    el.addEventListener('input', () => { values[key] = el.value; err.hidden = true; });
    return el;
  };
  const row = (label, el) => h('div', { class: 'fr' }, h('label', null, label), el);
  const nameEl = input('name', { label: '名字', placeholder: '例如 园区办公' });
  nameEl.setAttribute('autofocus', '');
  const rows = [row('名字', nameEl)];
  for (const i of inputs) {
    const pwd = i.type === 'password';
    rows.push(row(fieldLabel(i), input(i.key, {
      type: pwd ? 'password' : 'text', label: fieldLabel(i),
      placeholder: pwd ? '不改就留空' : i.key === 'server' ? '例如 vpn.example.com' : '',
    })));
  }
  rows.push(row('验证地址', input('target', { label: '验证地址', placeholder: '连上以后才能打开的内网地址，可以不填' })));
  const err = h('div', { class: 'add-err', role: 'alert', hidden: true });

  const cancel = btn('取消', { onClick: () => sheet.close() });
  const save = btn('保存', { cls: 'pri', onClick: submit });
  const sheet = openSheet({
    title: '修改连接信息', sub: `${ch.name} · ${typeLabel(ch.vpn_type, adapter && adapter.label)}`,
    body: [h('div', { class: 'form' }, rows), err],
    foot: [h('span', { class: 'sp' }), cancel, save],
    onConfirm: () => submit(),
  });
  sheet.el.classList.add('add');

  function fail(text) {
    err.textContent = text;
    err.hidden = false;
  }

  async function submit() {
    if (busy) return;
    const r = editBody(ch, inputs, values);
    if (r.error) { fail(r.error); return; }
    if (!Object.keys(r.body).length) { sheet.close(); return; }
    busy = true;
    save.disabled = cancel.disabled = true;
    save.lastChild.textContent = '正在保存…';
    err.hidden = true;
    try {
      const out = await api.update(ch.id, r.body);
      sheet.close();
      showDone(out, reconnects(r.body) ? '已保存，正在用新的设置重新连接' : '已保存');
      refresh();
    } catch (e) {
      busy = false;
      save.disabled = cancel.disabled = false;
      save.lastChild.textContent = '保存';
      if (!isAbort(e)) fail(message(e, '没能保存'));
    }
  }
}

// ---- 删除通道 ----

function confirmDelete(ch) {
  let busy = false;
  let runtime = false; // 核心要求先启动运行环境
  const text = h('p', { class: 'ch-del-text' }, deleteText(ch));
  const err = h('div', { class: 'add-err', role: 'alert', hidden: true });
  const cancel = btn('取消', { onClick: () => sheet.close() });
  const go = btn('删除', { cls: 'danger', icon: 'trash', onClick: remove });
  const sheet = openSheet({
    title: `删除「${ch.name}」？`, width: 'w520',
    body: [text, err],
    foot: [h('span', { class: 'sp' }), cancel, go],
  });
  sheet.el.classList.add('add');

  async function remove() {
    if (busy) return;
    busy = true;
    go.disabled = cancel.disabled = true;
    go.lastChild.textContent = runtime ? '正在启动并删除…' : '正在删除…';
    err.hidden = true;
    try {
      await api.remove(ch.id, runtime);
      sheet.close();
      notify(`已删除「${ch.name}」`, { kind: 'ok' });
      env.navigate('#/channels');
      refresh();
    } catch (e) {
      busy = false;
      go.disabled = cancel.disabled = false;
      if (isAbort(e)) return;
      if (e && e.body && e.body.runtime_required === true) runtime = true;
      go.lastChild.textContent = runtime ? '启动运行环境并删除' : '删除';
      err.textContent = message(e, '没能删除');
      err.hidden = false;
    }
  }
}

// 离开通道页：关掉菜单（弹层由 sheet.js 管，换页不关，操作照常做完）
export function unbind() {
  if (globalThis.document) closeMenu();
}
