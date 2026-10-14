// 通道 · 登录（界面 07）。由 channels.js 分派进详情列（root），本模块画详情头（用 channels.js 的 detailHead）和下面的全部内容。
// 左边保留厂商登录窗口，右边默认概览；概览、规则、日志使用与无窗口通道一致的导航。
// 登录窗口打开了不等于登录成功：是否已连接只看 /health 的 connected（命门 #1）；
// 发送到登录窗口的文字直接发给登录窗口，不经任何接口（命门 #5）。
// canOpen、workspaceTab 是纯函数，顶层不碰 document / window。
import { h, fill, btn } from '../ui.js';
import { api, isAbort } from '../api.js';
import { poll } from '../poll.js';
import { displayState } from '../derive.js';
import { stateText, stateKind } from '../terms.js';
import { humanize, showError, showDone } from '../feedback.js';
import { createVncLifecycle } from '../login/vnc-lifecycle.js';
import { waitNovncReady } from '../login/novnc-ready.js';
import { sendBar } from '../login/vnc-text.js';
import { detailHead, channelTabs, headStatus } from './channels.js';
import { mount as mountRules } from './channel-rules.js';
import { mount as mountOverview } from './channel-overview.js';
import { mount as mountLogs } from './channel-logs.js';

export const workspaceTab = query => ['rules', 'logs'].includes(query?.tab) ? query.tab : 'overview';

export const HEALTH_MS = 5000;
const READY_SEC = 60;

// 能不能打开登录窗口（同旧 native-login.js）：通道在、没在停止、没出错、没在排队重建
export function canOpen(ch) {
  return !!ch && ch.stop_pending !== true && !['stopped', 'error', 'down'].includes(ch.status) &&
    !(ch.replacement && ch.replacement.phase === 'queued');
}

export function mount(root, ctx) {
  const cid = ctx.params && ctx.params.id;
  // 自己的 signal：channels.js 切换分页时只调 unmount，不一定取消它的 signal
  const ctrl = new AbortController();
  const signal = ctrl.signal;
  if (ctx.signal) {
    if (ctx.signal.aborted) ctrl.abort();
    else ctx.signal.addEventListener('abort', () => ctrl.abort(), { once: true });
  }

  let ch = ctx.channel || null;
  let loaded = !!ch;
  let health = ctx.health || null;
  let win = 'idle';
  let url = '';
  let busy = false;
  let checker = null;
  let checking = false;
  let runtimeUnavailable = false;
  let system = ctx.system;
  let activeTool = workspaceTab(ctx.query);
  let toolView = null;
  let toolCtrl = null;
  let focused = false;
  let inerted = [];

  // ---- 结构 ----
  let $head = h('div', { class: 'dhead' });
  const $view = h('div', { class: 'remote-view' });
  const $startup = h('div', { class: 'login-startup', role: 'status', hidden: true });
  const $remote = h('div', { class: 'remote' }, $view, sendBar(() => url, { signal }));
  const $toolHost = h('div', { class: 'login-tool-host', id: 'login-tool-panel', role: 'tabpanel' });
  const $tabs = channelTabs(activeTool, key => selectTool(key, true), 'login-tool-panel');
  const $side = h('aside', { class: 'login-side', id: 'login-channel-panel', 'aria-label': '通道工具' },
    $toolHost);
  const $focusBtn = btn('放大窗口', { cls: 'login-focus', onClick: () => setFocused(!focused) });
  const $reopenBtn = btn('重新打开窗口', { cls: 'login-reopen', icon: 'refresh', onClick: () => view.open() });
  const $workspace = h('div', { class: 'login-workspace' },
    h('div', { class: 'login-links' },
      h('span', { class: 'login-focus-name' }, ch?.name || '登录窗口'),
      $tabs,
      h('div', { class: 'login-window-actions' }, $reopenBtn, $focusBtn)),
    h('div', { class: 'login-body' }, $remote, $side));
  fill(root, $head, $startup, $workspace);

  poll(async s => {
      if (ch?.vpn_type !== 'custom' || !canOpen(ch)) { $startup.hidden = true; return; }
      try {
        const result = await api.startupStatus(cid, { signal: s });
        if (s.aborted) return;
        $startup.hidden = result.phase === 'ready';
        $startup.textContent = result.message || '客户端启动状态尚未确认';
      } catch (error) {
        if (isAbort(error)) return;
        $startup.hidden = false;
        $startup.textContent = '暂时无法确认客户端窗口是否启动，请查看通道日志。';
      }
  }, HEALTH_MS, { signal, immediate: true });

  function childContext(childSignal) {
    return {
      params: { id: cid }, query: { tab: activeTool }, signal: childSignal, navigate: ctx.navigate, data: ctx.data,
      channel: ch, health, system,
      onVerified(result) {
        health = { ...(health || {}), connected: true, awaiting_target: false, probe_failed: false, target: result.target };
        renderHead();
      },
    };
  }

  function closeTool() {
    if (toolCtrl) toolCtrl.abort();
    if (toolView?.unmount) toolView.unmount();
    toolCtrl = null;
    toolView = null;
  }

  signal.addEventListener('abort', closeTool, { once: true });

  function selectTool(key, updateUrl = false) {
    const next = workspaceTab({ tab: key });
    if (next !== activeTool || !$toolHost.firstChild) {
      closeTool();
      activeTool = next;
      for (const button of $tabs.querySelectorAll('[role="tab"]')) {
        const on = button.dataset.key === activeTool;
        button.classList.toggle('on', on);
        button.setAttribute('aria-selected', String(on));
        button.tabIndex = on ? 0 : -1;
      }
      $toolHost.setAttribute('aria-labelledby', `login-tool-panel-tab-${activeTool}`);
      fill($toolHost);
      toolCtrl = new AbortController();
      const mounts = { rules: mountRules, overview: mountOverview, logs: mountLogs };
      toolView = mounts[activeTool]($toolHost, childContext(toolCtrl.signal)) || null;
    }
    if (updateUrl) ctx.navigate(`#/channels/${encodeURIComponent(cid)}?tab=${activeTool}`);
  }

  function setBackgroundInert(on) {
    if (!on) {
      for (const [el, had] of inerted) if (!had) el.removeAttribute('inert');
      inerted = [];
      return;
    }
    let node = $workspace;
    while (node && node !== document.body) {
      const parent = node.parentElement;
      if (!parent) break;
      for (const sibling of parent.children) {
        if (sibling === node || sibling.id === 'notice') continue;
        const had = sibling.hasAttribute('inert');
        if (!had) sibling.setAttribute('inert', '');
        inerted.push([sibling, had]);
      }
      node = parent;
    }
  }

  function setFocused(on) {
    focused = !!on;
    setBackgroundInert(focused);
    $head.inert = focused;
    $workspace.classList.toggle('is-focus', focused);
    $focusBtn.textContent = focused ? '返回布局' : '放大窗口';
    $focusBtn.setAttribute('aria-pressed', String(focused));
    if (focused) $focusBtn.focus({ preventScroll: true });
  }

  const onKeydown = e => {
    if (e.key === 'Escape' && focused) { e.preventDefault(); setFocused(false); }
  };
  document.addEventListener('keydown', onKeydown);
  selectTool(activeTool);

  // ---- 登录窗口 ----
  const message = (title, text, action) => fill($view, h('div', { class: 'remote-msg', role: 'status' },
    h('b', null, title), text ? h('span', null, text) : null, action || null));

  const setWin = w => { win = w; };

  const view = createVncLifecycle({
    channelId: () => cid,
    isActive: () => loaded && canOpen(ch) && !signal.aborted,
    signal,
    close: () => {
      url = '';
      if (signal.aborted) return;
      setWin('idle');
      showIdle();
    },
    open: async ({ signal: own, current, login }) => {
      setWin('opening');
      message('正在打开登录窗口…');
      try {
        const result = await login();
        if (!current()) return;
        runtimeUnavailable = false;
        renderHead();
        if (!result || result.login_mode === 'headless' || !result.url) {
          setWin('none');
          message('这条通道不需要登录窗口', '它会自己连接，连上以后自动变为已连接。');
          return;
        }
        if (!await waitNovncReady(result.url, READY_SEC, { signal: own })) {
          if (current()) { setWin('failed'); message('登录窗口还没准备好', '通道刚启动时要等一会儿，稍后点「重新打开窗口」。', reopenBtn()); }
          return;
        }
        if (!current()) return;
        url = result.url;
        fill($view, h('iframe', { class: 'remote-frame', title: '登录窗口', src: result.url }));
        setWin('open');
      } catch (e) {
        if (!current() || isAbort(e)) return;
        const m = humanize(e, { fallback: '登录窗口没能打开' });
        runtimeUnavailable = e.status === 503 && e.body?.runtime === true;
        renderHead();
        setWin('failed');
        message(m ? m.title : '登录窗口没能打开', runtimeUnavailable ? '请先点击上方的「启动运行环境」。' : (m && m.text ? m.text : ''), runtimeUnavailable ? null : reopenBtn());
      }
    },
  });

  const reopenBtn = () => btn('重新打开窗口', { icon: 'refresh', onClick: () => view.open() });

  // 打不开登录窗口时，左边写清楚为什么
  function showIdle() {
    if (!loaded) { message('正在读取通道…'); return; }
    if (!ch) { message('这条通道已经不在了'); return; }
    if (canOpen(ch)) { message('登录窗口已暂停', '回到这个窗口时会自动重新打开。'); return; }
    const state = displayState(ch, health);
    if (state === 'stopped') message('通道已停止', '启动以后才能登录。');
    else message('通道出错了，现在不能登录', '到概览里看看原因，或者重新启动它。');
  }

  // ---- 详情头 ----
  async function act(kind) {
    if (busy || !ch) return;
    busy = true;
    renderHead();
    try {
      const r = kind === 'runtime' ? await api.runtimeStart({ signal })
        : kind === 'stop' ? await api.stop(cid, { signal }) : await api.start(cid, { signal });
      showDone(r, kind === 'stop' ? '已停止' : '已启动');
      if (kind === 'runtime' && !signal.aborted) { runtimeUnavailable = false; view.open(); }
    } catch (e) {
      if (!isAbort(e)) showError(e);
    } finally {
      busy = false;
      if (!signal.aborted) {
        renderHead();
        if (ctx.data) ctx.data.refresh('core');
      }
    }
  }

  function renderHead() {
    if (!ch) { fill($head); return; }
    let state = displayState(ch, health);
    // 检测已经通过、通道列表还没刷新过来时，先按检测结果显示
    if (state === 'waiting' && health && health.connected === true) state = 'connected';
    $reopenBtn.disabled = runtimeUnavailable || !canOpen(ch);
    const acts = [];
    acts.push(btn(checking ? '正在检测' : '检测连通', { disabled: runtimeUnavailable || checking || busy || !canOpen(ch), onClick: checkNow }));
    if (runtimeUnavailable) acts.push(btn(busy ? '正在启动' : '启动运行环境', { icon: 'play', disabled: busy, onClick: () => act('runtime') }));
    else if (state !== 'stopped') acts.push(btn(busy ? '正在停止' : '停止', { icon: 'stop', disabled: busy, onClick: () => act('stop') }));
    else acts.push(btn(busy ? '正在启动' : '启动', { icon: 'play', disabled: busy, onClick: () => act('start') }));
    $head.replaceWith($head = detailHead(ch, { status: state === 'connected' ? headStatus({ ...ch, status: 'logged_in' }, health) : { kind: stateKind(state), text: stateText(state) }, acts }));
    $head.inert = focused;
  }

  function syncRules() {
    if (toolView && toolView.channel) toolView.channel(ch, health, system);
  }

  async function checkNow() {
    if (checking || !canOpen(ch)) return;
    checking = true;
    resetChecker();
    renderHead();
    try {
      const r = await api.status(cid, { signal });
      if (!signal.aborted && canOpen(ch)) { health = r; syncRules(); }
      if (!signal.aborted && ctx.data) ctx.data.refresh('core');
    } catch (e) { if (!isAbort(e)) showError(e); }
    finally {
      checking = false;
      if (!signal.aborted) { renderHead(); if (canOpen(ch)) startChecker(false); }
    }
  }

  // 已连接后仍检测，降为 15 秒；断开时恢复 5 秒。
  function startChecker(immediate = true) {
    if (checker || checking || signal.aborted) return;
    const stop = new AbortController();
    const cancel = () => stop.abort();
    signal.addEventListener('abort', cancel, { once: true });
    stop.signal.addEventListener('abort', () => signal.removeEventListener('abort', cancel), { once: true });
    checker = stop;
    const interval = health?.connected === true ? 15000 : HEALTH_MS;
    poll(async s => {
      if (!canOpen(ch)) return;
      let r;
      try { r = await api.channelHealth(cid, { signal: s }); } catch (e) { if (!isAbort(e)) return; throw e; }
      if (s.aborted || signal.aborted || !r || typeof r !== 'object') return;
      const was = !!(health && health.connected === true);
      health = r;
      renderHead();
      syncRules();
      if (was !== (r.connected === true) && ctx.data) ctx.data.refresh('core');
      if (interval !== (r.connected === true ? 15000 : HEALTH_MS)) {
        stop.abort();
        checker = null;
        startChecker(false);
      }
    }, interval, { signal: stop.signal, immediate });
  }

  function resetChecker() {
    if (checker) checker.abort();
    checker = null;
    health = null;
    syncRules();
  }

  // ---- 通道记录 ----
  const onCore = snap => {
    const channels = snap && snap.value && snap.value.channels;
    if (!Array.isArray(channels)) return;
    if (Object.hasOwn(snap.value, 'system')) system = snap.value.system;
    loaded = true;
    const next = channels.find(c => c.id === cid) || null;
    const wasOpen = canOpen(ch);
    const lostConnection = ch?.status === 'logged_in' && next?.status === 'running';
    ch = next;
    // 停止或出错过的通道，上一轮的检测结果不再算数
    if ((wasOpen && !canOpen(ch)) || lostConnection) resetChecker();
    renderHead();
    syncRules();
    if (canOpen(ch)) startChecker();
    if (canOpen(ch) !== wasOpen || win === 'idle') {
      view.sync();
      if (!canOpen(ch)) showIdle();
    }
  };

  if (loaded && !canOpen(ch)) health = null;
  renderHead();
  showIdle();
  // channels.js 已经把通道记录交过来时，不等下一轮通道列表就打开
  if (loaded) {
    if (canOpen(ch)) startChecker();
    view.sync();
  }
  if (ctx.data) ctx.data.subscribe('core', onCore, { signal });
  else api.channels({ signal }).then(list => onCore({ value: { channels: list } })).catch(() => {});

  return {
    update(query) {
      selectTool(workspaceTab(query));
    },
    unmount() {
      document.removeEventListener('keydown', onKeydown);
      setBackgroundInert(false);
      closeTool();
      ctrl.abort();
    },
  };
}
