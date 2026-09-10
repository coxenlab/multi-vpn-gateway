import { api } from "./api.js";
import { fb } from "./feedback.js";

// 只显示看门狗缓存；打开详情不会触发宿主 socket 扫描或修复。
export function setupUsernetStatus() {
  const section = document.getElementById("usernet-sect");
  const host = document.getElementById("usernet-status");
  const render = (system) => {
    section.hidden = !Object.hasOwn(system, "usernet");
    if (section.hidden) return;
    const sample = system.usernet;
    if (!sample) { host.textContent = "等待本地引擎首次采样。"; return; }
    const time = new Date(sample.checked_at);
    const stale = !Number.isFinite(time.getTime()) || Date.now() - time.getTime() > 150000;
    const runtime = sample.runtime || {};
    const observed = sample.status === "observed";
    const state = stale ? "历史采样" : !observed ? "采样不可用"
      : sample.at_default_limit ? "拨号压力偏高，需结合出站检测定位" : "采样已更新";
    const guard = system.egress_guard_applied == null ? "尚未核对"
      : `${system.egress_guard_applied ? "最近下发成功" : "最近核对失败"} · ${system.egress_guard_checked_at || ""}`;
    const rows = [
      ["观测时间", Number.isFinite(time.getTime()) ? time.toLocaleString("zh-CN", { hour12: false }) : "未知"],
      ["关联网络 / 进程", `${sample.network || "未知"} / ${sample.pid || "未知"}`],
      ["等待建连数", observed ? String(sample.syn_sent) : "未知"],
      ["版本默认上限", runtime.default_dial_limit == null ? "未知，不推断是否占满" : String(runtime.default_dial_limit)],
      ["运行版本", `Lima ${runtime.lima || "未知"} · gvisor-tap-vsock ${runtime.gvisor_tap_vsock || "未知"}${runtime.gvisor_replaced ? "（自定义替换）" : ""}`],
      ...(runtime.gvisor_patch ? [["出站补丁", runtime.gvisor_patch]] : []),
      ["私网基础防护", guard],
    ];
    host.innerHTML = `<p class="t-sm">${fb.esc(state)}</p><dl class="kv-list">${rows.map(([label, value]) => `<dt>${fb.esc(label)}</dt><dd>${fb.esc(value)}</dd>`).join("")}</dl>
      <p class="t-xs muted">${fb.esc(sample.attribution)}。等待建连数只是候选信号，不代表 VPN 已连接，也不会据此自动重启。</p>
      ${sample.reason || sample.runtime_error ? `<p class="t-xs muted">采样说明：${fb.esc(sample.reason || sample.runtime_error)}</p>` : ""}
      ${(sample.destinations || []).length ? `<table class="table"><thead><tr><th>等待建连目标</th><th>数量</th></tr></thead><tbody>${sample.destinations.map(d => `<tr><td class="mono">${fb.esc(d.address)}</td><td>${fb.esc(d.count)}</td></tr>`).join("")}</tbody></table>` : ""}
      ${sample.destinations_omitted ? `<p class="t-xs muted">另有 ${fb.esc(sample.destinations_omitted)} 个目标未展开。</p>` : ""}`;
  };
  const refresh = async () => {
    if (!section.open || document.hidden || !section.getClientRects().length) return;
    try { render(await api.system()); }
    catch (_) { host.textContent = "状态读取失败，请收起后重新打开。"; }
  };
  section.addEventListener("toggle", refresh);
  api.poll(refresh, 60000, { immediate: false });
  return render;
}
