/* 全站共享「反馈层」—— 纯增量,挂 window.fb.*。
 * 依赖加载顺序:必须在 app.js 之后引入(本文件包一层 app.js 的 window.toast)。
 * 设计系统:Neutral Modern,颜色/间距一律取 css/app.css :root token,无裸 hex;
 * 动画克制(fade/slide,150-250ms ease)。命门只加反馈,不碰 /api 语义、状态机、端口、凭据。 */
(function () {
  "use strict";

  /* ── 小图标(stroke SVG,非 emoji) ── */
  const SVG = {
    check: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4"><path d="M20 6L9 17l-5-5"/></svg>',
    cross: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4"><path d="M18 6L6 18M6 6l12 12"/></svg>',
    spin:  '<svg class="fb-spin" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4"><path d="M21 12a9 9 0 1 1-6.2-8.6"/></svg>',
  };
  const esc = (s) =>
    String(s == null ? "" : s).replace(/[&<>"']/g, (c) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));

  /* ── toast 包一层:兼容旧签名 toast(msg, icon=true),并支持 toast(msg, {action}) ──
   * 旧调用 toast("x") / toast("x", false) 行为不变;新增 {variant, action:{label,onClick}, icon}。 */
  const baseToast = window.toast;   // app.js 已定义的原版(可能为 undefined,做兜底)
  function tHost() {
    let h = document.querySelector(".toast-host");
    if (!h) { h = document.createElement("div"); h.className = "toast-host"; document.body.appendChild(h); }
    return h;
  }
  function richToast(msg, opt) {
    opt = opt || {};
    const variant = opt.variant || "default";          // default | success | danger | info
    const wantIcon = opt.icon !== false;
    const t = document.createElement("div");
    t.className = "toast" + (variant !== "default" ? " toast-" + variant : "");
    let icon = "";
    if (wantIcon) icon = `<span class="toast-ic">${variant === "danger" ? SVG.cross : SVG.check}</span>`;
    let act = "";
    if (opt.action && opt.action.label)
      act = `<button class="toast-act" type="button">${esc(opt.action.label)}</button>`;
    t.innerHTML = icon + `<span class="toast-msg">${esc(msg)}</span>` + act;
    tHost().appendChild(t);
    let gone = false;
    const dismiss = () => {
      if (gone) return; gone = true;
      t.style.opacity = "0"; t.style.transform = "translateY(8px)";
      setTimeout(() => t.remove(), 300);
    };
    if (act) {
      t.querySelector(".toast-act").addEventListener("click", () => {
        try { opt.action.onClick(); } finally { dismiss(); }
      });
    }
    // 带操作的 toast 多留时间(给用户点重试),普通 2.4s
    setTimeout(dismiss, opt.action ? 6000 : 2400);
    return { dismiss };
  }
  // 覆盖 window.toast:旧入参(string/boolean)走原版逻辑,对象入参走富 toast。
  window.toast = function (msg, opt) {
    if (opt === undefined || typeof opt === "boolean") {
      if (typeof baseToast === "function") return baseToast(msg, opt);
      return richToast(msg, { icon: opt !== false });
    }
    return richToast(msg, opt);
  };

  /* ── friendlyError:把 api.js 抛的结构化 Error 映射成中文人话 + 可操作建议 ──
   * 返回 {title, message, detail, hint}。title=一行人话;message=补充;detail=技术原文(折叠);hint=下一步建议。 */
  function friendlyError(err) {
    const e = err || {};
    const status = e.status;
    const reason = String(e.reason || e.message || "").trim();
    const rawLow = (String(e.raw || e.reason || e.message || "")).toLowerCase();
    const detail = e.raw || e.message || (reason || "未知错误");

    const has = (kw) => rawLow.includes(kw);

    // 0) 底座降级/中断优先(gatewayMonitor 轮询维护 window.fb.gatewayHealth):
    //    docker 通道死时一切容器操作都会失败,按根因提示,别误报成「镜像拉取失败」等表象。
    const gh = window.fb && window.fb.gatewayHealth;
    if (gh === "transport_dead" || gh === "transport_degraded" || gh === "vm_down") {
      return F("底座连接中断,容器操作暂不可用",
        gh === "vm_down" ? "本地引擎(VM)不可达,通道管理操作都会失败。"
                         : "宿主到 VM 的连接通道中断,看门狗正在自愈;现有分流不受影响。",
        gh === "vm_down" ? "重新打开 app 会自动重建底座后再重试。"
                         : "等顶部横幅消失(约 1 分钟内自动恢复)后重试;若持续,重开 app 会自动修复。", detail);
    }

    // 1) 语义关键词优先(后端原文里的特征串)
    if (has("colima") ||
        (has("docker") && (has("not running") || has("cannot connect") || has("daemon") || has("refused") || has("connection refused")))) {
      return F("Docker 引擎未就绪", "底层容器引擎没起来或连不上,通道无法运行。",
        "去「环境诊断」跑一次体检,按提示启动 Docker / 引擎。", detail);
    }
    if (has("pull") || has("manifest") || has("registry") || has("toomanyrequests") ||
        has("imagenotfound") || (has("image") && has("not found"))) {
      return F("镜像拉取失败", "从镜像仓库下载镜像没成功,可能是网络或仓库限流。",
        "稍后重拉;若反复失败,去「环境诊断 → 镜像源」换一个国内镜像源。", detail);
    }
    if (has("exited") || has("exit code 1") || has("exited (1)") || has("oci runtime") || has("crash")) {
      return F("容器启动后退出", "容器起来了又立刻崩退,通常是配置或镜像架构不匹配。",
        "查看该通道日志定位原因;arm64 机器需用原生 arm64 镜像。", detail);
    }
    if (has("novnc") || has("vnc") || has("websockify") || has("5901") || has("8080")) {
      return F("登录界面未就绪", "远程桌面(noVNC)还没起好,稍等几十秒再重载。",
        "等容器内桌面服务启动后点「重新加载」。", detail);
    }
    if (has("probe") || has("socks") || (has("connect") && has("timed out")) || has("unreachable") || has("no route")) {
      return F("内网未连通", "经 SOCKS5 探活没访问到目标,说明 VPN 还没真正连上。",
        "确认已在登录界面成功登录;凭据/服务器无误后重试探活。", detail);
    }
    if (has("port") && (has("in use") || has("already") || has("bind") || has("address already"))) {
      return F("端口被占用", "需要的本地端口已被别的进程占着。",
        "释放占用端口或重试;端口为随机高位,通常重试即可。", detail);
    }
    if (has("network") && (has("not found") || has("no such")) || has("vpn-net")) {
      return F("Docker 网络缺失", "通道所需的 Docker 网络还没建。",
        "去「环境诊断」点「创建网络」一键修复。", detail);
    }
    if (has("timed out") || has("timeout") || has("deadline")) {
      return F("操作超时", "等待响应超时,可能是网络慢或服务还在启动。",
        "稍后重试;若多次超时检查网络与 Docker 状态。", detail);
    }

    // 2) 状态码兜底
    if (status === 404) {
      return F("找不到对象", reason || "请求的资源不存在,可能已被删除。",
        "刷新页面后重试。", detail);
    }
    if (status === 400 || status === 422) {
      return F("请求有误", reason || "提交的参数不被接受。",
        "检查表单填写是否完整正确后重试。", detail);
    }
    if (status === 401 || status === 403) {
      return F("无权限", reason || "当前操作未被授权。", "确认登录状态后重试。", detail);
    }
    if (status === 409) {
      return F("状态冲突", reason || "当前状态下不能执行该操作。", "刷新后按最新状态重试。", detail);
    }
    if (status === 500 || status === 502 || status === 503 || status === 504) {
      return F("服务端出错", reason || "后端处理出错。",
        "稍后重试;若持续失败查看后端日志。", detail);
    }
    if (status === undefined && (e instanceof Error)) {
      // fetch 本身抛(断网/CORS),api.js 不包这类
      return F("网络请求失败", "请求没发出去或被中断,可能是网络问题。",
        "检查网络连接后重试。", detail);
    }

    // 3) 默认兜底:包装友好但保留原始信息
    return F("操作失败", reason || "发生了未预期的错误。", "重试一次;若反复出现请查看技术细节。", detail);
  }
  function F(title, message, hint, detail) { return { title, message, hint, detail }; }
  window.fb = window.fb || {};
  window.fb.esc = esc;             // 全站唯一 HTML 转义(&<>"' 五字符),各屏 innerHTML sink 共用,取代屏内阉割版
  window.fb.friendlyError = friendlyError;

  /* ── errorBanner:友好错误条(标题 + 说明 + 可折叠技术细节 + 重试按钮) ──
   * target: DOM 元素或选择器;opts: {title, message, detail, hint, onRetry, retryLabel, fromError}
   * 传 fromError(api Error)会自动经 friendlyError 填 title/message/hint/detail。
   * 返回 {el, remove()}。 */
  function errorBanner(target, opts) {
    opts = opts || {};
    const host = typeof target === "string" ? document.querySelector(target) : target;
    if (!host) return { el: null, remove() {} };
    let f = {};
    if (opts.fromError) f = friendlyError(opts.fromError);
    const title  = opts.title  || f.title  || "操作失败";
    const message= opts.message|| f.message|| "";
    const hint   = opts.hint   || f.hint   || "";
    const detail = opts.detail || f.detail || "";

    const el = document.createElement("div");
    el.className = "banner danger fb-errbanner";
    const detailId = "fbd-" + Math.random().toString(36).slice(2, 8);
    el.innerHTML = `
      <span class="bicon">${SVG.cross}</span>
      <div class="fb-eb-body">
        <div class="bt">${esc(title)}</div>
        ${message ? `<p>${esc(message)}</p>` : ""}
        ${hint ? `<p class="fb-eb-hint">${esc(hint)}</p>` : ""}
        ${detail ? `<button class="fb-eb-toggle" type="button" aria-expanded="false">技术细节</button>
          <pre class="fb-eb-detail" id="${detailId}" hidden>${esc(detail)}</pre>` : ""}
        ${opts.onRetry ? `<div class="fb-eb-acts"><button class="btn btn-secondary btn-sm fb-eb-retry" type="button">${esc(opts.retryLabel || "重试")}</button></div>` : ""}
      </div>`;
    if (opts.prepend && host.firstChild) host.insertBefore(el, host.firstChild);
    else host.appendChild(el);

    if (detail) {
      const tg = el.querySelector(".fb-eb-toggle");
      const pre = el.querySelector(".fb-eb-detail");
      tg.addEventListener("click", () => {
        const open = pre.hidden;
        pre.hidden = !open;
        tg.setAttribute("aria-expanded", String(open));
        tg.classList.toggle("open", open);
      });
    }
    const remove = () => el.remove();
    if (opts.onRetry) {
      el.querySelector(".fb-eb-retry").addEventListener("click", () => {
        remove();
        opts.onRetry();
      });
    }
    return { el, remove };
  }
  window.fb.errorBanner = errorBanner;

  /* ── stepper:多环节进度指示(待定/进行中/成功/失败+重试) ──
   * stepper(container, steps): steps = [{key, label}, ...]。
   * 返回 {el, setStep(key, state, msg?), reset()}。state ∈ pending | active | done | error。
   * error 态自动渲染「重试」按钮,点击触发该步注册的 onRetry(由 setStep 第三参为函数时绑定,
   *   或通过 steps[i].onRetry 预绑;msg 为字符串则作为状态文案)。 */
  function stepper(container, steps) {
    const host = typeof container === "string" ? document.querySelector(container) : container;
    const list = (steps || []).slice();
    const retryFns = {};   // key -> fn

    const el = document.createElement("div");
    el.className = "fb-stepper";
    el.innerHTML = list.map((s) => `
      <div class="fb-st-row" data-key="${esc(s.key)}" data-state="pending">
        <span class="fb-st-ic"><span class="fb-st-dot"></span></span>
        <div class="fb-st-body">
          <div class="fb-st-label">${esc(s.label)}</div>
          <div class="fb-st-msg"></div>
        </div>
        <div class="fb-st-act"></div>
      </div>`).join("");
    if (host) { host.innerHTML = ""; host.appendChild(el); }
    list.forEach((s) => { if (typeof s.onRetry === "function") retryFns[s.key] = s.onRetry; });

    const ICON = {
      pending: '<span class="fb-st-dot"></span>',
      active: SVG.spin,
      done: SVG.check,
      error: SVG.cross,
    };
    function setStep(key, state, msg) {
      const row = el.querySelector(`.fb-st-row[data-key="${CSS.escape(key)}"]`);
      if (!row) return;
      row.setAttribute("data-state", state);
      row.querySelector(".fb-st-ic").innerHTML = ICON[state] || ICON.pending;
      let text = "", onRetry = retryFns[key];
      if (typeof msg === "function") onRetry = msg;
      else if (msg != null) text = String(msg);
      row.querySelector(".fb-st-msg").textContent = text;
      const act = row.querySelector(".fb-st-act");
      if (state === "error" && onRetry) {
        act.innerHTML = `<button class="btn btn-secondary btn-sm" type="button">重试</button>`;
        act.querySelector("button").onclick = onRetry;
      } else {
        act.innerHTML = "";
      }
    }
    function reset() { list.forEach((s) => setStep(s.key, "pending")); }
    return { el, setStep, reset };
  }
  window.fb.stepper = stepper;

  /* ── spinner / skeleton 小工具 ── */
  window.fb.spinner = function (text) {
    const el = document.createElement("div");
    el.className = "fb-spinner";
    el.innerHTML = `<span class="fb-spinner-ic">${SVG.spin}</span>` + (text ? `<span>${esc(text)}</span>` : "");
    return el;
  };
  window.fb.skeleton = function (lines) {
    const n = Math.max(1, lines || 3);
    const el = document.createElement("div");
    el.className = "fb-skeleton";
    let h = "";
    for (let i = 0; i < n; i++) h += `<div class="fb-sk-line"${i === n - 1 ? ' style="width:60%"' : ""}></div>`;
    el.innerHTML = h;
    return el;
  };

  /* SVG 给 preflightPanel 等复用(同源克制图标) */
  window.fb.SVG = SVG;

  /* ── 分流口健康全局横幅 ──────────────────────────────────────────────────
   * 后端看门狗(/api/system 的 gateway_health)会在「容器全绿但宿主分流口不可达」
   * (lima 转发睡醒/抖动后静默丢失)时自动 restart mihomo 自愈;本横幅把这个过程讲清楚,
   * 并在自愈放弃/VM 断时给手动修复入口——解决「哪儿都连通就是打不开」的盲区。
   * 所有屏都引 feedback.js,故此处自启,全站一致。命门 #1 不破:不碰通道登录态。 */
  (function gatewayMonitor() {
    if (!window.api || !window.api.system) return;
    let host = null, wasBroken = false, healing = false;
    // 横幅可关:记住被收起的状态签名,同一状态不再弹;状态一变(新事故/恢复)自动复位。
    let dismissedKey = null, currentKey = null;

    function ensureHost() {
      if (host && document.body.contains(host)) return host;
      host = document.createElement("div");
      host.className = "fb-gw-host";
      host.style.cssText =
        "position:fixed;top:12px;left:50%;transform:translateX(-50%);z-index:90;" +
        "width:min(680px,calc(100vw - 32px));box-shadow:var(--shadow-md,0 6px 24px rgba(0,0,0,.12));";
      document.body.appendChild(host);
      return host;
    }
    function clear() {
      if (host) { host.remove(); host = null; }
    }
    function banner(variant, icon, title, msg, actionsHtml) {
      const h = ensureHost();
      h.innerHTML =
        `<div class="banner ${variant}" style="background:var(--surface);">
           <span class="bicon">${icon}</span>
           <div style="flex:1;min-width:0;">
             <div class="bt">${esc(title)}</div>
             ${msg ? `<p>${esc(msg)}</p>` : ""}
             ${actionsHtml || ""}
           </div>
           <button class="fb-gw-close" type="button" title="收起(状态变化时会再提醒)"
             style="border:0;background:none;cursor:pointer;font-size:16px;line-height:1;
                    color:var(--text-secondary,#6b7280);padding:2px 4px;align-self:flex-start;">×</button>
         </div>`;
      const c = h.querySelector(".fb-gw-close");
      if (c) c.addEventListener("click", () => { dismissedKey = currentKey; clear(); });
      return h;
    }
    function healBtn(label) {
      return `<div class="fb-eb-acts" style="margin-top:8px;display:flex;gap:8px;align-items:center;">
                <button class="btn btn-secondary btn-sm fb-gw-heal" type="button">${esc(label || "手动修复")}</button>
                <a class="btn btn-ghost btn-sm" href="env-check.html" style="text-decoration:none;">查看诊断</a>
              </div>`;
    }
    let healInFlight = false; // 修复进行中:挡住 10s 轮询重渲染(会把禁用按钮重置成可点 → 双发)

    function bindHeal() {
      const b = host && host.querySelector(".fb-gw-heal");
      if (!b) return;
      b.addEventListener("click", async () => {
        if (healInFlight) return;
        healInFlight = true;
        b.disabled = true;
        b.textContent = "修复中…";
        try {
          const r = await window.api.healProxy();
          if (r && r.reachable) window.toast("分流链路已修复");
          else window.toast(r && r.error ? r.error : "修复未生效,请重开 app", { variant: "danger" });
        } catch (e) {
          window.toast("修复失败:" + (e && e.message ? e.message : e), { variant: "danger" });
        }
        healInFlight = false;
        poll(); // 立刻复检刷新横幅
      });
    }

    /* 侧栏状态芯片真实化:原「mihomo 运行中」是写死的 HTML(诊断时误导),
     * 这里跟着每次 /api/system 轮询同步真实状态(全部屏共用此一处)。
     * 定性优先用 gateway_health(看门狗查容器+探口)——mihomo_status 走 ctrl 口探活,
     * 转发断时它也 down,单看它会把「链路中断」错报成「未运行」,与横幅自相矛盾。 */
    function syncChip(sys) {
      const chip = document.querySelector(".sidebar-foot .sys-chip");
      if (!chip) return;
      const txt = chip.querySelector("span:not(.dot):not(.k)");
      if (!sys) { chip.classList.remove("ok", "bad"); return; } // 后端未应答:置灰,不乱定性
      const gh = sys.gateway_health;
      const running = sys.mihomo_status === "running";
      let ok = false, label;
      if (gh === "forward_dead") label = "mihomo 分流链路中断";
      else if (gh === "container_down") label = "mihomo 未运行";
      else if (gh === "transport_dead") label = "底座传输中断,修复中";
      else if (gh === "transport_degraded") { ok = true; label = "分流可用(底座降级)"; }
      else if (gh === "vm_down") label = "底座(VM)连接断开";
      else { ok = running; label = running ? "mihomo 运行中" : "mihomo 未运行"; }
      chip.classList.toggle("ok", ok);
      chip.classList.toggle("bad", !ok);
      if (txt) txt.textContent = label;
      // 坏态芯片 = 常驻修复入口:点击重新展开横幅(横幅被收起后仍有地方点)。
      // 仅桌面版(有 gateway_health 字段、有 heal-proxy 端点)可点;web 版保持纯展示。
      chip._fbCanHeal = !ok && gh !== undefined;
      chip.style.cursor = chip._fbCanHeal ? "pointer" : "";
      chip.title = chip._fbCanHeal ? "点击查看修复选项" : "";
      if (!chip._fbHealBound) {
        chip._fbHealBound = true;
        chip.addEventListener("click", () => {
          if (!chip._fbCanHeal) return;
          dismissedKey = null;
          poll();
        });
      }
    }

    function render(sys) {
      syncChip(sys);
      const gh = sys && sys.gateway_health;
      window.fb.gatewayHealth = gh || null; // 供 friendlyError 按根因映射容器操作失败
      if (healInFlight) return; // 修复请求在途:别重绘横幅(保住「修复中…」禁用态)
      // 看门狗盲区:容器/转发口探测全绿(gateway_health=healthy),但 mihomo 控制口无应答
      // (转发器半僵死:TCP 能连、无响应)。芯片会红,横幅若不接手就无处可点(2026-07-16)。
      const ctrlDown = gh === "healthy" && sys && sys.mihomo_status !== "running";
      currentKey = (gh || "healthy") + "|" + (sys && sys.tunnel_fallback ? 1 : 0) + "|" + (ctrlDown ? 1 : 0);
      if (dismissedKey !== null && dismissedKey !== currentKey) dismissedKey = null; // 状态变化 → 复位收起记忆
      const dismissed = dismissedKey === currentKey;
      // 后端无此字段(老版本)或健康 → 收横幅;若刚从坏态恢复,提示一声。
      if (!gh || gh === "healthy") {
        if (ctrlDown) {
          wasBroken = true; healing = false;
          if (dismissed) { clear(); return; }
          banner("danger", SVG.cross, "分流路由(mihomo)无响应",
            "底座检测正常,但分流路由不应答(常见于端口转发半僵死)。点「手动修复」重启分流路由。", healBtn());
          bindHeal();
          return;
        }
        if (wasBroken) window.toast("分流链路已恢复");
        wasBroken = false; healing = false;
        if (sys && sys.tunnel_fallback && !dismissed) {
          // 降级态:分流口已由备援 SSH 隧道接管(底座转发服务僵死)。现有口可用,
          // 但新端口(如新建通道的登录窗)不会再被转发——持续提示,别让用户撞墙。
          banner("warn", SVG.check, "分流口已由备援隧道接管",
            "底座(VM)端口转发服务异常,已自动改走 SSH 隧道。现有通道不受影响;" +
            "新建通道的登录窗可能打不开。方便时重启电脑(重建底座 VM)可彻底修复。");
          return;
        }
        clear();
        return;
      }
      wasBroken = true;
      if (dismissed) { clear(); return; } // 用户已收起当前状态的横幅(芯片仍如实显示)
      if (gh === "vm_down") {
        healing = false;
        banner("danger", SVG.cross, "与本地引擎(VM)连接中断",
          "所有通道当前都不可达。请从托盘菜单退出并重新打开 app 以重建连接。", healBtn("重试修复"));
        bindHeal();
      } else if (sys.gave_up) {
        healing = false;
        banner("danger", SVG.cross, "分流口反复不可达,已暂停自动修复",
          "容器正常但浏览器/Clash 连不到分流口,通道当前都打不开。点「手动修复」重试,或查看诊断。", healBtn());
        bindHeal();
      } else if (gh === "container_down") {
        healing = false;
        banner("danger", SVG.cross, "分流路由(mihomo)未运行",
          "通道全部不可达。点「手动修复」重新拉起分流路由。", healBtn());
        bindHeal();
      } else if (gh === "forward_dead") {
        // 自愈进行中:只报进度、不给按钮(避免与看门狗重复重启)。
        healing = true;
        banner("warn", SVG.spin, "分流链路中断,正在自动修复…",
          "容器正常,但宿主分流口暂不可达(通常睡醒/网络抖动所致)。约 1 分钟内自动恢复,稍候。");
      } else if (gh === "transport_dead") {
        // 传输层(mux)坏死:VM/容器都正常,看门狗正在直上备援隧道(盲区 #3)。
        healing = true;
        banner("warn", SVG.spin, "底座传输中断,正在切换备援隧道…",
          "VM 与容器正常,但宿主到 VM 的连接通道断了(通常睡醒/网络切换所致)。正在自动改走备援隧道恢复分流,稍候。");
      } else if (gh === "transport_degraded") {
        // 降级稳态:分流口活着(备援隧道),容器管理不可用。重开 app 由 boot 自愈重建底座。
        healing = false;
        banner("warn", SVG.check, "分流已由备援隧道接管(降级运行)",
          "现有通道可正常使用;容器管理(新建/启停通道、登录窗)暂不可用。重新打开 app 将自动修复底座。");
      }
    }

    let busy = false;
    async function poll() {
      if (busy) return;
      busy = true;
      try {
        render(await window.api.system());
      } catch (_) {
        /* /api/system 都不通 = 整个 UI 后端挂了,交给各屏自身错误处理,这里不抢戏;
         * 只把芯片置灰,别让写死的初始绿点一直撒谎 */
        syncChip(null);
      } finally {
        busy = false;
      }
    }

    const start = () => { poll(); setInterval(poll, 10000); };
    if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", start);
    else start();
    window.fb.checkGateway = poll; // 供手动触发(如修复后)
  })();
})();
