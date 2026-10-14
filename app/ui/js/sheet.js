// 弹层：焦点在弹层内循环，Esc 关闭，回车确认。输入法组字时的回车只是选字，不提交（isEnter）。
// 同一时间只有一个弹层；打开新的会先关掉旧的。
import { h, fill, isEnter } from './ui.js';

const FOCUSABLE = 'a[href],button:not([disabled]),input:not([disabled]),select:not([disabled]),textarea:not([disabled]),[tabindex]:not([tabindex="-1"])';
let current = null;

// 回车不当作确认的目标：多行输入要换行，按钮和链接本身会响应回车，下拉要展开
export function confirmTarget(t) {
  const tag = t && t.tagName ? t.tagName.toLowerCase() : '';
  return !(tag === 'textarea' || tag === 'button' || tag === 'a' || tag === 'select' || (t && t.isContentEditable));
}

// width: '' | 'w520' | 'w680'。body、foot 为节点或节点数组。
// onConfirm：回车确认时调用；onClose(reason)：关闭时调用一次，reason 为 esc / close / replace。
export function openSheet({ title, sub, body, foot, width, onConfirm, onClose, root } = {}) {
  if (current) current.close('replace');
  const opener = document.activeElement;
  const titleId = 'sheet-t-' + Math.random().toString(36).slice(2, 8);
  const tEl = h('b', { id: titleId }, title || '');
  const sEl = h('span', null, sub || '');
  const bEl = h('div', { class: 'sh-b' }, body);
  const fEl = h('div', { class: 'sh-f' }, foot);
  const box = h('div', { class: 'sheet' + (width ? ' ' + width : ''), role: 'dialog', 'aria-modal': 'true', 'aria-labelledby': titleId },
    h('div', { class: 'sh-h' }, tEl, sEl), bEl, fEl);
  const veil = h('div', { class: 'veil' }, box);

  let closed = false;
  const handle = {
    el: box, body: bEl, foot: fEl,
    setTitle(t, s) { tEl.textContent = t || ''; sEl.textContent = s || ''; },
    setBody(...kids) { fill(bEl, ...kids); focusFirst(box); },
    setFoot(...kids) { fill(fEl, ...kids); },
    close(reason = 'close') {
      if (closed) return;
      closed = true;
      document.removeEventListener('keydown', onKey);
      veil.remove();
      if (current === handle) current = null;
      if (opener && opener.isConnected && typeof opener.focus === 'function') opener.focus();
      if (onClose) onClose(reason);
    },
  };

  function onKey(e) {
    if (e.key === 'Escape') {
      if (e.isComposing || e.keyCode === 229) return;
      e.preventDefault();
      handle.close('esc');
      return;
    }
    if (e.key === 'Tab') { trap(e, box); return; }
    if (onConfirm && isEnter(e) && !e.defaultPrevented && confirmTarget(e.target)) {
      e.preventDefault();
      onConfirm(handle);
    }
  }
  // 挂在 document 的冒泡阶段：焦点落到背景上也能收到按键；输入框自己的回车处理先执行，可以 preventDefault 拦下
  document.addEventListener('keydown', onKey);

  const host = root || document.getElementById('sheet-root') || document.body;
  host.append(veil);
  current = handle;
  focusFirst(box);
  return handle;
}

export const closeSheet = reason => { if (current) current.close(reason); };
export const currentSheet = () => current;

function focusable(box) {
  return Array.from(box.querySelectorAll(FOCUSABLE)).filter(el => !el.hidden && el.getClientRects().length > 0);
}

function focusFirst(box) {
  const auto = box.querySelector('[autofocus]');
  const list = focusable(box);
  const first = auto || list.find(el => /^(input|textarea|select)$/i.test(el.tagName)) || list[0];
  if (first) first.focus();
  else { box.tabIndex = -1; box.focus(); }
}

function trap(e, box) {
  const list = focusable(box);
  if (!list.length) { e.preventDefault(); return; }
  const first = list[0], last = list[list.length - 1];
  const at = document.activeElement;
  if (e.shiftKey && (at === first || !box.contains(at))) { e.preventDefault(); last.focus(); }
  else if (!e.shiftKey && (at === last || !box.contains(at))) { e.preventDefault(); first.focus(); }
}
