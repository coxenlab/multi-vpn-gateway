import { api } from "./api.js";

// 每个页面只有一个登录视图。离开/隐藏时取消等待，旧请求完成后也不能重新挂载 iframe。
export function createVncLifecycle({ channelId, isActive, open, close }) {
  let session = null;
  const release = view => {
    if (view.leased) api.releaseLoginViewer(view.cid, view.viewer).catch(() => {}); // 丢失时由 TTL 回收。
  };
  const pause = () => {
    const old = session;
    session = null;
    if (old) { clearTimeout(old.timer); old.controller.abort(); release(old); }
    close();
  };
  const start = () => {
    pause();
    if (document.hidden || !isActive()) return;
    const view = { controller: new AbortController(), cid: channelId(), viewer: crypto.randomUUID(), leased: false, timer: null };
    session = view;
    const signal = view.controller.signal;
    const current = () => session === view && !signal.aborted && !document.hidden && isActive();
    const renew = async () => {
      if (!current()) return;
      try {
        await api.renewLoginViewer(view.cid, view.viewer);
        if (current()) view.timer = setTimeout(renew, 20000);
      } catch (error) {
        if (!current()) return;
        if (error.status === 404) start();
        else view.timer = setTimeout(renew, 5000);
      }
    };
    const login = async () => {
      const result = await api.login(view.cid, { viewer: view.viewer });
      if (result.viewer_id === view.viewer) {
        view.leased = true;
        if (current()) view.timer = setTimeout(renew, 20000);
        else release(view); // 离开时仍在拉起的请求，完成后只释放它自己的观看标识。
      }
      return result; // Web/旧后端无 viewer_id 时不发送续期和释放请求。
    };
    Promise.resolve(open({ signal, current, login })).catch(error => {
      if (current()) console.warn("[vnc]", error.message);
    });
  };
  const sync = () => {
    if (document.hidden || !isActive()) pause();
    else if (!session) start();
  };
  document.addEventListener("visibilitychange", sync);
  window.addEventListener("pagehide", pause);
  window.addEventListener("pageshow", sync);
  return { open: start, pause, sync };
}
