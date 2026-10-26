// 请求层：现有接口和新接口一次写全，后面的包不改这个文件。
// 顶层不碰 document / window：createApi 可以传入替身 fetch，node 直接测试。
//
// 判定规则：
// - HTTP ≥ 400 抛 ApiError，reason 取体里的 error ?? detail；503 的 kind 为 runtime
// - 2xx 但体里 ok === false 也按失败抛出（kind 为 failed）
// - 202 或体里 deferred === true：照常返回，并带 deferred: true
// - 新接口返回 404：返回 UNAVAILABLE，调用方隐藏对应元素
// - 同一地址的 GET 在途时合并；写请求不合并
// - 每个函数最后一个参数都接受 {signal}

export const TIMEOUT = 25000;
export const LONG_TIMEOUT = 20 * 60 * 1000;
const SLOW_TIMEOUT = 45000;
export const UPLOAD_LIMIT = 1024 * 1024 * 1024;

export class ApiError extends Error {
  constructor({ status = 0, reason = '', raw = '', body = null, kind = 'http' } = {}) {
    super(reason || (status ? `HTTP ${status}` : kind));
    this.name = 'ApiError';
    this.status = status;
    this.reason = reason;
    this.raw = raw;
    this.body = body;
    this.kind = kind; // http / runtime / failed / timeout / network
  }
}

export const UNAVAILABLE = Object.freeze({ unavailable: true });
export const isUnavailable = v => v === UNAVAILABLE;

// 离开页面或被新请求取代：调用方直接忽略
export const isAbort = e => !!e && (e.name === 'AbortError' || e.name === 'StaleError');

const abortError = () => {
  const e = new Error('aborted');
  e.name = 'AbortError';
  return e;
};

const enc = encodeURIComponent;

function qs(params) {
  const q = new URLSearchParams();
  for (const [k, v] of Object.entries(params || {})) if (v !== undefined && v !== null && v !== '') q.set(k, String(v));
  const s = q.toString();
  return s ? '?' + s : '';
}

export function createApi({ fetch: fetchImpl, timeout = TIMEOUT, longTimeout = LONG_TIMEOUT, slowTimeout = SLOW_TIMEOUT } = {}) {
  const doFetch = (url, init) => (fetchImpl || globalThis.fetch)(url, init);
  const inflight = new Map();

  // 发一次请求。signal 取消时抛 AbortError；超时抛 ApiError(kind=timeout)
  async function send(method, url, { json, body, contentType, ms = timeout, signal, optional, keepalive } = {}) {
    if (signal && signal.aborted) throw abortError();
    const ctrl = new AbortController();
    let timedOut = false;
    const timer = setTimeout(() => { timedOut = true; ctrl.abort(); }, ms);
    const onAbort = () => ctrl.abort();
    if (signal) signal.addEventListener('abort', onAbort, { once: true });
    const init = { method, headers: {}, signal: ctrl.signal };
    if (keepalive) init.keepalive = true;
    if (json !== undefined) { init.headers['Content-Type'] = 'application/json'; init.body = JSON.stringify(json); }
    else if (body !== undefined) { if (contentType) init.headers['Content-Type'] = contentType; init.body = body; }
    let r, text;
    try {
      r = await doFetch(url, init);
      text = await r.text();
    } catch (e) {
      if (timedOut) throw new ApiError({ kind: 'timeout', reason: '' });
      if ((signal && signal.aborted) || (e && e.name === 'AbortError')) throw abortError();
      throw new ApiError({ kind: 'network', raw: String((e && e.message) || e || '') });
    } finally {
      clearTimeout(timer);
      if (signal) signal.removeEventListener('abort', onAbort);
    }
    const ct = (r.headers && r.headers.get && r.headers.get('content-type')) || '';
    let data = text;
    let isJson = false;
    if (ct.includes('json') && text) {
      try { data = JSON.parse(text); isJson = true; } catch { /* 按文本处理 */ }
    }
    const obj = isJson && data && typeof data === 'object' && !Array.isArray(data) ? data : null;
    if (r.status === 404 && optional) return UNAVAILABLE;
    if (r.status >= 400 || !r.ok) {
      throw new ApiError({
        status: r.status, reason: String((obj && (obj.error ?? obj.detail)) || ''), raw: text, body: obj,
        kind: r.status === 503 ? 'runtime' : 'http',
      });
    }
    if (obj) {
      if (obj.ok === false) {
        throw new ApiError({ status: r.status, reason: String(obj.error ?? obj.detail ?? ''), raw: text, body: obj, kind: 'failed' });
      }
      if (r.status === 202 || obj.deferred === true) obj.deferred = true;
    }
    return data;
  }

  // 同一地址的 GET 共用一次请求。某个调用方取消只影响它自己；全部取消时才真正中止请求
  function get(url, opts = {}) {
    const { signal, ...rest } = opts;
    if (signal && signal.aborted) return Promise.reject(abortError());
    let entry = inflight.get(url);
    if (!entry) {
      const ctrl = new AbortController();
      entry = { ctrl, waiters: 0, settled: false };
      entry.promise = send('GET', url, { ...rest, signal: ctrl.signal });
      entry.promise.then(() => {}, () => {}).finally(() => {
        entry.settled = true;
        if (inflight.get(url) === entry) inflight.delete(url);
      });
      inflight.set(url, entry);
    }
    return join(url, entry, signal);
  }

  function join(url, entry, signal) {
    entry.waiters++;
    return new Promise((resolve, reject) => {
      let done = false;
      const finish = () => {
        done = true;
        entry.waiters--;
        if (signal) signal.removeEventListener('abort', onAbort);
      };
      const onAbort = () => {
        if (done) return;
        finish();
        if (entry.waiters === 0 && !entry.settled) {
          if (inflight.get(url) === entry) inflight.delete(url);
          entry.ctrl.abort();
        }
        reject(abortError());
      };
      if (signal) signal.addEventListener('abort', onAbort, { once: true });
      entry.promise.then(v => { if (!done) { finish(); resolve(v); } }, e => { if (!done) { finish(); reject(e); } });
    });
  }

  const post = (url, json, o = {}) => send('POST', url, { ...o, json });
  const put = (url, json, o = {}) => send('PUT', url, { ...o, json });
  const patch = (url, json, o = {}) => send('PATCH', url, { ...o, json });
  const del = (url, o = {}) => send('DELETE', url, o);
  const long = o => ({ ...o, ms: longTimeout });
  const slow = o => ({ ...o, ms: slowTimeout });
  const optional = o => ({ ...o, optional: true });

  const api = {
    // 通道
    channels: o => get('/api/channels', o),
    create: (data, o) => post('/api/channels', data, long(o)),
    update: (id, data, o) => patch(`/api/channels/${enc(id)}`, data, long(o)),
    remove: (id, prepareRuntime = false, o) => del(`/api/channels/${enc(id)}${prepareRuntime ? '?prepare_runtime=true' : ''}`, long(o)),
    start: (id, o) => post(`/api/channels/${enc(id)}/start`, undefined, long(o)),
    stop: (id, o) => post(`/api/channels/${enc(id)}/stop`, undefined, long(o)),
    restore: (id, o) => post(`/api/channels/${enc(id)}/restore`, undefined, long(o)),
    status: (id, o) => get(`/api/channels/${enc(id)}/status`, slow(o)),
    verifyTarget: (id, probeUrl, o) => post(`/api/channels/${enc(id)}/verify-target`, { probe_url: probeUrl }, slow(o)),
    channelHealth: (id, o) => get(`/api/channels/${enc(id)}/health`, slow(o)),
    logs: (id, tail = 200, o) => get(`/api/channels/${enc(id)}/logs?tail=${enc(tail || 200)}`, o),
    login: (id, { viewer } = {}, o) => get(`/api/channels/${enc(id)}/login${qs({ viewer })}`, slow(o)),
    renewLoginViewer: (id, viewer, o) => put(`/api/channels/${enc(id)}/login/viewers/${enc(viewer)}`, undefined, o),
    releaseLoginViewer: (id, viewer, o) => del(`/api/channels/${enc(id)}/login/viewers/${enc(viewer)}`, { ...o, keepalive: true }),
    noteGet: (id, o) => get(`/api/channels/${enc(id)}/note`, o),
    noteSet: (id, note, expected_note, o) =>
      put(`/api/channels/${enc(id)}/note`, expected_note === undefined ? { note } : { note, expected_note }, o),
    upload: async (id, file, o = {}) => {
      if (!file || !file.size || file.size > UPLOAD_LIMIT) throw new ApiError({ status: 400, reason: '请选择非空且不超过 1 GiB 的安装包' });
      const fd = new FormData();
      fd.append('file', file, file.name);
      const r = await send('POST', `/api/channels/${enc(id)}/upload`, { ...long(o), body: fd });
      if (!r || r.ok !== true) throw new ApiError({ status: 200, reason: '上传结果没有确认，请在通道里核对', body: r, kind: 'failed' });
      return r;
    },
    installStatus: (id, o) => get(`/api/channels/${enc(id)}/install`, o),
    startupStatus: (id, o) => get(`/api/channels/${enc(id)}/startup`, o),

    // 规则
    addRules: (id, patterns, kind, o) =>
      post(`/api/channels/${enc(id)}/rules`, kind ? { patterns, kind } : { patterns }, o),
    delRule: (id, rid, o) => del(`/api/channels/${enc(id)}/rules/${enc(rid)}`, o),
    toggleRule: (id, rid, enabled, o) => patch(`/api/channels/${enc(id)}/rules/${enc(rid)}`, { enabled }, o),
    updateRule: (id, rid, body, o) => patch(`/api/channels/${enc(id)}/rules/${enc(rid)}`, body, o),
    toggleRules: (ids, enabled, o) => patch('/api/rules', { ids, enabled }, o),

    // 运行环境、分流、诊断
    system: o => get('/api/system', o),
    runtimeRestart: o => post('/api/runtime/restart', undefined, long(o)),
    runtimeStart: o => post('/api/runtime/start', undefined, long(o)),
    // 只在托管运行环境时才发启动（旧界面同名函数的语义）
    startRuntime: async o => {
      const system = await get('/api/system', o);
      if (system && system.runtime) return post('/api/runtime/start', undefined, long(o));
      return undefined;
    },
    healProxy: o => post('/api/system/heal-proxy', undefined, long(o)),
    selfHeal: (enabled, o) => post('/api/system/self-heal', { enabled }, o),
    routing: o => get('/api/routing', o),
    setRouting: (off, o) => post('/api/routing', { off }, o),
    diag: o => get('/api/diag', slow(o)),
    preflight: (vpnType, version, scope, o) =>
      get(`/api/preflight${qs({ scope: scope || 'preflight', vpn_type: vpnType, version })}`, slow(o)),
    preflightFix: (action, params, o) => post(`/api/preflight/fix/${enc(action)}`, params || {}, o),
    preflightFixStatus: (taskId, o) => get(`/api/preflight/fix/${enc(taskId)}`, o),
    containers: o => get('/api/containers', o),
    containerLogs: (name, tail = 200, o) => get(`/api/containers/${enc(name)}/logs?tail=${enc(tail)}`, o),
    containerRemove: (name, o) => del(`/api/containers/${enc(name)}`, o),

    // 类型、版本、客户端下载
    vpnTypes: o => get('/api/vpn-types', o),
    vpnVersions: (type, o) => get(`/api/vpn-types/${enc(type)}/versions`, o),
    images: o => get('/api/images', o),
    imageImport: (file, o) => send('POST', '/api/images/imports', { ...long(o), body: file, contentType: 'application/octet-stream' }),
    imageImportStatus: (id, o) => get(`/api/images/imports/${enc(id)}`, o),
    imageImportDiscard: (id, o) => del(`/api/images/imports/${enc(id)}`, o),
    imageImportConfirm: (id, o) => post(`/api/images/imports/${enc(id)}/confirm`, undefined, long(o)),
    mirrors: o => get('/api/mirrors', o),
    addMirror: (host, o) => post('/api/mirrors', { host }, o),
    patchMirror: (id, body, o) => patch(`/api/mirrors/${enc(id)}`, body, o),
    delMirror: (id, o) => del(`/api/mirrors/${enc(id)}`, o),
    testMirror: (host, o) => post('/api/mirrors/test', { host }, slow(o)),

    // 连接
    connections: o => get('/api/connections', o),
    proxies: o => get('/api/proxies', o),

    // 运行日志
    runtimeEvents: (params = {}, o) => get(`/api/events${qs(params)}`, o),
    runtimeEventsEnabled: o => get('/api/events/enabled', o),
    runtimeEventsSetEnabled: (enabled, o) => post('/api/events/enabled', { enabled }, o),
    runtimeEventsExportUrl: (from, to) => `/api/events/export${qs({ from, to })}`,
    runtimeEventsExport: (from, to, o) => get(`/api/events/export${qs({ from, to })}`, o),

    // 接入网络（配合 Clash、系统代理、虚拟网卡）
    entrySetup: o => get('/api/entry/setup-commands', o),
    snippet: o => get('/api/clash-snippet', o),
    clashDetect: o => get('/api/entry/clash-detect', o),
    mergeProfile: o => get('/api/entry/merge-profile', o),
    systemProxyGet: o => get('/api/entry/system-proxy', o),
    systemProxySet: (enable, o) => post('/api/entry/system-proxy', { enable }, o),
    tunGet: o => get('/api/entry/tun', o),
    tunSet: (enable, o) => post('/api/entry/tun', { enable }, long(o)),
    tunInstall: o => post('/api/entry/tun/install', undefined, long(o)),
    tunUninstall: o => post('/api/entry/tun/uninstall', undefined, long(o)),

    // 备份
    exportConfig: o => get('/api/config/export', o),
    importConfig: (doc, o) => post('/api/config/import', doc, long(o)),
    retryConfig: o => post('/api/config/retry', undefined, long(o)),

    // 新接口（DESIGN 5.0）。返回 404 时得到 UNAVAILABLE
    rulesTest: (q, o) => get(`/api/rules/test${qs({ q })}`, optional(slow(o))),
    cause: (id, o) => get(`/api/channels/${enc(id)}/cause`, optional(o)),
    setConfigFile: (id, content, filename, o) =>
      put(`/api/channels/${enc(id)}/config-file`, { content, filename }, optional(long(o))),
    trafficToday: o => get('/api/traffic/today', optional(o)),
    latency: (id, o) => get(`/api/channels/${enc(id)}/latency`, optional(o)),
    appSettings: o => get('/api/app-settings', optional(o)),
    setAppSettings: (patchBody, o) => put('/api/app-settings', patchBody, optional(o)),
    takeover: o => get('/api/takeover', optional(o)),
    setTakeover: (body, o) => put('/api/takeover', body, optional(long(o))),
  };
  return api;
}

export const api = createApi();
