/* 真实后端 API 封装,所有屏共用。失败抛 Error(消息含状态码+响应体)。 */
(function () {
  "use strict";

  /* 把响应体解析成结构:有 JSON 就挂 .body(对象);{error}/{detail} 提到 .reason 供 friendlyError 用。
   * .message 仍保持旧格式 "<status> <原文>",所有现有 catch(e => e.message) 调用零改动。 */
  function apiError(status, raw, ct) {
    const text = String(raw || "");
    let body = null, reason = "";
    if ((ct || "").includes("application/json") && text) {
      try {
        body = JSON.parse(text);
        if (body && typeof body === "object")
          reason = body.error || body.detail || body.message || "";
      } catch (_) { /* 非合法 JSON,留 text 兜底 */ }
    }
    const e = new Error(`${status} ${text}`.trim());
    e.status = status;          // HTTP 状态码(数字),friendlyError 据此分类
    e.body = body;              // 解析后的 JSON(无则 null)
    e.reason = reason || text;  // 后端给的人话原因(优先 {error}/{detail},否则原文)
    e.raw = text;               // 响应体原文(技术细节折叠区用)
    return e;
  }

  async function req(method, url, body) {
    const opt = { method, headers: {} };
    if (body !== undefined) {
      opt.headers["Content-Type"] = "application/json";
      opt.body = JSON.stringify(body);
    }
    const r = await fetch(url, opt);
    if (!r.ok) {
      const t = await r.text().catch(() => "");
      throw apiError(r.status, t, r.headers.get("content-type"));
    }
    const ct = r.headers.get("content-type") || "";
    return ct.includes("application/json") ? r.json() : r.text();
  }
  window.api = {
    channels: () => req("GET", "/api/channels"),
    vpnTypes: () => req("GET", "/api/vpn-types"),
    vpnVersions: (type) => req("GET", `/api/vpn-types/${type}/versions`),
    system: () => req("GET", "/api/system"),
    healProxy: () => req("POST", "/api/system/heal-proxy"),
    preflight: (vpnType, version, scope) =>
      req("GET", `/api/preflight?scope=${scope || "preflight"}${vpnType ? "&vpn_type=" + encodeURIComponent(vpnType) : ""}${version ? "&version=" + encodeURIComponent(version) : ""}`),
    preflightFix: (action, params) => req("POST", `/api/preflight/fix/${action}`, params || {}),
    preflightFixStatus: (taskId) => req("GET", `/api/preflight/fix/${taskId}`),
    images: () => req("GET", "/api/images"),
    exportConfig: () => req("GET", "/api/config/export"),
    importConfig: (doc) => req("POST", "/api/config/import", doc),
    create: (data) => req("POST", "/api/channels", data),
    update: (id, data) => req("PATCH", `/api/channels/${id}`, data),
    login: (id) => req("GET", `/api/channels/${id}/login`),
    upload: async (id, file) => {
      const fd = new FormData();
      fd.append("file", file, file.name);
      const r = await fetch(`/api/channels/${id}/upload`, { method: "POST", body: fd });
      if (!r.ok) throw apiError(r.status, await r.text().catch(() => ""), r.headers.get("content-type"));
      const ct = r.headers.get("content-type") || "";
      return ct.includes("application/json") ? r.json() : r.text();
    },
    status: (id) => req("GET", `/api/channels/${id}/status`),
    // 通道诊断(桌面版 host-only;web 版 404 → 前端 feature-detect 降级)
    diag: () => req("GET", "/api/diag"),
    // 容器管理(桌面版 host-only;web 版 404 → 前端 feature-detect 降级)
    containers: () => req("GET", "/api/containers"),
    containerLogs: (name, tail = 200) => req("GET", `/api/containers/${name}/logs?tail=${tail}`),
    containerRemove: (name) => req("DELETE", `/api/containers/${name}`),
    addRules: (id, patterns, kind) =>
      req("POST", `/api/channels/${id}/rules`, kind ? { patterns, kind } : { patterns }),
    delRule: (id, rid) => req("DELETE", `/api/channels/${id}/rules/${rid}`),
    toggleRule: (id, rid, enabled) =>
      req("PATCH", `/api/channels/${id}/rules/${rid}`, { enabled }),
    toggleRules: (ids, enabled) => req("PATCH", "/api/rules", { ids, enabled }),
    start: (id) => req("POST", `/api/channels/${id}/start`),
    stop: (id) => req("POST", `/api/channels/${id}/stop`),
    remove: (id) => req("DELETE", `/api/channels/${id}`),
    logs: (id, tail) => req("GET", `/api/channels/${id}/logs?tail=${tail || 200}`),
    connections: () => req("GET", "/api/connections"),
    proxies: () => req("GET", "/api/proxies"),
    entrySetup: () => req("GET", "/api/entry/setup-commands"),
    snippet: () => req("GET", "/api/clash-snippet"),
    // 7c 宿主接管层(Tauri/host-only;调用方 try/catch 做 feature-detect)
    clashDetect: () => req("GET", "/api/entry/clash-detect"),
    mergeProfile: () => req("GET", "/api/entry/merge-profile"),
    systemProxyGet: () => req("GET", "/api/entry/system-proxy"),
    systemProxySet: (enable) => req("POST", "/api/entry/system-proxy", { enable }),
    // 层3 TUN 入口(桌面版 host-only;web 版 404 → 前端 feature-detect 隐藏)
    tunGet: () => req("GET", "/api/entry/tun"),
    tunSet: (enable) => req("POST", "/api/entry/tun", { enable }),
    tunInstall: () => req("POST", "/api/entry/tun/install"),
    tunUninstall: () => req("POST", "/api/entry/tun/uninstall"),
    mirrors: () => req("GET", "/api/mirrors"),
    addMirror: (host) => req("POST", "/api/mirrors", { host }),
    patchMirror: (id, body) => req("PATCH", `/api/mirrors/${id}`, body),
    delMirror: (id) => req("DELETE", `/api/mirrors/${id}`),
    testMirror: (host) => req("POST", "/api/mirrors/test", { host }),
  };
})();
