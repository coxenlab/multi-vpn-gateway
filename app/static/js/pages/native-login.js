import { api } from "../api.js";
import { waitNovncReady } from "../app.js";
import { vncText } from "../vncText.js";
import { createVncLifecycle } from "../vnc-lifecycle.js";

const cid = new URLSearchParams(location.search).get("id");
const host = document.querySelector("#login-host");
const state = document.querySelector("#state");
let selected = false;
const view = createVncLifecycle({
  channelId: () => cid,
  isActive: () => selected,
  close: () => { host.replaceChildren(); document.querySelector("#sendbar").replaceChildren(); },
  open: async ({ signal, current, login }) => {
    state.textContent = "正在打开登录窗口…";
    try {
      const result = await login();
      if (!current()) return;
      if (!result.url || result.login_mode === "headless") { state.textContent = "此通道没有交互登录窗口。"; return; }
      if (!await waitNovncReady(result.url, 60, { signal })) {
        if (current()) state.textContent = "登录窗口尚未就绪，请稍后重新打开。";
        return;
      }
      if (!current()) return;
      const frame = document.createElement("iframe"); frame.title = "VPN 登录窗口"; frame.src = result.url; host.replaceChildren(frame);
      state.textContent = "完成登录后，请回到通道概览检测内网连通。";
      vncText.mountBar(document.querySelector("#sendbar"), () => result.url, async text => {
        const value = text.trim(); if (!value || !current()) return;
        const stored = await api.noteGet(cid); if (!current()) return;
        const note = stored.note || "";
        if (!note.includes(value)) await api.noteSet(cid, (note ? note + "\n" : "") + `[${new Date().toLocaleString("zh-CN")}] 键入：${value}`);
      }, { signal });
    } catch (_) { if (current()) state.textContent = "登录窗口打开失败，可以重新打开。"; }
  },
});
const retry = document.querySelector("#retry");
async function openCurrentChannel() {
  view.pause(); selected = false; retry.disabled = true;
  state.textContent = "正在读取通道…";
  try {
    const ch = (await api.channels()).find(ch => ch.id === cid);
    selected = !!ch && !ch.stop_pending && !["stopped", "error", "down"].includes(ch.status) && ch.replacement?.phase !== "queued";
    if (selected) { retry.disabled = false; view.sync(); }
    else state.textContent = ch ? "请先启动这条通道。" : "通道不存在。";
  } catch (_) {
    state.textContent = "无法读取通道，可以重新打开。";
    retry.disabled = false;
  }
}
retry.addEventListener("click", openCurrentChannel);
openCurrentChannel();
