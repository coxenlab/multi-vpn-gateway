import { api } from "./api.js";
/* 共享交互层 —— 状态徽章、复制、toast、tab、侧栏、抽屉、迷你图。
 * 每屏 <body data-page="..."> 决定侧栏高亮。 */
(function () {
  "use strict";
  const $ = (s, r = document) => r.querySelector(s);
  const $$ = (s, r = document) => Array.from(r.querySelectorAll(s));
  window.$ = $; window.$$ = $$;

  /* noVNC 就绪探测:EC/aTrust 容器内 noVNC web(8080)在 status:running 后还要数十秒才起
   * (Xvfb/x11vnc/GUI 链启动慢)。iframe 若早于服务就绪就 set src → 连接被拒 → 永久白屏,
   * 且向导/详情不会自动重载它。这里先用跨源 no-cors fetch 探到 origin 在伺服再塞 src:服务起着
   * → resolve(opaque),连接被拒 → reject。单次请求与整体等待都有上限;离开视图立即取消,
   * 超时返回 false,由页面提供重试。 */
  window.waitNovncReady = async function (url, maxSec = 60, { signal } = {}) {
    const deadline = Date.now() + maxSec * 1000;
    while (Date.now() < deadline && !signal?.aborted) {
      const request = new AbortController();
      const abort = () => request.abort();
      signal?.addEventListener("abort", abort, { once: true });
      const timer = setTimeout(abort, Math.min(5000, deadline - Date.now()));
      try {
        await fetch(url, { mode: "no-cors", cache: "no-store", signal: request.signal });
        return !signal?.aborted;
      } catch (_) { /* 有界重试；离开登录视图立即取消。 */ }
      finally { clearTimeout(timer); signal?.removeEventListener("abort", abort); }
      if (signal?.aborted) return false;
      await new Promise(resolve => {
        const done = () => { clearTimeout(delay); signal?.removeEventListener("abort", done); resolve(); };
        const delay = setTimeout(done, Math.min(2000, Math.max(0, deadline - Date.now())));
        signal?.addEventListener("abort", done, { once: true });
      });
    }
    return false;
  };

  /* ── 状态机文案/样式映射 ── */
  const STATUS = {
    created:   { label: "已创建", cls: "is-created" },
    starting:  { label: "启动中", cls: "is-starting", pulse: true },
    running:   { label: "待登录", cls: "is-running" },
    logged_in: { label: "已连接", cls: "is-logged_in" },
    down:      { label: "掉线",   cls: "is-down" },
    stopped:   { label: "已停止", cls: "is-stopped" },
    error:     { label: "出错",   cls: "is-error" },
  };
  window.statusMeta = (s) => STATUS[s] || STATUS.created;
  window.badgeHTML = (s) => {
    const m = statusMeta(s);
    return `<span class="badge ${m.cls}"><i class="bdot ${m.pulse ? "pulse" : ""}"></i>${m.label}</span>`;
  };
  const KIND_LABEL = {
    easyconnect: "EasyConnect", atrust: "aTrust",
    anyconnect: "Cisco AnyConnect", globalprotect: "GlobalProtect",
    "fortinet-oc": "Fortinet (openconnect)", juniper: "Juniper / Pulse",
    pulse: "Pulse / Ivanti", openfortivpn: "openfortivpn",
    openvpn: "OpenVPN", wireguard: "WireGuard", custom: "自定义 / BYO",
  };
  // cls 仅决定标签配色(只有 ec/atrust 有专属色),其余沿用 ec 中性蓝;label 取准确名。
  window.kindMeta = (t) => ({
    cls: t === "atrust" ? "atrust" : "ec",
    label: KIND_LABEL[t] || t || "未知",
  });

  /* ── toast ── */
  function host() {
    let h = $(".toast-host");
    if (!h) { h = document.createElement("div"); h.className = "toast-host"; document.body.appendChild(h); }
    return h;
  }
  const CHECK = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2"><path d="M20 6L9 17l-5-5"/></svg>';
  window.toast = function (msg, icon = true) {
    const t = document.createElement("div");
    t.className = "toast";
    // msg 可能夹带动态文案:内联最小 5 字符转义(app.js 早于 feedback.js 加载,取不到 fb.esc);CHECK 图标是可信 HTML,不转义
    const safe = String(msg == null ? "" : msg).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
    t.innerHTML = (icon ? CHECK : "") + `<span>${safe}</span>`;
    host().appendChild(t);
    setTimeout(() => { t.style.opacity = "0"; t.style.transform = "translateY(8px)"; }, 2400);
    setTimeout(() => t.remove(), 2700);
  };

  /* ── 复制到剪贴板 ── */
  window.copyText = function (text, label) {
    const ok = () => toast((label || "已复制") + " ✓");
    const fail = () => toast("复制失败,请手动选择后复制", { variant: "danger" });
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).then(ok, fail);
    } else {
      const ta = document.createElement("textarea");
      ta.value = text; document.body.appendChild(ta); ta.select();
      let copied = false;
      try { copied = document.execCommand("copy"); } catch (e) { copied = false; }
      ta.remove();
      copied ? ok() : fail();
    }
  };

  /* ── 侧栏:全站唯一一份,由这里渲染进每页的空 <aside class="sidebar">;
   *    页面只负责 <body data-page> 高亮。底部状态灯由 /api/system 驱动,不再写死。 ── */
  const NAV = [
    ["channels", "index.html", "通道", '<path d="M12 3l9 5-9 5-9-5 9-5z"/><path d="M3 13l9 5 9-5"/>'],
    ["routing", "routing-table.html", "分流规则", '<path d="M3 6h.01M3 12h.01M3 18h.01M8 6h13M8 12h13M8 18h13"/>'],
    ["monitor", "monitor.html", "流量监控", '<path d="M3 12h4l3 8 4-16 3 8h4"/>'],
    ["system", "clash-config.html", "设置", '<circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.7 1.7 0 00.3 1.8l.1.1a2 2 0 11-2.8 2.8l-.1-.1a1.7 1.7 0 00-1.8-.3 1.7 1.7 0 00-1 1.5V21a2 2 0 11-4 0v-.1a1.7 1.7 0 00-1.1-1.5 1.7 1.7 0 00-1.8.3l-.1.1a2 2 0 11-2.8-2.8l.1-.1a1.7 1.7 0 00.3-1.8 1.7 1.7 0 00-1.5-1H3a2 2 0 110-4h.1a1.7 1.7 0 001.5-1.1 1.7 1.7 0 00-.3-1.8l-.1-.1a2 2 0 112.8-2.8l.1.1a1.7 1.7 0 001.8.3H9a1.7 1.7 0 001-1.5V3a2 2 0 114 0v.1a1.7 1.7 0 001 1.5 1.7 1.7 0 001.8-.3l.1-.1a2 2 0 112.8 2.8l-.1.1a1.7 1.7 0 00-.3 1.8V9a1.7 1.7 0 001.5 1H21a2 2 0 110 4h-.1a1.7 1.7 0 00-1.5 1z"/>'],
  ];
  function renderSidebar() {
    const aside = $(".sidebar");
    if (!aside || aside.children.length) return;
    const ic = (d) => `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8">${d}</svg>`;
    aside.innerHTML = `
      <div class="sidebar-logo">
        <span class="logo-mark">${ic('<path d="M12 2l8 3v6c0 5-3.5 8-8 11-4.5-3-8-6-8-11V5l8-3z"/>')}</span>
        <span class="logo-text">VPN 管理网关</span>
      </div>
      <nav class="sidebar-nav">
        ${NAV.map(([k, href, label, d]) => `<a class="nav-item" data-nav="${k}" href="${href}">${ic(d)}${label}${k === "channels" ? ' <span class="count" id="nav-count">—</span>' : ""}</a>`).join("")}
      </nav>
      <div class="sidebar-foot">
        <div class="sys-chip" id="shell-chip" style="width:100%; justify-content:flex-start;">
          <span class="dot"></span><span>正在检测…</span><span class="k spacer" id="foot-port"></span>
        </div>
      </div>`;
  }
  /* 底部端口号;状态灯文字与颜色由 feedback.js 的 syncChip 随 /api/system 轮询同步(全站一处) */
  async function refreshShellPort() {
    const fp = $("#foot-port"); if (!fp || !window.api) return;
    try { const s = await api.system(); fp.textContent = s.mihomo_port ? ":" + s.mihomo_port : ""; } catch (_) {}
  }
  // 脚本挂在 body 末尾,此刻 <aside> 已在 DOM:立即渲染,免得各页脚本在 DOMContentLoaded 前就写 #nav-count / #foot-port 时找不到节点。
  renderSidebar();
  function initShell() {
    refreshShellPort();
    const page = document.body.dataset.page;
    if (page) $$(".nav-item[data-nav]").forEach((a) => a.classList.toggle("active", a.dataset.nav === page));
    const app = $(".app");
    $$(".menu-btn").forEach((b) => b.addEventListener("click", () => app && app.classList.toggle("nav-open")));
    if (app) app.addEventListener("click", (e) => {
      // 点遮罩（::after 区域之外的空白）关闭：点 sidebar 外即关
      if (app.classList.contains("nav-open") && !e.target.closest(".sidebar") && !e.target.closest(".menu-btn"))
        app.classList.remove("nav-open");
    });
  }

  /* ── tabs ── */
  function initTabs() {
    $$("[data-tabs]").forEach((group) => {
      const tabs = $$(".tab", group);
      tabs.forEach((tab) => tab.addEventListener("click", () => {
        tabs.forEach((t) => t.classList.remove("active"));
        tab.classList.add("active");
        const scope = group.closest("[data-tabscope]") || document;
        // 只切当前 scope 的直属 panel；合并页允许 panel 内再嵌一组 tabs。
        Array.from(scope.children)
          .filter((p) => p.classList && p.classList.contains("tabpanel"))
          .forEach((p) => p.classList.toggle("active", p.dataset.panel === tab.dataset.tab));
      }));
    });
  }

  /* ── 抽屉/弹窗 ── */
  window.openOverlay = (id) => { const o = document.getElementById(id); if (o) o.classList.add("open"); };
  window.closeOverlay = (id) => { const o = document.getElementById(id); if (o) o.classList.remove("open"); };
  function initOverlays() {
    $$(".overlay").forEach((o) => o.addEventListener("click", (e) => { if (e.target === o) o.classList.remove("open"); }));
    $$("[data-open]").forEach((b) => b.addEventListener("click", () => openOverlay(b.dataset.open)));
    $$("[data-close]").forEach((b) => b.addEventListener("click", () => closeOverlay(b.dataset.close)));
    document.addEventListener("keydown", (e) => { if (e.key === "Escape") $$(".overlay.open").forEach((o) => o.classList.remove("open")); });
  }

  /* ── 迷你图：把数组转成 SVG 折线 / 面积 path ── */
  window.linePath = function (vals, w, h, pad = 2) {
    const max = Math.max(...vals), min = Math.min(...vals), span = max - min || 1;
    const step = (w - pad * 2) / (vals.length - 1);
    return vals.map((v, i) => {
      const x = (pad + i * step).toFixed(1);
      const y = (pad + (h - pad * 2) * (1 - (v - min) / span)).toFixed(1);
      return (i ? "L" : "M") + x + " " + y;
    }).join(" ");
  };
  window.areaPath = function (vals, w, h, pad = 2) {
    return linePath(vals, w, h, pad) + ` L${(w - pad).toFixed(1)} ${h} L${pad} ${h} Z`;
  };

  /* ── 延迟 → 颜色档 ── */
  window.latClass = (ms) => (ms == null ? "" : ms < 60 ? "ok" : ms < 120 ? "warn" : "bad");

  /* ── 批量切分：一次粘多条，按空格 / 逗号 / 分号 / 换行分隔 ── */
  window.parseTokens = (raw) =>
    String(raw || "").split(/[\s,;]+/).map((t) => t.trim()).filter(Boolean);

  /* ── IP / CIDR 规范化 + 校验 ──
   * 裸 IPv4 → /32，裸 IPv6 → /128；带掩码则校验范围。非法返回 null。
   * 这是 IP 分流的唯一入口，三屏共用，保证写入 ips 的 pattern 始终是合法 CIDR。 */
  window.normIp = function (raw) {
    const s = String(raw || "").trim();
    if (!s) return null;
    const slash = s.indexOf("/");
    const addr = slash === -1 ? s : s.slice(0, slash);
    const maskRaw = slash === -1 ? null : s.slice(slash + 1);
    if (maskRaw !== null && !/^\d+$/.test(maskRaw)) return null;   // 掩码须为纯数字，拒绝空掩码 "x/" 等
    if (s.includes(":")) {
      // IPv6：最多一个 ::、无 :::、首尾不得是裸冒号；每段 1–4 位十六进制；段数对齐
      if (addr.includes(":::") || (addr.match(/::/g) || []).length > 1) return null;
      if ((addr.startsWith(":") && !addr.startsWith("::")) ||
          (addr.endsWith(":") && !addr.endsWith("::"))) return null;
      const hasDC = addr.includes("::");
      const groups = addr.split(":").filter((g) => g !== "");
      if (groups.some((g) => !/^[0-9a-fA-F]{1,4}$/.test(g))) return null;
      if (hasDC ? groups.length > 7 : groups.length !== 8) return null;
      const m = maskRaw === null ? 128 : Number(maskRaw);
      if (!Number.isInteger(m) || m < 0 || m > 128) return null;
      return addr + "/" + m;
    }
    const oct = addr.split(".");
    if (oct.length !== 4 || oct.some((o) => !/^\d{1,3}$/.test(o) || +o > 255)) return null;
    const m = maskRaw === null ? 32 : Number(maskRaw);
    if (!Number.isInteger(m) || m < 0 || m > 32) return null;
    return addr + "/" + m;
  };

  /* ── 看起来像 IP 吗？（用于自动识别：纯数字/点[/掩码] 或含冒号的 v6）
   *    斜杠后允许零位数字，好让 "1.2.3.4/" 这类残缺输入也走 IP 路径、由 normIp 统一判废。 ── */
  window.looksLikeIp = (t) => /^[0-9.]+(\/\d*)?$/.test(t) || t.includes(":");

  document.addEventListener("DOMContentLoaded", () => {
    initShell(); initTabs(); initOverlays();
  });
})();

export const { $, $$, statusMeta, badgeHTML, kindMeta, copyText, openOverlay, closeOverlay, linePath, areaPath, latClass, parseTokens, normIp, looksLikeIp, waitNovncReady } = window;
export const toast = (...args) => window.toast(...args);
