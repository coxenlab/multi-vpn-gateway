import { api } from "../api.js";
import { $, badgeHTML, kindMeta, toast } from "../app.js";
import { fb } from "../feedback.js";
import { loadWithSystem } from "../page-data.js";

    let chs = [];
    let sys = {};
    let loaded = false;   // 首次加载成功过？决定失败时给整页错误条 or 静默轮询提示
    const ruleCount = (c) => (c.domains || []).length + (c.ips || []).length;
    const routingControlsSupported = () => typeof sys.routing_off === "boolean";
    const routingOn = (c) => !routingControlsSupported() || (c.routing_enabled !== false && c.routing_enabled !== 0);
    const channelBadge = (c) => {
      if (!routingOn(c) && c.status === "logged_in")
        return `<span class="badge is-logged_in"><i class="bdot"></i>已连接 · 不分流</span>`;
      return badgeHTML(c.status) + (!routingOn(c) ? `<span class="badge is-stopped">不分流</span>` : "");
    };

    // 通道列表与系统状态不绑死:/api/system 要探分流口,链路故障时它慢/失败,
    // 而通道列表是本屏主体、拿到就该显示(2026-08-04 事故:二者捆在 Promise.all 里整页停在骨架屏)。
    async function fetchAll() {
      ({ data: chs, system: sys } = await loadWithSystem());
      paint();
      loaded = true;
    }

    async function load(isPoll) {
      if (!loaded && !isPoll) {
        const box = $("#channel-list");
        box.innerHTML = "";
        box.appendChild(fb.skeleton(5));
      }
      try {
        await fetchAll();
      } catch (e) {
        if (isPoll) {
          toast("刷新失败，将自动重试", { variant: "danger", action: { label: "立即重试", onClick: () => load(false) } });
          return;
        }
        const box = $("#channel-list");
        box.innerHTML = "";
        fb.errorBanner(box, { fromError: e, onRetry: () => load(false), retryLabel: "重新加载" });
      }
    }

    function actionsFor(c) {
      const detail = `<a class="btn btn-sm btn-secondary spacer" href="channel.html?id=${fb.esc(c.id)}">详情</a>`;
      if (c.status === "stopped")
        return `<button class="btn btn-sm btn-primary" data-action="start" data-id="${fb.esc(c.id)}">启动</button>${detail}`;
      const headless = c.login_method === "headless";
      const login = headless ? ""
        : (c.status === "logged_in"
            ? `<a class="btn btn-sm btn-secondary" href="channel.html?id=${fb.esc(c.id)}#login">重新登录</a>`
            : `<a class="btn btn-sm btn-primary" href="channel.html?id=${fb.esc(c.id)}#login">登录</a>`);
      return `${login}
        <button class="btn btn-sm btn-secondary" data-action="probe" data-id="${fb.esc(c.id)}">检测连通</button>
        <button class="btn btn-sm btn-secondary" data-action="stop" data-id="${fb.esc(c.id)}">停止</button>
        ${detail}`;
    }
    function cardHTML(c) {
      const k = kindMeta(c.vpn_type);
      const lat = (c.status === "logged_in" && c.latency_ms != null) ? `${c.latency_ms} ms` : "—";   // 已停止 / 待登录时旧延迟没有意义
      const tags = [];
      (c.domains || []).forEach(d => tags.push(`<span class="tag mono${d.enabled ? "" : " off"}">${fb.esc(d.pattern)}</span>`));
      (c.ips || []).forEach(d => tags.push(`<span class="tag mono ip${d.enabled ? "" : " off"}">${fb.esc(d.pattern)}</span>`));
      const more = tags.length > 6 ? `<span class="tag">+${tags.length - 6}</span>` : "";
      const doms = tags.length ? tags.slice(0, 6).join("") + more : `<span class="t-sm muted">未绑定规则</span>`;
      const host = (c.server || "").replace(/^https?:\/\//, "");
      return `<article class="ch-card" data-od-id="ch-${c.id}">
        <div class="ch-top">
          <div class="grow">
            <div class="ch-name">${fb.esc(c.name)}</div>
            <div class="ch-meta">${fb.esc(k.label)}${host ? " · " + fb.esc(host) : ""}</div>
          </div>
          ${channelBadge(c)}
        </div>
        <div class="ch-grid">
          <div class="kv"><div class="k">延迟</div><div class="v">${lat}</div></div>
          <div class="kv"><div class="k">运行时长</div><div class="v">${c.uptime != null ? fb.esc(c.uptime) : "—"}</div></div>
          <div class="kv"><div class="k">分流规则</div><div class="v">${ruleCount(c)} 条</div></div>
        </div>
        <div class="ch-domains">${doms}</div>
        <div class="ch-actions">${actionsFor(c)}</div>
      </article>`;
    }

    function paint() {
      const box = $("#channel-list");
      box.innerHTML = chs.length ? chs.map(cardHTML).join("")
        : `<div class="empty card card-pad" style="grid-column:1/-1;">还没有通道。点右上角「新建通道」接入第一家客户的 VPN。</div>`;
      $("#s-total").textContent = chs.length;
      $("#s-ok").textContent = chs.filter(c => c.status === "logged_in").length;
      $("#s-wait").textContent = chs.filter(c => c.status === "running" || c.status === "starting").length;
      $("#s-dom").textContent = chs.reduce((n, c) => n + ruleCount(c), 0);
      $("#nav-count").textContent = chs.length;
    }

    // 检测连通:唯一判据是后端经容器 SOCKS5 访问验证地址(不认 VNC 连上)。
    async function probe(id, btn) {
      const reBtn = () => document.querySelector(`[data-od-id="ch-${id}"] button[data-action="probe"]`);
      if (btn) { btn.disabled = true; btn.replaceChildren(fb.spinner("检测中…")); }
      try {
        const r = await api.status(id);
        await load();
        if (r.connected) toast(`已连通 · ${r.latency_ms ?? "?"} ms`, { variant: "success" });
        else toast("未连通：请先在登录窗口完成登录", {
          variant: "danger", action: { label: "重试", onClick: () => probe(id, reBtn()) },
        });
      } catch (e) {
        toast("检测失败：" + fb.friendlyError(e).title, {
          variant: "danger", action: { label: "重试", onClick: () => probe(id, reBtn()) },
        });
      } finally { if (btn) { btn.disabled = false; btn.textContent = "检测连通"; } }
    };
    // 启动(后端重建容器,复用同卷 / MAC)/ 停止。成功后 load() 重绘卡片。
    async function power(id, action, btn) {
      const label = action === "start" ? "启动" : "停止";
      btn.disabled = true;
      btn.replaceChildren(fb.spinner(label + "中…"));
      try {
        await (action === "start" ? api.start(id) : api.stop(id));
        await load();
        toast(action === "start" ? "已启动，请登录后检测连通" : "已停止", { variant: "success" });
      } catch (e) {
        toast(label + "失败：" + fb.friendlyError(e).title, {
          variant: "danger", action: { label: "重试", onClick: () => power(id, action, btn) },
        });
        btn.disabled = false; btn.textContent = label;
      }
    };

    $("#channel-list").addEventListener("click", (event) => {
      const button = event.target.closest("button[data-action]");
      if (!button || button.disabled) return;
      const { id, action } = button.dataset;
      if (action === "probe") probe(id, button);
      else if (action === "start" || action === "stop") power(id, action, button);
    });
    load();
    api.poll(() => load(true), 8000, { immediate: false });
  
