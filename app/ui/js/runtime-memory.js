import { h, btn, pop } from './ui.js';
import { openSheet } from './sheet.js';
import { api, isUnavailable } from './api.js';
import { showError, showDone } from './feedback.js';

export const runningMemory = system => Number.isInteger(system?.memory_pressure?.memory_gib) && system.memory_pressure.memory_gib >= 2 ? system.memory_pressure.memory_gib : null;
export const memoryText = system => `运行环境内存不够，客户端被系统结束了${runningMemory(system) ? `（现在 ${runningMemory(system)} GiB）` : ''}`;
export function recommendedMemory(current, host) {
  if (!Number.isFinite(host)) return null;
  const cap = Math.min(16, Math.floor(host / 2));
  return cap > current ? Math.min(current * 2, cap) : null;
}
export const needsMemoryRestart = (selected, system) => system?.runtime_restart_available === true && runningMemory(system) !== null && selected !== runningMemory(system);

// 打开弹层本身没有写操作；只有确认后先保存、再重启。
export function memoryRestart(system, { selected, bigger = false, onDone } = {}) {
  const current = runningMemory(system);
  const cap = Math.min(16, Math.floor((system?.host_memory_gib || 0) / 2));
  let value = selected ?? recommendedMemory(current || 2, system?.host_memory_gib);
  const canChoose = bigger && current !== null && value !== null && cap > current;
  let busy = false;
  const text = h('p', null, '重启会断开所有通道，大约需要 60 秒，之后自动重连刚才在用的通道。需要重新登录的通道（EC/aTrust 等）要再登录一次。');
  const controls = canChoose ? pop(Array.from({ length: cap - current }, (_, i) => ({ value: current + i + 1, label: `${current + i + 1} GiB` })), { value, label: '新的运行环境内存', onChange: v => { value = Number(v); } }) : null;
  const failure = bigger && !canChoose ? h('p', null, '当前无法推荐更大的内存。请到设置中检查运行环境内存和本机可用容量。') : null;
  const confirm = btn('确认重启', { cls: 'pri', disabled: system?.runtime_restart_available !== true || !!failure, onClick: submit });
  const sheet = openSheet({ title: bigger ? '加大内存并重启' : '重启运行环境以应用', body: [text, controls, failure], foot: [btn('取消', { onClick: () => sheet.close() }), confirm] });
  async function submit() {
    if (busy) return;
    busy = true; confirm.disabled = true;
    try {
      const saved = await api.setAppSettings({ vm_memory_gib: value });
      if (isUnavailable(saved)) throw new Error('当前版本不支持内存设置');
      sheet.close();
      const result = await api.runtimeRestart();
      showDone(result, '运行环境已重启，需要登录的通道请重新登录');
      if (onDone) onDone();
    } catch (e) { showError(e); busy = false; confirm.disabled = false; }
  }
}
