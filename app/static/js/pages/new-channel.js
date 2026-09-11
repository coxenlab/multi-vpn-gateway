import { api } from "../api.js";
import { $, $$, parseTokens, waitNovncReady, toast } from "../app.js";
import { fb } from "../feedback.js";
import { PreflightPanel } from "../preflightPanel.js";
import { vncText } from "../vncText.js";
import { createVncLifecycle } from "../vnc-lifecycle.js";

    api.channels().then(cs => { $("#nav-count").textContent = cs.length; }).catch(() => {});

    const model = { name: "", type: "easyconnect", ecver: "", login: "password", probe: "", fields: {} };
    let ADAPTERS = [];
    let cur = 1;
    let createdId = null;

    function gotoStep(n) {
      cur = n;
      $$(".step-panel").forEach(p => p.classList.toggle("active", +p.dataset.panel === n));
      $$("#stepper .step").forEach(s => {
        const i = +s.dataset.step;
        s.classList.toggle("active", i === n);
        s.classList.toggle("done", i < n);
      });
      window.scrollTo({ top: 0, behavior: "smooth" });
      vncView.sync();
    }

    /* ── 类型(数据驱动) ── */
    const spec = () => ADAPTERS.find(a => a.key === model.type) || {};
    const modes = () => Array.isArray(spec().login_modes) ? spec().login_modes : [];
    const headlessOnly = () => modes().length === 1 && modes()[0] === "headless";
    const byoOnly = () => modes().length === 1 && modes()[0] === "byo";
    const isHeadless = () => headlessOnly() || model.login === "password";

    function renderTypeGrid(list) {
      $("#type-grid").innerHTML = list.map((a, i) => `
        <label class="choice${i === 0 ? " sel" : ""}" data-type="${fb.esc(a.key)}">
          <div class="ct">${fb.esc(a.label)}</div>
          <div class="cd">${fb.esc(a.desc || "")}</div>
        </label>`).join("");
    }

    async function selectType(key) {
      $$("#type-grid .choice").forEach(x => x.classList.toggle("sel", x.dataset.type === key));
      model.type = key;
      const sp = spec();
      $("#ecver-field").style.display = sp.versioned ? "" : "none";
      if (sp.versioned) await loadVersions(key);
      renderInputs(sp.inputs || []);
      // 登录方式:只有既能账密无头、又能交互登录的类型才需要用户选;其余由类型决定
      const onlyGui = modes().length > 0 && !modes().includes("headless");
      const pwd = $('#login-grid .choice[data-login="password"]');
      pwd.classList.toggle("disabled", onlyGui);
      if (onlyGui && model.login === "password") setLogin("interactive");
      if (!headlessOnly() && !byoOnly() && !["password", "interactive"].includes(model.login)) setLogin("password");
      $("#login-field").style.display = (headlessOnly() || byoOnly()) ? "none" : "";
      if (headlessOnly()) model.login = "headless";
      if (byoOnly()) model.login = "byo";
      // 类型说明(byo 家族的诚实标注:哪些客户端在容器里跑不起来)
      const notice = typeof sp.notice === "string" && sp.notice.trim();
      $("#type-notice").innerHTML = byoOnly()
        ? (notice
          ? `<div class="banner info"><div><div class="bt">已预装 ${fb.esc(sp.label || sp.key)} 客户端</div><p>${fb.esc(notice)}</p></div></div>`
          : `<div class="banner warn"><div><div class="bt">自带客户端，尽力而为</div><p>创建后会打开一个 Linux 桌面，你在里面自行安装并登录 VPN 客户端。依赖系统服务、硬件令牌或仅有 Windows / macOS 版本的客户端无法在这里运行。</p></div></div>`)
        : "";
    }

    function renderInputs(inputs) {
      const host = $("#dyn-fields");
      host.innerHTML = inputs.map(inp => {
        const id = "dyn-" + inp.key;
        const req = inp.required ? " required" : "";
        if (inp.type === "file") {
          return `<div class="field col-span"><label>${fb.esc(inp.label)}</label>
            <input class="input" id="${fb.esc(id)}" type="file" data-key="${fb.esc(inp.key)}"${req}></div>`;
        }
        const t = inp.secret ? "password" : (inp.type === "url" ? "url" : "text");
        return `<div class="field col-span"><label>${fb.esc(inp.label)}</label>
          <input class="input mono" id="${fb.esc(id)}" type="${t}" data-key="${fb.esc(inp.key)}" autocomplete="off"${req}></div>`;
      }).join("");
      model.fields = {};
      host.querySelectorAll("input").forEach(el => {
        el.addEventListener(el.type === "file" ? "change" : "input", e => {
          const key = e.target.dataset.key;
          if (e.target.type === "file") { model.fields[key] = e.target.files[0] || null; return; }
          // 非密码字段直接过滤空格:误输空格会致认证 / 连接失败
          if (e.target.type !== "password") {
            const cleaned = e.target.value.replace(/\s+/g, "");
            if (cleaned !== e.target.value) e.target.value = cleaned;
          }
          model.fields[key] = e.target.value;
        });
      });
    }

    async function loadVersions(key) {
      const sel = $("#f-ecver");
      try {
        const { versions } = await api.vpnVersions(key);
        const def = versions.find(v => v.usable_here) || versions[0];
        sel.innerHTML = versions.map(v =>
          `<option value="${fb.esc(v.tag)}"${def && v.tag === def.tag ? " selected" : ""}${v.usable_here ? "" : " disabled"}>${fb.esc(v.tag)}${v.usable_here ? "" : "（本机不可用）"}</option>`).join("");
        model.ecver = def ? def.tag : "";
      } catch (e) {
        sel.innerHTML = `<option value="7.6.3" selected>7.6.3</option>`;
        model.ecver = "7.6.3";
      }
    }
    $("#f-ecver").addEventListener("change", e => { model.ecver = e.target.value; });
    $("#type-grid").addEventListener("click", (e) => { const c = e.target.closest(".choice"); if (c) selectType(c.dataset.type); });

    function loadTypes() {
      $("#types-err").innerHTML = "";
      const grid = $("#type-grid");
      const stash = grid.innerHTML;
      grid.replaceChildren(fb.skeleton(2));
      api.vpnTypes().then(list => {
        ADAPTERS = list;
        renderTypeGrid(list);
        if (list.length) selectType(list[0].key);
      }).catch(e => {
        grid.innerHTML = stash;
        fb.errorBanner("#types-err", { fromError: e, retryLabel: "重新加载", title: "类型列表加载失败", onRetry: loadTypes });
      });
    }
    loadTypes();

    function setLogin(v) {
      model.login = v;
      $$("#login-grid .choice").forEach(x => x.classList.toggle("sel", x.dataset.login === v));
    }
    $$("#login-grid .choice").forEach(c => c.addEventListener("click", () => { if (!c.classList.contains("disabled")) setLogin(c.dataset.login); }));
    $("#f-name").addEventListener("input", e => { model.name = e.target.value.trim(); });
    $("#f-probe").addEventListener("input", e => { model.probe = e.target.value.trim(); });

    /* ── 创建:校验 → 环境检查(静默,只在失败时露出) → 建容器 → 进登录 ── */
    function validate() {
      if (!model.name) return "请填写客户名称";
      const serverInp = (spec().inputs || []).find(i => i.key === "server");
      if (serverInp && serverInp.type === "url") {
        const sv = model.fields.server || "";
        if (isHeadless() && !sv) return "自动登录需要填写网关地址";
        if (sv && !/^https:\/\/.+/.test(sv)) return "网关地址须以 https:// 开头";
      }
      for (const inp of (spec().inputs || [])) {
        if (inp.required && !model.fields[inp.key] && inp.key !== "server") return `请填写${inp.label}`;
      }
      if (!model.probe) return "请填写内网验证地址";
      return null;
    }

    let pfPanel = null, pfSkipped = false;
    async function preflightOk() {
      if (pfSkipped) return true;
      const wrap = $("#pf-wrap"), host = $("#pf-host");
      // 先静默跑一遍;只有未通过才把检查面板露出来
      let res = null;
      try { res = await api.preflight(model.type, spec().versioned ? model.ecver : undefined, "preflight"); } catch (_) { res = null; }
      if (res && res.overall !== "fail") { wrap.style.display = "none"; return true; }
      wrap.style.display = "";
      pfPanel = PreflightPanel(host, { vpnType: model.type, version: spec().versioned ? model.ecver : undefined });
      await pfPanel.run();
      return false;
    }
    $("#pf-recheck").addEventListener("click", () => runCreate());
    $("#pf-skip").addEventListener("click", () => { pfSkipped = true; runCreate(); });

    let creating = false;
    async function runCreate() {
      if (creating) return;
      const err = validate();
      if (err) return toast(err, false);
      creating = true;
      const btn = $("#create-btn");
      btn.disabled = true; btn.replaceChildren(fb.spinner("正在创建…"));
      $("#create-err").innerHTML = "";
      try {
        if (!createdId) {
          btn.replaceChildren(fb.spinner("准备运行环境…"));
          await api.startRuntime();
        }
        if (!createdId && !(await preflightOk())) {
          toast("环境检查未通过，请先修复", { variant: "danger" });
          return;
        }
        const pkgFile = (model.fields.package instanceof File) ? model.fields.package : null;
        const steps = [{ key: "create", label: "创建容器" }];
        if (pkgFile) steps.push({ key: "upload", label: "上传安装包" });
        const sp = fb.stepper($("#create-steps"), steps);
        if (!createdId) {   // 重试 upload 失败时跳过 create,避免重复建容器
          sp.setStep("create", "active", "正在启动…");
          const cfg = {};
          for (const [k, v] of Object.entries(model.fields)) {
            if (v == null) continue;
            if (v instanceof File && k === "package") continue;   // byo 安装包走 multipart
            cfg[k] = (v instanceof File) ? await v.text() : v;   // .ovpn / wg .conf 随 config 加密落库
          }
          const ch = await api.create({
            name: model.name, vpn_type: model.type,
            ec_ver: spec().versioned ? model.ecver : "",
            login_method: model.login === "password" ? "password" : model.login,
            probe_url: model.probe,
            server: cfg.server || "", username: cfg.username || "",
            password: model.login === "password" ? (model.fields.password || "") : "",
            config: cfg,
          });
          createdId = ch.id;
        }
        sp.setStep("create", "done", "已启动");
        if (pkgFile) {
          sp.setStep("upload", "active", "上传 " + pkgFile.name + " …");
          await api.upload(createdId, pkgFile);
          sp.setStep("upload", "done", "已上传");
        }
        $("#create-steps").innerHTML = "";
        enterLogin();
      } catch (e) {
        fb.errorBanner("#create-err", { fromError: e, retryLabel: "重试", onRetry: runCreate });
        // 起容器失败:把环境检查面板露出来帮定位
        $("#pf-wrap").style.display = "";
        pfPanel = PreflightPanel($("#pf-host"), { vpnType: model.type, version: spec().versioned ? model.ecver : undefined });
        pfPanel.run();
      } finally {
        creating = false;
        btn.disabled = false; btn.textContent = createdId ? "重试" : "创建通道";
      }
    }
    $("#create-btn").addEventListener("click", runCreate);

    /* ── ② 登录:交互登录拉 noVNC;无论哪种方式,底部每 5 秒自动探活直到连通 ── */
    let wizVncUrl = null, probeTimer = null, connected = false;
    function enterLogin() {
      gotoStep(2);
      const headless = isHeadless();
      $("#login-headless").style.display = headless ? "" : "none";
      $("#login-interactive").style.display = headless ? "none" : "";
      $("#conn-spin").replaceChildren(fb.spinner(""));
      startProbing();
    }
    function startProbing() {
      if (probeTimer) probeTimer(); probeTimer = null;
      const tick = async () => {
        if (connected || document.hidden) return;
        try {
          const r = await api.channelHealth(createdId);
          if (r.connected && !r.stale) {
            connected = true; if (probeTimer) probeTimer(); probeTimer = null;
            $("#conn-line").classList.add("ok");
            $("#conn-spin").innerHTML = "";
            $("#conn-text").textContent = `已连通 · ${r.latency_ms ?? "?"} ms`;
            $("#login-badge").outerHTML = '<span id="login-badge" class="badge is-logged_in"><i class="bdot"></i>已连接</span>';
            $("#conn-hint").textContent = "";
            toast("已连通", { variant: "success" });
          } else {
            $("#conn-text").textContent = isHeadless() ? "正在连接…" : "等待登录…登录完成后自动检测";
          }
        } catch (_) { /* 下一拍再试 */ }
      };
      probeTimer = api.poll(tick, 5000);
    }
    $("#go-3").addEventListener("click", () => {
      if (!connected) $("#conn-hint").textContent = "";
      gotoStep(3);
      if (!connected) toast("还没连通也可以先绑规则，稍后在通道详情里登录", { variant: "info" });
    });

    // 「键入到容器」自动留档到该通道的登录备注(同文本只记一次;旧后端无 /note 则跳过)
    async function wizRecordTyped(text) {
      const t = text.trim();
      if (!t || !createdId) return;
      try {
        const { note } = await api.noteGet(createdId);
        if ((note || "").includes(t)) return;
        const ts = new Date().toLocaleString("zh-CN", { hour12: false });
        await api.noteSet(createdId, (note ? note.replace(/\n*$/, "\n") : "") + `[${ts}] 键入：${t}`);
        toast("已记入登录备注");
      } catch (_e) { /* 旧后端不支持 */ }
    }
    const vncPlaceholder = $("#vnc-placeholder").cloneNode(true);
    const vncView = createVncLifecycle({
      channelId: () => createdId,
      isActive: () => createdId && cur === 2 && !isHeadless(),
      open: loadVnc,
      close: () => {
        $("#vnc-frame")?.replaceWith(vncPlaceholder.cloneNode(true));
        $("#vnc-sendbar")?.remove();
        $("#vnc-err").replaceChildren();
        wizVncUrl = null;
      },
    });
    async function loadVnc({ signal, current, login }) {
      const ph = document.getElementById("vnc-placeholder");
      $("#vnc-err").innerHTML = "";
      try {
        if (ph) ph.replaceChildren(fb.spinner("正在打开登录窗口…"));
        const { url } = await login();
        if (!current()) return;
        // 容器内登录界面启动慢:探到在伺服再塞 iframe,否则早加载会白屏
        if (ph) ph.replaceChildren(fb.spinner("等待登录界面就绪…首次启动可能需要一两分钟"));
        const ready = await waitNovncReady(url, 60, { signal });
        if (!current()) return;
        if (!ready) throw new Error("登录界面还未就绪，请稍后重新打开");
        let f = document.getElementById("vnc-frame");
        if (!f) {
          f = document.createElement("iframe");
          f.id = "vnc-frame"; f.title = "VPN 登录窗口"; f.style.cssText = "width:100%;height:80vh;min-height:640px;border:0;border-radius:8px;resize:vertical;overflow:auto;";
          const p = document.getElementById("vnc-placeholder");
          if (p) p.replaceWith(f); else $("#login-interactive").prepend(f);
        }
        f.src = url;
        wizVncUrl = url;
        let sh = document.getElementById("vnc-sendbar");
        if (!sh) { sh = document.createElement("div"); sh.id = "vnc-sendbar"; f.insertAdjacentElement("afterend", sh); }
        vncText.mountBar(sh, () => wizVncUrl, wizRecordTyped, { signal });
      } catch (e) {
        if (!current()) return;
        if (ph) ph.textContent = "登录窗口打开失败";
        fb.errorBanner("#vnc-err", { fromError: e, retryLabel: "重新打开", onRetry: vncView.open });
      }
    }
    document.getElementById("vnc-reload-wiz").addEventListener("click", (e) => { e.preventDefault(); vncView.open(); });

    /* ── ③ 绑定规则(域名 / IP 由后端自动识别) ── */
    async function bindRules() {
      const toks = parseTokens($("#f-domain").value);
      if (!toks.length) { toast("请填写域名或 IP", false); return; }
      const btn = $("#bind-btn");
      btn.disabled = true; btn.replaceChildren(fb.spinner("绑定中…"));
      $("#bind-err").innerHTML = "";
      try {
        const r = await api.addRules(createdId, toks);
        $("#f-domain").value = "";
        const all = [...r.domains.map(d => ({ pattern: d.pattern, ip: false })), ...r.ips.map(d => ({ pattern: d.pattern, ip: true }))];
        $("#bound-list").innerHTML = all.map(b => `<span class="tag mono${b.ip ? " ip" : ""}">${fb.esc(b.pattern)}</span>`).join("");
        if (r.added.domain || r.added.ip) toast(`已绑定 ${r.added.domain + r.added.ip} 条${r.deferred ? "，连接后生效" : ""}`, { variant: r.deferred ? "info" : "success" });
        else if (r.rejected.length) toast("无法识别：" + r.rejected.join(", "), { variant: "danger" });
        else toast("这些规则已存在", { variant: "info" });
      } catch (e) {
        fb.errorBanner("#bind-err", { fromError: e, title: "绑定失败", retryLabel: "重试", onRetry: bindRules });
      } finally {
        btn.disabled = false; btn.textContent = "绑定";
      }
    }
    $("#bind-form").addEventListener("submit", () => { bindRules(); return false; });

    setLogin("password");
  
