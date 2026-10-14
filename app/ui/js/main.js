// 启动入口：外壳、hash 路由、弹层路由、顶栏分流标记、运行环境提示条、全局拖放、启动时恢复通道。
// 每个界面模块导出 mount(root, ctx)，返回 {update(query), unmount()}；
// ctx = {params, query, signal, navigate, data}，弹层另有 close() 和 files（拖进来的文件）。
import { fill, initShell, setNav, setRouteChip, pageState, btn } from './ui.js';
import { closeSheet } from './sheet.js';
import { parse, splitQuery, plan, planSheet, resolveOpen, withQuery } from './router.js';
import { resolve, load, sheetModule, SHEETS, SHEET_KEYS, HOME } from './routes.js';
import { api } from './api.js';
import { data, resumeChannels } from './data.js';
import { chipState } from './derive.js';
import { showError, notify } from './feedback.js';
import { installDrop } from './drop.js';

const $page = document.getElementById('page');
const $sheet = document.getElementById('sheet-root');
const $notice = document.getElementById('notice');

let cur = null;   // 当前页面 {module, params, query, ctrl, view}
let sh = null;    // 当前弹层 {name, query, ctrl, view}
let pendingFiles = null;

// ---- 跳转 ----

// target 可以是 '#/…'、'/…'，或只带 query 的 '?sheet=add'（并到当前地址上）
function navigate(target, { replace = false } = {}) {
  const hash = resolveOpen(location.hash, target);
  if (hash === location.hash) { render(); return; }
  if (replace) { history.replaceState(history.state, '', hash); render(); }
  else location.hash = hash; // 触发 hashchange → render
}

// 外壳的跳转入口（⌘N、⌘,、菜单栏「去登录」）
window.__tongdaoOpen = target => navigate(target);

// ---- 路由 ----

function render() {
  const r = parse(location.hash);
  const rt = resolve(r.path, r.query);
  if (!rt) { navigate(HOME, { replace: true }); return; }
  const { page, sheet } = splitQuery(r.query, SHEET_KEYS);
  setNav(rt.nav);
  renderPage({ module: rt.module, params: rt.params, query: page });
  renderSheet(sheet);
}

// 界面模块加载失败：浏览器会记住失败的 import()，重新 import 仍然失败，所以「重试」整页重新载入
function failed(root) {
  fill(root, pageState('error', { title: '这一页没能打开', onRetry: () => location.reload() }));
}

async function renderPage(next) {
  const act = plan(cur, next);
  if (act === 'keep') return;
  if (act === 'update') {
    cur.query = next.query;
    if (cur.view && cur.view.update) cur.view.update(next.query);
    return;
  }
  unmountPage();
  const ctrl = new AbortController();
  const my = { ...next, ctrl, view: null };
  cur = my;
  fill($page, pageState('loading'));
  let mod;
  try { mod = await load(next.module); } catch (e) {
    if (cur !== my) return;
    console.error('[route]', next.module, e);
    failed($page);
    return;
  }
  if (cur !== my) return;
  fill($page);
  const q = my.query;
  try {
    my.view = mod.mount($page, { params: my.params, query: q, signal: ctrl.signal, navigate, data }) || null;
  } catch (e) {
    console.error('[mount]', next.module, e);
    failed($page);
    return;
  }
  // 加载期间 query 又变了
  if (my.query !== q && my.view && my.view.update) my.view.update(my.query);
}

function unmountPage() {
  if (!cur) return;
  const old = cur;
  cur = null;
  old.ctrl.abort();
  try { if (old.view && old.view.unmount) old.view.unmount(); } catch (e) { console.error('[unmount]', old.module, e); }
}

async function renderSheet(query) {
  const name = sheetModule(query.sheet) ? query.sheet : '';
  const act = planSheet(sh ? sh.name : '', name, sh ? sh.query : {}, query);
  if (act === 'keep') return;
  if (act === 'close') { closeSheetView(); return; }
  if (act === 'update') {
    sh.query = query;
    if (sh.view && sh.view.update) sh.view.update(query);
    return;
  }
  closeSheetView();
  const ctrl = new AbortController();
  const my = { name, query, ctrl, view: null };
  sh = my;
  let mod;
  try { mod = await load(sheetModule(name)); } catch (e) {
    if (sh !== my) return;
    console.error('[sheet]', name, e);
    notify('这个窗口没能打开，稍后再试一次', { kind: 'bad' });
    leaveSheet(my);
    return;
  }
  if (sh !== my) return;
  const files = name === 'add' ? pendingFiles : null;
  if (files) pendingFiles = null;
  const q = my.query;
  try {
    my.view = mod.mount($sheet, {
      params: {}, query: q, signal: ctrl.signal, navigate, data, files,
      close: () => leaveSheet(my),
    }) || null;
  } catch (e) {
    console.error('[mount]', name, e);
    leaveSheet(my);
    return;
  }
  if (my.query !== q && my.view && my.view.update) my.view.update(my.query);
}

function closeSheetView() {
  if (!sh) return;
  const old = sh;
  sh = null;
  old.ctrl.abort();
  try { if (old.view && old.view.unmount) old.view.unmount(); } catch (e) { console.error('[unmount]', old.name, e); }
  closeSheet('close');
}

// 弹层自己关闭（Esc、取消、完成）：把地址栏里归弹层的参数去掉
function leaveSheet(my) {
  if (sh !== my || my.ctrl.signal.aborted) return;
  const drop = { sheet: null };
  for (const k of SHEETS[my.name] ? SHEETS[my.name].keys : []) drop[k] = null;
  navigate(withQuery(location.hash, drop), { replace: true });
}

// ---- 顶栏与提示条 ----

let runtimeBar = null;
let starting = false;
let retryingConfig = false;

function syncRuntimeBar(state) {
  const show = state === 'runtime' || state === 'pending';
  if (!show) {
    if (runtimeBar) { runtimeBar.remove(); runtimeBar = null; }
    $notice.hidden = !$notice.children.length;
    return;
  }
  const bar = state === 'pending'
    ? pageState('runtime', { title: '规则还没同步', text: '已保存，还没同步到入口。' })
    : pageState('runtime', { onStart: startRuntime, starting });
  if (state === 'pending') bar.append(btn(retryingConfig ? '正在同步' : '重试', { cls: 'pri sm', disabled: retryingConfig, onClick: retryConfig }));
  bar.dataset.runtime = '';
  if (runtimeBar) runtimeBar.replaceWith(bar); else $notice.prepend(bar);
  runtimeBar = bar;
  $notice.hidden = false;
}

async function retryConfig() {
  if (retryingConfig) return;
  retryingConfig = true;
  syncRuntimeBar('pending');
  try { await api.retryConfig(); } catch (e) { showError(e); }
  finally {
    retryingConfig = false;
    await data.refresh('core');
    syncRuntimeBar(chipState(data.get('core').value?.system));
  }
}

async function startRuntime() {
  if (starting) return;
  starting = true;
  syncRuntimeBar('runtime');
  try { await api.runtimeStart(); } catch (e) { showError(e); }
  starting = false;
  await data.refresh('core');
  syncRuntimeBar(chipState(data.get('core').value && data.get('core').value.system));
}

function watchSystem() {
  data.subscribe('core', snap => {
    const state = chipState(snap.value && snap.value.system);
    setRouteChip(state);
    syncRuntimeBar(state);
  });
}

// ---- 启动 ----

function boot() {
  initShell();
  document.getElementById('top-add').addEventListener('click', () => navigate('?sheet=add'));
  installDrop((files, info) => {
    const payload = { files, folders: (info && info.folders) || 0 };
    if (sh && sh.name === 'add' && sh.view && sh.view.files) { sh.view.files(payload); return; }
    pendingFiles = payload;
    navigate('?sheet=add');
  });
  window.addEventListener('hashchange', render);
  watchSystem();
  render();
  resumeChannels(api)
    .then(out => { if (out.length) data.refresh('core'); })
    .catch(e => { if (e && e.name !== 'AbortError') console.warn('[resume]', e); });
}

boot();
