// 设置 · 高级（界面 22）：运行环境、客户端下载、问题诊断、运行日志，平时用不到。
// 四个二级页（下载源、已下载的客户端、各通道的运行情况、查看日志）共用这里导出的行、标题和下载任务。
// 不显示内存和磁盘占用（DESIGN 5.12：没有数据来源）。环境检查要跑一个检测任务，只在用户点击时才跑。
// 导出的推导函数都是纯函数，顶层不碰 document / window。
import { h, fill, card, lb, btn, st, tog, ic, field } from '../ui.js';
import { openSheet } from '../sheet.js';
import { api, isAbort } from '../api.js';
import { day, clock, duration, ms as msText } from '../format.js';
import { showError, notify, clean } from '../feedback.js';

// ---- 纯函数 ----

// 运行环境一行：/api/system → {kind, text, sub, start, heal}。
// start 为真时顶栏下方已经有全局的「启动」提示条（main.js），这里不再重复放按钮
export function runtimeView(system) {
  if (!system || typeof system !== 'object') return { kind: 'plain', text: '正在读取', sub: '', start: false, heal: false };
  const rt = system.runtime && typeof system.runtime === 'object' ? system.runtime : null;
  const phase = rt ? rt.phase : null;
  const err = rt ? clean(rt.error) : '';
  if (phase === 'starting') return { kind: 'wait', text: '正在启动', sub: clean(rt.detail), start: false, heal: false };
  if (phase === 'dormant') return { kind: 'off', text: '没有启动', sub: '有通道要连接时会自动启动', start: true, heal: false };
  if (phase === 'releasing') return { kind: 'off', text: '正在释放', sub: '一段时间没有通道在用', start: false, heal: false };
  if (phase === 'closing') return { kind: 'off', text: '正在关闭', sub: '', start: false, heal: false };
  if (phase === 'failed') return { kind: 'bad', text: '没能启动', sub: err || '再启动一次试试', start: true, heal: false };
  if (system.healing === true) return { kind: 'wait', text: '正在自动修复', sub: '', start: false, heal: false };
  if (system.gave_up === true || (system.gateway_health && system.gateway_health !== 'healthy')) {
    return { kind: 'bad', text: '分流有问题', sub: system.gave_up === true ? '自动修复试了几次都没有成功' : '分流入口暂时不通', start: false, heal: true };
  }
  if (system.vm_egress_dead === true) return { kind: 'bad', text: '连不上外网', sub: '重新打开应用试试', start: false, heal: false };
  return { kind: 'ok', text: '运行中', sub: '', start: false, heal: false };
}

// 对外连接的采样（看门狗缓存）。没有这一项时返回 null，整行不显示
export function outboundView(system, now = Date.now()) {
  if (!system || typeof system !== 'object' || !Object.hasOwn(system, 'usernet')) return null;
  const u = system.usernet;
  if (!u || typeof u !== 'object') return { kind: 'plain', text: '还没有记录', sub: '' };
  const t = Date.parse(u.checked_at);
  const old = !Number.isFinite(t) || now - t > 150000;
  if (u.status !== 'observed') return { kind: 'plain', text: '看不出来', sub: old ? '' : '这台电脑上读不到' };
  const n = Number(u.syn_sent);
  const sub = `${old ? '稍早的记录，' : ''}正在等待建立的连接 ${Number.isFinite(n) ? n : '—'} 个`;
  if (u.at_default_limit === true) return { kind: 'wait', text: '连接排队偏多', sub };
  return { kind: 'ok', text: '正常', sub };
}

// 环境检查的项目名。编号里的内部用词先换掉，界面文字只用这一套
const CHECK_TITLE = Object.freeze({
  engine_daemon: '运行环境能连上',
  engine_version: '运行环境版本',
  host_arch: '电脑的芯片类型',
  image_present: '客户端已经下载',
  image_arch_match: '客户端和芯片类型一致',
  vpn_network: '通道网络',
  dev_net_tun: '虚拟网卡设备',
  disk_space: '磁盘空间',
  mirror_reachable: '下载源能连上',
  entry_health: '分流入口',
});
const normId = id => String(id || '').replace(/^docker_/, 'engine_').replace(/^mihomo_/, 'entry_');
export const checkTitle = id => CHECK_TITLE[normId(id)] || '其他检查';

// 没通过时的说明。后台原文常带内部名称，认识的项目用这里的说法
const CHECK_TEXT = Object.freeze({
  engine_daemon: '运行环境连不上',
  image_present: '还没下载',
  image_arch_match: '和这台电脑的芯片类型不一致',
  vpn_network: '通道要用的网络还没建好',
  dev_net_tun: '虚拟网卡设备可能用不了，通道可能连不上',
  mirror_reachable: '下载源都连不上，下载客户端可能失败',
  entry_health: '分流入口没在运行，通道连上了也不会分流',
});

const FIXES = Object.freeze({ create_network: '创建', pull_image: '下载' });

// /api/preflight?scope=full 的结果 → {ok, summary, items:[{id, title, kind, text, fix}]}。只列要处理的项
export function checkView(res) {
  const checks = res && Array.isArray(res.checks) ? res.checks : [];
  const items = [];
  for (const c of checks) {
    if (!c || (c.status !== 'warn' && c.status !== 'fail')) continue;
    const auto = c.fix && c.fix.kind === 'auto' && Object.hasOwn(FIXES, c.fix.action);
    items.push({
      id: String(c.id || ''),
      title: checkTitle(c.id),
      kind: c.status === 'fail' ? 'bad' : 'wait',
      text: CHECK_TEXT[normId(c.id)] || clean(c.detail) || (c.status === 'fail' ? '没有通过' : '需要留意'),
      fix: auto ? { action: c.fix.action, params: c.fix.params || {}, label: FIXES[c.fix.action] } : null,
    });
  }
  const ok = !items.length && checks.length > 0;
  return { ok, summary: ok ? '全部正常' : items.length ? `有 ${items.length} 项要处理` : '没有结果', items };
}

// 「上次检查 今天 07:29，全部正常」
export function checkSub(state, now = Date.now()) {
  if (!state || !state.at) return state && state.running ? '正在检查' : '还没检查过';
  if (state.running) return '正在检查';
  const when = day(state.at) === day(now) ? `今天 ${clock(state.at)}` : `${day(state.at)} ${clock(state.at)}`;
  return state.error ? `上次检查 ${when}，没能完成` : `上次检查 ${when}，${checkView(state.res).summary}`;
}

const isRunning = c => !!c && (c.status === 'running' || c.status === 'logged_in');
export function channelsSub(channels) {
  if (!Array.isArray(channels)) return '正在读取';
  return `${channels.length} 条，${channels.filter(isRunning).length} 条在运行`;
}

const on = v => v === true || v === 1;
export function mirrorsSub(list) {
  if (!Array.isArray(list)) return '';
  return `${list.filter(m => m && on(m.enabled)).length} 个可用`;
}

// 已下载的客户端：通道客户端里本机已经就绪的个数（按版本下载的看不出来，不算）
export function imagesSub(inv) {
  const list = inv && Array.isArray(inv.images) ? inv.images : null;
  if (!list) return '';
  return `${list.filter(e => e && e.role === 'vpn' && e.present === true).length} 个已就绪`;
}

// 距今多久：秒数 → 「3 分钟」
export const ageText = sec => duration(sec);

// 检查整条链路：这台电脑 → 分流入口 → 每条通道的客户端、隧道、内网。
// diag 是 /api/diag 的结果（拿不到时为 null），按 id 对应通道
export function chainView(system, channels, diag) {
  const rv = runtimeView(system);
  const entry = rv.kind === 'ok'
    ? { kind: 'ok', text: '正常' }
    : { kind: rv.kind, text: rv.sub ? `${rv.text}，${rv.sub}` : rv.text };
  const map = new Map();
  for (const d of (diag && Array.isArray(diag.channels) ? diag.channels : [])) if (d && d.id != null) map.set(String(d.id), d);
  const rows = (Array.isArray(channels) ? channels : []).map(ch => {
    const name = (ch && ch.name) || '未命名通道';
    if (!isRunning(ch)) {
      const off = { kind: 'off', text: ch && ch.status === 'error' ? '出错' : '已停止' };
      return { id: ch && ch.id, name, idle: true, client: off, tunnel: { kind: 'off', text: '跳过' }, inner: { kind: 'off', text: '跳过' } };
    }
    const d = map.get(String(ch.id)) || null;
    let client = { kind: 'plain', text: '看不出来' };
    if (d) {
      if (d.crash_loop === true) client = { kind: 'bad', text: `反复重启 ${Number(d.restart_count) || 0} 次` };
      else if (d.state === 'running') client = { kind: 'ok', text: '在运行' };
      else if (d.state === 'missing') client = { kind: 'bad', text: '没有运行' };
      else if (d.state === 'timeout') client = { kind: 'wait', text: '检查超时' };
      else if (d.state && d.state !== 'unknown') client = { kind: 'bad', text: '没有运行' };
    }
    let tunnel = { kind: 'plain', text: '看不出来' };
    if (d && d.state === 'running') {
      if (d.ec_client_alive === false) tunnel = { kind: 'bad', text: '客户端没在运行' };
      else if (d.tunnel_up === false) tunnel = { kind: 'bad', text: '隧道没起来' };
      else if (d.wg_handshake_age != null && Number(d.wg_handshake_age) > 180) tunnel = { kind: 'wait', text: `${ageText(d.wg_handshake_age)}没握手` };
      else if (d.ec_kick_age != null && Number(d.ec_kick_age) < 7200) tunnel = { kind: 'wait', text: `${ageText(d.ec_kick_age)}前被踢下线` };
      else if (d.tunnel_up === true || d.ec_client_alive === true) tunnel = { kind: 'ok', text: '已建立' };
    }
    const inner = ch.status === 'logged_in'
      ? { kind: 'ok', text: ch.latency_ms != null ? `通，${msText(ch.latency_ms)}` : '通' }
      : { kind: 'wait', text: '待登录' };
    return { id: ch.id, name, idle: false, client, tunnel, inner };
  });
  return { entry, heal: rv.heal, rows };
}

// ---- 各页共用的界面片段 ----

// 一行：标题、小字说明、右边的控件
export function sr(title, sub, ...ctl) {
  return h('div', { class: 'row' },
    h('div', { class: 'two sp' }, h('span', { class: 'adv-t' }, title), sub ? h('small', null, sub) : null),
    ctl);
}

// 点进二级页的一行，整行都是链接
export function linkRow(title, sub, href) {
  return h('a', { class: 'row adv-go', href },
    h('div', { class: 'two sp' }, h('span', { class: 'adv-t' }, title), sub ? h('small', null, sub) : null),
    h('span', { class: 'dim3' }, ic('chev', 14)));
}

// 页头：返回链接加大标题，右边可以放按钮
export function pageHead(back, href, title, ...right) {
  return [
    h('div', { class: 'hrow adv-hrow' }, h('a', { class: 'adv-back dim', href }, ic('back', 14, 1.8), back)),
    h('div', { class: 'hrow adv-title' }, h('h1', { class: 'h1' }, title), right.length ? [h('span', { class: 'sp' }), right] : null),
  ];
}

export function download(name, text, type) {
  const url = URL.createObjectURL(new Blob([text], { type }));
  const a = h('a', { href: url, download: name, hidden: true });
  document.body.append(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

// 确认弹层，返回 Promise<boolean>
export function confirmSheet({ title, sub, text, ok = '确定', danger = false }) {
  return new Promise(resolve => {
    let done = false;
    const finish = (v, handle) => { if (done) return; done = true; if (handle) handle.close('close'); resolve(v); };
    const sheet = openSheet({
      title, sub, width: 'w520',
      body: text ? h('p', { class: 'adv-p' }, text) : null,
      foot: [h('span', { class: 'sp' }),
        btn('取消', { onClick: () => finish(false, sheet) }),
        btn(ok, { cls: danger ? 'pri adv-danger' : 'pri', onClick: () => finish(true, sheet) })],
      onConfirm: handle => finish(true, handle),
      onClose: () => finish(false),
    });
  });
}

// 下载客户端：发起后台任务，按任务号查进度直到完成。同一个客户端在途时接着查，不重复发起
const pulls = new Map();
const wait = (ms, signal) => new Promise((resolve, reject) => {
  const t = setTimeout(resolve, ms);
  if (signal) signal.addEventListener('abort', () => { clearTimeout(t); const e = new Error('aborted'); e.name = 'AbortError'; reject(e); }, { once: true });
});

export async function pullImage(image, { signal, onProgress } = {}) {
  let id = pulls.get(image);
  if (!id) {
    const r = await api.preflightFix('pull_image', { image }, { signal });
    if (!r || typeof r.task_id !== 'string' || !r.task_id) throw new Error('没有拿到下载任务，刷新后再看看');
    id = r.task_id;
    pulls.set(image, id);
  }
  const deadline = Date.now() + 20 * 60 * 1000;
  let failures = 0;
  for (;;) {
    if (Date.now() > deadline) throw new Error('等了很久还没下载完，稍后再来看看');
    let s;
    try {
      s = await api.preflightFixStatus(id, { signal });
      failures = 0;
    } catch (e) {
      if (isAbort(e)) throw e;
      if (e && e.status === 404) { pulls.delete(image); throw new Error('这次下载的记录已经失效，刷新后再看看'); }
      if (++failures >= 5) throw e;
      await wait(2000, signal);
      continue;
    }
    if (onProgress) onProgress(s);
    if (s && s.status === 'done') { pulls.delete(image); return s; }
    if (s && s.status === 'error') { pulls.delete(image); throw new Error(clean(s.error) || '下载没有成功'); }
    await wait(2000, signal);
  }
}
export const pulling = image => pulls.has(image);

// 下载进度的一句话：后台原文能直接给人看就用，否则用通用说法
export const progressText = s => clean(s && s.progress) || '正在下载';

// 修复分流：返回 200 但 ok:false 也按失败显示（V-09）
export async function healNow({ signal, done } = {}) {
  try {
    await api.healProxy({ signal });
    notify('分流已经修好', { kind: 'ok' });
    if (done) done();
    return true;
  } catch (e) {
    if (!isAbort(e)) showError(e, { fallback: '没能修好分流' });
    return false;
  }
}

// ---- 页面 ----

// 环境检查的结果只在内存里留一份，回到这一页还能看到上次的
let lastCheck = { at: 0, running: false, res: null, error: false };

export function mount(root, ctx) {
  const { signal, data } = ctx;
  let system = null;
  let channels = null;
  let mirrors = null;
  let images = null;
  let logOn;           // undefined 读取中 / true / false / null 不支持
  let busy = '';       // heal / self / log / fix:<id>
  let chain = null;   // 检查整条链路的弹层
  const sigs = {};

  const slot = () => h('div', { class: 'adv-slot' });
  const rtSlot = slot(), dlSlot = slot(), diagSlot = slot(), logSlot = slot();

  fill(root, h('div', { class: 'adv' },
    pageHead('设置', '#/settings', '高级'),
    h('div', { class: 'grid adv-cols' },
      h('div', { class: 'grid' }, rtSlot, dlSlot),
      h('div', { class: 'grid' }, diagSlot, logSlot))));

  function paint(key, el, sig, build) {
    if (sigs[key] === sig) return;
    sigs[key] = sig;
    fill(el, build());
  }

  const refresh = () => data.refresh('core');

  async function act(kind, fn) {
    if (busy) return;
    busy = kind;
    renderAll();
    try { await fn(); } catch (e) { if (!isAbort(e)) showError(e); }
    if (signal.aborted) return;
    busy = '';
    // 失败时开关要回到原来的位置，所以整页重画一次
    for (const k of Object.keys(sigs)) delete sigs[k];
    renderAll();
  }

  // ---- 运行环境 ----

  function renderRuntime() {
    const rv = runtimeView(system);
    const ob = outboundView(system);
    const cv = checkView(lastCheck.res);
    const selfHeal = system && typeof system.self_heal_enabled === 'boolean' ? system.self_heal_enabled : null;
    paint('rt', rtSlot, JSON.stringify([rv, ob, selfHeal, busy, lastCheck.at, lastCheck.running, lastCheck.error, cv]), () => {
      const rows = [];
      const stateCtl = [st(rv.kind, rv.text)];
      if (rv.heal) stateCtl.push(btn(busy === 'heal' ? '正在修复' : '修复', { cls: 'pri sm', disabled: !!busy, onClick: heal }));
      rows.push(sr('状态', rv.sub, h('div', { class: 'adv-acts' }, stateCtl)));
      if (ob) rows.push(sr('对外连接', ob.sub, st(ob.kind, ob.text)));
      if (selfHeal !== null) {
        rows.push(sr('出问题时自动修复', selfHeal ? '' : '已暂停，出问题时不会自动修复', tog({
          checked: selfHeal, label: '出问题时自动修复', disabled: !!busy, onChange: setSelfHeal,
        })));
      }
      rows.push(sr('环境检查', checkSub(lastCheck),
        btn(lastCheck.running ? '正在检查' : lastCheck.at ? '重新检查' : '开始检查', { cls: 'sm', disabled: lastCheck.running, onClick: runCheck })));
      for (const it of lastCheck.running ? [] : cv.items) {
        rows.push(h('div', { class: 'row slim adv-check' },
          st(it.kind, ''),
          h('div', { class: 'two sp' }, h('span', null, it.title), h('small', null, it.text)),
          it.fix ? btn(busy === 'fix:' + it.id ? '正在处理' : it.fix.label, { cls: 'sm', disabled: !!busy, onClick: () => fix(it) }) : null));
      }
      return card([lb('运行环境'), h('div', { class: 'rows' }, rows)]);
    });
  }

  const heal = () => act('heal', () => healNow({ signal, done: refresh }));

  const setSelfHeal = enabled => act('self', async () => {
    const r = await api.selfHeal(enabled, { signal });
    if (system) system = { ...system, self_heal_enabled: r && typeof r.enabled === 'boolean' ? r.enabled : enabled };
    notify(enabled ? '自动修复已打开' : '自动修复已暂停', { kind: 'ok' });
    refresh();
  });

  async function runCheck() {
    if (lastCheck.running) return;
    lastCheck = { ...lastCheck, running: true };
    renderRuntime();
    try {
      const res = await api.preflight(undefined, undefined, 'full', { signal });
      lastCheck = { at: Date.now(), running: false, res, error: false };
    } catch (e) {
      if (isAbort(e)) { lastCheck = { ...lastCheck, running: false }; return; }
      lastCheck = { at: Date.now(), running: false, res: null, error: true };
      showError(e, { fallback: '环境检查没能完成' });
    }
    renderRuntime();
  }

  const fix = it => act('fix:' + it.id, async () => {
    if (it.fix.action === 'create_network') {
      await api.preflightFix('create_network', it.fix.params, { signal });
      notify('已经创建好', { kind: 'ok' });
    } else {
      const close = notify('正在下载，下载完会提示你', { kind: 'info', ms: 0 });
      try { await pullImage(it.fix.params.image, { signal }); } finally { close(); }
      notify('已经下载好', { kind: 'ok' });
    }
    busy = '';
    await runCheck();
  });

  // ---- 客户端下载 ----

  function renderDownloads() {
    paint('dl', dlSlot, JSON.stringify([mirrorsSub(mirrors), imagesSub(images), mirrors === null, images === null]), () => card([lb('客户端下载'), h('div', { class: 'rows' },
      linkRow('下载源', mirrors === null ? '正在读取' : mirrorsSub(mirrors), '#/settings/advanced/mirrors'),
      linkRow('已下载的客户端', images === null ? '正在读取' : imagesSub(images), '#/settings/advanced/images'))]));
  }

  // ---- 问题诊断 ----

  function renderDiag() {
    paint('diag', diagSlot, channelsSub(channels), () => card([lb('问题诊断'), h('div', { class: 'rows' },
      sr('检查整条链路', '从这台电脑到每条通道，逐段检查', btn('开始', { cls: 'sm', onClick: openChain })),
      linkRow('各通道的运行情况', channelsSub(channels), '#/settings/advanced/channels'))]));
  }

  function openChain() {
    const body = h('div', { class: 'adv-chain' }, h('p', { class: 'dim adv-p' }, '正在检查，每条通道最多等几秒'));
    const my = new AbortController();
    signal.addEventListener('abort', () => my.abort(), { once: true });
    chain = openSheet({
      title: '检查整条链路', sub: '从这台电脑到每条通道，逐段检查。', width: 'w680',
      body,
      foot: [h('span', { class: 'sp' }), btn('关闭', { onClick: () => chain && chain.close('close') })],
      onClose: () => { chain = null; my.abort(); },
    });
    const draw = diag => {
      const v = chainView(system, channels, diag);
      const top = h('div', { class: 'rows adv-chain-top' }, sr('这台电脑 → 分流入口', '', h('div', { class: 'adv-acts' },
        st(v.entry.kind, v.entry.text),
        v.heal ? btn('修复', { cls: 'pri sm', onClick: async e => { e.currentTarget.disabled = true; if (await healNow({ signal: my.signal, done: refresh })) run(); } }) : null)));
      const head = h('div', { class: 'tr th adv-chain-row' }, h('div', null, '通道'), h('div', null, '客户端'), h('div', null, '隧道'), h('div', null, '内网'));
      const rows = v.rows.map(r => h('div', { class: 'tr adv-chain-row' + (r.idle ? ' adv-idle' : '') },
        h('div', { class: 'cell', title: r.name }, r.name),
        [r.client, r.tunnel, r.inner].map(x => h('div', { class: 'cell', title: x.text }, st(x.kind, x.text)))));
      fill(body, top, v.rows.length
        ? h('div', { class: 'adv-chain-list' }, head, rows)
        : h('p', { class: 'dim adv-p' }, '还没有通道。'),
        diag ? null : h('p', { class: 'dim adv-p' }, '没能读取每条通道的运行情况，只按连接状态显示。'));
    };
    const run = async () => {
      let diag = null;
      try { diag = await api.diag({ signal: my.signal }); } catch (e) { if (isAbort(e)) return; }
      if (!my.signal.aborted) draw(diag);
    };
    run();
  }

  // ---- 运行日志 ----

  function renderLogs() {
    if (logOn === null) { paint('log', logSlot, 'none', () => []); return; }
    paint('log', logSlot, JSON.stringify([logOn === undefined ? 'loading' : logOn, busy]), () => card([lb('运行日志'), h('div', { class: 'rows' },
      sr('记录运行日志', logOn === false ? '已关闭，出问题时查不到经过' : '保留 14 天，不记录密码', tog({
        checked: logOn === true, label: '记录运行日志', disabled: logOn === undefined || !!busy, onChange: setLog,
      })),
      linkRow('查看日志', '', '#/settings/advanced/logs'),
      sr('导出', '选一段日期', btn('导出', { cls: 'sm', onClick: () => openExport({ signal }) })))]));
  }

  async function setLog(next) {
    if (!next) {
      const ok = await confirmSheet({
        title: '关闭运行日志', ok: '关闭',
        text: '关闭以后不再记录任何运行和操作经过，直到重新打开。出问题时就查不到当时发生了什么。',
      });
      if (!ok) { sigs.log = null; renderLogs(); return; }
    }
    act('log', async () => {
      const r = await api.runtimeEventsSetEnabled(next, { signal });
      logOn = !!(r && r.enabled);
      notify(logOn ? '运行日志已打开' : '运行日志已关闭', { kind: 'ok' });
    });
  }

  function renderAll() {
    renderRuntime();
    renderDownloads();
    renderDiag();
    renderLogs();
  }

  // ---- 数据 ----

  data.subscribe('core', snap => {
    const v = snap.value || {};
    channels = v.channels ?? channels;
    system = v.system ?? system;
    renderRuntime();
    renderDiag();
  }, { signal });

  api.mirrors({ signal }).then(r => { mirrors = Array.isArray(r) ? r : []; renderDownloads(); }, e => { if (!isAbort(e)) { mirrors = undefined; renderDownloads(); } });
  api.images({ signal }).then(r => { images = r; renderDownloads(); }, e => { if (!isAbort(e)) { images = undefined; renderDownloads(); } });
  api.runtimeEventsEnabled({ signal }).then(r => { logOn = !!(r && r.enabled); renderLogs(); }, e => {
    if (isAbort(e)) return;
    logOn = e && e.status === 404 ? null : false;
    renderLogs();
  });
  renderAll();

  return {
    update() {},
    unmount() { if (chain) chain.close('close'); },
  };
}

// 导出运行日志：选一段日期（最多保留 14 天），默认昨天到今天。查看日志页也用它
export function exportRange(from, to, now = Date.now()) {
  const re = /^\d{4}-\d{2}-\d{2}$/;
  if (!re.test(from || '') || !re.test(to || '')) return '请选好开始和结束日期';
  if (from > to) return '开始日期不能晚于结束日期';
  if (to > day(now)) return '结束日期不能晚于今天';
  if (to < day(now - 13 * 86400000)) return '只保留最近 14 天的日志';
  return '';
}

export function openExport({ signal } = {}) {
  const now = Date.now();
  const from = field({ type: 'date', value: day(now - 86400000), label: '开始日期', cls: 'adv-date' });
  const to = field({ type: 'date', value: day(now), label: '结束日期', cls: 'adv-date' });
  const err = h('div', { class: 'adv-err', role: 'alert', hidden: true });
  let running = false;
  const go = async handle => {
    if (running) return;
    const msg = exportRange(from.value, to.value);
    if (msg) { err.textContent = msg; err.hidden = false; return; }
    running = true;
    try {
      const text = await api.runtimeEventsExport(from.value, to.value, { signal });
      download(`通道管家-运行日志-${from.value}_${to.value}.jsonl`, typeof text === 'string' ? text : '', 'text/plain;charset=utf-8');
      handle.close('close');
      notify('已导出', { kind: 'ok' });
    } catch (e) {
      running = false;
      if (!isAbort(e)) showError(e, { fallback: '没能导出' });
    }
  };
  const sheet = openSheet({
    title: '导出运行日志', sub: '只保留最近 14 天，不含密码。', width: 'w520',
    body: [h('div', { class: 'form' },
      h('div', { class: 'fr' }, h('label', null, '开始'), from),
      h('div', { class: 'fr' }, h('label', null, '结束'), to)), err],
    foot: [h('span', { class: 'sp' }), btn('取消', { onClick: () => sheet.close('close') }), btn('导出', { cls: 'pri', onClick: () => go(sheet) })],
    onConfirm: go,
  });
  return sheet;
}

