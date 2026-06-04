// 共享体检面板:渲染 checks + 接 fix 按钮 + 轮询 + 自动重检。
// 用法:const pf = PreflightPanel(hostEl, {vpnType, version, onPass}); pf.run();
(function () {
  const ICON = { pass: "✓", warn: "!", fail: "✕", skip: "–" };
  const TUTORIAL_PAGE = {
    install_docker: "tutorials/install-docker.html",
    switch_registry_mirror: "tutorials/registry-mirror.html",
  };

  function row(c) {
    const fix = c.fix && c.fix.kind === "auto"
      ? `<button class="btn btn-secondary btn-sm" data-fix="${c.fix.action}" data-image="${(c.fix.params && c.fix.params.image) || ""}">${c.fix.label || "修复"}</button>`
      : c.fix && c.fix.kind === "tutorial"
      ? `<a class="btn btn-secondary btn-sm" target="_blank" href="${TUTORIAL_PAGE[c.fix.action] || "#"}">${c.fix.label || "查看教程"}</a>`
      : "";
    return `<div class="chk chk-${c.status}">
      <span class="chk-ic">${ICON[c.status] || "?"}</span>
      <div class="chk-body"><div class="chk-title">${c.title}</div>
        <div class="chk-detail">${c.detail || ""}</div></div>
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
      catch (e) { host.innerHTML = `<div class="banner danger">体检失败:${e.message}</div>`; return null; }
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
          toast("网络已创建"); await run();
        } else if (action === "pull_image") {
          const tip = document.createElement("div");
          tip.className = "banner info"; host.prepend(tip);
          try {
            await pullImageTask(image, function (st) { tip.textContent = st.progress || st.status; });
            toast("镜像就绪"); await run();
          } catch (e) { tip.className = "banner danger"; tip.textContent = e.message; }
        }
      } catch (e) { toast("修复失败:" + e.message, false); }
      finally { busy = false; }
    }

    return { run };
  };
})();
