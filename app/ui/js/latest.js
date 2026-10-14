// 只认最新：每次切换（换通道、换查询）开一代，上一代的请求被取消，晚到的旧响应直接丢弃（V-08）。
// 顶层不碰 document / window。

export class StaleError extends Error {
  constructor() { super('stale'); this.name = 'StaleError'; }
}
export const isStale = e => !!e && (e.name === 'StaleError' || e.name === 'AbortError');

export function createLatest(parent) {
  let gen = 0;
  let ctrl = null;
  let unlink = null;

  function drop() {
    if (unlink) unlink();
    unlink = null;
    if (ctrl) ctrl.abort();
  }

  // 开新一代：取消上一代，返回 {gen, signal, current()}
  function next() {
    drop();
    ctrl = new AbortController();
    const own = ctrl;
    if (parent) {
      const onAbort = () => own.abort();
      if (parent.aborted) own.abort();
      else {
        parent.addEventListener('abort', onAbort, { once: true });
        unlink = () => parent.removeEventListener('abort', onAbort);
      }
    }
    const my = ++gen;
    return { gen: my, signal: own.signal, current: () => my === gen && !own.signal.aborted };
  }

  // fn(signal) 的结果只在它仍是最新一代时交出；否则抛 StaleError（调用方用 isStale 忽略）
  async function run(fn) {
    const t = next();
    let value;
    try {
      value = await fn(t.signal);
    } catch (e) {
      if (!t.current()) throw new StaleError();
      throw e;
    }
    if (!t.current()) throw new StaleError();
    return value;
  }

  function cancel() {
    gen++;
    drop();
    ctrl = null;
  }

  return { next, run, cancel, get gen() { return gen; } };
}
