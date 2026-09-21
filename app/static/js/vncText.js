/* vncText.js — 宿主 → VNC 容器的文本注入(复制粘贴打通)。
 *
 * 原理:登录 iframe 与本页不同源,驱动不了 iframe 内的 RFB;这里用 vendor 的 noVNC
 * core 另起一条 **shared** RFB 连接(不顶掉 iframe 会话),把文本逐字符转成 X keysym
 * 经 KeyEvent 模拟键入到容器当前聚焦的控件,并同步写入容器剪贴板(ClientCutText,
 * Xtigervnc 会落到 X selection,容器内 Ctrl+V 可用)。文本只走 浏览器→websockify→VNC,
 * 不经任何后端 API / 命令行(命门 #5)。
 * 命门 #1 不动摇:本模块只是输入辅助,登录成功与否仍只认后端 SOCKS5 探活。
 *
 * 依赖:页面控制器显式 import；window.vncText 作为兼容出口保留。
 */
import RFB from "../vendor/novnc/core/rfb.js";
import keysyms from "../vendor/novnc/core/input/keysymdef.js";
import KeyTable from "../vendor/novnc/core/input/keysym.js";

(() => {
  "use strict";

  const KEY_GAP_MS = 25;      // 字符间隔:EC(Qt/Xvfb)吃太快的连击会丢键
  const CONNECT_TIMEOUT_MS = 10000;

  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

  /* 从后端 login url(vnc.html?path=websockify/&password=…)推导 WS 端点与 VNC 密码。
   * 两栈(web loopback 映射 / 桌面 SSH 转发)给的 url 同形,统一在此解析。 */
  function wsInfo(loginUrl) {
    const u = new URL(loginUrl, location.href);
    const path = u.searchParams.get("path") || "websockify/";
    const scheme = u.protocol === "https:" ? "wss" : "ws";
    return {
      wsUrl: `${scheme}://${u.host}/${path.replace(/^\//, "")}`,
      password: u.searchParams.get("password") || "",
    };
  }

  function charKeysym(ch) {
    if (ch === "\n" || ch === "\r") return KeyTable.XK_Return;
    if (ch === "\t") return KeyTable.XK_Tab;
    return keysyms.lookup(ch.codePointAt(0));
  }

  /* 建临时 shared RFB 连接 → 键入 text(+写容器剪贴板)→ 断开。resolve 键入的字符数。 */
  async function send(loginUrl, text, { signal } = {}) {
    if (!text) return 0;
    const checkActive = () => { if (signal?.aborted) throw new DOMException("登录视图已关闭", "AbortError"); };
    checkActive();
    const { wsUrl, password } = wsInfo(loginUrl);

    // RFB 需要一个挂载点;隐藏容器,不渲给用户(这条连接只发输入)。
    const holder = document.createElement("div");
    holder.style.cssText = "position:fixed;left:-9999px;width:2px;height:2px;overflow:hidden;";
    document.body.appendChild(holder);

    let rfb, pasted = false;
    const disconnect = () => {
      if (pasted) { try { rfb?.clipboardPasteFrom(" "); } catch (_e) { /* 连接可能已关闭 */ } }
      try { rfb?.disconnect(); } catch (_e) { /* 已断开 */ }
    };
    signal?.addEventListener("abort", disconnect, { once: true });
    try {
      rfb = new RFB(holder, wsUrl, { credentials: { password }, shared: true });
      rfb.clipViewport = false;
      await new Promise((resolve, reject) => {
        const finish = (error) => {
          clearTimeout(timer);
          signal?.removeEventListener("abort", abort);
          rfb.removeEventListener("connect", connect);
          rfb.removeEventListener("securityfailure", failure);
          rfb.removeEventListener("disconnect", ended);
          if (error) reject(error); else resolve();
        };
        const connect = () => finish();
        const failure = e => finish(new Error("VNC 认证失败:" + (e.detail?.reason || "密码不符")));
        const ended = () => finish(signal?.aborted
          ? new DOMException("登录视图已关闭", "AbortError") : new Error("远程桌面连接被断开"));
        const abort = () => finish(new DOMException("登录视图已关闭", "AbortError"));
        const timer = setTimeout(() => finish(new Error("连接远程桌面超时")), CONNECT_TIMEOUT_MS);
        rfb.addEventListener("connect", connect);
        rfb.addEventListener("securityfailure", failure);
        rfb.addEventListener("disconnect", ended);
        signal?.addEventListener("abort", abort, { once: true });
      });
      await sleep(300);                 // 服务端 session 就绪缓冲
      checkActive();
      rfb.clipboardPasteFrom(text);     // 顺带写容器剪贴板(Ctrl+V 兜底)
      pasted = true;
      let typed = 0;
      for (const ch of text) {
        checkActive();
        const ks = charKeysym(ch);
        if (!ks) continue;              // 无对应 keysym 的控制字符跳过
        rfb.sendKey(ks, null);
        typed++;
        await sleep(KEY_GAP_MS);
      }
      await sleep(150);                 // 让尾部 KeyEvent 冲出去再断
      checkActive();
      return typed;
    } finally {
      // 成功、失败或关闭视图都尽力清掉已写入的剪贴板，再断开临时连接。
      disconnect();
      signal?.removeEventListener("abort", disconnect);
      holder.remove();
    }
  }

  /* 在 host 元素里装「发送文本到容器」输入条。getUrl() 返回当前登录窗 url(未就绪返回空)。
   * 键入内容只进容器,不留档(备忘录只收用户手写内容)。 */
  function mountBar(host, getUrl, { signal } = {}) {
    if (!host || host.querySelector(".vnc-sendbar")) return;
    const bar = document.createElement("div");
    bar.className = "vnc-sendbar";
    bar.innerHTML = `
      <div class="row" style="gap:var(--space-2);">
        <input class="input" type="text" autocomplete="off" spellcheck="false"
               placeholder="粘贴要发进容器的文本（密码 / 验证码…）" style="flex:1;" />
        <button class="btn btn-secondary" type="button" style="flex:none;">键入到容器</button>
      </div>
      <p class="note" style="margin:var(--space-1) 0 0;">先在登录窗里点选目标输入框，再点「键入到容器」；文本同时写入容器剪贴板，容器内 Ctrl+V 也可粘贴。</p>`;
    const input = bar.querySelector("input");
    const btn = bar.querySelector("button");
    signal?.addEventListener("abort", () => { input.value = ""; }, { once: true });
    const doSend = async () => {
      if (signal?.aborted || btn.disabled) return;
      const text = input.value;
      if (!text) { input.focus(); return; }
      const url = getUrl();
      if (!url) { window.toast?.("登录窗还没就绪，稍候再试", { variant: "danger" }); return; }
      btn.disabled = true;
      const label = btn.textContent;
      btn.textContent = "键入中…";
      try {
        const n = await send(url, text, { signal });
        if (signal?.aborted) return;
        window.toast?.(`已向容器键入 ${n} 个字符`);
      } catch (e) {
        if (!signal?.aborted) window.toast?.("键入失败:" + (e?.message || e), { variant: "danger" });
      } finally {
        btn.disabled = false;
        btn.textContent = label;
      }
    };
    btn.addEventListener("click", doSend);
    input.addEventListener("keydown", (e) => { if (e.key === "Enter") doSend(); });
    host.appendChild(bar);
  }

  window.vncText = { send, mountBar };
})();

export const vncText = window.vncText;
