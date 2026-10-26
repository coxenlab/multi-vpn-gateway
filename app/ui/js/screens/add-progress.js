// 添加通道 · 正在连接（界面 15）：准备环境 → 启动客户端 → 等你登录 → 检测内网。
// 不单独占路由，也不另开弹层：add.js 在「添加并连接」后把自己的弹层句柄交过来，本模块换标题、正文和底栏。
//   import('./add-progress.js').then(m => m.mount(sheet, { ...ctx, draft }))
// sheet 是 openSheet 的返回值；ctx 是 add.js 收到的 ctx（signal、navigate、data、close）；
// draft 是填好的内容，字段见 createBody：name、vpn_type、ec_ver、login_method、验证地址，另有
//   spec（/api/vpn-types 里这一类型的那一项）、fields（填写的文字项）、file（拖进来或选中的文件，没有为 null）。
// 前两步由页面发请求，弹层关掉（「在后台继续」、Esc）也会做完，结束时用提示条告诉结果；
// 后两步只是看结果，弹层关掉就不再看，通道本身照常连接。
import { h, fill, btn, steps4, verdict } from '../ui.js';
import { api, isAbort, ApiError } from '../api.js';
import { humanize, notify } from '../feedback.js';
import { poll } from '../poll.js';
import { duration, ms as fmtMs } from '../format.js';
import { configProblem } from './add.js';

export const TITLES = Object.freeze(['准备环境', '启动客户端', '等你登录', '检测内网']);
export const HEALTH_MS = 5000;

const inputs = draft => (draft && draft.spec && Array.isArray(draft.spec.inputs) ? draft.spec.inputs : []);

// 登录方式：add.js 给了就用；否则按类型的 login_modes 取（只有无头 → headless，只有自备 → byo，其余交互）
export function loginMethodOf(draft) {
  if (draft && draft.login_method) return draft.login_method;
  const modes = (draft && draft.spec && draft.spec.login_modes) || [];
  if (modes.length === 1 && (modes[0] === 'headless' || modes[0] === 'byo')) return modes[0];
  return 'interactive';
}

// 文件怎么用：类型有 config_file 文件项 → 读成文本放进 config；有 package 文件项 → 建好后上传；其余只用来识别
export function fileRole(draft) {
  if (!draft || !draft.file) return null;
  const keys = inputs(draft).filter(i => i && i.type === 'file').map(i => i.key);
  if (keys.includes('config_file')) return 'config';
  if (keys.includes('package')) return 'package';
  return null;
}

// 配置文件的文本。超过上限不读，直接按失败处理（add.js 选文件时已经拦过，这里兜底）
export async function configText(draft) {
  if (fileRole(draft) !== 'config') return undefined;
  const big = configProblem(draft.file);
  if (big) throw new ApiError({ status: 413, reason: `${big.title}。${big.text}` });
  return draft.file.text();
}

// POST /api/channels 的请求体。configText 是 config_file 文件的文本（没有为 undefined）
export function createBody(draft, configText) {
  const fields = { ...((draft && draft.fields) || {}) };
  const config = {};
  for (const [k, v] of Object.entries(fields)) if (v != null && v !== '') config[k] = v;
  if (configText !== undefined) config.config_file = configText;
  const versioned = !!(draft.spec && draft.spec.versioned);
  return {
    name: String(draft.name || '').trim(),
    vpn_type: draft.vpn_type,
    ec_ver: versioned ? (draft.ec_ver || '') : '',
    login_method: loginMethodOf(draft),
    probe_url: draft.probe_url || '',
    server: config.server || '',
    username: config.username || '',
    password: config.password || '',
    config,
  };
}

// /health 的结果 → connected / unverified / error / waiting
export function healthOutcome(r) {
  if (!r || typeof r !== 'object') return 'waiting';
  if (r.connected === true && r.stale !== true) return 'connected';
  if (r.awaiting_target === true) return 'unverified';
  if (r.status === 'error' || r.status === 'down') return 'error';
  return 'waiting';
}

export function installOutcome(r) {
  if (!r || typeof r !== 'object') return 'failed';
  if (r.phase === 'succeeded') return 'succeeded';
  if (r.phase === 'manual') return 'manual';
  if (r.phase === 'queued' || r.phase === 'running') return 'waiting';
  return 'failed';
}

export function installFailureText(r) {
  const lines = String((r && r.log) || '').split(/\r?\n/).map(s => s.trim()).filter(Boolean);
  const detail = lines.at(-1) || (r && r.phase === 'interrupted'
    ? '安装结果尚未确认，可能仍在通道内运行，不会自动重装'
    : '官方安装程序返回失败');
  return detail.length > 180 ? detail.slice(0, 177) + '…' : detail;
}

export function installAttemptMatches(expected, state) {
  return !!expected && !!state && state.attempt_id === expected;
}

export function installationError(error, fallback = '安装状态没有确认') {
  const out = error instanceof Error ? error : new ApiError({ reason: String(error || fallback) });
  return Object.assign(out, { step: 1, fallback, installation: true });
}

export function requireInstallAttempt(expected, state) {
  if (!installAttemptMatches(expected, state)) {
    throw installationError(new ApiError({ reason: '安装状态已变成另一项任务，未确认本次安装完成' }));
  }
  return state;
}

// 建通道的请求失败时，核心可能已经落了库（状态为出错）。按建之前的清单找出这条新通道，重试时启动它，不再建一条同名的
export function newChannelId(before, after, name) {
  if (!Array.isArray(before) || !Array.isArray(after)) return null;
  const old = new Set(before.map(c => c && c.id));
  const hit = after.filter(c => c && !old.has(c.id) && c.name === name);
  return hit.length === 1 ? hit[0].id : null;
}

// 步骤状态 → steps4 的参数。steps[i] = {state: done|now|todo|fail|hold, sub, sec}
export const stepsView = steps => steps.map((s, i) => ({
  state: s.state, title: TITLES[i], sub: s.sub || '', time: s.state === 'done' && s.sec != null ? duration(s.sec) : '',
}));

const errText = m => (m ? (m.text ? `${m.title}。${m.text}` : m.title) : '');

export function mount(sheet, ctx) {
  const draft = ctx.draft || {};
  const name = String(draft.name || '').trim();
  const headless = () => (ch && ch.login_method ? ch.login_method : loginMethodOf(draft)) === 'headless';
  const byo = () => loginMethodOf(draft) === 'byo';
  const role = fileRole(draft);

  const steps = TITLES.map(() => ({ state: 'todo', sub: '', sec: null }));
  let at = 0;
  let t0 = 0;
  let ch = null;          // 建好的通道（POST /api/channels 的返回）
  let id = null;          // 通道 id：建好的，或者建失败但已落库的那条
  let created = false;    // 请求已确认建好
  let uploaded = role !== 'package';
  let installFailed = false;
  let manualInstall = false;
  let automaticInstallDone = false;
  let installAttempt = null;
  let installState = null;
  let skipPreflight = false;
  let running = false;
  let end = null;         // connected / unverified / error，或 fail（某一步失败）/ preflight（环境检查没通过）
  let extra = null;       // 步骤下方的补充（环境检查没通过时的说明）
  let bg = false;         // 弹层已关：前两步照常做完，结果用提示条说
  let checking = false;
  let watch = null;       // 看结果的轮询
  const toBackground = () => { bg = true; if (watch) watch.abort(); };
  if (ctx.signal) ctx.signal.addEventListener('abort', toBackground, { once: true });

  sheet.el.classList.remove('w680');
  sheet.el.classList.add('w520', 'ap-sheet');

  // ---- 画 ----

  function title() {
    if (end === 'connected') return '已连接 ' + name;
    if (end === 'unverified') return '已启动 ' + name;
    if (installFailed) return '没能准备好 ' + name;
    if (end === 'fail' || end === 'error' || end === 'preflight') return '没能连接 ' + name;
    return '正在连接 ' + name;
  }

  function foot() {
    const go = hash => () => ctx.navigate(hash);
    const cid = encodeURIComponent(id || '');
    const kids = [h('span', { class: 'sp' })];
    if (id && created && at >= 2 && (!end || end === 'unverified')) {
      kids.push(btn(checking ? '正在检测' : '检测连通', { disabled: checking, onClick: checkNow }));
    }
    if (end === 'connected') {
      kids.push(btn('完成', { cls: 'pri', onClick: go(`#/channels/${cid}`) }));
    } else if (end === 'unverified') {
      kids.push(btn('稍后再说', { onClick: go(`#/channels/${cid}`) }));
      // 只有不需要登录窗口的通道能用规则里的地址来确认；其余的登录以后在通道页填内网地址
      if (!headless()) kids.push(btn('去登录', { cls: 'pri', onClick: go(`#/channels/${cid}?tab=login`) }));
      else kids.push(btn('添加规则', { cls: 'pri', onClick: go(`#/channels/${cid}?sheet=rule-add&channel=${cid}`) }));
    } else if (end === 'error') {
      kids.push(btn('关闭', { onClick: ctx.close }), btn('查看原因', { cls: 'pri', onClick: go(`#/channels/${cid}`) }));
    } else if (end === 'preflight') {
      kids.push(btn('取消', { onClick: ctx.close }), btn('重新检查', { onClick: retry }),
        btn('仍然继续', { cls: 'pri', onClick: () => { skipPreflight = true; retry(); } }));
    } else if (end === 'fail') {
      kids.push(btn('关闭', { onClick: ctx.close }));
      if (installFailed && id) kids.push(btn('打开窗口排查', { cls: 'pri', onClick: go(`#/channels/${cid}?tab=login`) }));
      else kids.push(btn('重试', { cls: 'pri', icon: 'refresh', onClick: retry }));
    } else {
      kids.push(btn('在后台继续', { onClick: () => { toBackground(); ctx.close(); } }));
      if (at === 2 && !headless() && id) kids.push(btn('去登录', { cls: 'pri', onClick: go(`#/channels/${cid}?tab=login`) }));
      // 安装程序可能在登录窗口的终端里提问；离开后安装在后台继续，结果用提示条说
      if (at === 1 && installAttempt && id) kids.push(btn('打开登录窗口', { onClick: go(`#/channels/${cid}?tab=login`) }));
    }
    return kids;
  }

  function render() {
    if (bg) return;
    sheet.setTitle(title(), '');
    fill(sheet.body, h('div', { class: 'ap' }, steps4(stepsView(steps)), extra));
    const list = sheet.body.querySelector('.steps4');
    if (list) list.setAttribute('aria-live', 'polite');
    sheet.setFoot(...foot());
    // 原来获得焦点的元素已经换掉：焦点放到底栏最后一个按钮上，回车和 Tab 仍在弹层里
    const at0 = globalThis.document && document.activeElement;
    if (!at0 || at0 === sheet.el || !sheet.el.contains(at0)) {
      const last = sheet.foot.querySelector('button:last-of-type');
      if (last) last.focus({ preventScroll: true, focusVisible: false });
    }
  }

  // ---- 步骤 ----

  function begin(i, sub = '') {
    at = i;
    t0 = Date.now();
    steps[i] = { state: 'now', sub, sec: null };
    render();
  }
  function done(i, sub = '') {
    steps[i] = { state: 'done', sub, sec: Math.round((Date.now() - t0) / 1000) };
    t0 = Date.now();
  }
  function failAt(i, e, fallback) {
    installFailed = !!(e && e.installation);
    const m = humanize(e, { fallback });
    steps[i] = { state: 'fail', sub: errText(m) || fallback, sec: null };
    end = 'fail';
    if (bg) notify(`「${name}」没能添加：${steps[i].sub}`, { kind: 'bad', ms: 8000 });
    render();
  }

  async function prepare(signal) {
    begin(0);
    try { await api.runtimeStart({ signal }); } catch (e) {
      // 没有这个接口的后台（网页版）不用启动运行环境
      if (!(e && e.status === 404)) throw Object.assign(e, { step: 0, fallback: '运行环境没能启动' });
    }
    if (!skipPreflight) {
      let pf = null;
      try { pf = await api.preflight(draft.vpn_type, draft.spec && draft.spec.versioned ? draft.ec_ver : undefined, 'preflight', { signal }); } catch (e) {
        if (isAbort(e)) throw e; // 检查本身没跑成就不拦，建通道失败时会说原因
      }
      if (pf && pf.overall === 'fail') return false;
    }
    done(0);
    return true;
  }

  async function launch(signal) {
    begin(1);
    if (!id) {
      let before = null;
      try { before = await api.channels({ signal }); } catch (e) { if (isAbort(e)) throw e; }
      try {
        const text = await configText(draft);
        ch = await api.create(createBody(draft, text), { signal });
        id = ch && ch.id;
        created = true;
      } catch (e) {
        if (isAbort(e)) throw e;
        if (before) {
          try { id = newChannelId(before, await api.channels({ signal }), name); } catch { /* 找不到就按没建处理 */ }
        }
        throw Object.assign(e, { step: 1, fallback: '客户端没能启动' });
      }
    } else if (!created) {
      try { ch = await api.start(id, { signal }); created = true; } catch (e) {
        throw Object.assign(e, { step: 1, fallback: '客户端没能启动' });
      }
    }
    if (!uploaded) {
      steps[1].sub = '正在上传官方安装包';
      render();
      let uploadResult;
      try { uploadResult = await api.upload(id, draft.file, { signal }); } catch (e) {
        // 安装包已经放进去、只是记录没写完：不要让人再传一次
        if (!(e && e.body && e.body.uploaded === true)) throw Object.assign(e, { step: 1, fallback: '安装包没能放进去' });
      }
      uploaded = true;
      if (draft.vpn_type === 'custom') {
        if (!uploadResult || !uploadResult.install || !uploadResult.install.attempt_id) {
          throw Object.assign(new ApiError({ reason: '安装包已上传，但后台没有启动官方安装程序' }),
            { step: 1, fallback: '官方安装没有启动', installation: true });
        }
        installAttempt = uploadResult.install.attempt_id;
        installState = uploadResult.install;
      }
    }
    if (draft.vpn_type === 'custom' && role === 'package' && !automaticInstallDone && !manualInstall) {
      requireInstallAttempt(installAttempt, installState);
      steps[1].sub = '正在安装；安装程序如有提问，会在登录窗口里等你回答';
      render();
      while (installOutcome(installState) === 'waiting') {
        await new Promise(resolve => setTimeout(resolve, 1000));
        try { installState = await api.installStatus(id, { signal }); } catch (e) {
          throw installationError(e, '暂时读不到安装状态');
        }
        requireInstallAttempt(installAttempt, installState);
      }
      if (installOutcome(installState) === 'manual') {
        manualInstall = true;
        steps[1] = { state: 'hold', sub: '需要交互安装，请在登录窗口中完成', sec: null };
        ctx.data.refresh('core');
        return;
      }
      if (installOutcome(installState) !== 'succeeded') {
        throw Object.assign(new ApiError({ reason: installFailureText(installState), body: installState, kind: 'failed' }),
          { step: 1, fallback: '官方安装失败', installation: true });
      }
      automaticInstallDone = true;
    }
    ctx.data.refresh('core');
    done(1);
  }

  // 前两步不带 signal：弹层关掉不打断，建通道的请求本来也不会因为页面断开而停
  async function run() {
    if (running) return;
    running = true;
    end = null;
    extra = null;
    try {
      if (steps[0].state !== 'done' && !(await prepare())) {
        steps[0] = { state: 'hold', sub: '检查没通过', sec: null };
        end = 'preflight';
        extra = verdict('wait', { title: '运行环境检查没通过', text: '可以到「设置 · 高级」里查看详情后重新检查，也可以直接继续试试。' });
        if (bg) notify(`运行环境检查没通过，「${name}」没有添加`, { kind: 'bad', ms: 8000 });
        render();
        return;
      }
      if (steps[1].state !== 'done') await launch();
      if (bg) {
        notify(manualInstall ? `「${name}」的安装包已放好，请打开登录窗口完成安装`
          : headless() ? `「${name}」已添加，正在检测内网` : `「${name}」已添加，到通道里登录`, { kind: 'ok' });
        return;
      }
      if (headless()) {
        steps[2] = { state: 'done', sub: '自动登录，不用你操作', sec: null };
        begin(3);
      } else {
        begin(2, manualInstall ? '在窗口里装好客户端并登录'
          : automaticInstallDone ? '客户端已安装，在窗口里完成登录'
          : byo() ? '在窗口里装好客户端并登录' : '点「去登录」，在登录窗口里完成登录');
      }
      watchHealth();
    } catch (e) {
      if (isAbort(e)) return;
      failAt(e.step ?? at, e, e.fallback || '操作没有完成');
    } finally {
      running = false;
    }
  }

  function retry() {
    if (running) return;
    for (let i = 0; i < steps.length; i++) if (steps[i].state === 'fail') steps[i] = { state: 'todo', sub: '', sec: null };
    run();
  }

  function applyHealth(r) {
    const out = healthOutcome(r);
    if (out === 'waiting') return;
    if (out === 'connected') {
      if (steps[2].state !== 'done') done(2);
      steps[3] = { state: 'done', sub: r.latency_ms != null ? '延迟 ' + fmtMs(r.latency_ms) : '', sec: at === 3 ? Math.round((Date.now() - t0) / 1000) : null };
    } else if (out === 'unverified') {
      // 交互型还不能确认登录了没有：「等你登录」保持原样，底栏仍给「去登录」
      steps[3] = { state: 'hold', sub: headless() ? '已启动，还没验证。加一条域名或单独的地址，就用它来确认。'
        : '已启动，还没验证。登录以后填一个内网地址，就用它来确认。', sec: null };
    } else {
      steps[at] = { state: 'fail', sub: '连不上', sec: null };
    }
    end = out;
    at = 3;
    if (watch) watch.abort();
    ctx.data.refresh('core');
    render();
  }

  async function checkNow() {
    if (checking || bg || !id) return;
    checking = true;
    if (watch) watch.abort();
    watch = null;
    render();
    try {
      const r = await api.status(id, { signal: ctx.signal });
      if (!bg) {
        applyHealth(r);
        if (healthOutcome(r) === 'waiting') notify('暂时还没连通，登录后可以再检测');
      }
    } catch (e) {
      if (!bg && !isAbort(e)) notify(errText(humanize(e, { fallback: '检测没有完成' })), { kind: 'bad' });
    } finally {
      checking = false;
      if (!bg) { render(); if (!end) watchHealth(); }
    }
  }

  // 后两步：每 5 秒看一次 /health，直到连上；还没有可用的验证地址时停在「还没验证」
  function watchHealth() {
    if (watch || bg) return;
    watch = new AbortController();
    poll(async signal => {
      let r;
      try { r = await api.channelHealth(id, { signal }); } catch (e) { return; } // 下一轮再看
      if (signal.aborted || bg) return;
      applyHealth(r);
    }, HEALTH_MS, { signal: watch.signal });
  }

  render();
  run();

  return {
    update() {},
    unmount: toBackground,
  };
}
