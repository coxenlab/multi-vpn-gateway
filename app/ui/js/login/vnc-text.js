// 发送到登录窗口（从 app/static/js/vncText.js 复制后改）。
//
// 原理：登录 iframe 与本页不同源，驱动不了 iframe 里的远程桌面；这里用 vendor 的 noVNC core 另起一条
// shared 连接（不顶掉 iframe 的会话），把文字逐字符转成按键发到当前聚焦的输入框，并同时写入对方剪贴板。
// 文字只走 页面 → 登录窗口的 websockify，不经任何后端接口，不写日志，不留档（命门 #5）。
// 这只是输入辅助，登录成功与否仍只认后端的连通检测（命门 #1）。
//
// 改动：noVNC 在第一次发送时才按需加载，页面打开不受它影响；输入条用 ui.js 构建，提示走 feedback.js；
// 界面文字改成「登录窗口」的说法。wsInfo 是纯函数。
import { h, field, btn } from '../ui.js';
import { notify, clean } from '../feedback.js';

const KEY_GAP_MS = 25;       // 字符间隔：Qt / Xvfb 上连击太快会丢键
const CONNECT_TIMEOUT_MS = 10000;

const sleep = ms => new Promise(r => setTimeout(r, ms));
const closed = () => new DOMException('登录窗口已关闭', 'AbortError');

let novnc = null;
function loadNovnc() {
  if (!novnc) {
    novnc = Promise.all([
      import('../../vendor/novnc/core/rfb.js'),
      import('../../vendor/novnc/core/input/keysymdef.js'),
      import('../../vendor/novnc/core/input/keysym.js'),
    ]).then(([rfb, def, table]) => ({ RFB: rfb.default, keysyms: def.default, KeyTable: table.default }))
      .catch(e => { novnc = null; throw e; });
  }
  return novnc;
}

// 从 /login 返回的 url（vnc.html?path=websockify/&password=…）推导连接地址与密码。
// 两种栈（本机映射 / 转发）给的 url 同形，统一在这里解析。
export function wsInfo(loginUrl, base = globalThis.location && globalThis.location.href) {
  const u = new URL(loginUrl, base);
  const path = u.searchParams.get('path') || 'websockify/';
  const scheme = u.protocol === 'https:' ? 'wss' : 'ws';
  return {
    wsUrl: `${scheme}://${u.host}/${path.replace(/^\//, '')}`,
    password: u.searchParams.get('password') || '',
  };
}

function charKeysym(ch, { keysyms, KeyTable }) {
  if (ch === '\n' || ch === '\r') return KeyTable.XK_Return;
  if (ch === '\t') return KeyTable.XK_Tab;
  return keysyms.lookup(ch.codePointAt(0));
}

// 建临时 shared 连接 → 键入 text（并写对方剪贴板）→ 断开。返回键入的字符数。
export async function send(loginUrl, text, { signal } = {}) {
  if (!text) return 0;
  const checkActive = () => { if (signal?.aborted) throw closed(); };
  checkActive();
  const lib = await loadNovnc();
  checkActive();
  const { RFB } = lib;
  const { wsUrl, password } = wsInfo(loginUrl);

  // 连接需要一个挂载点；藏起来，不显示给用户（这条连接只发输入）
  const holder = document.createElement('div');
  holder.style.cssText = 'position:fixed;left:-9999px;width:2px;height:2px;overflow:hidden;';
  document.body.appendChild(holder);

  let rfb, pasted = false;
  const disconnect = () => {
    if (pasted) { try { rfb?.clipboardPasteFrom(' '); } catch (_e) { /* 连接可能已关闭 */ } }
    try { rfb?.disconnect(); } catch (_e) { /* 已断开 */ }
  };
  signal?.addEventListener('abort', disconnect, { once: true });
  try {
    rfb = new RFB(holder, wsUrl, { credentials: { password }, shared: true });
    rfb.clipViewport = false;
    await new Promise((resolve, reject) => {
      const finish = error => {
        clearTimeout(timer);
        signal?.removeEventListener('abort', abort);
        rfb.removeEventListener('connect', connect);
        rfb.removeEventListener('securityfailure', failure);
        rfb.removeEventListener('disconnect', ended);
        if (error) reject(error); else resolve();
      };
      const connect = () => finish();
      const failure = () => finish(new Error('登录窗口拒绝了连接'));
      const ended = () => finish(signal?.aborted ? closed() : new Error('和登录窗口的连接断开了'));
      const abort = () => finish(closed());
      const timer = setTimeout(() => finish(new Error('连接登录窗口超时')), CONNECT_TIMEOUT_MS);
      rfb.addEventListener('connect', connect);
      rfb.addEventListener('securityfailure', failure);
      rfb.addEventListener('disconnect', ended);
      signal?.addEventListener('abort', abort, { once: true });
    });
    await sleep(300);                 // 对方会话就绪缓冲
    checkActive();
    rfb.clipboardPasteFrom(text);     // 顺带写对方剪贴板（登录窗口里 Ctrl+V 兜底）
    pasted = true;
    let typed = 0;
    for (const ch of text) {
      checkActive();
      const ks = charKeysym(ch, lib);
      if (!ks) continue;              // 没有对应按键的控制字符跳过
      rfb.sendKey(ks, null);
      typed++;
      await sleep(KEY_GAP_MS);
    }
    await sleep(150);                 // 让最后几个按键发出去再断
    checkActive();
    return typed;
  } finally {
    // 成功、失败或关闭视图都尽力清掉已写入的剪贴板，再断开临时连接
    disconnect();
    signal?.removeEventListener('abort', disconnect);
    holder.remove();
  }
}

// 「发送到登录窗口」输入条。getUrl() 返回当前登录窗口的 url（没打开时返回空）。
// 输入的文字只发给登录窗口，不留档；signal 取消时清空输入框。
export function sendBar(getUrl, { signal } = {}) {
  const input = field({ placeholder: '在这里输入或粘贴，发送到登录窗口', label: '发送到登录窗口', onEnter: () => doSend() });
  input.title = '先在登录窗口里点一下要填的输入框，再发送；文字也会放进登录窗口的剪贴板';
  const button = btn('发送', { cls: 'pri', onClick: () => doSend() });
  signal?.addEventListener('abort', () => { input.value = ''; }, { once: true });

  async function doSend() {
    if (signal?.aborted || button.disabled) return;
    const text = input.value;
    if (!text) { input.focus(); return; }
    const url = getUrl();
    if (!url) { notify('登录窗口还没打开，稍候再试', { kind: 'bad' }); return; }
    button.disabled = true;
    button.textContent = '发送中…';
    try {
      const n = await send(url, text, { signal });
      if (signal?.aborted) return;
      notify(`已发送 ${n} 个字符到登录窗口`, { kind: 'ok' });
    } catch (e) {
      if (!signal?.aborted) notify('发送失败：' + (clean(e && e.message) || '稍后再试一次'), { kind: 'bad' });
    } finally {
      button.disabled = false;
      button.textContent = '发送';
    }
  }

  return h('div', { class: 'typebar' }, input, button);
}
