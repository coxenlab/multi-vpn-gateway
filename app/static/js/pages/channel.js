import { api } from "../api.js";
import { $, $$, badgeHTML, kindMeta, copyText, closeOverlay, latClass, parseTokens, waitNovncReady, toast } from "../app.js";
import { fb } from "../feedback.js";
import { vncText } from "../vncText.js";
import { loadWithSystem } from "../page-data.js";
import { createVncLifecycle } from "../vnc-lifecycle.js";

    const params = new URLSearchParams(location.search);
    const wantId = params.get("id");
    let ch, sys;
    let k, kindLabel, containerId, hostName, methodLabel;
    const routingControlsSupported = () => sys && typeof sys.routing_off === "boolean";
    const routingOn = () => ch && (!routingControlsSupported() || (ch.routing_enabled !== false && ch.routing_enabled !== 0));
    const channelBadge = () => {
      if (!routingOn() && ch.status === "logged_in")
        return `<span class="badge is-logged_in"><i class="bdot"></i>已连接 · 不分流</span>`;
      return badgeHTML(ch.status) + (!routingOn() ? `<span class="badge is-stopped">不分流</span>` : "");
    };

    // initial=true:首屏加载,失败→骨架屏让位给可重试错误条;否则动作后的静默重取,失败抛给调用方。
    async function boot(initial = false) {
      if (initial) {
        $("#ch-feedback").innerHTML = "";
        $("#cfg-list").replaceChildren(fb.skeleton(6));
      }
      let list;
      try {
        ({ data: list, system: sys } = await loadWithSystem());
        ch = list.find(c => c.id === wantId) || list[0];
        if (!ch) {
          $("#cfg-list").innerHTML = "";
          fb.errorBanner("#ch-feedback", {
            title: "通道不存在", message: "没找到对应的通道，可能已被删除。",
            onRetry: () => { location.href = "index.html"; }, retryLabel: "返回通道列表",
          });
          return;
        }
      } catch (e) {
        if (initial) {
          $("#cfg-list").innerHTML = "";
          fb.errorBanner("#ch-feedback", { fromError: e, onRetry: () => boot(true) });
          return;
        }
        throw e;
      }
      $("#nav-count").textContent = list.length;
      k = kindMeta(ch.vpn_type);
      kindLabel = k.label + (ch.ec_ver ? " " + ch.ec_ver : "");
      containerId = "vpn-" + ch.id;
      hostName = (ch.server || "").replace(/^https?:\/\//, "");
      methodLabel = (ch.login_method === "headless" || ch.login_method === "password") ? "账号密码自动登录" : "在登录窗口手动登录";
      renderAll();
    }

    function renderAll() {
      $("#top-badge").innerHTML = channelBadge();
      $("#ch-title").textContent = ch.name;
      $("#ch-kind").innerHTML = `<span class="kind-tag ${k.cls}">${fb.esc(k.label)}</span>`;
      $("#ch-ident").textContent = hostName;
      renderConfig();
      renderHealth();
      loadNote();
      renderRuleTable();
      $("#del-name").textContent = ch.name;
      syncStatus();
      // 无头通道无登录窗口 → 隐藏「登录」tab
      const headless = ch.login_method === "headless";
      const loginTabBtn = document.querySelector('.tab[data-tab="login"]');
      if (loginTabBtn) loginTabBtn.style.display = headless ? "none" : "";
    }

    function rowsConfig() {
      return [
        ["VPN 网关", `<span class="mono">${fb.esc(hostName || "—")}</span>`],
        ["类型", fb.esc(kindLabel)],
        ["登录方式", methodLabel],
        ["账号", ch.username ? `<span class="mono">${fb.esc(ch.username)}</span>` : "—"],
        ["内网验证地址", `<span class="mono t-xs">${fb.esc(ch.probe_url)}</span>`],
        ["容器", `<span class="copyable"><span class="mono">${containerId}</span><button class="icon-btn icon-copy" id="copy-cid" type="button" aria-label="复制"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><rect x="9" y="9" width="11" height="11" rx="2"/><path d="M5 15V5a2 2 0 0 1 2-2h10"/></svg></button></span>`],
      ];
    }
    function renderConfig() {
      $("#cfg-list").innerHTML = rowsConfig().map(([dt, dd]) => `<dt>${dt}</dt><dd>${dd}</dd>`).join("");
      const cc = $("#copy-cid"); if (cc) cc.addEventListener("click", () => copyText(containerId, "已复制"));
    }

    function renderHealth() {
      const lat = ch.status === "logged_in" ? ch.latency_ms : null;   // 未连通时旧延迟没有意义
      $("#h-lat").innerHTML = (lat != null ? lat : "—") + ` <span class="unit">ms</span>`;
      const bar = $("#h-lat-bar"), cls = latClass(lat);
      bar.className = "lat-bar" + (cls ? " " + cls : "");
      bar.querySelector("i").style.width = lat != null ? Math.min(100, Math.round(lat / 1.8)) + "%" : "0";
      $("#h-status").innerHTML = badgeHTML(ch.status);
      $("#h-uptime").textContent = ch.uptime != null ? ch.uptime : "—";
    }
    function setLastProbe(when = Date.now()) { $("#h-last").textContent = new Date(when).toLocaleTimeString("zh-CN", { hour12: false }); }

    /* ── 登录备注(加密落库;旧后端无 /note → 卡片保持隐藏) ── */
    const noteCard = document.querySelector('[data-od-id="ch-note"]');
    let noteLoaded = false;
    async function loadNote() {
      if (noteLoaded) return;
      try {
        const { note } = await api.noteGet(ch.id);
        noteLoaded = true;
        const ta = $("#note-text");
        if (!ta.value) ta.value = note || "";
        noteCard.hidden = false;
      } catch (_e) { /* 旧后端无此端点 */ }
    }
    async function saveNote(silent = false) {
      try {
        await api.noteSet(ch.id, $("#note-text").value);
        $("#note-state").textContent = "已保存 " + new Date().toLocaleTimeString("zh-CN", { hour12: false });
        if (!silent) toast("已保存");
      } catch (e) {
        toast("保存失败：" + fb.friendlyError(e).title, { variant: "danger" });
      }
    }
    async function recordTyped(text) {
      if (!noteLoaded) await loadNote();
      if (!noteLoaded) return;
      const t = text.trim();
      const ta = $("#note-text");
      if (!t || ta.value.includes(t)) return;
      const ts = new Date().toLocaleString("zh-CN", { hour12: false });
      ta.value = (ta.value ? ta.value.replace(/\n*$/, "\n") : "") + `[${ts}] 键入：${t}`;
      await saveNote(true);
      toast("已记入登录备注");
    }
    $("#note-save").addEventListener("click", () => saveNote());
    $("#note-copy").addEventListener("click", () => {
      const head = [`通道：${ch.name}（${kindLabel}）`, `网关：${hostName}`, `账号：${ch.username || "—"}`].join("\n");
      const body = $("#note-text").value;
      copyText(head + (body ? "\n---\n" + body : ""), body ? "已复制（含备注，注意其中可能有密码）" : "已复制");
    });

    /* ── 登录 tab:真 noVNC iframe ── */
    let vncUrl = null;
    const vncStageTemplate = $("#vnc-stage").cloneNode(true);
    const vncView = createVncLifecycle({
      channelId: () => ch.id,
      isActive: () => ch && ch.status !== "stopped" && ch.login_method !== "headless"
        && document.querySelector('.tab[data-tab="login"]').classList.contains("active"),
      open: loadVnc,
      close: () => {
        $("#vnc-frame")?.replaceWith(vncStageTemplate.cloneNode(true));
        $("#vnc-sendbar")?.remove();
        $("#vnc-feedback").replaceChildren();
        vncUrl = null;
      },
    });
    async function loadVnc({ signal, current, login }) {
      const fbHost = $("#vnc-feedback");
      fbHost.replaceChildren(fb.spinner("正在打开登录窗口…"));
      try {
        const res = await login();
        if (!current()) return;
        if (res.login_mode === "headless" || !res.url) {
          fbHost.innerHTML = "";
          const stage = $("#vnc-stage");
          if (stage) stage.textContent = "该通道使用账号密码自动登录，没有登录窗口。是否连通以「检测连通」为准。";
          return;
        }
        const { url } = res;
        const spEl = fbHost.querySelector(".fb-spinner span:last-child");
        if (spEl) spEl.textContent = "等待登录界面就绪…";
        const mountVnc = (u) => {
          if (!current()) return;
          fbHost.innerHTML = "";
          let f = $("#vnc-frame");
          if (!f) {
            f = document.createElement("iframe");
            f.id = "vnc-frame"; f.title = "VPN 登录窗口"; f.style.cssText = "width:100%;height:80vh;min-height:640px;border:0;border-radius:8px;resize:vertical;overflow:auto;";
            const stage = $("#vnc-stage");
            if (stage) stage.replaceWith(f);
          }
          f.src = u;
          vncUrl = u;
          let sh = document.getElementById("vnc-sendbar");
          if (!sh) {
            sh = document.createElement("div");
            sh.id = "vnc-sendbar";
            document.querySelector('[data-od-id="vnc"]')?.insertAdjacentElement("afterend", sh);
          }
          vncText.mountBar(sh, () => vncUrl, recordTyped, { signal });
        };
        const ready = await waitNovncReady(url, 60, { signal });
        if (!current()) return;
        if (!ready) {
          // 就绪探测超时:不塞 iframe(早加载会白屏);后台继续等至多 5 分钟,就绪即自动加载
          fbHost.innerHTML = "";
          fb.errorBanner(fbHost, {
            title: "登录界面还在启动", message: "首次启动可能要一两分钟，就绪后会自动打开。",
            onRetry: vncView.open, retryLabel: "重新打开",
          });
          waitNovncReady(url, 300, { signal }).then((ok) => { if (ok && current()) mountVnc(url); });
          return;
        }
        mountVnc(url);
      } catch (e) {
        if (!current()) return;
        fbHost.innerHTML = "";
        fb.errorBanner(fbHost, { fromError: e, onRetry: vncView.open });
      }
    }

    function syncStatus() {
      const stopped = ch.status === "stopped";
      const headless = ch.login_method === "headless";
      $("#top-badge").innerHTML = channelBadge();
      $("#h-status").innerHTML = badgeHTML(ch.status);
      $("#act-power").textContent = stopped ? "启动" : "停止";
      // 容器已停:登录窗口与检测都无意义 → 只留「启动」+「删除」
      $("#act-probe").style.display = stopped ? "none" : "";
      $("#act-relogin").style.display = (stopped || headless) ? "none" : "";
      const routing = $("#act-routing");
      routing.hidden = !routingControlsSupported();
      routing.textContent = routingOn() ? "参与分流 · 开" : "参与分流 · 关";
      routing.setAttribute("aria-checked", String(routingOn()));
      vncView.sync();
    }

    function goLogin() {
      const loginTab = document.querySelector('.tab[data-tab="login"]');
      if (loginTab) loginTab.click();
    }
    $("#act-relogin").addEventListener("click", () => {
      const alreadyOpen = document.querySelector('.tab[data-tab="login"]').classList.contains("active");
      goLogin();
      if (alreadyOpen) vncView.open();
    });
    $("#vnc-reload").addEventListener("click", vncView.open);

    async function doProbe(btn) {
      const orig = btn ? btn.textContent : null;
      if (btn) { btn.textContent = "检测中…"; btn.disabled = true; }
      try {
        const r = await api.status(ch.id);
        await boot();
        setLastProbe(r.checked_at ?? Date.now());
        if (r.connected) toast(`已连通 · ${r.latency_ms ?? "?"} ms`, { variant: "success" });
        else toast("未连通：请先在登录窗口完成登录", { variant: "danger", action: { label: "重试", onClick: () => doProbe(btn) } });
      } catch (e) {
        toast("检测失败：" + fb.friendlyError(e).title, { variant: "danger", action: { label: "重试", onClick: () => doProbe(btn) } });
      } finally { if (btn) { btn.disabled = false; if (orig != null) btn.textContent = orig; } }
    }
    $("#act-probe").addEventListener("click", (e) => doProbe(e.currentTarget));

    /* ── 编辑连接信息 ── */
    function openEdit() {
      $("#e-name").value = ch.name || "";
      $("#e-server").value = ch.server || (ch.config && ch.config.server) || "";
      $("#e-username").value = ch.username || (ch.config && ch.config.username) || "";
      $("#e-password").value = "";
      $("#e-probe").value = ch.probe_url || "";
      $("#cfg-list").style.display = "none"; $("#cfg-edit").style.display = "none";
      $("#cfg-edit-form").style.display = "";
    }
    function closeEdit() {
      $("#cfg-edit-form").style.display = "none";
      $("#cfg-list").style.display = ""; $("#cfg-edit").style.display = "";
    }
    $("#cfg-edit").addEventListener("click", openEdit);
    $("#e-cancel").addEventListener("click", closeEdit);
    async function submitEdit() {
      const body = { name: $("#e-name").value, server: $("#e-server").value, username: $("#e-username").value, probe_url: $("#e-probe").value };
      const pw = $("#e-password").value;
      if (pw) body.password = pw;
      const connChanged = body.server !== (ch.server || "") ||
        body.username !== (ch.username || (ch.config && ch.config.username) || "") || !!pw;
      const btn = $("#e-save");
      btn.disabled = true; btn.textContent = connChanged ? "保存并重新连接…" : "保存中…";
      try {
        await api.update(ch.id, body);
        toast(connChanged ? "已保存，正在重新连接" : "已保存", { variant: "success" });
        location.reload();
      } catch (err) {
        toast("保存失败：" + fb.friendlyError(err).title, { variant: "danger", action: { label: "重试", onClick: submitEdit } });
        btn.disabled = false; btn.textContent = "保存";
      }
    }
    $("#cfg-edit-form").addEventListener("submit", (e) => { e.preventDefault(); submitEdit(); });

    async function doPower() {
      const btn = $("#act-power"), starting = ch.status === "stopped", orig = btn.textContent;
      btn.disabled = true; btn.textContent = starting ? "启动中…" : "停止中…";
      try {
        if (starting) {
          await api.start(ch.id); await boot();
          // 重建会换登录窗口地址:丢掉旧 iframe,再进登录 tab 时重新打开
          $("#vnc-frame")?.replaceWith(Object.assign(document.createElement("div"), { className: "vnc-stage", id: "vnc-stage", textContent: "登录窗口" }));
          $("#vnc-sendbar")?.remove();
          toast("已启动，请登录后检测连通", { variant: "success" });
        }
        else { await api.stop(ch.id); await boot(); toast("已停止", { variant: "success" }); }
      } catch (err) {
        toast((starting ? "启动失败：" : "停止失败：") + fb.friendlyError(err).title, { variant: "danger", action: { label: "重试", onClick: doPower } });
        btn.textContent = orig;
      } finally { btn.disabled = false; }
    }
    $("#act-power").addEventListener("click", doPower);
    $("#act-routing").addEventListener("click", async (e) => {
      const btn = e.currentTarget;
      const next = !routingOn();
      btn.disabled = true;
      try {
        await api.update(ch.id, { routing_enabled: next });
        await boot();
        toast(next ? "已恢复分流" : "已暂停分流，连接保持", { variant: "success" });
      } catch (err) {
        toast("切换失败：" + fb.friendlyError(err).title, { variant: "danger" });
        syncStatus();
      } finally { btn.disabled = false; }
    });

    async function doDelete() {
      const btn = $("#del-confirm"), orig = btn.textContent;
      btn.disabled = true; btn.textContent = "删除中…";
      try {
        await api.remove(ch.id);
        closeOverlay("del-modal");
        toast(`已删除 ${ch.name}`, { variant: "success" });
        setTimeout(() => location.href = "index.html", 900);
      } catch (e) {
        toast("删除失败：" + fb.friendlyError(e).title, { variant: "danger", action: { label: "重试", onClick: doDelete } });
        btn.disabled = false; btn.textContent = orig;
      }
    }
    $("#del-confirm").addEventListener("click", doDelete);

    /* ── 分流规则:域名与 IP 同一张表,后端自动识别类型 ── */
    let rules = [];
    function renderRuleTable() {
      rules = [...(ch.domains || []).map(d => ({ ...d, kind: "domain" })), ...(ch.ips || []).map(d => ({ ...d, kind: "ip" }))];
      const body = $("#rule-body");
      if (!rules.length) {
        body.innerHTML = `<tr><td colspan="4"><div class="empty" style="padding:var(--space-6);">还没有规则。绑定后，命中的域名或 IP 会走这条通道。</div></td></tr>`;
        return;
      }
      body.innerHTML = rules.map(d => `<tr data-id="${d.id}">
        <td class="mono">${fb.esc(d.pattern)}</td>
        <td class="muted">${d.kind === "ip" ? "IP / 网段" : "域名"}</td>
        <td><button class="switch ${d.enabled ? "on" : ""}" type="button" role="switch" aria-checked="${!!d.enabled}" aria-label="${d.enabled ? "停用" : "启用"} ${fb.esc(d.pattern)}" data-toggle="${d.id}"></button></td>
        <td class="r"><button class="btn btn-sm btn-ghost" data-del="${d.id}" aria-label="删除">删除</button></td>
      </tr>`).join("");
      $$("#rule-body [data-toggle]").forEach(sw => sw.addEventListener("click", async function onToggle() {
        const d = rules.find(x => String(x.id) === sw.dataset.toggle);
        if (!d) return;
        if (d.locked && !await fb.confirm(`「${d.pattern}」已锁定。仍要${d.enabled ? "停用" : "启用"}吗？`, { title: "修改锁定规则", confirmLabel: d.enabled ? "仍然停用" : "仍然启用" })) return;
        try {
          await api.toggleRule(ch.id, d.id, !d.enabled);
          d.enabled = d.enabled ? 0 : 1;
          sw.classList.toggle("on", !!d.enabled); sw.setAttribute("aria-checked", String(!!d.enabled));
          toast(`${d.pattern} 已${d.enabled ? "启用" : "停用"}`, { variant: "success" });
        } catch (e) {
          toast("操作失败：" + fb.friendlyError(e).title, { variant: "danger", action: { label: "重试", onClick: onToggle } });
        }
      }));
      $$("#rule-body [data-del]").forEach(b => b.addEventListener("click", async function onDel() {
        const d = rules.find(x => String(x.id) === b.dataset.del);
        if (!d) return;
        if (d.locked && !await fb.confirm(`「${d.pattern}」已锁定。仍要删除吗？`, { title: "删除锁定规则", confirmLabel: "仍然删除", danger: true })) return;
        b.disabled = true;
        try {
          await api.delRule(ch.id, d.id);
          if (d.kind === "ip") ch.ips = ch.ips.filter(x => x.id !== d.id); else ch.domains = ch.domains.filter(x => x.id !== d.id);
          renderRuleTable();
          toast(`已删除 ${d.pattern}`, { variant: "success" });
        } catch (e) {
          b.disabled = false;
          toast("删除失败：" + fb.friendlyError(e).title, { variant: "danger", action: { label: "重试", onClick: onDel } });
        }
      }));
    }
    const ruleForm = $("#add-rule");
    async function addRules(toks) {
      const btn = ruleForm.querySelector('button[type="submit"]');
      btn.disabled = true; btn.replaceChildren(fb.spinner("绑定中…"));
      try {
        const r = await api.addRules(ch.id, toks);
        ruleForm.pat.value = "";
        ch.domains = r.domains; ch.ips = r.ips;
        renderRuleTable();
        const n = r.added.domain + r.added.ip;
        if (n) toast(`已绑定 ${n} 条，立即生效`, { variant: "success" });
        else if (r.rejected.length) toast(`无法识别：${r.rejected.join("，")}`, { variant: "danger" });
        else toast("这些规则已存在", { variant: "info" });
      } catch (err) {
        toast("绑定失败：" + fb.friendlyError(err).title, { variant: "danger", action: { label: "重试", onClick: () => addRules(toks) } });
      } finally { btn.disabled = false; btn.textContent = "绑定"; }
    }
    ruleForm.addEventListener("submit", (e) => {
      e.preventDefault();
      const toks = parseTokens(e.target.pat.value);
      if (!toks.length) { toast("请填写域名或 IP", false); return; }
      addRules(toks);
    });

    /* ── 日志 ── */
    async function loadLogs() {
      const body = $("#logs-body");
      body.replaceChildren(fb.spinner("正在读取…"));
      try {
        const { lines } = await api.logs(ch.id, 500);
        body.textContent = lines.join("\n");
        body.scrollTop = body.scrollHeight;
      } catch (e) {
        body.innerHTML = "";
        fb.errorBanner(body, { fromError: e, onRetry: () => loadLogs() });
      }
    }
    $("#logs-refresh").addEventListener("click", () => loadLogs());
    document.querySelector('.tab[data-tab="logs"]').addEventListener("click", () => loadLogs());
    document.querySelectorAll('[data-od-id="ch-tabs"] [data-tabs] > .tab').forEach(tab => {
      tab.addEventListener("click", () => queueMicrotask(vncView.sync));
    });

    /* ── 自动刷新:后端合并探活并退避,页面不可见时暂停 ── */
    function probeUnconfirmed() {
      $("#h-last").textContent += " · 待确认";
      $("#h-status").textContent = "待确认";
      $("#top-badge").innerHTML = '<span class="badge is-running"><i class="bdot"></i>待确认</span>';
    }
    async function refreshStatus() {
      if (!ch || ch.status === "stopped") return;
      try {
        const r = await api.channelHealth(ch.id);
        setLastProbe(r.checked_at ?? Date.now());
        if (r.stale) { probeUnconfirmed(); return; }
        ch.status = r.status;
        ch.latency_ms = r.latency_ms;
        syncStatus();
        renderHealth();
      } catch (e) {
        $("#h-last").textContent = "检测失败";
        probeUnconfirmed();
        console.warn("[channel] 自动检测失败，8s 后重试:", e.message);
      }
    }

    boot(true).then(() => {
      if (!ch) return;
      if (location.hash === "#login") goLogin();
      api.poll(refreshStatus, 8000);
    });
  
