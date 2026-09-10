import { api } from "../api.js";
import { $, toast } from "../app.js";
import { fb } from "../feedback.js";

    /* ═══ 路由决策日志流 ═══
     * 数据源只有既有的 /api/connections + /api/proxies（无新后端接口）：
     * 每 1.5s 拉一次 connections，对 connections[].id 做集合差分，
     * 新出现的 id 合成一行 Clash 风格日志。连接结束后 id 不再复用，
     * 因此每帧把「已记录集合」直接重置为当前活跃 id 集合即可，内存恒定。 */

    const MAX_LINES = 500;                 // 环形缓冲上限

    let chs = [], sys = {}, nodes = [];
    let prevTotals = null, prevTs = null;  // 速率差分锚点（沿用旧页算法）
    let pollTimer = null, feedDown = false, booted = false;

    let seen = new Set();   // 已记录过日志的 connection id（= 上一帧的活跃集合）
    let buf = [];           // 环形缓冲：日志条目对象
    let pending = [];       // 暂停期间挂起的条目（恢复后按序补入）
    let seq = 0;            // 全局行号
    let paused = false, follow = true;
    let filterChan = "all", filterQ = "", filterRe = null, chanSig = "";


    const PAUSE_IC = '<path d="M9 5v14M15 5v14"/>';
    const PLAY_IC  = '<path d="M7 4l13 8-13 8V4z"/>';

    /* ───────── 反馈层状态：遥测更新 pill + 故障错误条(单条、自愈即清) ───────── */
    function setFeed(state, text) {
      const pill = $("#feed-pill");
      pill.classList.toggle("is-live", state === "live");
      pill.classList.toggle("is-down", state === "down");
      $("#feed-text").textContent = text;
    }
    function pad2(n) { return String(n).padStart(2, "0"); }
    function hhmmss(d) { return pad2(d.getHours()) + ":" + pad2(d.getMinutes()) + ":" + pad2(d.getSeconds()); }
    function nowHHMMSS() { return hhmmss(new Date()); }
    function clearFeedError() {
      if (!feedDown) return;
      feedDown = false;
      $("#feed-error").innerHTML = "";
    }
    function showFeedError(e, onRetry) {
      // 同一故障只渲染一条错误条,避免每 1.5s 刷屏
      if (feedDown) return;
      feedDown = true;
      $("#feed-error").innerHTML = "";
      if (window.fb && fb.errorBanner) {
        // 点「重试」时 fb 先 remove 自身;须放开 feedDown,使再次失败能重渲一条新错误条
        fb.errorBanner("#feed-error", {
          fromError: e, retryLabel: "重试",
          onRetry: () => { feedDown = false; onRetry(); },
        });
      }
    }

    /* 首屏占位骨架(仅未成功渲染过时) */
    function showInitialSkeleton() {
      if (booted || !(window.fb && fb.skeleton)) return;
      const box = document.createElement("div");
      box.className = "log-skel";
      box.appendChild(fb.skeleton(8));
      $("#log-view").innerHTML = "";
      $("#log-view").appendChild(box);
    }

    async function boot() {
      // 初始化-获取系统配置:进行中提示(loading pill + 首屏骨架)
      setFeed("loading", "连接中…");
      showInitialSkeleton();
      $("#feed-error").innerHTML = ""; feedDown = false;
      try {
        sys = await api.system();
      } catch (e) {
        // 失败:友好错误条 + 带操作的 toast,重试重跑 boot(同一 api,参数不变)
        setFeed("down", "连接失败");
        feedDown = true;
        if (window.fb && fb.errorBanner)
          fb.errorBanner("#feed-error", { fromError: e, onRetry: boot, retryLabel: "重新连接" });
        toast("无法连接遥测,数据未加载", { variant: "danger", action: { label: "重试", onClick: boot } });
        return;
      }
      $("#foot-port").textContent = ":" + sys.mihomo_port;
      // 先起轮询(只起一次)再拉首帧 —— 即便首帧失败,后续仍会每 1.5s 自动重试自愈
      if (!pollTimer) pollTimer = api.poll(tick, 1500, { immediate: false });
      await tick();
    }
    function chanName(id) { const c = chs.find(x => x.id === id); return c ? c.name : "未知通道"; }

    /* ───────── 日志条目合成 ─────────
     * 出口取 chains 里的 ch-* 项：本工具的通道在 mihomo 里就是 ch-{id} 代理，
     * chains 末项即它；若上游把分流口(vpn-router)也串进 chains，则末项不是 ch-*，
     * 这里退一步在整条链里找 ch-*，找不到再退回末项（DIRECT/REJECT 等兜底）。 */
    function outbound(chains) {
      const arr = Array.isArray(chains) ? chains : [];
      const hit = arr.find(x => typeof x === "string" && x.indexOf("ch-") === 0);
      if (hit) return hit;
      return arr.length ? String(arr[arr.length - 1] || "") : "";
    }
    function tsOf(c) {
      const d = c.start ? new Date(c.start) : null;
      return d && !isNaN(d.getTime()) ? hhmmss(d) : nowHHMMSS();
    }
    function makeEntry(c) {
      const m = c.metadata || {};
      const net = String(m.network || "tcp").toUpperCase();
      const src = (m.sourceIP || "?") + (m.sourcePort ? ":" + m.sourcePort : "");
      const dst = (m.host || m.destinationIP || "?") + (m.destinationPort ? ":" + m.destinationPort : "");
      const tail = outbound(c.chains);
      const cid = tail.indexOf("ch-") === 0 ? tail.slice(3) : null;
      const rule = c.rule || "Match";
      const payload = c.rulePayload || "";
      const via = cid ? tail + "[" + chanName(cid) + "]" : (tail || "DIRECT");
      const e = {
        seq: 0, ts: tsOf(c), net, src, dst, rule, payload, tail, via,
        direct: !cid,   // 末跳非 ch-* ＝ 未经本工具通道（DIRECT / REJECT 兜底）
      };
      // 纯文本形态：既供正则搜索，也供导出 .log
      e.text = "[" + net + "] " + src + " --> " + dst + " match " + rule +
               (payload ? "(" + payload + ")" : "") + " using " + via;
      e.search = e.ts + " " + e.text;
      return e;
    }

    function rowHTML(e) {
      const rule = e.payload
        ? fb.esc(e.rule) + "(<span class=\"lg-pl\">" + fb.esc(e.payload) + "</span>)"
        : fb.esc(e.rule);
      return '<div class="log-row' + (e.direct ? " is-direct" : "") + '" data-seq="' + e.seq + '">' +
        '<span class="lg-seq">' + String(e.seq).padStart(4, "0") + "</span>" +
        '<span class="lg-ts">' + fb.esc(e.ts) + "</span>" +
        '<span class="lg-msg">' +
          '<span class="lg-net">[' + fb.esc(e.net) + "]</span> " +
          '<span class="lg-src">' + fb.esc(e.src) + "</span> " +
          '<span class="lg-kw">--&gt;</span> ' +
          '<span class="lg-dst">' + fb.esc(e.dst) + "</span> " +
          '<span class="lg-kw">match</span> ' + rule + " " +
          '<span class="lg-kw">using</span> ' +
          '<span class="lg-via">' + fb.esc(e.via) + "</span>" +
        "</span></div>";
    }

    /* ───────── 筛选（出口下拉 + 正则搜索） ───────── */
    function matchEntry(e) {
      if (filterChan === "direct" && !e.direct) return false;
      if (filterChan !== "all" && filterChan !== "direct" && e.tail !== filterChan) return false;
      if (!filterQ) return true;
      if (filterRe) { filterRe.lastIndex = 0; return filterRe.test(e.search); }
      return e.search.toLowerCase().indexOf(filterQ.toLowerCase()) !== -1;
    }
    function filterDesc() {
      const chan = filterChan === "all" ? "全部通道" : filterChan === "direct" ? "DIRECT" : filterChan;
      return chan + (filterQ ? " · /" + filterQ + "/" : "");
    }
    function readFilters() {
      filterChan = $("#f-chan").value || "all";
      filterQ = $("#f-re").value.trim();
      filterRe = null;
      let bad = false;
      if (filterQ) {
        try { filterRe = new RegExp(filterQ, "i"); }
        catch (_) { filterRe = null; bad = true; }   // 正则写坏了不报错，降级成原文包含匹配
      }
      $("#f-re").classList.toggle("is-bad", bad);
      $("#re-hint").hidden = !bad;
    }

    /* ───────── 视图维护（环形缓冲 → DOM，增量 append + 头部裁剪） ───────── */
    function view() { return $("#log-view"); }
    function removeEmpty() { const ph = $("#log-empty"); if (ph) ph.remove(); }
    function syncEmpty() {
      const v = view();
      if (v.querySelector(".log-row")) { removeEmpty(); return; }
      if ($("#log-empty")) return;
      const d = document.createElement("div");
      d.className = "log-empty";
      d.id = "log-empty";
      d.textContent = buf.length
        ? "当前筛选下没有匹配的日志行"
        : "等待新连接…";
      v.innerHTML = "";
      v.appendChild(d);
    }
    function trimDom() {
      const v = view();
      const oldest = buf.length ? buf[0].seq : Infinity;
      let first = v.querySelector(".log-row");
      while (first && Number(first.dataset.seq) < oldest) {
        first.remove();
        first = v.querySelector(".log-row");
      }
    }
    function scrollBottom() { const v = view(); v.scrollTop = v.scrollHeight; }
    function syncCount() { $("#log-count").textContent = buf.length; }

    function appendEntries(list) {
      if (!list.length) return;
      list.forEach(e => { e.seq = ++seq; buf.push(e); });
      if (buf.length > MAX_LINES) buf.splice(0, buf.length - MAX_LINES);
      const html = list.filter(matchEntry).map(rowHTML).join("");
      if (html) { removeEmpty(); view().insertAdjacentHTML("beforeend", html); }
      trimDom();
      syncEmpty();
      syncCount();
      if (follow) scrollBottom();
    }
    function renderAll() {
      const html = buf.filter(matchEntry).map(rowHTML).join("");
      view().innerHTML = html;
      syncEmpty();
      syncCount();
      if (follow) scrollBottom();
    }

    /* ───────── 暂停 / 清空 / 导出 ───────── */
    function syncPause() {
      $("#pause-ic").innerHTML = paused ? PLAY_IC : PAUSE_IC;
      $("#pause-label").textContent = paused ? "恢复" : "暂停";
      const st = $("#log-state");
      st.hidden = !paused;
      st.textContent = "已暂停" + (pending.length ? " · 挂起 " + pending.length + " 条" : "");
    }
    function togglePause() {
      paused = !paused;
      if (!paused && pending.length) { const p = pending; pending = []; appendEntries(p); }
      syncPause();
    }
    function clearLog() {
      // 只清缓冲与视图；seen 保留，避免把仍活跃的连接当成「新出现」再记一遍
      buf = []; pending = [];
      view().innerHTML = "";
      follow = true;
      $("#log-jump").hidden = true;
      syncEmpty(); syncCount(); syncPause();
    }
    function stamp() {
      const d = new Date();
      return d.getFullYear() + pad2(d.getMonth() + 1) + pad2(d.getDate()) + "-" +
             pad2(d.getHours()) + pad2(d.getMinutes()) + pad2(d.getSeconds());
    }
    function exportLog() {
      const rows = buf.filter(matchEntry);
      if (!rows.length) { toast("当前没有可导出的日志行", { variant: "danger" }); return; }
      const head = [
        "# VPN 管理网关 · 连接记录",
        "# 导出于 " + new Date().toLocaleString(),
        "# 共 " + rows.length + " 行 · 缓冲 " + buf.length + "/" + MAX_LINES + " · 筛选 " + filterDesc(),
        "",
      ];
      const text = head.concat(rows.map(e => e.ts + "  " + e.text)).join("\n") + "\n";
      const url = URL.createObjectURL(new Blob([text], { type: "text/plain;charset=utf-8" }));
      const a = document.createElement("a");
      a.href = url;
      a.download = "vpn-router-" + stamp() + ".log";
      document.body.appendChild(a);
      a.click();
      a.remove();
      setTimeout(() => URL.revokeObjectURL(url), 1000);
      toast("已导出 " + rows.length + " 行日志", { variant: "success" });
    }

    /* ───────── 出口下拉：随通道列表变化重建，保留当前选中 ───────── */
    function syncChanFilter() {
      const sig = chs.map(c => c.id + ":" + c.name).join("|");
      if (sig === chanSig) return;
      chanSig = sig;
      const sel = $("#f-chan");
      const cur = sel.value || "all";
      sel.innerHTML =
        '<option value="all">全部通道</option>' +
        '<option value="direct">DIRECT 兜底直连</option>' +
        chs.map(c => '<option value="ch-' + fb.esc(c.id) + '">ch-' + fb.esc(c.id) + " · " + fb.esc(c.name) + "</option>").join("");
      const has = Array.from(sel.options).some(o => o.value === cur);
      sel.value = has ? cur : "all";
      if (!has) { readFilters(); renderAll(); }
    }

    /* ───────── 指标带 ───────── */
    function rateParts(kb) {
      return kb >= 1024 ? [(kb / 1024).toFixed(1), "MB/s"] : [String(Math.round(kb)), "KB/s"];
    }
    function renderMetrics(up, down, conn) {
      const u = rateParts(up), d = rateParts(down);
      $("#m-up").textContent = u[0];   $("#m-up-u").textContent = u[1];
      $("#m-down").textContent = d[0]; $("#m-down-u").textContent = d[1];
      $("#m-conn").textContent = conn;
      const aliveN = nodes.filter(p => p.alive).length;
      $("#m-nodes").textContent = aliveN;
      $("#m-nodes-u").textContent = "/ " + nodes.length;
      // 指针:0 节点在线指最左(0deg 基准为竖直向上,-90 起点),全在线指最右(+90)
      const ratio = nodes.length ? aliveN / nodes.length : 0;
      $("#m-needle").style.transform = `rotate(${ratio * 180}deg)`;
    }

    /* ───────── tick：拉真实遥测 → 差分出新连接 → 合成日志行 ───────── */
    let tickPending = null, lastCatalog = 0;
    function tick() {
      if (document.hidden) return Promise.resolve();
      if (!tickPending) tickPending = tickOnce().finally(() => { tickPending = null; });
      return tickPending;
    }
    async function tickOnce() {
      let data;
      try {
        if (Date.now() - lastCatalog > 15000) {
          [chs, data, nodes] = await Promise.all([
            api.channels(), api.connections(), api.proxies().then(r => r.proxies || []),
          ]);
          lastCatalog = Date.now();
        } else data = await api.connections();
      } catch (e) {
        // 失败:轮询会每 1.5s 自动重试,但给用户可见的离线指示 + 一条(只一条)友好错误条
        const firstFail = !feedDown;
        setFeed("down", "数据离线 · 重试中");
        showFeedError(e, () => { tick(); });
        if (firstFail)
          toast("遥测数据加载失败,将自动重试", { variant: "danger", action: { label: "立即重试", onClick: () => tick() } });
        return;
      }
      if (document.hidden) return;
      // 成功:更新「更新于」时间戳 + 清掉可能存在的故障错误条(自愈)
      if (!booted) { booted = true; view().innerHTML = ""; }
      clearFeedError();
      setFeed("live", "更新于 " + nowHHMMSS());
      $("#nav-count").textContent = chs.length;
      syncChanFilter();

      // 速率 = 总量差分 / 时间差；累计值变小 = mihomo 重载/重启归零，本帧不计差分（避免负值突刺）
      const now = Date.now();
      let up = 0, down = 0;
      if (prevTotals && prevTs) {
        const dt = (now - prevTs) / 1000 || 1;
        const dUp = data.uploadTotal - prevTotals.up, dDown = data.downloadTotal - prevTotals.down;
        up = dUp >= 0 ? dUp / dt / 1024 : 0;
        down = dDown >= 0 ? dDown / dt / 1024 : 0;
      }
      prevTotals = { up: data.uploadTotal, down: data.downloadTotal }; prevTs = now;

      const list = data.connections || [];
      renderMetrics(up, down, list.length);

      // 差分：本帧活跃 id 里没被记过的就是新连接；随后把 seen 换成本帧活跃集合
      const live = new Set();
      const fresh = [];
      list.forEach(c => {
        const id = String(c.id == null ? "" : c.id);
        if (!id) return;
        live.add(id);
        if (!seen.has(id)) fresh.push(c);
      });
      seen = live;
      if (!fresh.length) { syncEmpty(); return; }
      fresh.sort((a, b) => String(a.start || "").localeCompare(String(b.start || "")));
      const entries = fresh.map(makeEntry);
      if (paused) {
        pending = pending.concat(entries).slice(-MAX_LINES);
        syncPause();
      } else {
        appendEntries(entries);
      }
    }

    /* ───────── 事件绑定 ───────── */
    $("#f-chan").addEventListener("change", () => { readFilters(); renderAll(); });
    $("#f-re").addEventListener("input", () => { readFilters(); renderAll(); });
    $("#btn-pause").addEventListener("click", togglePause);
    $("#btn-clear").addEventListener("click", clearLog);
    $("#btn-export").addEventListener("click", exportLog);
    $("#log-jump").addEventListener("click", () => {
      follow = true;
      $("#log-jump").hidden = true;
      scrollBottom();
    });
    // 用户上滚即脱离跟随；滚回底部自动恢复跟随
    $("#log-view").addEventListener("scroll", () => {
      const v = view();
      const atBottom = v.scrollHeight - v.scrollTop - v.clientHeight < 24;
      follow = atBottom;
      $("#log-jump").hidden = atBottom;
    });

    syncPause();
    boot();
  
