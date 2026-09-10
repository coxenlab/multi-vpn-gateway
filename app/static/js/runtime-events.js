(function () {
  "use strict";

  const byId = (id) => document.getElementById(id);
  const esc = (value) => window.fb && window.fb.esc
    ? window.fb.esc(value)
    : String(value == null ? "" : value).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
  const rank = { debug: 0, info: 1, warn: 2, error: 3 };
  const healthLabels = {
    healthy: "正常",
    forward_dead: "分流链路中断",
    container_down: "分流路由未运行",
    transport_dead: "底座传输中断",
    transport_degraded: "分流可用，底座降级",
    vm_down: "本地引擎断开",
  };
  const state = {
    events: [], history: [], seq: 0, dropped: 0, retainedDays: 14,
    paused: false, auditOnly: false, busy: false, expanded: new Set(), enabled: null,
  };

  function featureVisible(visible) {
    const tab = byId("runtime-logs-tab");
    const panel = byId("runtime-logs-panel");
    if (!tab || !panel) return;
    tab.hidden = !visible;
    panel.hidden = !visible;
    if (window.fb) window.fb.runtimeEventsAvailable = visible;
    if (!visible && panel.classList.contains("active")) {
      document.querySelector('#system-tabs [data-tab="entry"]')?.click();
    }
    if (visible) {
      const requested = location.hash === "#logs" || new URLSearchParams(location.search).get("tab") === "logs";
      if (requested) tab.click();
    }
    if (window.fb && typeof window.fb.checkGateway === "function") window.fb.checkGateway();
  }

  function parseJsonl(text) {
    return String(text || "").split(/\r?\n/).filter(Boolean).flatMap((line) => {
      try { return [JSON.parse(line)]; } catch (_) { return []; }
    });
  }

  function mergeEvents(items) {
    const merged = new Map(state.events.map((event) => [event.seq, event]));
    (items || []).forEach((event) => merged.set(event.seq, event));
    state.events = [...merged.values()].sort((a, b) => a.seq - b.seq).slice(-1000);
  }

  function summaryFeed() {
    const merged = new Map();
    [...state.history, ...state.events].forEach((event) => {
      const key = `${event.ts_ms}|${event.src}|${event.event}|${event.msg}`;
      merged.set(key, event);
    });
    return [...merged.values()].sort((a, b) => (a.ts_ms || 0) - (b.ts_ms || 0));
  }

  function formatTime(event) {
    const date = new Date(event.ts || event.ts_ms);
    if (Number.isNaN(date.getTime())) return "—";
    const pad = (n, width = 2) => String(n).padStart(width, "0");
    const time = `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}.${pad(date.getMilliseconds(), 3)}`;
    const now = new Date();
    return date.toDateString() === now.toDateString()
      ? time
      : `${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${time}`;
  }

  function formatWhen(event) {
    const date = new Date(event.ts || event.ts_ms);
    if (Number.isNaN(date.getTime())) return "未知时间";
    const hm = `${String(date.getHours()).padStart(2, "0")}:${String(date.getMinutes()).padStart(2, "0")}`;
    return date.toDateString() === new Date().toDateString()
      ? `今天 ${hm}`
      : `${date.getMonth() + 1} 月 ${date.getDate()} 日 ${hm}`;
  }

  function formatDuration(ms) {
    const value = Number(ms || 0);
    if (value < 1000) return `${value} 毫秒`;
    if (value < 60000) return `${Math.round(value / 1000)} 秒`;
    const minutes = Math.floor(value / 60000);
    const seconds = Math.round((value % 60000) / 1000);
    return seconds ? `${minutes} 分 ${seconds} 秒` : `${minutes} 分钟`;
  }

  function renderSummary() {
    const feed = summaryFeed();
    const healthEvent = [...state.events].reverse().find((event) =>
      event.event === "health_tick" || event.event === "health_changed" || event.event === "recovered");
    let health = null;
    if (healthEvent?.event === "health_tick") health = healthEvent.detail?.health;
    else if (healthEvent?.event === "health_changed") health = healthEvent.detail?.to;
    else if (healthEvent?.event === "recovered") health = "healthy";
    byId("runtime-current").textContent = healthLabels[health] || "尚无体检记录";

    const recovered = [...feed].reverse().find((event) => event.event === "recovered");
    byId("runtime-last-outage").textContent = recovered
      ? `${formatWhen(recovered)}，持续 ${formatDuration(recovered.detail?.outage_ms)}，自动修复 ${Number(recovered.detail?.heal_count || 0)} 次`
      : "暂无已恢复的中断记录";

    const since = Date.now() - 24 * 60 * 60 * 1000;
    const heals = feed.filter((event) => event.src === "watchdog" && event.event === "heal_start" && Number(event.ts_ms) >= since).length;
    byId("runtime-heals").textContent = `自动修复 ${heals} 次 · 保留 ${state.retainedDays} 天`;
  }

  function filteredEvents() {
    const minLevel = byId("runtime-level").value;
    const source = byId("runtime-source").value;
    const query = byId("runtime-search").value.trim().toLowerCase();
    return state.events.filter((event) => {
      const level = rank[event.level] == null ? rank.info : rank[event.level];
      if (level < rank[minLevel]) return false;
      if (source && event.src !== source) return false;
      if (state.auditOnly && event.src !== "audit") return false;
      if (query && !String(event.msg || "").toLowerCase().includes(query)) return false;
      return true;
    });
  }

  function updateSources() {
    const select = byId("runtime-source");
    const selected = select.value;
    const sources = [...new Set(state.events.map((event) => event.src).filter(Boolean))].sort();
    select.replaceChildren(new Option("全部来源", ""), ...sources.map((source) => new Option(source, source)));
    if (sources.includes(selected)) select.value = selected;
  }

  // 操作记录一眼看到对象:目标名 + 规则 pattern(其余细节在展开的 detail 里)
  function auditTail(event) {
    if (event.src !== "audit" || !event.detail) return "";
    const d = event.detail;
    const bits = [];
    if (d.target_name) bits.push(d.target_name);
    const pat = (d.after && d.after.pattern) || (d.before && d.before.pattern);
    if (pat) bits.push(pat);
    if (d.result === "failed") bits.push("失败");
    return bits.length ? ` <span class="muted">· ${esc(bits.join(" · "))}</span>` : "";
  }

  function renderRows() {
    const scroll = byId("runtime-log-scroll");
    const follow = scroll.scrollHeight - scroll.scrollTop - scroll.clientHeight < 48;
    const events = filteredEvents();
    const body = byId("runtime-log-body");
    body.innerHTML = events.map((event) => {
      const level = rank[event.level] == null ? "info" : event.level;
      const detailId = `runtime-detail-${event.seq}`;
      const expanded = state.expanded.has(event.seq);
      return `<tr class="event-row level-${level}">
          <td class="runtime-time">${esc(formatTime(event))}</td>
          <td><span class="runtime-level ${level}">${esc(level)}</span></td>
          <td class="runtime-src">${esc(event.src || "—")}</td>
          <td><button class="runtime-msg-btn" type="button" data-seq="${event.seq}" data-detail="${detailId}" aria-expanded="${expanded}" aria-controls="${detailId}">${esc(event.msg || event.event || "—")}${auditTail(event)}</button></td>
        </tr>
        <tr class="runtime-detail" id="${detailId}" ${expanded ? "" : "hidden"}><td colspan="4"><pre>${esc(JSON.stringify(event.detail || {}, null, 2))}</pre></td></tr>`;
    }).join("");
    body.querySelectorAll(".runtime-msg-btn").forEach((button) => button.addEventListener("click", () => {
      const detail = document.getElementById(button.dataset.detail);
      const expanded = button.getAttribute("aria-expanded") === "true";
      button.setAttribute("aria-expanded", String(!expanded));
      const seq = Number(button.dataset.seq);
      if (expanded) state.expanded.delete(seq); else state.expanded.add(seq);
      if (detail) detail.hidden = expanded;
    }));
    byId("runtime-empty").hidden = events.length !== 0;
    const paused = state.paused ? "已暂停 · " : "";
    const dropped = state.dropped ? ` · ${state.dropped} 条未能写入磁盘` : "";
    const off = state.enabled === false ? "日志记录已关闭 · " : "";
    byId("runtime-meta").textContent = `${off}${paused}显示 ${events.length} / ${state.events.length} 条${dropped}`;
    if (follow) requestAnimationFrame(() => { scroll.scrollTop = scroll.scrollHeight; });
  }

  function render() {
    updateSources();
    renderSummary();
    renderRows();
  }

  function renderEnabled() {
    const btn = byId("runtime-enabled");
    if (!btn) return;
    if (state.enabled === null) { btn.hidden = true; return; }
    btn.hidden = false;
    btn.textContent = state.enabled ? "日志记录 · 开" : "日志记录 · 关";
    btn.setAttribute("aria-checked", String(state.enabled));
  }

  function applyResponse(response) {
    if (typeof response.enabled === "boolean") { state.enabled = response.enabled; renderEnabled(); }
    mergeEvents(response.events || []);
    state.seq = Number(response.seq || state.seq);
    state.dropped = Number(response.dropped || 0);
    state.retainedDays = Number(response.retained_days || 14);
    render();
  }

  async function poll() {
    if (state.paused || state.busy || document.hidden) return;
    state.busy = true;
    try {
      const response = await window.api.runtimeEvents({ since_seq: state.seq, limit: 1000 });
      if (Number(response.seq || 0) < state.seq) {
        state.events = [];
        state.seq = 0;
        applyResponse(await window.api.runtimeEvents({ limit: 1000 }));
      } else {
        applyResponse(response);
      }
    } catch (error) {
      if (error && error.status === 404) featureVisible(false);
      else byId("runtime-meta").textContent = `自动刷新失败：${error?.reason || error?.message || error}`;
    } finally {
      state.busy = false;
    }
  }

  function bindControls() {
    ["runtime-level", "runtime-source"].forEach((id) => byId(id).addEventListener("change", renderRows));
    byId("runtime-search").addEventListener("input", renderRows);
    byId("runtime-audit-only").addEventListener("click", (event) => {
      state.auditOnly = !state.auditOnly;
      event.currentTarget.setAttribute("aria-pressed", String(state.auditOnly));
      renderRows();
    });
    // 日志开关(桌面版新后端才有;缺端点则隐藏按钮)
    byId("runtime-enabled").addEventListener("click", async (event) => {
      const btn = event.currentTarget;
      const next = !state.enabled;
      if (!next && !await window.fb.confirm("关闭后不再记录任何运行与操作日志，直到重新开启。", { title: "关闭日志记录", confirmLabel: "关闭" })) return;
      btn.disabled = true;
      try {
        const r = await window.api.runtimeEventsSetEnabled(next);
        state.enabled = !!r.enabled;
        renderEnabled();
        window.toast(state.enabled ? "日志记录已开启" : "日志记录已关闭", { variant: "success" });
      } catch (error) {
        window.toast("切换失败：" + (error?.reason || error?.message || error), { variant: "danger" });
      } finally { btn.disabled = false; }
    });
    // 按日期导出:默认区间 = 今天;from/to 由后端裁到保留窗口内
    const today = new Date(); const pad = (n) => String(n).padStart(2, "0");
    const iso = (d) => `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
    byId("runtime-to").value = iso(today);
    byId("runtime-from").value = iso(new Date(today.getTime() - 24 * 60 * 60 * 1000));
    byId("runtime-export").addEventListener("click", () => {
      const from = byId("runtime-from").value, to = byId("runtime-to").value;
      if (!from || !to) return window.toast("请选择日期范围", { variant: "info" });
      if (from > to) return window.toast("开始日期不能晚于结束日期", { variant: "info" });
      const a = document.createElement("a");
      a.href = window.api.runtimeEventsExportUrl(from, to);
      a.download = `vpnmgr-events-${from}_${to}.jsonl`;
      document.body.appendChild(a); a.click(); a.remove();
    });
    byId("runtime-pause").addEventListener("click", (event) => {
      state.paused = !state.paused;
      event.currentTarget.setAttribute("aria-pressed", String(state.paused));
      event.currentTarget.textContent = state.paused ? "继续" : "暂停";
      renderRows();
      if (!state.paused) poll();
    });
    byId("runtime-copy").addEventListener("click", () => {
      const text = filteredEvents().map((event) => JSON.stringify(event)).join("\n");
      if (!text) return window.toast("没有可复制的运行记录", { variant: "info" });
      window.copyText(text, `已复制 ${filteredEvents().length} 条运行记录`);
    });
  }

  async function init() {
    if (!byId("runtime-logs-tab") || !window.api?.runtimeEvents) return;
    bindControls();
    renderEnabled();
    const [eventsResult, historyResult] = await Promise.allSettled([
      window.api.runtimeEvents({ limit: 1000 }),
      window.api.runtimeEventsExport(2),
    ]);
    if (eventsResult.status === "rejected" && eventsResult.reason?.status === 404) {
      featureVisible(false);
      return;
    }
    featureVisible(true);
    if (historyResult.status === "fulfilled") state.history = parseJsonl(historyResult.value);
    if (eventsResult.status === "fulfilled") applyResponse(eventsResult.value);
    else {
      render();
      byId("runtime-meta").textContent = `运行日志暂时不可读：${eventsResult.reason?.reason || eventsResult.reason?.message || eventsResult.reason}`;
    }
    api.poll(poll, 5000, { immediate: false });
  }

  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", init);
  else init();
})();
