// 提示与错误转人话。humanize / clean 是纯函数；notify 只在调用时才碰 document。
// 后端原文先过禁词表：命中内部用词，或者根本不是中文，就换成按状态码写的通用句子；
// 原文只留在内存里（recentErrors），给「设置 · 高级 · 查看日志」展示。
import { FORBIDDEN, PHRASE } from './terms.js';
import { isAbort } from './api.js';
import { h, ic } from './ui.js';

const CJK = /[㐀-鿿]/;

// 原文能不能直接给用户看：含中文，且不含内部用词
export const clean = text => {
  const s = String(text ?? '').trim();
  return s && CJK.test(s) && !FORBIDDEN.test(s) ? s : '';
};

const GENERIC = {
  400: '填写的内容不对，请检查后再试',
  403: '当前不允许这个操作',
  404: '要找的内容已经不在了',
  409: '有别的操作正在进行，请稍后再试',
  413: '文件太大了',
};

const RECENT_MAX = 50;
const recent = [];
export const recentErrors = () => recent.slice();
export const savedPending = result => result?.rules_applied === false || result?.config_application?.pending === true;

function remember(title, raw) {
  if (!raw) return;
  recent.push({ at: Date.now(), title, raw: String(raw) });
  if (recent.length > RECENT_MAX) recent.splice(0, recent.length - RECENT_MAX);
}

// 错误 → {title, text, kind}；取消和被新请求取代的返回 null（不用提示）
export function humanize(err, { fallback = '操作没有完成' } = {}) {
  if (!err || isAbort(err)) return null;
  const kind = err.kind || 'http';
  const status = Number(err.status) || 0;
  const reason = clean(err.reason);
  let out;
  if (err.body?.saved === true || savedPending(err.body)) out = { title: '已保存，还没同步到入口', text: '可以重试同步，无需重复添加。' };
  else if (kind === 'runtime') out = { title: PHRASE.runtimeTitle, text: reason || PHRASE.runtimeText };
  else if (kind === 'timeout') out = { title: '等了太久没有回应', text: '稍后再试一次。' };
  else if (kind === 'network') out = { title: '连不上通道管家的后台', text: '重新打开应用试试。' };
  else if (reason) out = { title: reason, text: '' };
  else if (GENERIC[status]) out = { title: GENERIC[status], text: '' };
  else out = { title: fallback, text: '稍后再试一次；一直不行的话，到「设置 · 高级」里查看日志。' };
  remember(out.title, err.raw || err.reason || err.message);
  return { ...out, kind, status };
}

const toasts = new WeakMap();

// 悬浮消息，不挤压页面；相同消息合并，最多保留三条，可手动关闭。
export function notify(text, { kind = 'info', ms = 4000, root } = {}) {
  const doc = globalThis.document;
  const host = root || (doc && doc.getElementById('notice'));
  if (!host || !text) return () => {};
  let entries = toasts.get(host);
  if (!entries) { entries = []; toasts.set(host, entries); }
  const same = entries.find(item => item.text === text && item.kind === kind);
  if (same) { same.restart(ms); return same.close; }
  const el = h('div', { class: `toast ${kind}`, role: kind === 'bad' ? 'alert' : 'status', 'data-toast': '' },
    ic(kind === 'bad' ? 'warn' : kind === 'ok' ? 'okc' : 'info', 18, 1.6),
    h('span', { class: 'toast-text' }, text));
  let timer = null;
  const close = () => {
    clearTimeout(timer);
    el.remove();
    const at = entries.indexOf(item);
    if (at !== -1) entries.splice(at, 1);
    if (!host.children.length) host.hidden = true;
  };
  const restart = delay => { clearTimeout(timer); if (delay > 0) timer = setTimeout(close, delay); };
  const item = { text, kind, close, restart };
  el.append(h('button', { type: 'button', class: 'toast-close', 'aria-label': '关闭提示', onClick: close }, '×'));
  while (entries.length >= 3) entries[0].close();
  entries.push(item);
  host.append(el);
  host.hidden = false;
  restart(ms);
  return close;
}

// 把错误转成提示条；取消的请求不提示。返回 humanize 的结果
export function showError(err, opts = {}) {
  const m = humanize(err, opts);
  if (m) notify(m.text ? `${m.title}。${m.text}` : m.title, { kind: 'bad', ms: opts.ms ?? 6000, root: opts.root });
  return m;
}

// 写操作成功的提示；返回里 deferred 为真时说「已保存，连接后生效」
export function showDone(result, text = '已完成', opts = {}) {
  const pending = savedPending(result);
  return notify(pending ? '已保存，还没同步到入口' : result && result.deferred ? PHRASE.deferred : text, { kind: pending ? 'info' : 'ok', ...opts });
}
