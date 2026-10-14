// 轮询：上一轮结束后才开始计时；页面隐藏时暂停（外壳关窗时改写 document.hidden 并发 visibilitychange）；
// signal 取消就停。单页里切路由不会触发 pagehide，所以不监听它，靠 signal 收尾。
// doc 可以传入替身，顶层不碰 document / window。

// fn(signal) 可以是异步函数。返回 {kick()}：立刻再跑一轮（用户操作后刷新用），返回这一轮的 Promise。
export function poll(fn, ms, { signal, doc = globalThis.document, immediate = true, onError } = {}) {
  let timer = null;
  let running = null;
  let again = null; // 本轮进行中又被要求刷新：本轮结束后紧接着再跑一轮，这个 Promise 等那一轮
  let stopped = !!(signal && signal.aborted);
  const hidden = () => !!(doc && doc.hidden);

  const clear = () => { if (timer !== null) { clearTimeout(timer); timer = null; } };
  const schedule = () => { clear(); if (!stopped && !hidden()) timer = setTimeout(() => { timer = null; tick(); }, ms); };

  function tick(force) {
    if (stopped) return Promise.resolve();
    if (running) {
      if (!force) return running;
      if (!again) {
        let resolve;
        again = { promise: new Promise(r => { resolve = r; }), resolve: () => resolve() };
      }
      return again.promise;
    }
    if (hidden() && !force) return Promise.resolve();
    clear();
    running = (async () => {
      try { await fn(signal); } catch (e) { if (onError && !stopped) onError(e); }
    })().finally(() => {
      running = null;
      const follow = again;
      again = null;
      if (follow) tick(true).then(follow.resolve);
      else schedule();
    });
    return running;
  }

  const onVisible = () => { if (hidden()) clear(); else tick(); };
  const stop = () => {
    stopped = true;
    clear();
    if (doc && doc.removeEventListener) doc.removeEventListener('visibilitychange', onVisible);
  };

  if (!stopped) {
    if (doc && doc.addEventListener) doc.addEventListener('visibilitychange', onVisible);
    if (signal) signal.addEventListener('abort', stop, { once: true });
    if (immediate) tick(); else schedule();
  }
  return { kick: () => tick(true), stop };
}
