/* 系统与接入页的环境诊断 / 镜像源 / 镜像清单共享逻辑。 */
(function () {
  "use strict";
    function loadDiagTypes() {
      const sel = document.getElementById("diag-type");
      sel.disabled = true;
      const base = sel.options[0];   // 保留首项「(只查环境，不挑类型)」
      api.vpnTypes().then(function (list) {
        sel.innerHTML = "";
        sel.appendChild(base);
        list.forEach(function (a) {
          const o = document.createElement("option");
          o.value = a.key;
          o.textContent = a.label;
          sel.appendChild(o);
        });
        sel.disabled = false;
      }).catch(function (e) {
        sel.disabled = false;   // 失败不锁死：仍可「只查环境」跑体检
        toast("VPN 类型列表加载失败", {
          variant: "danger",
          action: { label: "重试", onClick: loadDiagTypes },
        });
      });
    }
    loadDiagTypes();

    function runDiag() {
      const btn = document.getElementById("diag-run");
      const t = document.getElementById("diag-type").value;
      const host = document.getElementById("diag-host");
      btn.disabled = true; const orig = btn.textContent;
      btn.replaceChildren(fb.spinner("体检中…"));
      // PreflightPanel.run() 自管 host(loading→结果/失败 banner),失败时 resolve(null)。
      Promise.resolve(
        PreflightPanel(host, { vpnType: t || undefined, scope: "full" }).run()
      ).then(function (res) {
        if (res === null) {
          // 体检请求本身失败:清掉 preflightPanel 渲染的原始失败行,只留一条可重试的友好条。
          host.innerHTML = "";
          fb.errorBanner(host, {
            title: "体检未能完成",
            message: "环境体检请求失败,可能是后端或 Docker 引擎未就绪。",
            hint: "稍后重试;若反复失败检查 Docker / 引擎状态。",
            retryLabel: "重新体检", onRetry: runDiag,
          });
        }
      }).finally(function () {
        btn.disabled = false; btn.textContent = orig;
      });
    }
    document.getElementById("diag-run").addEventListener("click", runDiag);

    api.system().then(function (sys) {
      const fp = document.getElementById("foot-port");
      if (fp && sys.mihomo_port) fp.textContent = ":" + sys.mihomo_port;
    }).catch(function () {});

    async function renderMirrors() {
      const box = document.getElementById("mir-list");
      box.innerHTML = "";
      box.appendChild(fb.skeleton(3));   // 加载占位
      let ms;
      try {
        ms = await api.mirrors();
      } catch (e) {
        box.innerHTML = "";
        fb.errorBanner(box, { fromError: e, retryLabel: "重新加载", onRetry: renderMirrors });
        return;
      }
      box.innerHTML = ms.map(m => `
        <div class="chk" data-id="${m.id}">
          <label class="row" style="gap:6px;flex:1;"><input type="checkbox" ${m.enabled ? "checked" : ""} data-toggle> ${fb.esc(m.host)}</label>
          <span class="chk-detail" data-lat>p${m.priority}</span>
          <div class="chk-act">
            <button class="btn btn-secondary btn-sm" data-test>测速</button>
            <button class="btn btn-secondary btn-sm" data-up>↑</button>
            <button class="btn btn-danger btn-sm" data-del>删</button>
          </div></div>`).join("");
      box.querySelectorAll(".chk").forEach((row, idx) => {
        const id = +row.dataset.id;
        const toggle = row.querySelector("[data-toggle]");
        toggle.addEventListener("change", e => {
          const on = e.target.checked;
          api.patchMirror(id, { enabled: on })
            .then(() => { toast(on ? "已启用镜像源" : "已禁用镜像源", { variant: "info" }); renderMirrors(); })
            .catch(err => {
              e.target.checked = !on;   // 失败回滚视图
              toast("切换失败", { variant: "danger", action: { label: "重试", onClick: () => { toggle.checked = on; toggle.dispatchEvent(new Event("change")); } } });
            });
        });
        row.querySelector("[data-del]").addEventListener("click", () =>
          api.delMirror(id)
            .then(() => { toast("已删除", { variant: "success" }); renderMirrors(); })
            .catch(err => toast("删除失败", { variant: "danger", action: { label: "重试", onClick: () => row.querySelector("[data-del]").click() } })));
        row.querySelector("[data-up]").addEventListener("click", (e) => {
          if (idx === 0) return;
          const prev = ms[idx - 1];
          const btn = e.currentTarget; btn.disabled = true;
          Promise.all([api.patchMirror(id, { priority: prev.priority }),
                       api.patchMirror(prev.id, { priority: ms[idx].priority })])
            .then(() => { toast("已上移", { variant: "info" }); renderMirrors(); })
            .catch(err => { btn.disabled = false; toast("调整优先级失败", { variant: "danger", action: { label: "重试", onClick: () => btn.click() } }); });
        });
        row.querySelector("[data-test]").addEventListener("click", async (e) => {
          const btn = e.currentTarget;
          btn.disabled = true; const orig = btn.textContent; btn.textContent = "测速中…";
          const latEl = row.querySelector("[data-lat]");
          try {
            const r = await api.testMirror(ms[idx].host);
            latEl.textContent = r.reachable ? `${r.latency_ms}ms` : "不可达";
          } catch (err) {
            latEl.textContent = "测速失败";
            toast("测速失败", { variant: "danger", action: { label: "重试", onClick: () => btn.click() } });
          } finally {
            btn.disabled = false; btn.textContent = orig;
          }
        });
      });
    }
    document.getElementById("mir-add").addEventListener("click", () => {
      const input = document.getElementById("mir-host");
      const h = input.value.trim();
      if (!h) return;
      const btn = document.getElementById("mir-add");
      btn.disabled = true; const orig = btn.textContent; btn.textContent = "添加中…";
      api.addMirror(h)
        .then(() => { input.value = ""; toast("已添加镜像源", { variant: "success" }); return renderMirrors(); })
        .catch(e => toast("添加失败", { variant: "danger", action: { label: "重试", onClick: () => btn.click() } }))
        .finally(() => { btn.disabled = false; btn.textContent = orig; });
    });
    renderMirrors();

    /* ── 镜像清单 ── */
    const COPY_ICON = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><rect x="9" y="9" width="11" height="11" rx="2"/><path d="M5 15V5a2 2 0 012-2h10"/></svg>';
    const KIND_LABEL = { pull: "上游拉取", build: "自建镜像", compose: "compose 构建" };
    // 转义统一走 fb.esc(全站唯一实现);下方各调用均在运行时(renderImages/imgCard),feedback.js 已加载。

    function imgCommands(e, arch, tag, mirrors) {
      const out = [];
      if (e.kind === "pull") {
        const plat = `--platform linux/${arch}`;
        const ref = `${e.repo}:${tag}`;
        out.push({ h: "直连拉取", t: `docker pull ${plat} ${ref}` });
        (mirrors || []).forEach(m => out.push({
          h: `经镜像源 ${m}`,
          t: `docker pull ${plat} ${m}/${ref}\ndocker tag ${m}/${ref} ${ref}\ndocker rmi ${m}/${ref}`,
        }));
        const tar = ref.replace(/[\/:]/g, "_") + ".tar";
        out.push({ h: "离线导出 / 导入", t: `# 有网机器上导出\ndocker save ${ref} -o ${tar}\n# 拷到目标机后导入\ndocker load -i ${tar}` });
      } else if (e.kind === "build") {
        out.push({ h: "本机构建(仓库根执行)", t: `docker build -t ${e.image} ${e.build_context}` });
        out.push({ h: "多架构构建(需 buildx)", t: `# --load 仅单平台;多架构改 --push 到 registry 或分架构 save\ndocker buildx build --platform linux/amd64,linux/arm64 -t ${e.image} ${e.build_context}` });
        const tar = e.image.replace(/[\/:]/g, "_") + ".tar";
        out.push({ h: "离线导出 / 导入", t: `docker save ${e.image} -o ${tar}\ndocker load -i ${tar}` });
      } else {
        out.push({ h: "构建 / 重启(仓库根执行)", t: `docker compose build app\n# 或重建并重启后端\ndocker compose up -d --build app` });
      }
      return out;
    }

    function imgCard(e, hostArch, mirrors) {
      const card = document.createElement("div");
      card.className = "card card-pad stack-3";
      card.style.marginBottom = "var(--space-4)";
      const archOpts = (e.arch && e.arch.length) ? e.arch : [hostArch];
      const firstUsable = (e.versions.find(v => v.usable_here) || e.versions[0] || {});
      const state = {
        arch: archOpts.includes(hostArch) ? hostArch : archOpts[0],
        tag: e.versioned ? (firstUsable.tag || "latest") : (e.tag || "latest"),
      };
      const presentBadge = e.present === true ? `<span class="badge is-logged_in">本机已就绪</span>`
        : e.present === false ? `<span class="badge is-stopped">未就绪</span>` : "";

      card.innerHTML = `
        <div class="row" style="justify-content:space-between; align-items:flex-start; gap:var(--space-3);">
          <div>
            <div class="row" style="gap:var(--space-2); align-items:center; flex-wrap:wrap;">
              <strong>${fb.esc(e.title)}</strong>
              <span class="tag mono">${fb.esc(e.display)}</span>
              ${presentBadge}
            </div>
            <div class="t-xs muted" style="margin-top:4px;">
              ${fb.esc(KIND_LABEL[e.kind] || e.kind)}${e.used_by && e.used_by.length ? " · 用于 " + fb.esc(e.used_by.join("、")) : ""}${e.build_context ? " · 上下文 " + fb.esc(e.build_context) : ""}
            </div>
          </div>
          <div class="img-actions"></div>
        </div>
        <div class="img-controls row" style="gap:var(--space-4); flex-wrap:wrap;"></div>
        <div class="img-cmds stack-3"></div>`;

      const controls = card.querySelector(".img-controls");
      const cmds = card.querySelector(".img-cmds");
      const actions = card.querySelector(".img-actions");
      let pullBtn = null;

      if (e.versioned && e.versions.length) {
        const sel = document.createElement("select");
        sel.className = "input"; sel.style.maxWidth = "220px";
        sel.innerHTML = e.versions.map(v =>
          `<option value="${fb.esc(v.tag)}" ${v.tag === state.tag ? "selected" : ""}>${fb.esc(v.tag)}${v.arch && v.arch.length ? ` (${fb.esc(v.arch.join("/"))})` : ""}${v.usable_here ? "" : " ·本机不可用"}</option>`).join("");
        sel.addEventListener("change", () => { state.tag = sel.value; paint(); });
        const wrap = document.createElement("label");
        wrap.className = "t-xs muted"; wrap.style.cssText = "display:flex;gap:6px;align-items:center;";
        wrap.append("版本", sel);
        controls.appendChild(wrap);
      }
      if (e.kind === "pull") {
        const sel = document.createElement("select");
        sel.className = "input"; sel.style.maxWidth = "140px";
        sel.innerHTML = archOpts.map(a =>
          `<option value="${fb.esc(a)}" ${a === state.arch ? "selected" : ""}>${fb.esc(a)}</option>`).join("");
        sel.addEventListener("change", () => { state.arch = sel.value; paint(); });
        const wrap = document.createElement("label");
        wrap.className = "t-xs muted"; wrap.style.cssText = "display:flex;gap:6px;align-items:center;";
        wrap.append("命令架构", sel);
        controls.appendChild(wrap);
      }
      if (e.kind === "pull") {
        pullBtn = document.createElement("button");
        pullBtn.className = "btn btn-primary btn-sm";
        pullBtn.textContent = `一键拉取(本机 ${hostArch})`;
        pullBtn.addEventListener("click", function doPull() {
          const image = `${e.repo}:${state.tag}`;
          pullBtn.disabled = true; const orig = pullBtn.textContent; pullBtn.textContent = "拉取中…";
          const tip = document.createElement("div"); tip.className = "banner info"; cmds.prepend(tip);
          const sp = fb.spinner("拉取镜像…"); tip.appendChild(sp);
          const spMsg = sp.querySelector("span:last-child");
          pullImageTask(image, st => { if (spMsg) spMsg.textContent = st.progress || st.status || "拉取中…"; })
            .then(() => {
              tip.remove();
              toast("镜像就绪", { variant: "success" });
              renderImages();
            })
            .catch(err => {
              tip.remove();
              const wrap = document.createElement("div"); cmds.prepend(wrap);
              fb.errorBanner(wrap, {
                fromError: err, retryLabel: "重拉",
                onRetry: () => { wrap.remove(); doPull(); },
              });
              pullBtn.disabled = false; pullBtn.textContent = orig;
            });
        });
        actions.appendChild(pullBtn);
      }

      function paint() {
        cmds.innerHTML = imgCommands(e, state.arch, state.tag, mirrors).map(c => `
          <div>
            <div class="code-head"><span>${fb.esc(c.h)}</span>
              <button class="copy-btn" data-cmd="${encodeURIComponent(c.t)}">${COPY_ICON}复制</button></div>
            <pre class="code">${fb.esc(c.t)}</pre>
          </div>`).join("");
        cmds.querySelectorAll("[data-cmd]").forEach(b =>
          b.addEventListener("click", () => copyText(decodeURIComponent(b.dataset.cmd), "已复制命令")));
        if (pullBtn && e.versioned) {
          const v = e.versions.find(x => x.tag === state.tag);
          const ok = !v || v.usable_here;
          pullBtn.disabled = !ok;
          pullBtn.title = ok ? "" : "该版本无本机架构镜像,改用命令在对应架构机器上拉取";
        }
      }
      paint();
      return card;
    }

    let IMG_LOADING = false;
    async function renderImages() {
      if (IMG_LOADING) return;
      IMG_LOADING = true;
      const box = document.getElementById("img-list");
      box.innerHTML = "";
      box.appendChild(fb.skeleton(4));
      let data;
      try { data = await api.images(); }
      catch (e) {
        box.innerHTML = "";
        IMG_LOADING = false;
        fb.errorBanner(box, { fromError: e, retryLabel: "重新加载", onRetry: renderImages });
        return;
      }
      box.innerHTML = "";
      [{ role: "vpn", title: "VPN 通道镜像" }, { role: "infra", title: "基础设施镜像" }].forEach(g => {
        const items = data.images.filter(e => e.role === g.role);
        if (!items.length) return;
        const h = document.createElement("div");
        h.className = "t-sm"; h.style.cssText = "font-weight:600; margin:var(--space-4) 0 var(--space-2);";
        h.textContent = g.title;
        box.appendChild(h);
        items.forEach(e => box.appendChild(imgCard(e, data.host_arch, data.mirrors)));
      });
      if (data.mirrors && !data.mirrors.length) {
        const note = document.createElement("div");
        note.className = "t-xs muted"; note.style.marginTop = "var(--space-2)";
        note.textContent = "未配置镜像源,「经镜像源」命令已隐藏;可在「镜像源」tab 添加。";
        box.appendChild(note);
      }
      IMG_LOADING = false;
    }

    document.querySelector('[data-tab="images"]').addEventListener("click", renderImages);

    if (document.body.dataset.page === "system" && location.pathname.endsWith("/env-check.html")) {
      document.addEventListener("DOMContentLoaded", function () {
        const requested = new URLSearchParams(location.search).get("tab");
        const tab = requested && document.querySelector(`[data-tabs] > [data-tab="${CSS.escape(requested)}"]`);
        if (tab) tab.click();
      });
    }
})();
