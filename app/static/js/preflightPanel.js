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
          const { task_id } = await api.preflightFix("pull_image", { image });
          await poll(task_id);
        }
      } catch (e) { toast("修复失败:" + e.message, false); }
      finally { busy = false; }
    }

    function poll(taskId) {
      return new Promise((resolve) => {
        const tip = document.createElement("div");
        tip.className = "banner info"; host.prepend(tip);
        const iv = setInterval(async () => {
          let st; try { st = await api.preflightFixStatus(taskId); } catch { return; }
          tip.textContent = st.progress || st.status;
          if (st.status === "done") { clearInterval(iv); toast("镜像就绪"); await run(); resolve(); }
          else if (st.status === "error") { clearInterval(iv); tip.className = "banner danger"; tip.textContent = st.error || "拉取失败"; resolve(); }
        }, 2000);
      });
    }

    return { run };
  };
})();
