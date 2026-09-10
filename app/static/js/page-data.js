import { api } from "./api.js";

// 列表是页面主体；系统状态失败时返回未知状态,仍显示已成功读取的主数据。
export async function loadWithSystem(loadPrimary = api.channels) {
  const [primary, system] = await Promise.allSettled([loadPrimary(), api.system()]);
  if (primary.status === "rejected") throw primary.reason;
  return {
    data: primary.value,
    system: system.status === "fulfilled" ? system.value : {},
    systemError: system.status === "rejected" ? system.reason : null,
  };
}
