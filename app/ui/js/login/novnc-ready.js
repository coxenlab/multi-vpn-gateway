// 登录窗口就绪探测（从 app/static/js/app.js:15-35 抽出）。
// 通道起来以后，登录窗口的网页服务还要几十秒才起。iframe 早于服务就绪就设 src，连接被拒后会一直白屏，
// 也不会自己重载。所以先用跨源 no-cors 请求探到对方在伺服再设 src：服务起着 → 返回不透明响应，
// 连接被拒 → 抛错。单次请求与整体等待都有上限；signal 取消立即停，超时返回 false，由页面给重试。
// 顶层不碰 document / window；fetch 在调用时才取，可以传入替身。
export async function waitNovncReady(url, maxSec = 60, { signal, fetch: fetchImpl } = {}) {
  const doFetch = fetchImpl || globalThis.fetch;
  const deadline = Date.now() + maxSec * 1000;
  while (Date.now() < deadline && !signal?.aborted) {
    const request = new AbortController();
    const abort = () => request.abort();
    signal?.addEventListener('abort', abort, { once: true });
    const timer = setTimeout(abort, Math.min(5000, deadline - Date.now()));
    try {
      await doFetch(url, { mode: 'no-cors', cache: 'no-store', signal: request.signal });
      return !signal?.aborted;
    } catch (_) { /* 有界重试；离开登录页立即取消 */ }
    finally { clearTimeout(timer); signal?.removeEventListener('abort', abort); }
    if (signal?.aborted) return false;
    await new Promise(resolve => {
      const done = () => { clearTimeout(delay); signal?.removeEventListener('abort', done); resolve(); };
      const delay = setTimeout(done, Math.min(2000, Math.max(0, deadline - Date.now())));
      signal?.addEventListener('abort', done, { once: true });
    });
  }
  return false;
}
