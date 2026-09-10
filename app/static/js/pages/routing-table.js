import { api } from "../api.js";
import { $, $$, badgeHTML, kindMeta, toast } from "../app.js";
import { fb } from "../feedback.js";
import { loadWithSystem } from "../page-data.js";

    let chs = [];
    let sys = {};
    let loaded = false;
    let busy = false;
    let filterChannel = "";
    let filterQuery = "";
    let visibleRuleIds = [];
    const selectedRules = new Set();

    const routingControlsSupported = () => typeof sys.routing_off === "boolean";
    const channelRoutingOn = (c) => !routingControlsSupported() || (c.routing_enabled !== false && c.routing_enabled !== 0);
    const isDeadCh = (c) => c.status === "stopped" || c.status === "error" || c.status === "down";
    // 规则自身 enabled(不折叠通道态)——「失效」清单用它,才能列出停止通道上仍启用的规则
    const chEnabledRaw = (c) => [...(c.domains || []), ...(c.ips || [])].filter((r) => r.enabled);
    // 实际生效规则:对齐后端 effective_rules 的折叠(全局开关 + 通道路由开关 + 通道状态)。
    // 红队 M12:不折叠状态会导致计数虚高、交叠误报、干线图给死通道画分支。
    const enabledRules = (c) => (sys.routing_off || !channelRoutingOn(c) || isDeadCh(c)) ? [] : chEnabledRaw(c);
    const allRules = (c) => [...(c.domains || []), ...(c.ips || [])];

    /* IPv4 CIDR → [lo, hi] 数值区间;非 v4 返回 null(v6 只比对全等) */
    function cidr4(p) {
      const m = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})\/(\d{1,2})$/.exec(p);
      if (!m) return null;
      const oct = m.slice(1, 5).map(Number);
      const bits = +m[5];
      if (oct.some((o) => o > 255) || bits > 32) return null;
      const ip = (oct[0] * 16777216 + oct[1] * 65536 + oct[2] * 256 + oct[3]) >>> 0;
      const mask = bits === 0 ? 0 : (~0 << (32 - bits)) >>> 0;
      const lo = (ip & mask) >>> 0;
      return [lo, lo + 2 ** (32 - bits) - 1];
    }
    /* DOMAIN-SUFFIX 语义:相等或互为后缀即相互覆盖 */
    function domainOverlap(a, b) {
      a = a.toLowerCase(); b = b.toLowerCase();
      return a === b || a.endsWith("." + b) || b.endsWith("." + a);
    }

    /* 跨通道找启用规则的交叠对 → [{a:{ch,r}, b:{ch,r}, why}] */
    function findConflicts() {
      const ents = [];
      chs.forEach((c) => enabledRules(c).forEach((r) => ents.push({ ch: c, r })));
      const out = [];
      for (let i = 0; i < ents.length; i++) {
        for (let j = i + 1; j < ents.length; j++) {
          const a = ents[i], b = ents[j];
          if (a.ch.id === b.ch.id) continue;
          if (a.r.kind === "ip" && b.r.kind === "ip") {
            const ra = cidr4(a.r.pattern), rb = cidr4(b.r.pattern);
            if (ra && rb ? ra[0] <= rb[1] && rb[0] <= ra[1] : a.r.pattern === b.r.pattern)
              out.push({ a, b, why: "IP 网段交叠" });
          } else if (a.r.kind !== "ip" && b.r.kind !== "ip") {
            if (domainOverlap(a.r.pattern, b.r.pattern))
              out.push({ a, b, why: "域名后缀相互覆盖" });
          }
        }
      }
      return out;
    }

    async function fetchAll() {
      ({ data: chs, system: sys } = await loadWithSystem());
      paint();
      loaded = true;
    }
    async function load(isPoll) {
      if (isPoll && busy) return;
      if (!loaded && !isPoll) {
        const fbk = $("#table-fallback");
        fbk.style.display = "";
        fbk.innerHTML = "";
        fbk.appendChild(fb.skeleton(5));
      }
      try {
        await fetchAll();
        $("#table-fallback").style.display = "none";
      } catch (e) {
        if (isPoll) {
          toast("刷新状态失败,将自动重试", { variant: "danger", action: { label: "立即重试", onClick: () => load(false) } });
          return;
        }
        const fbk = $("#table-fallback");
        fbk.style.display = "";
        fbk.innerHTML = "";
        fb.errorBanner(fbk, { fromError: e, onRetry: () => load(false), retryLabel: "重新加载" });
      }
    }

    const STATUS_RANK = { logged_in: 0, starting: 1, running: 1, created: 1, down: 2, error: 2, stopped: 3 };

    /* ── 穿透图 ──
     * 入口三接口是桌面版 host-only(web 版 / 旧版 404 → allSettled 降级为「接口不可用」),
     * 且 tun 状态要过 helper socket(慢),故与主轮询分离:启动拉一次 + 每 30s 刷新。 */
    let entry = null;   // null=检测中;{clash,sysproxy,tun} 各项 null=该接口不可用
    const node = (dot, pn, ps, notes) => {
      // pn / ps / note.text 一律是纯文本(server、socks_endpoint、probeHost、clash.version、k.label 等经此渲染);
      // dot / n.cls 为固定 class 常量,不转义。统一在 node 出口转义,覆盖所有穿透图节点 sink。
      const extra = (notes || []).map((n) => `<div class="ps ${n.cls}">${fb.esc(n.text)}</div>`).join("");
      return `<div class="pipe-node"><span class="pdot ${dot}"></span><div><div class="pn">${fb.esc(pn)}</div>${ps ? `<div class="ps">${fb.esc(ps)}</div>` : ""}${extra}</div></div>`;
    };
    const ARROW = `<span class="pipe-arrow">→</span>`;
    const fmtAge = (s) => (s < 60 ? `${s} 秒` : s < 3600 ? `${Math.floor(s / 60)} 分钟` : `${Math.floor(s / 3600)} 小时`);

    /* ── 通道诊断(容器死循环 / wg 握手 / EC 踢线)──
     * 桌面版 host-only 接口,每通道走容器内探针(有 exec 开销)→ 与主轮询分离,20s 一拍;
     * web 版 / 旧版 404 → diagMap=null,穿透图退回基础状态灯。 */
    let diagMap = null;
    async function loadDiag() {
      try {
        const d = await api.diag();
        diagMap = {};
        (d.channels || []).forEach((x) => { diagMap[x.id] = x; });
      } catch (_) { diagMap = null; }
      if (loaded) paint();
    }

    async function loadEntry() {
      const [cd, sp, tn] = await Promise.allSettled([api.clashDetect(), api.systemProxyGet(), api.tunGet()]);
      const val = (x) => (x.status === "fulfilled" && x.value && typeof x.value === "object" ? x.value : null);
      entry = { clash: val(cd), sysproxy: val(sp), tun: val(tn) };
      paintEntry();
    }

    function paintEntry() {
      const clash = entry && entry.clash, sp = entry && entry.sysproxy, tun = entry && entry.tun;
      const bits = [];
      if (clash) bits.push(clash.running ? `Clash ${clash.version || ""} 运行中` : "Clash 未运行");
      if (sp) bits.push(sp.enabled ? (sp.is_ours ? "系统代理 → 本工具" : "系统代理 → 其他") : "系统代理 关");
      if (tun) bits.push(tun.enabled ? "层3 TUN 开" : tun.installed ? "层3 TUN 关" : "层3 TUN 未安装");
      const active = !!(clash && clash.running) || !!(sp && sp.enabled && sp.is_ours) || !!(tun && tun.enabled);
      const entryDot = entry === null ? "" : active ? "ok" : "warn";
      const entryPs = entry === null ? "检测中…" : bits.length ? bits.join(" · ") : "无法探测（Web 版）";
      const mihomoOk = sys.mihomo_status === "running";
      const online = chs.filter((c) => c.status === "logged_in").length;
      $("#entry-pipe").innerHTML = [
        node("", "本机流量", "浏览器 / 终端 / 任意程序"),
        ARROW,
        node(entryDot, "入口接管", entryPs),
        ARROW,
        node(mihomoOk ? "ok" : "bad", "入口", `127.0.0.1:${sys.mihomo_port || "—"} · ${mihomoOk ? "运行中" : sys.mihomo_status || "未知"}`),
        ARROW,
        node(online ? "ok" : "warn", "按规则分流", `${chs.length} 条通道 · ${online} 在线 · 未命中 → DIRECT`),
      ].join("");
    }

    function paintPipes(sorted) {
      $("#pipe-rows").innerHTML = sorted.map((c) => {
        const d = diagMap && diagMap[c.id];
        const doms = enabledRules(c).filter((r) => r.kind === "domain").length;
        const ips = enabledRules(c).filter((r) => r.kind === "ip").length;
        const en = doms + ips;
        const up = c.status === "logged_in";
        const running = c.status === "running" || c.status === "starting" || c.status === "created";
        const k = kindMeta(c.vpn_type);
        const server = (c.server || (c.config && c.config.server) || "").replace(/^https?:\/\//, "") || "配置内指定";
        const probeHost = (c.probe_url || "").replace(/^https?:\/\//, "").replace(/\/.*$/, "");
        const lat = c.latency_ms != null ? `${c.latency_ms} ms` : "未检测";

        // 容器节点:死循环盖过一切
        let contDot = up ? "ok" : running ? "warn" : "bad";
        const contNotes = [];
        if (d && d.crash_loop) {
          contDot = "bad";
          contNotes.push({ cls: "note-conflict", text: `死循环:已重启 ${d.restart_count} 次${d.uptime_secs != null ? ` · 本次存活 ${fmtAge(d.uptime_secs)}` : ""}` });
        }

        // 隧道节点:诊断信号(踢线 / 客户端死 / 接口未起 / wg 握手)
        let tunDot = up ? "ok" : running ? "warn" : "bad";
        let tunPs = up ? `已连通 · ${server}` : running ? `待登录 · ${server}` : "容器已停止";
        const tunNotes = [];
        if (d) {
          if (d.tunnel_up === true && d.tunnel_iface) tunPs += ` · ${d.tunnel_iface}`;
          if (d.ec_kick_age != null && d.ec_kick_age < 7200)
            tunNotes.push({ cls: "note-conflict", text: `服务端踢线 · ${fmtAge(d.ec_kick_age)}前` });
          if (d.ec_client_alive === false) {
            tunDot = "bad";
            tunNotes.push({ cls: "note-conflict", text: "EC 客户端进程未运行" });
          }
          if (d.tunnel_up === false && c.status !== "stopped") {
            tunDot = "bad";
            tunNotes.push({ cls: "note-conflict", text: "隧道接口未起" });
          }
          if (d.wg_handshake_age != null) {
            if (d.wg_handshake_age <= 180) tunPs += ` · 握手 ${fmtAge(d.wg_handshake_age)}前`;
            else {
              if (tunDot === "ok") tunDot = "warn";
              tunNotes.push({ cls: "note-dead", text: `握手中断:已 ${fmtAge(d.wg_handshake_age)} 无响应` });
            }
          } else if (d.tunnel_up === true && c.vpn_type === "wireguard") {
            tunNotes.push({ cls: "note-dead", text: "从未握手(对端无响应)" });
          }
        }

        return `<div class="pipe-row${up ? "" : " dim"}">
          <div class="pipe-ch"><a class="grp-name" href="channel.html?id=${fb.esc(c.id)}">${fb.esc(c.name)}</a>${badgeHTML(c.status)}</div>
          <div class="pipe">
            ${node(en ? "ok" : "", "分流规则", en ? `${doms} 域名 + ${ips} IP 命中走此通道` : (sys.routing_off ? "分流总开关已关" : !channelRoutingOn(c) ? "通道已暂停分流" : "未绑定 · 不参与分流"))}
            ${ARROW}
            ${node(contDot, "通道容器", `${c.socks_endpoint}`, contNotes)}
            ${ARROW}
            ${node(tunDot, `${k.label} 隧道`, tunPs, tunNotes)}
            ${ARROW}
            ${node(up ? "ok" : "", "客户内网", `${probeHost || "—"} · ${up ? lat : "不可达"}`)}
          </div>
        </div>`;
      }).join("");
    }

    function syncChannelFilter(sorted) {
      const select = $("#channel-filter");
      if (filterChannel && !sorted.some((c) => c.id === filterChannel)) filterChannel = "";
      select.innerHTML = `<option value="">全部通道</option>` + sorted.map((c) =>
        `<option value="${fb.esc(c.id)}">${fb.esc(c.name)}</option>`).join("");
      select.value = filterChannel;
    }

    function updateBulkState() {
      const n = selectedRules.size;
      $("#bulk-count").textContent = n ? `已选 ${n} 条` : "未选择";
      $("#bulk-enable").disabled = busy || !n;
      $("#bulk-disable").disabled = busy || !n;
    }

    function paint() {
      const conflicts = findConflicts();
      // rid → 原始文本说明；输出到 HTML/attribute 时再统一 fb.esc。
      const confNote = new Map();
      const addNote = (r, txt) => confNote.set(r.id, [...(confNote.get(r.id) || []), txt]);
      conflicts.forEach(({ a, b, why }) => {
        addNote(a.r, `与「${b.ch.name}」的 ${b.r.pattern} ${why}`);
        addNote(b.r, `与「${a.ch.name}」的 ${a.r.pattern} ${why}`);
      });
      const deadRules = [];
      chs.filter(isDeadCh).forEach((c) => chEnabledRaw(c).forEach((r) => deadRules.push({ ch: c, r })));
      const deadRids = new Set(deadRules.map((d) => d.r.id));
      const online = chs.filter((c) => c.status === "logged_in").length;
      const totalRules = chs.reduce((n, c) => n + allRules(c).length, 0);
      const enabledN = chs.reduce((n, c) => n + enabledRules(c).length, 0);
      const hasIssues = conflicts.length > 0 || deadRules.length > 0;

      $("#routing-health").classList.toggle("has-issues", conflicts.length > 0);   // 只有交叠才算问题;通道停了规则自动折叠不是异常
      $("#health-title").textContent = hasIssues
        ? (conflicts.length ? `${conflicts.length} 组规则交叠` : "规则正常") + (deadRules.length ? ` · ${deadRules.length} 条随通道停用` : "")
        : "规则正常";
      $("#summary-online").textContent = `${online}/${chs.length}`;
      $("#summary-enabled").textContent = `${enabledN}/${totalRules}`;
      $("#summary-conflict").textContent = conflicts.length;
      $("#summary-dead").textContent = deadRules.length;
      $("#summary-conflict").parentElement.classList.toggle("active", conflicts.length > 0);
      $("#nav-count").textContent = chs.length;
      $("#foot-port").textContent = ":" + (sys.mihomo_port || "—");
      const globalRouting = $("#global-routing");
      globalRouting.hidden = !routingControlsSupported();
      globalRouting.disabled = busy;
      globalRouting.textContent = sys.routing_off ? "分流总开关 · 关" : "分流总开关 · 开";
      globalRouting.setAttribute("aria-checked", String(!sys.routing_off));
      $$(".desktop-routing").forEach((el) => { el.hidden = !routingControlsSupported(); });

      const issueDetails = $("#issue-details");
      issueDetails.style.display = hasIssues ? "" : "none";
      if (!hasIssues) issueDetails.open = false;
      const issueRows = [];
      conflicts.forEach(({ a, b, why }) => issueRows.push(`
        <div class="issue-item"><span class="badge is-down">交叠</span><div>
          <strong>${fb.esc(why)}</strong>：「${fb.esc(a.ch.name)}」<span class="mono">${fb.esc(a.r.pattern)}</span>
          ↔ 「${fb.esc(b.ch.name)}」<span class="mono">${fb.esc(b.r.pattern)}</span>。只有排在前面的那条会生效。
        </div></div>`));
      deadRules.forEach(({ ch, r }) => issueRows.push(`
        <div class="issue-item"><span class="badge is-stopped">通道已停</span><div>
          「${fb.esc(ch.name)}」<span class="mono">${fb.esc(r.pattern)}</span> 仍启用，但通道已停止，命中的流量会直连；启动通道后自动恢复。
        </div></div>`));
      $("#issues").innerHTML = issueRows.join("");

      const sorted = [...chs].sort((x, y) => (STATUS_RANK[x.status] ?? 9) - (STATUS_RANK[y.status] ?? 9));
      syncChannelFilter(sorted);
      const flat = [];
      sorted.forEach((c) => {
        allRules(c).forEach((r) => flat.push({ c, r }));
      });
      const validIds = new Set(flat.map(({ r }) => String(r.id)));
      [...selectedRules].forEach((id) => { if (!validIds.has(id)) selectedRules.delete(id); });
      const q = filterQuery.toLowerCase();
      const visible = flat.filter(({ c, r }) => {
        if (filterChannel && c.id !== filterChannel) return false;
        if (!q) return true;
        return [r.pattern, r.note, c.name].some((v) => String(v || "").toLowerCase().includes(q));
      });
      visibleRuleIds = visible.map(({ r }) => String(r.id));

      $("#t-visible").textContent = visible.length;
      $("#t-rules").textContent = totalRules;
      $("#tbody").innerHTML = visible.map(({ c, r }) => {
        const id = String(r.id);
        const conflictNotes = r.enabled ? (confNote.get(r.id) || []) : [];
        const dead = !!r.enabled && deadRids.has(r.id);
        const effective = !!r.enabled && channelRoutingOn(c) && !sys.routing_off;
        const cls = conflictNotes.length ? "hit-conflict" : dead ? "hit-dead" : effective ? "" : "off";
        const badges = [];
        if (conflictNotes.length) badges.push(`<span class="rule-badge conflict" title="${fb.esc(conflictNotes.join("；"))}">冲突 ${conflictNotes.length}</span>`);
        if (dead) badges.push(`<span class="rule-badge dead" title="通道已停止，命中的流量会直连；启动通道后自动恢复">通道已停</span>`);
        if (!channelRoutingOn(c)) badges.push(`<span class="rule-badge paused">通道暂停</span>`);
        if (sys.routing_off) badges.push(`<span class="rule-badge paused">分流已关</span>`);
        const metadata = routingControlsSupported() ? `<td><input class="input rule-note-input" data-rule-note="${fb.esc(id)}" data-cid="${fb.esc(c.id)}" value="${fb.esc(r.note || "")}" placeholder="添加备注" aria-label="${fb.esc(r.pattern)} 的备注" ${busy ? "disabled" : ""} /></td>` : "";
        const protection = routingControlsSupported() ? `<td><button class="btn btn-sm btn-ghost rule-lock" type="button" data-rule-lock="${fb.esc(id)}" data-cid="${fb.esc(c.id)}" aria-pressed="${!!r.locked}" ${busy ? "disabled" : ""}>
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><rect x="5" y="10" width="14" height="10" rx="2"/><path d="M8 10V7a4 4 0 018 0v3"/></svg>${r.locked ? "已锁" : "未锁"}</button></td>` : "";
        return `<tr class="${cls}">
          <td><input class="rule-check" type="checkbox" data-select-rule="${fb.esc(id)}" ${selectedRules.has(id) ? "checked" : ""} ${busy ? "disabled" : ""} aria-label="选择 ${fb.esc(r.pattern)}" /></td>
          <td><div class="rule-pattern">${fb.esc(r.pattern)}</div></td>
          <td class="muted nowrap">${r.kind === "ip" ? "IP / 网段" : "域名"}</td>
          <td><a class="grp-name" href="channel.html?id=${fb.esc(c.id)}">${fb.esc(c.name)}</a></td>
          <td>${badgeHTML(c.status)}</td>
          ${metadata}
          <td><button class="switch ${r.enabled ? "on" : ""}" type="button" role="switch" aria-checked="${!!r.enabled}" aria-label="${r.enabled ? "停用" : "启用"} ${fb.esc(r.pattern)}" data-rule-toggle="${fb.esc(id)}" data-cid="${fb.esc(c.id)}" data-enabled="${r.enabled ? "1" : "0"}" ${busy ? "disabled" : ""}></button></td>
          ${protection}
          <td><div class="rule-badges">${badges.join("") || `<span class="muted">—</span>`}</div></td>
        </tr>`;
      }).join("") || `<tr><td colspan="${routingControlsSupported() ? 9 : 7}"><div class="empty" style="padding:var(--space-6);">${totalRules ? "没有匹配当前筛选的规则" : "还没有规则。在通道详情里绑定域名或 IP 网段。"}</div></td></tr>`;

      const selectVisible = $("#select-visible");
      const selectedVisible = visibleRuleIds.filter((id) => selectedRules.has(id)).length;
      selectVisible.disabled = busy || visibleRuleIds.length === 0;
      selectVisible.checked = visibleRuleIds.length > 0 && selectedVisible === visibleRuleIds.length;
      selectVisible.indeterminate = selectedVisible > 0 && selectedVisible < visibleRuleIds.length;
      updateBulkState();
      paintPipes(sorted);
      paintEntry();
    }

    $("#rule-search").addEventListener("input", (e) => {
      filterQuery = e.target.value.trim();
      paint();
    });
    $("#channel-filter").addEventListener("change", (e) => { filterChannel = e.target.value; paint(); });
    $("#select-visible").addEventListener("change", (e) => {
      visibleRuleIds.forEach((id) => e.target.checked ? selectedRules.add(id) : selectedRules.delete(id));
      paint();
    });
    function findRule(id) {
      for (const c of chs) {
        const rule = allRules(c).find((r) => String(r.id) === String(id));
        if (rule) return { c, rule };
      }
      return null;
    }
    $("#tbody").addEventListener("change", async (e) => {
      const input = e.target.closest("[data-rule-note]");
      if (!input || busy) return;
      const found = findRule(input.dataset.ruleNote);
      if (!found || input.value === (found.rule.note || "")) return;
      const previous = found.rule.note || "";
      if (found.rule.locked && !await fb.confirm(`「${found.rule.pattern}」已锁定。仍要修改备注吗？`, {
        title: "修改锁定规则", confirmLabel: "仍然保存",
      })) { input.value = previous; return; }
      busy = true;
      input.disabled = true;
      try {
        await api.updateRule(found.c.id, found.rule.id, { note: input.value });
        found.rule.note = input.value;
        toast("规则备注已保存", { variant: "success" });
      } catch (err) {
        input.value = previous;
        const f = fb.friendlyError(err);
        toast("备注保存失败:" + f.title, { variant: "danger" });
      } finally {
        busy = false;
        paint();
      }
    });
    $("#tbody").addEventListener("change", (e) => {
      const box = e.target.closest("[data-select-rule]");
      if (!box) return;
      box.checked ? selectedRules.add(box.dataset.selectRule) : selectedRules.delete(box.dataset.selectRule);
      updateBulkState();
      const chosen = visibleRuleIds.filter((id) => selectedRules.has(id)).length;
      $("#select-visible").checked = visibleRuleIds.length > 0 && chosen === visibleRuleIds.length;
      $("#select-visible").indeterminate = chosen > 0 && chosen < visibleRuleIds.length;
    });
    $("#tbody").addEventListener("click", async (e) => {
      const lock = e.target.closest("[data-rule-lock]");
      if (lock && !busy) {
        const found = findRule(lock.dataset.ruleLock);
        if (!found) return;
        const next = !found.rule.locked;
        if (found.rule.locked && !await fb.confirm(`解除「${found.rule.pattern}」的批量操作保护？`, {
          title: "解除规则锁定", confirmLabel: "解除锁定",
        })) return;
        busy = true;
        lock.disabled = true;
        try {
          await api.updateRule(found.c.id, found.rule.id, { locked: next });
          found.rule.locked = next ? 1 : 0;
          toast(next ? "规则已锁定，批量启停会跳过" : "规则已解除锁定", { variant: "success" });
        } catch (err) {
          const f = fb.friendlyError(err);
          toast("锁定操作失败:" + f.title, { variant: "danger" });
        } finally {
          busy = false;
          paint();
        }
        return;
      }
      const btn = e.target.closest("[data-rule-toggle]");
      if (!btn || busy) return;
      const found = findRule(btn.dataset.ruleToggle);
      if (found && found.rule.locked && !await fb.confirm(`「${found.rule.pattern}」已锁定。仍要单独${found.rule.enabled ? "停用" : "启用"}吗？`, {
        title: "修改锁定规则", confirmLabel: found.rule.enabled ? "仍然停用" : "仍然启用",
      })) return;
      busy = true;
      btn.disabled = true;
      const enabled = btn.dataset.enabled !== "1";
      try {
        await api.toggleRule(btn.dataset.cid, Number(btn.dataset.ruleToggle), enabled);
        toast(`规则已${enabled ? "启用" : "停用"}`, { variant: "success" });
        await load(false);
      } catch (err) {
        const f = fb.friendlyError(err);
        toast("切换失败：" + f.title, { variant: "danger" });
      } finally {
        busy = false;
        paint();
      }
    });

    async function batchToggle(enabled) {
      if (!selectedRules.size || busy) return;
      const ids = [...selectedRules].map(Number);
      const button = enabled ? $("#bulk-enable") : $("#bulk-disable");
      const original = button.textContent;
      busy = true;
      button.textContent = enabled ? "启用中…" : "停用中…";
      updateBulkState();
      try {
        const result = await api.toggleRules(ids, enabled);
        selectedRules.clear();
        const skipped = Number(result.skipped_locked || 0);
        toast(`已${enabled ? "启用" : "停用"} ${result.updated || 0} 条规则${skipped ? `，${skipped} 条已跳过（锁定）` : ""}`, { variant: "success" });
        await load(false);
      } catch (err) {
        const f = fb.friendlyError(err);
        toast("批量操作失败：" + f.title, { variant: "danger" });
      } finally {
        busy = false;
        button.textContent = original;
        paint();
      }
    }
    $("#bulk-enable").addEventListener("click", () => batchToggle(true));
    $("#bulk-disable").addEventListener("click", () => batchToggle(false));

    async function toggleGlobalRouting() {
      if (busy || !routingControlsSupported()) return;
      const turningOff = !sys.routing_off;
      if (turningOff && !await fb.confirm("关闭后所有分流规则暂时失效，流量全部直连；规则本身的启停状态会保留。", {
        title: "关闭分流", confirmLabel: "关闭分流",
      })) return;
      const btn = $("#global-routing");
      busy = true;
      btn.disabled = true;
      try {
        const result = await api.setRouting(turningOff);
        sys.routing_off = !!result.off;
        await load(false);
        toast(turningOff ? "分流已关闭，流量全部直连" : "分流已恢复", { variant: "success" });
      } catch (err) {
        const f = fb.friendlyError(err);
        toast("切换分流总开关失败：" + f.title, { variant: "danger" });
      } finally {
        busy = false;
        btn.disabled = false;
        paint();
      }
    }
    $("#global-routing").addEventListener("click", toggleGlobalRouting);

    load();
    loadEntry();
    loadDiag();
    api.poll(() => load(true), 8000, { immediate: false });
    api.poll(loadEntry, 30000, { immediate: false });
    api.poll(loadDiag, 20000, { immediate: false });
  
