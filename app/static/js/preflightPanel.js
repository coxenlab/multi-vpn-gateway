import { api } from "./api.js";
import { fb } from "./feedback.js";
import { toast } from "./app.js";
// 共享体检面板:渲染 checks + 接 fix 按钮 + 轮询 + 自动重检。
// 用法:const pf = PreflightPanel(hostEl, {vpnType, version, onPass}); pf.run();
(function () {
  const ICON = { pass: "✓", warn: "!", fail: "✕", skip: "–" };
  const TUTORIAL_PAGE = {
    install_docker: "tutorials/install-docker.html",
    switch_registry_mirror: "tutorials/registry-mirror.html",
  };
  // 依赖由模块显式加载，转义统一走共享反馈层。
  const esc = fb.esc;

  function row(c) {
    const fix = c.fix && c.fix.kind === "auto"
      ? `<button class="btn btn-secondary btn-sm" data-fix="${esc(c.fix.action)}" data-image="${esc((c.fix.params && c.fix.params.image) || "")}">${esc(c.fix.label || "修复")}</button>`
      : c.fix && c.fix.kind === "tutorial"
      ? `<a class="btn btn-secondary btn-sm" target="_blank" href="${TUTORIAL_PAGE[c.fix.action] || "#"}">${esc(c.fix.label || "查看教程")}</a>`
      : "";
    return `<div class="chk chk-${c.status}">
      <span class="chk-ic">${ICON[c.status] || "?"}</span>
      <div class="chk-body"><div class="chk-title">${esc(c.title)}</div>
        <div class="chk-detail">${esc(c.detail || "")}</div></div>
      <div class="chk-act">${fix}</div></div>`;
  }

  // A failed progress query must resume the existing task, not start another download.
  const pullTasks = new Map();
  window.pullImageTask = async function (image, onProgress) {
    let id = pullTasks.get(image);
    if (!id) {
      const result = await api.preflightFix("pull_image", { image });
      if (typeof result.task_id !== "string" || !result.task_id) throw new Error("未收到下载任务，请刷新镜像清单核对");
      id = result.task_id; pullTasks.set(image, id);
    }
    return new Promise(function (resolve, reject) {
        const pendingError = e => { e.pullTaskPending = true; reject(e); };
        let failures = 0;
        const deadline = Date.now() + 20 * 60 * 1000;
        const stop = api.poll(async function () {
          if (Date.now() > deadline) { stop(); pendingError(new Error("等待下载结果超时，任务可能仍在进行，可稍后查看进度")); return; }
          let st;
          try { st = await api.preflightFixStatus(id); failures = 0; }
          catch (e) {
            if (e.status === 404) {
              pullTasks.delete(image); stop();
              reject(new Error("此下载记录已失效，请刷新镜像清单核对结果后再操作"));
            } else if (++failures >= 5) { stop(); pendingError(e); }
            return;
          }
          if (onProgress) onProgress(st);
          if (st.status === "done") { pullTasks.delete(image); stop(); resolve(st); }
          else if (st.status === "error") { pullTasks.delete(image); stop(); reject(new Error(st.error || "拉取失败")); }
        }, 2000);
    });
  };

  window.PreflightPanel = function (host, opts) {
    opts = opts || {};
    let busy = false;

    async function run() {
      host.innerHTML = `<div class="chk-loading">体检中…</div>`;
      let res;
      try { res = await api.preflight(opts.vpnType, opts.version, opts.scope); }
      catch (e) { host.innerHTML = `<div class="banner danger">体检失败:${esc(e.message)}</div>`; return null; }
      host.innerHTML = res.checks.map(row).join("");
      host.querySelectorAll("[data-fix]").forEach((b) =>
        b.addEventListener("click", () => doFix(b.dataset.fix, b.dataset.image)));
      if (res.overall !== "fail" && typeof opts.onPass === "function") opts.onPass(res);
      return res;
    }

    async function doFix(action, image) {
      if (busy) return; busy = true;
      try {
        if (action === "create_network") {
          await api.preflightFix("create_network", {});
          toast("网络已创建", { variant: "success" }); await run();
        } else if (action === "pull_image") {
          const tip = document.createElement("div");
          tip.className = "banner info"; host.prepend(tip);
          try {
            const sp = window.fb && fb.spinner ? fb.spinner("拉取镜像…") : null;
            if (sp) { tip.innerHTML = ""; tip.appendChild(sp); }
            await pullImageTask(image, function (st) {
              const txt = st.progress || st.status || "拉取中…";
              if (sp) { const m = sp.querySelector("span:last-child"); if (m) m.textContent = txt; else tip.textContent = txt; }
              else tip.textContent = txt;
            });
            toast("镜像就绪", { variant: "success" }); await run();
          } catch (e) {
            tip.remove();
            // 查询失败保留任务；只有确定失败才重新下载。
            if (window.fb && fb.errorBanner) {
              const wrap = document.createElement("div"); host.prepend(wrap);
              fb.errorBanner(wrap, {
                fromError: e, retryLabel: pullTasks.has(image) ? "查看进度" : "重试",
                onRetry: function () { wrap.remove(); doFix("pull_image", image); },
              });
            } else {
              tip.className = "banner danger"; tip.textContent = e.message; host.prepend(tip);
            }
          }
        }
      } catch (e) { toast("修复失败:" + (window.fb && fb.friendlyError ? fb.friendlyError(e).title : e.message), { variant: "danger" }); }
      finally { busy = false; }
    }

    return { run };
  };
})();

export const { PreflightPanel, pullImageTask } = window;
