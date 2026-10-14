// 公共组件。都返回 DOM 元素，文字一律经 textContent 写入（通道名、规则、备忘录都来自用户输入）。
// 顶层不碰 document / window：esc、mark、isEnter、routeChipView 是纯函数，node 可直接导入。
import { ic } from './icons.js';

export { ic };

const ESC = { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' };
export const esc = s => String(s ?? '').replace(/[&<>"']/g, c => ESC[c]);

// 通道标记：名称的第一个字（按字符而不是 UTF-16 单元取）
export function mark(name) {
  const c = Array.from(String(name ?? '').trim())[0];
  return c ? c.toUpperCase() : '·';
}

// 回车确认：输入法组字时的回车（isComposing，或 WebKit 的 keyCode 229）只是选字，不算确认
export function isEnter(e) {
  return !!e && e.key === 'Enter' && !e.isComposing && e.keyCode !== 229 &&
    !e.shiftKey && !e.altKey && !e.ctrlKey && !e.metaKey;
}

const PROPS = new Set(['value', 'checked', 'disabled', 'hidden', 'selected', 'type']);

// h('div', {class, text, title, onClick, ...}, ...子节点)。字符串子节点按文字处理。
export function h(tag, props, ...kids) {
  const el = document.createElement(tag);
  if (props) {
    for (const [k, v] of Object.entries(props)) {
      if (v == null || v === false) continue;
      if (k === 'class') el.className = v;
      else if (k === 'text') el.textContent = v;
      else if (k === 'style') el.style.cssText = v;
      else if (k.startsWith('on') && typeof v === 'function') el.addEventListener(k.slice(2).toLowerCase(), v);
      else if (PROPS.has(k)) el[k] = v;
      else el.setAttribute(k, v === true ? '' : String(v));
    }
  }
  put(el, kids);
  return el;
}

function put(el, kids) {
  for (const c of kids.flat(Infinity)) {
    if (c == null || c === false) continue;
    el.append(typeof c === 'object' ? c : String(c));
  }
  return el;
}

// 清空后放入新内容
export const fill = (el, ...kids) => { el.replaceChildren(); return put(el, kids); };

const cls = (...xs) => xs.filter(Boolean).join(' ');

export const card = (kids, { cls: c, style } = {}) => h('div', { class: cls('card', c), style }, kids);

export const lb = (text, right) => h('div', { class: 'lb' }, text, right ? [h('span', { class: 'sp' }), right] : null);

export function btn(text, { cls: c, icon, onClick, disabled, title, type = 'button' } = {}) {
  return h('button', { type, class: cls('btn', c), title, disabled: !!disabled, onClick }, icon ? ic(icon, 14, 1.6) : null, text);
}

// 状态点：kind 为 ok / wait / bad / off / plain
export const st = (kind, text) => h('span', { class: cls('st', kind) }, h('i'), text);

export function tog({ checked, onChange, label, disabled, xl } = {}) {
  return h('input', {
    type: 'checkbox', role: 'switch', class: cls('tog', xl && 'xl'), checked: !!checked, disabled: !!disabled,
    'aria-label': label, onChange: onChange && (e => onChange(e.target.checked, e)),
  });
}

// items: [{key, label, icon}]。当前项加 .on 和 aria-pressed。
export function seg(items, { value, onChange, label } = {}) {
  const root = h('span', { class: 'seg', role: 'group', 'aria-label': label });
  for (const it of items) {
    const on = it.key === value;
    root.append(h('button', {
      type: 'button', class: on ? 'on' : null, 'aria-pressed': on ? 'true' : 'false', 'data-key': it.key,
      onClick: () => { setSeg(root, it.key); if (onChange) onChange(it.key); },
    }, it.icon ? ic(it.icon, 14) : null, it.label));
  }
  return root;
}

// 不重建，只切换分段控件的当前项
export function setSeg(root, key) {
  for (const b of root.children) {
    const on = b.dataset.key === String(key);
    b.classList.toggle('on', on);
    b.setAttribute('aria-pressed', String(on));
  }
}

export function search({ placeholder, value, onInput, onEnter, label } = {}) {
  const input = h('input', { type: 'search', placeholder, value: value ?? '', 'aria-label': label || placeholder, autocomplete: 'off', spellcheck: 'false' });
  if (onInput) input.addEventListener('input', () => onInput(input.value));
  if (onEnter) input.addEventListener('keydown', e => { if (isEnter(e)) { e.preventDefault(); onEnter(input.value); } });
  return h('label', { class: 'search' }, ic('search', 14), input);
}

// 单行 <input class="field">，multiline 时 <textarea class="field">。onEnter 只在单行时生效，组字中的回车不触发。
export function field({ placeholder, value, type = 'text', multiline, rows, cls: c, style, label, onInput, onEnter, autofocus } = {}) {
  const el = multiline
    ? h('textarea', { class: cls('field', c), style, placeholder, rows, 'aria-label': label || placeholder, spellcheck: 'false' })
    : h('input', { class: cls('field', c), style, type, placeholder, 'aria-label': label || placeholder, autocomplete: 'off', spellcheck: 'false' });
  el.value = value ?? '';
  if (autofocus) el.setAttribute('autofocus', '');
  if (onInput) el.addEventListener('input', () => onInput(el.value));
  if (onEnter && !multiline) el.addEventListener('keydown', e => { if (isEnter(e)) { e.preventDefault(); onEnter(el.value); } });
  return el;
}

// 真 <select>。options: [{value, label}]
export function pop(options, { value, onChange, label, disabled } = {}) {
  const sel = h('select', { 'aria-label': label, disabled: !!disabled },
    options.map(o => h('option', { value: String(o.value), selected: String(o.value) === String(value) }, o.label)));
  if (onChange) sel.addEventListener('change', () => onChange(sel.value));
  return h('span', { class: 'pop' }, sel, ic('updown', 12, 1.8));
}

// 通道标记。size: sm / md / lg；idle 用于已停止的通道
export const bul = (name, { size, idle, solid } = {}) =>
  h('span', { class: cls('bul', size, idle && 'idle', solid && 'solid'), 'aria-hidden': 'true' }, mark(name));

// 通道名迷你标签：标记加名称，名称太长时截断，悬停看全
export const chip = (name, { idle, size = 'sm' } = {}) =>
  h('span', { class: 'ch' }, bul(name, { size, idle }), h('span', { class: 'cell', title: name }, name));

// 截断的单元格，悬停看全
export const cell = (text, { cls: c } = {}) => h('div', { class: cls('cell', c), title: text }, text);

export function kv(k, v) {
  const right = typeof v === 'object' && v ? v : h('span', { title: v ?? '' }, v ?? '');
  return h('div', { class: 'kv' }, h('span', null, k), right);
}

export const empty = ({ title, text, action } = {}) =>
  h('div', { class: 'empty' }, title ? h('b', null, title) : null, text ? h('p', null, text) : null, action);

const VERDICT_ICON = { ok: 'okc', bad: 'badc', wait: 'info', info: 'info' };
// 结论：kind 为 ok / bad / wait / info；panel 为独立的大卡片样式（还没验证、出错页）
export function verdict(kind, { title, text, action, panel } = {}) {
  return h('div', { class: cls('verdict', kind !== 'info' && kind, panel && 'panel') },
    ic(VERDICT_ICON[kind] || 'info', panel ? 22 : 20),
    h('div', null, title ? h('b', null, title) : null, text ? h('span', null, text) : null),
    action);
}

// 分步进度。steps: [{state: done|now|todo, title, sub, time}]
export function steps4(steps) {
  return h('div', { class: 'form steps4' }, steps.map(s => h('div', { class: cls('s4', s.state) },
    h('div', { class: 'mk' }, s.state === 'done' ? ic('check', 13, 2.2) : null),
    h('div', null, h('b', null, s.title), s.sub ? h('span', null, s.sub) : null),
    h('span', { class: 't' }, s.time || ''))));
}

const TONES = new Set(['brand', 'ok', 'wait', 'bad', 'ink-3']);
// 时间线的一跳。tone 取 brand / ok / wait / bad；skip 为还没走到的一跳
export function hop({ title, sub, tone, skip } = {}) {
  const el = h('div', { class: cls('hop', skip && 'skip') }, h('div', { class: 'pin' }, h('i')), h('div', null, h('b', null, title), sub ? h('span', null, sub) : null));
  if (tone && TONES.has(tone)) el.style.setProperty('--c', 'var(--' + tone + ')');
  return el;
}
export const path = hops => h('div', { class: 'path' }, hops.map(hop));

// 顶栏的分流标记，五种状态
const CHIP = {
  on: { cls: '', text: '分流已开启' },
  off: { cls: 'off', text: '分流已关闭' },
  bad: { cls: 'bad', text: '分流有问题' },
  runtime: { cls: 'off', text: '运行环境没启动' },
  dormant: { cls: 'off', text: '连接通道时会自动启动' },
  pending: { cls: 'warn', text: '规则还没同步' },
  starting: { cls: 'warn', text: '正在启动' },
};
export const routeChipView = state => CHIP[state] || null;

export function setRouteChip(state, root = document) {
  const el = root.getElementById ? root.getElementById('route-chip') : root.querySelector('#route-chip');
  if (!el) return;
  const v = routeChipView(state);
  el.hidden = !v;
  if (!v) return;
  el.className = cls('chipb', v.cls);
  fill(el, h('i'), v.text);
}

// 页面的四种状态：empty 没有数据 / loading 首次加载的骨架 / error 请求失败加重试 / runtime 运行环境没启动加启动
export function pageState(kind, { title, text, action, onRetry, onStart, starting } = {}) {
  if (kind === 'loading') {
    return h('div', { class: 'skel', 'aria-busy': 'true', 'aria-label': '正在加载' },
      [0, 1, 2].map(() => h('div', { class: 'card' }, h('i'), h('i'))));
  }
  if (kind === 'error') {
    return verdict('bad', {
      panel: true, title: title || '没能读取数据', text: text || '稍后再试一次；一直不行的话，到「设置 · 高级」里查看日志。',
      action: onRetry ? btn('重试', { icon: 'refresh', onClick: onRetry }) : null,
    });
  }
  if (kind === 'runtime') {
    return h('div', { class: 'todo', role: 'status' }, ic('warn', 18, 1.6),
      h('div', { class: 'two' }, h('span', null, title || '运行环境还没准备好'), h('small', null, text || '启动以后通道才能连接。')),
      onStart ? btn(starting ? '正在启动' : '启动', { cls: 'pri sm', disabled: starting, onClick: onStart }) : null);
  }
  return empty({ title: title || '这里还没有内容', text, action });
}

// 外壳：把 index.html 里的 [data-ic] 占位换成图标
export function initShell(root = document) {
  for (const el of root.querySelectorAll('[data-ic]')) {
    el.replaceWith(ic(el.dataset.ic, Number(el.dataset.size) || 16, Number(el.dataset.sw) || 1.35));
  }
}

// 侧栏当前项。key 为 home / channels / rules / conns / settings
export function setNav(key, root = document) {
  for (const a of root.querySelectorAll('[data-nav]')) {
    const on = a.dataset.nav === key;
    a.classList.toggle('on', on);
    if (on) a.setAttribute('aria-current', 'page'); else a.removeAttribute('aria-current');
  }
}
