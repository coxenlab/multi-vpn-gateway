// 共享快照：各页面订阅同一份数据，只在有订阅者时才轮询（DESIGN 4.3）。
// 顶层不碰 document / window：createData 可以传入替身 api 和 doc，node 直接测试。
//
// 键和间隔：
// - core：/api/channels + /api/system，8 秒；用户操作后调 refresh() 立即刷新一次
// - health：每条运行中的通道的 /health，15 秒。只用于显示，窗口隐藏时停
// - connections：/api/connections，按订阅者要的间隔取最短（总览 3 秒，连接页 1.5 秒）；速度用累计值差分
// - events：/api/events?since_seq=，10 秒，只留最近的一段
// - traffic：/api/traffic/today，30 秒；接口不存在时值为 UNAVAILABLE，不再轮询
import { api as defaultApi, isUnavailable, isAbort } from './api.js';
import { poll } from './poll.js';
import { isActive, speed } from './derive.js';

export const INTERVAL = Object.freeze({ core: 8000, health: 15000, connections: 3000, events: 10000, traffic: 30000 });
export const HISTORY_MS = 5 * 60 * 1000; // 速度曲线只留最近 5 分钟，只在内存里
export const EVENTS_KEEP = 200;
const EVENTS_FIRST = 1000; // 第一次取整个环形缓冲，之后按 since_seq 增量取

export function createData({ api = defaultApi, doc = globalThis.document, now = () => Date.now() } = {}) {
  const slots = new Map();

  function slot(key) {
    let s = slots.get(key);
    if (!s) {
      s = { snap: { value: undefined, error: null, at: 0 }, subs: new Set(), poller: null, ctrl: null, ms: 0, done: false };
      slots.set(key, s);
    }
    return s;
  }

  function publish(key, patch) {
    const s = slot(key);
    s.snap = { ...s.snap, ...patch };
    for (const sub of [...s.subs]) {
      try { sub.fn(s.snap); } catch (e) { if (globalThis.console) console.warn('[data]', key, e); }
    }
    if (key === 'core') healthGaps();
  }

  // ---- 各键的取数 ----

  async function fetchCore(signal) {
    const prev = slot('core').snap.value || {};
    const [ch, sys] = await Promise.allSettled([api.channels({ signal }), api.system({ signal })]);
    if (signal && signal.aborted) return;
    const error = [ch, sys].find(r => r.status === 'rejected');
    if (error && isAbort(error.reason)) return;
    publish('core', {
      value: {
        channels: ch.status === 'fulfilled' && Array.isArray(ch.value) ? ch.value : (prev.channels ?? null),
        system: sys.status === 'fulfilled' ? sys.value : (prev.system ?? null),
      },
      error: error ? error.reason : null,
      at: now(),
    });
  }

  async function fetchHealth(signal) {
    if (!slot('core').snap.value) await fetchCore(signal);
    const channels = (slot('core').snap.value && slot('core').snap.value.channels) || [];
    const ids = channels.filter(isActive).map(c => c.id);
    const res = await Promise.allSettled(ids.map(id => api.channelHealth(id, { signal })));
    if (signal && signal.aborted) return;
    const map = {};
    res.forEach((r, i) => { if (r.status === 'fulfilled' && r.value && typeof r.value === 'object') map[ids[i]] = r.value; });
    slot('health').asked = new Set(ids);
    publish('health', { value: map, error: null, at: now() });
  }

  async function fetchConnections(signal) {
    let conn;
    try { conn = await api.connections({ signal }); } catch (e) {
      if (!isAbort(e)) {
        const previous = slot('connections').snap.value;
        const t = now();
        const history = (previous?.history || []).filter(p => t - p.at <= HISTORY_MS);
        history.push({ at: t, up: null, down: null });
        publish('connections', { value: { ...previous, unavailable: true, up: null, down: null, last: null, history }, error: e, at: t });
      }
      return;
    }
    const s = slot('connections');
    const t = now();
    const prev = s.snap.value || null;
    const sp = speed(prev && prev.last, conn, t);
    const history = ((prev && prev.history) || []).filter(p => t - p.at <= HISTORY_MS);
    if (sp.up != null) history.push({ at: t, up: sp.up, down: sp.down });
    publish('connections', { value: { conn, unavailable: false, up: sp.up, down: sp.down, last: sp.next, history }, error: null, at: t });
  }

  async function fetchEvents(signal) {
    const prev = slot('events').snap.value || null;
    const params = prev ? { since_seq: prev.seq, limit: EVENTS_FIRST } : { limit: EVENTS_FIRST };
    params.level = 'info';
    let r;
    try { r = await api.runtimeEvents(params, { signal }); } catch (e) {
      if (!isAbort(e)) publish('events', { error: e, at: now() });
      return;
    }
    if (isUnavailable(r) || !r || !Array.isArray(r.events)) { publish('events', { value: r, error: null, at: now() }); return; }
    const seq = Number(r.seq) || 0;
    // seq 变小说明核心重启过，从头取
    const base = prev && seq >= prev.seq ? prev.events : [];
    const events = base.concat(r.events).slice(-EVENTS_KEEP);
    publish('events', { value: { events, seq, enabled: r.enabled }, error: null, at: now() });
  }

  async function fetchTraffic(signal) {
    let r;
    try { r = await api.trafficToday({ signal }); } catch (e) {
      if (!isAbort(e)) publish('traffic', { error: e, at: now() });
      return;
    }
    if (isUnavailable(r)) slot('traffic').done = true;
    publish('traffic', { value: r, error: null, at: now() });
  }

  const FETCH = { core: fetchCore, health: fetchHealth, connections: fetchConnections, events: fetchEvents, traffic: fetchTraffic };

  // ---- 轮询和订阅 ----

  function wantedMs(key) {
    const s = slot(key);
    let ms = INTERVAL[key];
    for (const sub of s.subs) if (sub.ms && sub.ms < ms) ms = sub.ms;
    return ms;
  }

  function start(key) {
    const s = slot(key);
    const ms = wantedMs(key);
    if (s.poller && s.ms === ms) return;
    stopPoller(key);
    if (s.done) return;
    s.ms = ms;
    s.ctrl = new AbortController();
    s.poller = poll(async signal => {
      await FETCH[key](signal);
      if (s.done) stopPoller(key);
    }, ms, { signal: s.ctrl.signal, doc });
  }

  function stopPoller(key) {
    const s = slot(key);
    if (s.ctrl) s.ctrl.abort();
    s.ctrl = null;
    s.poller = null;
  }

  // 订阅：fn(snapshot) 在每次更新时调用；有快照时立即调用一次。
  // snapshot = {value, error, at}。ms 可以要求更短的间隔（只对 connections 有意义）。
  // 返回取消订阅的函数；signal 取消时也会自动取消。
  function subscribe(key, fn, { signal, ms } = {}) {
    if (!FETCH[key]) throw new Error('unknown data key ' + key);
    if (signal && signal.aborted) return () => {};
    const s = slot(key);
    const sub = { fn, ms };
    s.subs.add(sub);
    // health 依赖通道列表
    const dep = key === 'health' ? subscribe('core', () => {}, { signal }) : null;
    start(key);
    if (s.snap.at) { try { fn(s.snap); } catch (e) { if (globalThis.console) console.warn('[data]', key, e); } }
    let off = false;
    const unsubscribe = () => {
      if (off) return;
      off = true;
      s.subs.delete(sub);
      if (signal) signal.removeEventListener('abort', unsubscribe);
      if (dep) dep();
      if (!s.subs.size) stopPoller(key); else start(key);
    };
    if (signal) signal.addEventListener('abort', unsubscribe, { once: true });
    return unsubscribe;
  }

  // 立刻刷新一次（用户操作后）。有订阅者时借轮询器插一轮，否则单独取一次
  function refresh(key = 'core') {
    const s = slot(key);
    if (s.poller) return s.poller.kick();
    return FETCH[key]();
  }

  // 通道列表里出现了上一轮没问过的运行中通道：不等 15 秒，马上补一轮
  function healthGaps() {
    const h = slots.get('health');
    if (!h || !h.poller || !h.asked) return;
    const channels = (slot('core').snap.value && slot('core').snap.value.channels) || [];
    if (channels.some(c => isActive(c) && !h.asked.has(c.id))) h.poller.kick();
  }

  const get = key => slot(key).snap;
  const polling = key => !!(slots.get(key) && slots.get(key).poller);

  return { subscribe, refresh, get, polling };
}

// 启动时恢复上次在用的通道（S-02）：开了「自动连接」且记下了通道时，先清空记录再逐条启动。
// 需要登录的通道会停在「待登录」，出现在总览的「需要处理」里，不弹窗。接口不存在就跳过。
// 返回每条的结果 [{id, ok, error}]
export async function resumeChannels(api, { signal } = {}) {
  const s = await api.appSettings({ signal });
  if (isUnavailable(s) || !s || s.auto_connect !== true) return [];
  const ids = Array.isArray(s.resume_channels) ? s.resume_channels.filter(Boolean) : [];
  if (!ids.length) return [];
  const put = await api.setAppSettings({ resume_channels: [] }, { signal });
  if (isUnavailable(put)) return [];
  const out = [];
  for (const id of ids) {
    try { await api.start(id, { signal }); out.push({ id, ok: true }); } catch (e) {
      if (isAbort(e)) throw e;
      out.push({ id, ok: false, error: e });
    }
  }
  return out;
}

export const data = createData();
