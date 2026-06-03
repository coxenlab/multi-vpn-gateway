/* 共享交互层 —— 状态徽章、复制、toast、tab、侧栏、抽屉、迷你图。
 * 每屏 <body data-page="..."> 决定侧栏高亮。 */
(function () {
  "use strict";
  const $ = (s, r = document) => r.querySelector(s);
  const $$ = (s, r = document) => Array.from(r.querySelectorAll(s));
  window.$ = $; window.$$ = $$;

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
    t.innerHTML = (icon ? CHECK : "") + `<span>${msg}</span>`;
    host().appendChild(t);
    setTimeout(() => { t.style.opacity = "0"; t.style.transform = "translateY(8px)"; }, 2400);
    setTimeout(() => t.remove(), 2700);
  };

  /* ── 复制到剪贴板 ── */
  window.copyText = function (text, label) {
    const done = () => toast((label || "已复制") + " ✓");
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).then(done, done);
    } else {
      const ta = document.createElement("textarea");
      ta.value = text; document.body.appendChild(ta); ta.select();
      try { document.execCommand("copy"); } catch (e) {}
      ta.remove(); done();
    }
  };

  /* ── 侧栏：高亮 + 移动端抽屉 ── */
  function initShell() {
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
        $$(".tabpanel", scope).forEach((p) => p.classList.toggle("active", p.dataset.panel === tab.dataset.tab));
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
