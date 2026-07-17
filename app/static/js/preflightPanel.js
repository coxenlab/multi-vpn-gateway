// 共享体检面板:渲染 checks + 接 fix 按钮 + 轮询 + 自动重检。
// 用法:const pf = PreflightPanel(hostEl, {vpnType, version, onPass}); pf.run();
(function () {
  const ICON = { pass: "✓", warn: "!", fail: "✕", skip: "–" };
  const TUTORIAL_PAGE = {
    install_docker: "tutorials/install-docker.html",
    switch_registry_mirror: "tutorials/registry-mirror.html",
  };
  // 转义统一走 feedback.js 的 fb.esc(全站唯一实现);本文件早于 feedback.js 加载,但 row()/run()
  // 均运行时触发(其时 fb 已就位),与本文件既有 `window.fb && fb.*` 惯用法一致;反馈层缺失则不渲染(fail-closed)。
  const esc = (s) => (window.fb && window.fb.esc) ? window.fb.esc(s) : "";

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

  // 共享:发起一次镜像拉取任务并轮询到结束。onProgress(st) 每 2s 调一次。
  window.pullImageTask = function (image, onProgress) {
    return new Promise(function (resolve, reject) {
      api.preflightFix("pull_image", { image }).then(function (r) {
        const iv = setInterval(async function () {
          let st;
          try { st = await api.preflightFixStatus(r.task_id); } catch (e) { return; }
          if (onProgress) onProgress(st);
          if (st.status === "done") { clearInterval(iv); resolve(st); }
          else if (st.status === "error") { clearInterval(iv); reject(new Error(st.error || "拉取失败")); }
        }, 2000);
      }, reject);
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
            // 失败带「重拉」:复用 fb.errorBanner(归地基管的共享组件)
            if (window.fb && fb.errorBanner) {
              const wrap = document.createElement("div"); host.prepend(wrap);
              fb.errorBanner(wrap, {
                fromError: e, retryLabel: "重拉",
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
