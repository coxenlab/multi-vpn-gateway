import { api } from "../api.js";
import { $, $$, copyText, toast } from "../app.js";
import { fb } from "../feedback.js";
import "../preflightPanel.js";
import "../environment.js";
import "../runtime-events.js";
import { loadWithSystem } from "../page-data.js";

    const cmt = (s) => `<span class="cmt">${fb.esc(s)}</span>`;
    const key = (s) => `<span class="key">${fb.esc(s)}</span>`;
    const str = (s) => `<span class="str">${fb.esc(s)}</span>`;

    let chs = [], sys = {}, cmds = {};

    async function boot() {
      const fbHost = $("#boot-feedback");
      fbHost.innerHTML = "";
      const loading = fb.spinner("正在加载…");
      loading.style.padding = "var(--space-3) 0";
      fbHost.appendChild(loading);
      try {
        ({ data: chs, system: sys } = await loadWithSystem());
      } catch (e) {
        fbHost.innerHTML = "";
        fb.errorBanner(fbHost, { fromError: e, onRetry: boot, retryLabel: "重新加载" });
        return;
      }
      try { cmds = await api.entrySetup(); } catch (e) { cmds = {}; }
      fbHost.innerHTML = "";
      render();
    }

    function renderSelfHeal() {
      const wrap = $("#self-heal-setting"), btn = $("#self-heal-toggle");
      if (typeof sys.self_heal_enabled !== "boolean") { wrap.hidden = true; return; }
      wrap.hidden = false;
      btn.textContent = sys.self_heal_enabled ? "开" : "已暂停";
      btn.setAttribute("aria-checked", String(sys.self_heal_enabled));
    }
    async function toggleSelfHeal() {
      const btn = $("#self-heal-toggle");
      const enabled = !sys.self_heal_enabled;
      btn.disabled = true;
      try {
        const result = await api.selfHeal(enabled);
        sys.self_heal_enabled = !!result.enabled;
        renderSelfHeal();
        toast(enabled ? "自动修复已开启" : "自动修复已暂停", { variant: "success" });
      } catch (e) {
        toast("切换失败：" + fb.friendlyError(e).title, { variant: "danger" });
      } finally { btn.disabled = false; }
    }
    $("#self-heal-toggle").addEventListener("click", toggleSelfHeal);

    function render() {
      const origin = location.origin;
      const mport = sys.mihomo_port;
      const endpoint = `127.0.0.1:${mport}`;
      renderSelfHeal();
      $("#nav-count").textContent = chs.length;
      $("#entry-endpoint").textContent = endpoint;
      $("#bp-port").textContent = mport;
      $("#sp-port").textContent = endpoint;

      // 生效规则(折叠全局 / 通道级开关);旧后端无新字段时按规则自身 enabled
      const patterns = [], cidrs = [];
      const routingOn = (c) => typeof sys.routing_off !== "boolean"
        || (!sys.routing_off && c.routing_enabled !== false && c.routing_enabled !== 0);
      chs.forEach((c) => (c.domains || []).forEach((d) => {
        if (routingOn(c) && d.enabled) {
          const p = d.pattern.replace(/^\+\./, "").replace(/^\*\./, "");
          if (!patterns.includes(p)) patterns.push(p);
        }
      }));
      chs.forEach((c) => (c.ips || []).forEach((d) => {
        if (routingOn(c) && d.enabled && !cidrs.includes(d.pattern)) cidrs.push(d.pattern);
      }));
      $("#rules-count").textContent = patterns.length + cidrs.length;

      const proxyPlain = `- name: vpn-router\n  type: socks5\n  server: 127.0.0.1\n  port: ${mport}`;
      $("#code-proxy").innerHTML = [
        `- ${key("name")}: ${str("vpn-router")}`,
        `  ${key("type")}: ${str("socks5")}`,
        `  ${key("server")}: ${str("127.0.0.1")}`,
        `  ${key("port")}: ${str(String(mport))}`,
      ].join("\n");

      const providerUrl = `${origin}/clash/vpn-rules.yaml`;
      const providerPlain =
`rule-providers:
  vpn-rules:
    type: http
    behavior: classical
    format: yaml
    url: ${providerUrl}
    interval: 3600
    path: ./providers/vpn-rules.yaml

# rules 顶部加一行：
- RULE-SET,vpn-rules,vpn-router,no-resolve`;
      $("#code-provider").innerHTML = [
        `${key("rule-providers")}:`,
        `  ${key("vpn-rules")}:`,
        `    ${key("type")}: ${str("http")}`,
        `    ${key("behavior")}: ${str("classical")}`,
        `    ${key("format")}: ${str("yaml")}`,
        `    ${key("url")}: ${str(providerUrl)}`,
        `    ${key("interval")}: ${str("3600")}`,
        `    ${key("path")}: ${str("./providers/vpn-rules.yaml")}`,
        ``,
        `${cmt("# rules 顶部加一行：")}`,
        `- ${key("RULE-SET")},${fb.esc("vpn-rules")},${str("vpn-router")},${fb.esc("no-resolve")}`,
      ].join("\n");

      const rulesPlain = ([
        ...patterns.map((p) => `- DOMAIN-SUFFIX,${p},vpn-router,no-resolve`),
        ...cidrs.map((p) => `- IP-CIDR,${p},vpn-router,no-resolve`),
      ].join("\n")) || "# （还没有规则）";
      $("#code-rules").innerHTML = (patterns.length + cidrs.length) ? [
        ...patterns.map((p) => `- ${key("DOMAIN-SUFFIX")},${fb.esc(p)},${str("vpn-router")},${fb.esc("no-resolve")}`),
        ...cidrs.map((p) => `- ${key("IP-CIDR")},${fb.esc(p)},${str("vpn-router")},${fb.esc("no-resolve")}`),
      ].join("\n") : cmt("# （还没有规则）");

      $("#code-browser").textContent = patterns.length ? patterns.join("\n") : "（还没有域名规则）";

      const pacUrl = cmds.pac_url || `${origin}/entry/proxy.pac`;
      const pacCmd = (cmds.macos && cmds.macos.pac_on) || `networksetup -setautoproxyurl Wi-Fi ${pacUrl}`;
      $("#code-pac-url").textContent = pacUrl;
      $("#code-pac-cmd").textContent = pacCmd;

      const sysOn = (cmds.macos && cmds.macos.socks_on) || `networksetup -setsocksfirewallproxy Wi-Fi 127.0.0.1 ${mport}`;
      const sysOff = (cmds.macos && cmds.macos.socks_off) || `networksetup -setsocksfirewallproxystate Wi-Fi off`;
      $("#code-sys").textContent = sysOn + "\n" + sysOff;

      const envSocks = (cmds.env && cmds.env.socks) || `export ALL_PROXY=socks5h://127.0.0.1:${mport}`;
      const envHttp = (cmds.env && cmds.env.http) || `export HTTPS_PROXY=http://127.0.0.1:${mport} HTTP_PROXY=http://127.0.0.1:${mport}`;
      $("#code-env").textContent = envSocks + "\n" + envHttp;

      $("#copy-endpoint").onclick = () => copyText(endpoint, "已复制");
      $("#copy-proxy").onclick = () => copyText(proxyPlain, "已复制");
      $("#copy-provider").onclick = () => copyText(providerPlain, "已复制");
      $("#copy-rules").onclick = () => copyText(rulesPlain, "已复制");
      $("#copy-browser").onclick = () => copyText(patterns.join("\n"), "已复制");
      $("#copy-pac").onclick = () => copyText(pacUrl, "已复制");
      $("#copy-pac-cmd").onclick = () => copyText(pacCmd, "已复制");
      $("#copy-sys").onclick = () => copyText(sysOn + "\n" + sysOff, "已复制");
      $("#copy-env").onclick = () => copyText(envSocks, "已复制");

      runClashDetect();
      runSystemProxyGet();
      tunPollLeft = 12;
      runTunGet();
    }

    // ── 接入方式检测 + 推荐:三路探测各自回填后重算。undefined=检测中,null=不支持/失败 ──
    const entryDetect = { clash: undefined, tun: undefined, sys: undefined };
    let entryRecoApplied = false, entryTabTouched = false, entryProgrammaticTab = false;
    $$('[data-od-id="entry-tabs"] .tab').forEach((b) => b.addEventListener("click", () => {
      if (!entryProgrammaticTab) entryTabTouched = true;
    }));
    function entryBadge(tab, kind, text) {
      const btn = document.getElementById("etab-" + tab);
      if (!btn) return;
      btn.querySelectorAll(".etab-badge").forEach((el) => el.remove());
      if (!kind) return;
      const s = document.createElement("span");
      s.className = "etab-badge pill";
      s.style.marginLeft = "6px";
      if (kind === "on") { s.style.background = "var(--success-soft)"; s.style.color = "var(--success)"; }
      s.textContent = text;
      btn.appendChild(s);
    }
    function updateEntryReco() {
      const t = entryDetect;
      if (t.clash === undefined || t.tun === undefined || t.sys === undefined) return;
      let reco;
      if (t.tun && t.tun.enabled)
        reco = { tab: "tun", title: "正在通过 TUN 接管", why: "规则里的 IP 网段已在路由级接管。要停用或调整，在「TUN 接管」里操作。" };
      else if (t.sys && t.sys.enabled && t.sys.is_ours)
        reco = { tab: "pac", title: "正在通过系统代理接管", why: "系统自动代理已指向本工具。要停用，在「系统代理」里点「取消」。" };
      else if (t.clash)
        reco = { tab: "rule", title: "推荐：接入你现有的 Clash", why: "检测到本机 Clash 正在运行。加一个节点、订阅一份规则即可，其余流量不受影响。" };
      else if (t.tun)
        reco = { tab: "tun", title: "推荐：TUN 接管", why: t.tun.installed
          ? "助手已安装，点「启用」即可，终端和不认代理的程序也生效。"
          : "没检测到 Clash。TUN 接管最彻底，首次点「安装助手」输入一次管理员密码即可。" };
      else if (t.sys)
        reco = { tab: "pac", title: "推荐：系统代理", why: "没检测到 Clash。点「应用到系统」即可，命中规则的走 VPN、其余直连。" };
      else
        reco = { tab: "pac", title: "推荐：系统代理", why: "复制 PAC 地址填进系统「自动代理配置」即可；如果你有 Clash，用「已有 Clash」。" };
      $("#entry-reco-title").textContent = reco.title;
      $("#entry-reco-line").textContent = reco.why;
      const badges = { tun: null, pac: null, rule: null, proc: null };
      if (t.tun && t.tun.enabled) badges.tun = ["on", "使用中"];
      if (t.sys && t.sys.enabled && t.sys.is_ours) badges.pac = ["on", "使用中"];
      if (t.clash) badges.rule = ["on", "运行中"];
      if (!badges[reco.tab]) badges[reco.tab] = ["reco", "推荐"];
      Object.entries(badges).forEach(([tab, b]) => entryBadge(tab, b && b[0], b && b[1]));
      if (!entryTabTouched && !entryRecoApplied) {
        entryRecoApplied = true;
        const btn = document.querySelector(`[data-od-id="entry-tabs"] .tab[data-tab="${reco.tab}"]`);
        if (btn && !btn.classList.contains("active")) { entryProgrammaticTab = true; btn.click(); entryProgrammaticTab = false; }
      }
    }

    // 本机 Clash 检测 + Verge 导入块(host-only;404 = 不支持,整卡隐藏)
    async function runClashDetect() {
      const card = $("#card-detect"), tag = $("#detect-tag"), line = $("#detect-line");
      card.style.display = "";
      tag.textContent = "检测中";
      line.replaceChildren(fb.spinner("正在检测本机 Clash…"));
      try {
        const [det, merge] = await Promise.all([api.clashDetect(), api.mergeProfile()]);
        tag.classList.remove("ok");
        entryDetect.clash = !!det.running; updateEntryReco();
        if (det.running) {
          tag.textContent = "已检测到"; tag.classList.add("ok");
          line.textContent = `本机 Clash 正在运行${det.version ? "（" + det.version + "）" : ""}，可直接用下面的配置块导入。`;
        } else {
          tag.textContent = "未运行";
          line.textContent = "没检测到运行中的 Clash。先启动它，或改用「系统代理」/「TUN 接管」。";
        }
        $("#code-merge").textContent = merge;
        $("#copy-merge").onclick = () => copyText($("#code-merge").textContent, "已复制");
      } catch (e) {
        entryDetect.clash = null; updateEntryReco();
        if (e && e.status === 404) { card.style.display = "none"; return; }
        const f = fb.friendlyError(e);
        tag.textContent = "检测失败"; tag.classList.remove("ok");
        line.textContent = f.title + "：" + f.message;
        toast("Clash 检测失败", { variant: "danger", action: { label: "重新检测", onClick: runClashDetect } });
      }
    }

    // 系统代理一键应用(host-only)
    async function runSystemProxyGet() {
      const wrap = $("#pac-host-wrap"), status = $("#pac-host-status");
      try {
        const st0 = await api.systemProxyGet();
        if (!st0 || !st0.supported) { entryDetect.sys = null; updateEntryReco(); wrap.style.display = "none"; return; }
        wrap.style.display = "";
        renderPacHost(st0);
        $("#pac-apply").onclick = onPacApply;
      } catch (e) {
        entryDetect.sys = null; updateEntryReco();
        if (e && e.status === 404) { wrap.style.display = "none"; return; }
        const f = fb.friendlyError(e);
        wrap.style.display = "";
        status.textContent = f.title + "：" + f.message;
        toast("读取系统代理状态失败", { variant: "danger", action: { label: "重新读取", onClick: runSystemProxyGet } });
      }
    }
    function renderPacHost(st) {
      const status = $("#pac-host-status"), btn = $("#pac-apply");
      const on = !!(st.enabled && st.is_ours);
      entryDetect.sys = { enabled: !!st.enabled, is_ours: !!st.is_ours }; updateEntryReco();
      status.textContent = on
        ? `已应用到「${st.service}」`
        : (st.enabled ? `「${st.service}」的自动代理当前指向其它地址，应用后会覆盖` : `「${st.service || "默认网络"}」未启用自动代理`);
      btn.textContent = on ? "取消" : "应用到系统";
      btn.dataset.on = on ? "1" : "";
    }
    async function onPacApply() {
      const btn = $("#pac-apply");
      const enable = btn.dataset.on !== "1";
      const label = btn.textContent;
      btn.disabled = true;
      btn.replaceChildren(fb.spinner(enable ? "正在应用…" : "正在取消…"));
      try {
        const r = await api.systemProxySet(enable);
        renderPacHost(r.state);
        toast(enable ? "已应用系统代理" : "已取消系统代理", { variant: "success" });
      } catch (e) {
        btn.textContent = label;
        toast(fb.friendlyError(e).title, { variant: "danger", action: { label: "重试", onClick: onPacApply } });
      } finally { btn.disabled = false; }
    }

    // ── TUN 接管(host-only;404 = 整卡隐藏) ──
    let tunRefreshTimer = null, tunPollLeft = 0;
    async function runTunGet() {
      const card = $("#card-tun");
      try {
        const st = await api.tunGet();
        if (!st || !st.supported) { entryDetect.tun = null; updateEntryReco(); card.style.display = "none"; return; }
        card.style.display = "";
        const webNote = document.querySelector('[data-od-id="m-tun"]');
        if (webNote) webNote.hidden = true;
        renderTun(st);
        $("#tun-action").onclick = onTunAction;
        $("#tun-uninstall").onclick = onTunUninstall;
      } catch (e) {
        entryDetect.tun = null; updateEntryReco();
        if (e && e.status === 404) { card.style.display = "none"; return; }
        const f = fb.friendlyError(e);
        card.style.display = "";
        $("#tun-tag").textContent = "读取失败";
        $("#tun-status").textContent = f.title + "：" + f.message;
        toast("读取 TUN 状态失败", { variant: "danger", action: { label: "重新读取", onClick: runTunGet } });
      }
    }
    function renderTun(st) {
      const tag = $("#tun-tag"), status = $("#tun-status"), btn = $("#tun-action"), un = $("#tun-uninstall");
      const h = st.helper;
      entryDetect.tun = { installed: !!st.installed, enabled: !!st.enabled }; updateEntryReco();
      const desired = (st.desired_v4 || []).length + (st.desired_v6 || []).length;
      const shadowed = (h && h.shadowed) || 0;
      tag.classList.remove("ok");
      btn.style.display = ""; un.style.display = "none";
      if (!st.installed) {
        tag.textContent = "未安装";
        status.textContent = st.resources
          ? `安装助手后即可接管 ${desired} 条 IP 网段规则。`
          : "当前安装包缺少助手组件，无法启用。";
        btn.textContent = "安装助手"; btn.dataset.mode = "install"; btn.disabled = !st.resources;
        return;
      }
      btn.disabled = false;
      un.style.display = "";
      if (!h) {
        tag.textContent = "未应答";
        status.textContent = "助手已安装但没有响应。刚安装的话稍等几秒；也可能被系统「后台项目」关闭。";
        btn.textContent = "重新检测"; btn.dataset.mode = "recheck";
        return;
      }
      const stale = h.version !== st.expected_version;
      if (stale) {
        tag.textContent = "待升级";
        status.textContent = "助手版本较旧，升级后才能确认新路由是否完整生效。";
        btn.textContent = "升级助手"; btn.dataset.mode = "install";
        return;
      }
      if (st.enabled && h.running) {
        const settled = h.alive && h.pending === false && h.applied >= desired;
        tag.textContent = !h.alive ? "启动中" : shadowed ? "部分生效" : settled ? "运行中" : "同步中";
        if (settled) tag.classList.add("ok");
        const shadowHint = shadowed ? `，${shadowed} 条网段本机已有路由、未接管` : "";
        status.textContent = h.alive
          ? `已接管 ${h.applied}/${desired} 条网段${shadowHint}${stale ? "。助手需升级，请重新安装" : ""}`
          : "已启用，正在生效…";
        btn.textContent = "停用"; btn.dataset.mode = "disable";
        if (!settled && tunPollLeft > 0 && !tunRefreshTimer) {
          tunPollLeft--;
          tunRefreshTimer = setTimeout(() => { tunRefreshTimer = null; runTunGet(); }, 2500);
        } else if (settled) { tunPollLeft = 0; }
      } else {
        tag.textContent = "已安装";
        status.textContent = `未启用。启用后接管 ${desired} 条 IP 网段规则${stale ? "。助手需升级，请重新安装" : ""}。`;
        btn.textContent = "启用"; btn.dataset.mode = "enable";
      }
    }
    async function onTunAction() {
      const btn = $("#tun-action");
      const mode = btn.dataset.mode;
      const label = btn.textContent;
      btn.disabled = true;
      try {
        if (mode === "install") {
          btn.replaceChildren(fb.spinner("等待授权…"));
          toast("请在系统弹窗中输入管理员密码", { variant: "info" });
          const r = await api.tunInstall();
          renderTun(r.state);
          toast("助手已安装", { variant: "success" });
        } else if (mode === "enable" || mode === "disable") {
          btn.replaceChildren(fb.spinner(mode === "enable" ? "正在启用…" : "正在停用…"));
          const r = await api.tunSet(mode === "enable");
          if (mode === "enable") tunPollLeft = 12;
          if (r.state && r.state.warning) toast(r.state.warning, { variant: "warning" });
          renderTun(r.state);
          toast(mode === "enable" ? "TUN 接管已启用" : "TUN 接管已停用", { variant: "success" });
        } else {
          await runTunGet();
          return;
        }
      } catch (e) {
        btn.textContent = label;
        const f = fb.friendlyError(e);
        toast(f.title + "：" + f.message, { variant: "danger", action: { label: "重试", onClick: onTunAction } });
      } finally { btn.disabled = false; }
    }
    async function onTunUninstall() {
      if (!await fb.confirm("卸载助手后 TUN 接管将停用（需要管理员密码）。", { title: "卸载助手", confirmLabel: "确认卸载", danger: true })) return;
      const un = $("#tun-uninstall");
      un.disabled = true;
      try {
        const r = await api.tunUninstall();
        renderTun(r.state);
        toast("助手已卸载", { variant: "success" });
      } catch (e) {
        const f = fb.friendlyError(e);
        toast(f.title + "：" + f.message, { variant: "danger" });
      } finally { un.disabled = false; }
    }

    // ── 备份:导出 / 导入配置 ──
    $("#btn-export").addEventListener("click", async () => {
      const btn = $("#btn-export"), txt = btn.textContent;
      btn.disabled = true; btn.replaceChildren(fb.spinner("导出中…"));
      try {
        const doc = await api.exportConfig();
        const blob = new Blob([JSON.stringify(doc, null, 2)], { type: "application/json" });
        const a = document.createElement("a");
        a.href = URL.createObjectURL(blob);
        a.download = `vpnmgr-config-${new Date().toISOString().slice(0, 10)}.json`;
        a.click();
        URL.revokeObjectURL(a.href);
        toast(`已导出 ${doc.channels.length} 条通道`, { variant: "success" });
      } catch (e) {
        toast("导出失败：" + fb.friendlyError(e).title, { variant: "danger" });
      } finally { btn.disabled = false; btn.textContent = txt; }
    });
    $("#btn-import").addEventListener("click", () => $("#import-file").click());
    $("#import-file").addEventListener("change", async (ev) => {
      const file = ev.target.files[0];
      ev.target.value = "";
      if (!file) return;
      let doc;
      try { doc = JSON.parse(await file.text()); }
      catch { toast("文件不是有效的 JSON", { variant: "danger" }); return; }
      try {
        const r = await api.importConfig(doc);
        const parts = [`已导入 ${r.imported.length} 条通道`];
        if (r.skipped.length) parts.push(`跳过 ${r.skipped.length} 条：` + r.skipped.map(s => `${s.name}（${s.reason}）`).join("、"));
        toast(parts.join("，"), { variant: r.imported.length ? "success" : "info" });
      } catch (e) {
        toast("导入失败：" + fb.friendlyError(e).title, { variant: "danger" });
      }
    });

    // 深链:?tab=diag|mirrors|images|backup;运行日志用 #logs
    document.addEventListener("DOMContentLoaded", () => {
      const tabs = [...document.querySelectorAll("#system-tabs > [data-tabs] > [data-tab]")];
      tabs.forEach((tab) => tab.addEventListener("click", () => {
        const url = new URL(location.href);
        if (tab.dataset.tab === "logs") { url.searchParams.delete("tab"); url.hash = "logs"; }
        else {
          url.hash = "";
          if (tab.dataset.tab === "entry") url.searchParams.delete("tab");
          else url.searchParams.set("tab", tab.dataset.tab);
        }
        history.replaceState(null, "", url);
      }));
      let requested = location.hash === "#logs" ? "logs" : new URLSearchParams(location.search).get("tab");
      const sect = { mirrors: "mirrors-sect", images: "images-sect" }[requested];
      if (sect) { requested = "diag"; const d = document.getElementById(sect); if (d) d.open = true; }
      const target = tabs.find((tab) => tab.dataset.tab === requested && !tab.hidden);
      if (target) target.click();
    });

    boot();
  
