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
    paused: false, healOnly: false, busy: false, expanded: new Set(),
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
    byId("runtime-heals").textContent = `自动修复 ${heals} 次 · 日志保留 ${state.retainedDays} 天`;
  }

  function filteredEvents() {
    const minLevel = byId("runtime-level").value;
    const source = byId("runtime-source").value;
    const query = byId("runtime-search").value.trim().toLowerCase();
    return state.events.filter((event) => {
      const level = rank[event.level] == null ? rank.info : rank[event.level];
      if (level < rank[minLevel]) return false;
      if (source && event.src !== source) return false;
      if (state.healOnly && !["watchdog", "tunnel"].includes(event.src)) return false;
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
          <td><button class="runtime-msg-btn" type="button" data-seq="${event.seq}" data-detail="${detailId}" aria-expanded="${expanded}" aria-controls="${detailId}">${esc(event.msg || event.event || "—")}</button></td>
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
    const dropped = state.dropped ? ` · 写盘队列丢弃 ${state.dropped} 条` : "";
    byId("runtime-meta").textContent = `${paused}显示 ${events.length} / ${state.events.length} 条 · 游标 ${state.seq}${dropped}`;
    if (follow) requestAnimationFrame(() => { scroll.scrollTop = scroll.scrollHeight; });
  }

  function render() {
    updateSources();
    renderSummary();
    renderRows();
  }

  function applyResponse(response) {
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
    byId("runtime-heal-only").addEventListener("click", (event) => {
      state.healOnly = !state.healOnly;
      event.currentTarget.setAttribute("aria-pressed", String(state.healOnly));
      renderRows();
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
    setInterval(poll, 5000);
  }

  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", init);
  else init();
})();
