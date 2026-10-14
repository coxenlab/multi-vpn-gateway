// 登录窗口的观看租约（从 app/static/js/vnc-lifecycle.js 复制后改）。
// 每个页面只有一个登录视图。隐藏保留视图、暂停续期；离开时释放并取消等待。
// 改动：api、doc、win 可以传入替身；请求带上本次视图的 signal；传入的 signal 取消时收尾并移除监听
// （单页里切路由不会触发 pagehide）。顶层不碰 document / window。
import { api as defaultApi } from '../api.js';

const RENEW_MS = 20000;
const RETRY_MS = 5000;

export function createVncLifecycle({
  channelId, isActive, open, close, signal,
  api = defaultApi, doc = globalThis.document, win = globalThis.window,
  newId = () => globalThis.crypto.randomUUID(),
  now = () => Date.now(),
}) {
  let session = null;
  let ended = false;
  const hidden = () => !!(doc && doc.hidden);
  // 释放不带视图的 signal：视图关掉时请求还要发出去（api 里是 keepalive）
  const release = view => {
    if (view.leased) api.releaseLoginViewer(view.cid, view.viewer).catch(() => {}); // 丢失时由 TTL 回收
  };
  const dispose = () => {
    const old = session;
    session = null;
    if (old) { clearTimeout(old.timer); old.controller.abort(); release(old); }
    close();
  };
  const pause = () => {
    if (session) { clearTimeout(session.timer); session.timer = null; }
  };
  const start = () => {
    dispose();
    if (ended || hidden() || !isActive()) return;
    const view = { controller: new AbortController(), cid: channelId(), viewer: newId(), leased: false, timer: null, expires: 0, ttl: 60000, renewing: false };
    session = view;
    const own = view.controller.signal;
    const current = () => session === view && !own.aborted && isActive();
    const renew = async () => {
      if (!current() || hidden() || view.renewing) return;
      clearTimeout(view.timer);
      view.renewing = true;
      try {
        await api.renewLoginViewer(view.cid, view.viewer, { signal: own });
        view.expires = now() + view.ttl;
        if (current() && !hidden()) view.timer = setTimeout(renew, RENEW_MS);
      } catch (error) {
        if (!current()) return;
        if (error.status === 404) {
          view.expires = 0;
          if (!hidden()) start();
        } else if (!hidden()) view.timer = setTimeout(renew, RETRY_MS);
      } finally {
        view.renewing = false;
      }
    };
    view.renew = renew;
    const login = async () => {
      const result = await api.login(view.cid, { viewer: view.viewer }, { signal: own });
      if (result && result.viewer_id === view.viewer) {
        view.leased = true;
        view.ttl = Number(result.viewer_ttl_seconds) > 0 ? Number(result.viewer_ttl_seconds) * 1000 : 60000;
        view.expires = now() + view.ttl;
        if (current()) {
          if (!hidden()) view.timer = setTimeout(renew, RENEW_MS);
        } else release(view); // 离开时仍在拉起的请求，完成后只释放它自己的观看标识
      }
      return result; // 旧后端没有 viewer_id 时不发送续期和释放请求
    };
    Promise.resolve(open({ signal: own, current, login })).catch(error => {
      if (current()) console.warn('[vnc]', error && error.message);
    });
  };
  const sync = () => {
    if (ended) return;
    if (!isActive()) dispose();
    else if (hidden()) pause();
    else if (!session) start();
    else if (session.leased) {
      if (session.expires <= now()) start();
      else session.renew();
    }
  };
  const end = () => {
    if (ended) return;
    ended = true;
    dispose();
    if (doc && doc.removeEventListener) doc.removeEventListener('visibilitychange', sync);
    if (win && win.removeEventListener) {
      win.removeEventListener('pagehide', dispose);
      win.removeEventListener('pageshow', sync);
    }
  };
  if (doc && doc.addEventListener) doc.addEventListener('visibilitychange', sync);
  if (win && win.addEventListener) {
    win.addEventListener('pagehide', dispose);
    win.addEventListener('pageshow', sync);
  }
  if (signal) {
    if (signal.aborted) end();
    else signal.addEventListener('abort', end, { once: true });
  }
  return { open: start, pause, sync, end };
}
