/* 真实后端 API 封装,所有屏共用。失败抛 Error(消息含状态码+响应体)。 */
(function () {
  "use strict";
  async function req(method, url, body) {
    const opt = { method, headers: {} };
    if (body !== undefined) {
      opt.headers["Content-Type"] = "application/json";
      opt.body = JSON.stringify(body);
    }
    const r = await fetch(url, opt);
    if (!r.ok) {
      const t = await r.text().catch(() => "");
      throw new Error(`${r.status} ${t}`.trim());
    }
    const ct = r.headers.get("content-type") || "";
    return ct.includes("application/json") ? r.json() : r.text();
  }
  window.api = {
    channels: () => req("GET", "/api/channels"),
    vpnTypes: () => req("GET", "/api/vpn-types"),
    vpnVersions: (type) => req("GET", `/api/vpn-types/${type}/versions`),
    system: () => req("GET", "/api/system"),
    preflight: (vpnType, version, scope) =>
      req("GET", `/api/preflight?scope=${scope || "preflight"}${vpnType ? "&vpn_type=" + encodeURIComponent(vpnType) : ""}${version ? "&version=" + encodeURIComponent(version) : ""}`),
    preflightFix: (action, params) => req("POST", `/api/preflight/fix/${action}`, params || {}),
    preflightFixStatus: (taskId) => req("GET", `/api/preflight/fix/${taskId}`),
    images: () => req("GET", "/api/images"),
    create: (data) => req("POST", "/api/channels", data),
    update: (id, data) => req("PATCH", `/api/channels/${id}`, data),
    login: (id) => req("GET", `/api/channels/${id}/login`),
    upload: async (id, file) => {
      const fd = new FormData();
      fd.append("file", file, file.name);
      const r = await fetch(`/api/channels/${id}/upload`, { method: "POST", body: fd });
      if (!r.ok) throw new Error(`${r.status} ${await r.text().catch(() => "")}`.trim());
      const ct = r.headers.get("content-type") || "";
      return ct.includes("application/json") ? r.json() : r.text();
    },
    status: (id) => req("GET", `/api/channels/${id}/status`),
    addRules: (id, patterns, kind) =>
      req("POST", `/api/channels/${id}/rules`, kind ? { patterns, kind } : { patterns }),
    delRule: (id, rid) => req("DELETE", `/api/channels/${id}/rules/${rid}`),
    toggleRule: (id, rid, enabled) =>
      req("PATCH", `/api/channels/${id}/rules/${rid}`, { enabled }),
    start: (id) => req("POST", `/api/channels/${id}/start`),
    stop: (id) => req("POST", `/api/channels/${id}/stop`),
    remove: (id) => req("DELETE", `/api/channels/${id}`),
    logs: (id, tail) => req("GET", `/api/channels/${id}/logs?tail=${tail || 200}`),
    connections: () => req("GET", "/api/connections"),
    proxies: () => req("GET", "/api/proxies"),
    entrySetup: () => req("GET", "/api/entry/setup-commands"),
    snippet: () => req("GET", "/api/clash-snippet"),
    mirrors: () => req("GET", "/api/mirrors"),
    addMirror: (host) => req("POST", "/api/mirrors", { host }),
    patchMirror: (id, body) => req("PATCH", `/api/mirrors/${id}`, body),
    delMirror: (id) => req("DELETE", `/api/mirrors/${id}`),
    testMirror: (host) => req("POST", "/api/mirrors/test", { host }),
  };
})();
