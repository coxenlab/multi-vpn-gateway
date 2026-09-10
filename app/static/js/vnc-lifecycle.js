// 每个页面只有一个登录视图。离开/隐藏时取消等待，旧请求完成后也不能重新挂载 iframe。
export function createVncLifecycle({ isActive, open, close }) {
  let controller = null;
  const pause = () => {
    controller?.abort();
    controller = null;
    close();
  };
  const start = () => {
    pause();
    if (document.hidden || !isActive()) return;
    controller = new AbortController();
    const signal = controller.signal;
    const current = () => controller?.signal === signal && !signal.aborted && !document.hidden && isActive();
    Promise.resolve(open({ signal, current })).catch(error => {
      if (current()) console.warn("[vnc]", error.message);
    });
  };
  const sync = () => {
    if (document.hidden || !isActive()) pause();
    else if (!controller) start();
  };
  document.addEventListener("visibilitychange", sync);
  window.addEventListener("pagehide", pause);
  window.addEventListener("pageshow", sync);
  return { open: start, pause, sync };
}
