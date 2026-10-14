// 通道（界面 03–05，并按状态分派到 06、07、08）。三栏：侧栏、通道列表、详情。
// 列表按「使用中」「已停止」分组；详情头和分页（概览 / 规则 / 日志）在这里渲染，分页内容由 channel-*.js 负责。
// 不带 tab 时按状态分派：出错 → channel-error，还没验证 → channel-unverified，交互型待登录 → channel-login，其余 → 概览。
// 分派到的模块和页面模块同一个签名：mount(root, ctx) → {update(query), unmount()}。
// 06、07、08 拿到整个详情列（root 是 .dcol），ctx 另带 channel、health；详情头可以用这里导出的 detailHead。
import { h, fill, btn, st, bul, search, empty, pageState } from '../ui.js';
import { api, isAbort } from '../api.js';
import { load } from '../routes.js';
import { displayState, isActive } from '../derive.js';
import { stateText, stateKind, typeLabel } from '../terms.js';
import { ms } from '../format.js';
import { showError, showDone, notify } from '../feedback.js';
import { bind, unbind, moreMenu, restoreBar } from './channel-edit.js';

const TABS = [
  { key: 'overview', label: '概览' },
  { key: 'rules', label: '规则' },
  { key: 'logs', label: '日志' },
];
const TAB_MODULE = { overview: 'channel-overview', rules: 'channel-rules', logs: 'channel-logs', login: 'channel-login' };
const STATE_MODULE = { error: 'channel-error', unverified: 'channel-unverified' };
// 这些模块自己渲染整个详情列（没有分页）
const WHOLE = new Set(['channel-error', 'channel-unverified', 'channel-login']);
// 这两个自己在详情头下面放「恢复上一次设置」；其余的由这里放
const OWN_RESTORE = new Set(['channel-error', 'channel-unverified']);

// 同一次运行里记住：上次选中的通道、搜索词、列表滚动位置（切换通道会整页重挂）
let lastId = null;
let lastSearch = '';
let lastScroll = 0;

// 交互型：要在登录窗口里登录的通道
export const interactive = ch => !!ch && ch.login_method !== 'headless';

// 有无客户端窗口都使用同一组导航；保持标签位置与键盘操作一致。
export function channelTabs(value, onChange, panelId = 'channel-tab-panel') {
  const choose = key => {
    for (const button of tabs.children) {
      const on = button.dataset.key === key;
      button.classList.toggle('on', on);
      button.setAttribute('aria-selected', String(on));
      button.tabIndex = on ? 0 : -1;
    }
    onChange(key);
  };
  const tabs = h('div', { class: 'channel-tabs', role: 'tablist', 'aria-label': '通道详情', onKeydown: e => {
    if (!['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(e.key)) return;
    e.preventDefault();
    const at = TABS.findIndex(t => t.key === e.target.dataset.key);
    const index = e.key === 'Home' ? 0 : e.key === 'End' ? TABS.length - 1
      : (at + (e.key === 'ArrowRight' ? 1 : -1) + TABS.length) % TABS.length;
    choose(TABS[index].key);
    tabs.children[index].focus({ preventScroll: true });
  } }, TABS.map(tab => h('button', {
    type: 'button', role: 'tab', class: tab.key === value ? 'on' : '',
    id: `${panelId}-tab-${tab.key}`, 'data-key': tab.key, 'aria-controls': panelId,
    'aria-selected': String(tab.key === value), tabindex: tab.key === value ? '0' : '-1',
    onClick: () => choose(tab.key),
  }, tab.label)));
  return tabs;
}

// 选哪个模块。tab 取 overview / rules / logs / login，其余当作没带。返回模块名
export function pickView(ch, health, tab, currentModule = null) {
  const s = displayState(ch, health);
  // 运行中的交互客户端始终留在统一工作台：分页只切换右侧工具，登录窗口不卸载。
  // 状态未知或已经出错时保留显式分页入口，便于查看日志和排障。
  if (s !== 'unknown' && s !== 'error' && interactive(ch) && isActive(ch)) return 'channel-login';
  if (TAB_MODULE[tab]) return TAB_MODULE[tab];
  if (s === 'unknown') return 'channel-overview';
  if (s === 'error') return STATE_MODULE.error;
  if (s === 'unverified') return STATE_MODULE.unverified;
  return 'channel-overview';
}

// 列表分组：使用中（没停止的）、已停止；按名称、服务器、类型搜索
export function groupChannels(channels, healthMap = {}, q = '') {
  const key = String(q || '').trim().toLowerCase();
  const hit = ch => !key || [ch.name, ch.server, typeLabel(ch.vpn_type)].some(v => String(v || '').toLowerCase().includes(key));
  const used = [], stopped = [];
  for (const ch of channels || []) {
    if (!hit(ch)) continue;
    (displayState(ch, healthMap[ch.id]) === 'stopped' ? stopped : used).push(ch);
  }
  return { used, stopped };
}

// 没指定通道时选哪条：上次选的，否则使用中的第一条，否则第一条
export function pickDefault(channels, last, healthMap = {}) {
  const list = channels || [];
  if (last && list.some(c => c.id === last)) return last;
  const g = groupChannels(list, healthMap);
  const first = g.used[0] || g.stopped[0];
  return first ? first.id : null;
}

// 运行时长「2小时15分」→「2 小时 15 分」
export const spaced = s => String(s || '').replace(/(\d)(?=[^\d\s.])/g, '$1 ').replace(/([^\d\s.])(?=\d)/g, '$1 ');

// 详情头的状态文字
export function headStatus(ch, health) {
  const s = displayState(ch, health);
  const text = s === 'connected' && ch.uptime ? `${stateText(s)} ${spaced(ch.uptime)}` : stateText(s);
  return { kind: stateKind(s), text };
}

// 详情头：标记、名称、类型、状态，右侧放按钮，最后是「更多」菜单
export function detailHead(ch, { status, acts = [] } = {}) {
  const s = status || headStatus(ch);
  return h('div', { class: 'dhead' },
    bul(ch.name, { size: 'lg', idle: s.kind === 'off' }),
    h('div', { class: 't' },
      h('b', { title: ch.name }, ch.name),
      h('span', { class: 'dim' }, typeLabel(ch.vpn_type), '　'),
      st(s.kind, s.text)),
    h('div', { class: 'acts' }, acts, moreMenu(ch)));
}

function listItem(ch, health, on) {
  const s = displayState(ch, health);
  const latency = health && health.latency_ms != null ? health.latency_ms : ch.latency_ms;
  const right = s === 'connected'
    ? h('span', { class: 'r' }, ms(latency))
    : h('span', { class: 'st ' + stateKind(s), 'aria-hidden': 'true' }, h('i'));
  return h('a', { class: on ? 'litem on' : 'litem', href: '#/channels/' + encodeURIComponent(ch.id), 'aria-current': on ? 'page' : null, title: ch.name },
    bul(ch.name, { size: 'md', idle: s === 'stopped' }),
    h('div', { class: 'two' }, h('span', null, ch.name), h('small', null, stateText(s))),
    right);
}

export function mount(root, ctx) {
  const { signal, navigate, data } = ctx;
  document.body.classList.add('channel-workspace-mode');
  bind(ctx);
  let params = ctx.params || {};
  let query = ctx.query || {};
  let core = null;          // {channels, system}
  let coreError = null;
  let healthLoaded = false;
  const hmap = {};          // 通道 id → 最近一次 /health 或 /status 的结果
  const asked = new Set();  // 已经单独补取过 /health 的通道
  let busy = '';            // '' / check / start / stop
  let sub = null;           // 当前详情 {module, id, ctrl, view, el}
  let subGen = 0;

  const listBox = h('div', { class: 'lscroll', onScroll: e => { lastScroll = e.currentTarget.scrollTop; } });
  const searchBox = search({ placeholder: '搜索通道', value: lastSearch, onInput: v => { lastSearch = v; renderList(); } });
  const lcol = h('div', { class: 'lcol' }, searchBox, listBox);
  const dcol = h('div', { class: 'dcol' });
  const body = h('div', { class: 'tri-pane' }, lcol, dcol);
  const title = h('div', { class: 'hrow' }, h('h1', { class: 'h1' }, '通道'));
  fill(root, title, pageState('loading'));

  const channels = () => (core && core.channels) || [];
  const selectedId = () => params.id || pickDefault(channels(), lastId, hmap);
  const current = () => channels().find(c => c.id === selectedId()) || null;

  function mergeHealth(id, v) {
    if (!v || typeof v !== 'object') return;
    const old = hmap[id];
    if (old && old.checked_at != null && v.checked_at != null && old.checked_at > v.checked_at) return;
    hmap[id] = v;
  }

  // ---- 列表 ----

  function renderList() {
    const id = selectedId();
    const g = groupChannels(channels(), hmap, lastSearch);
    const kids = [];
    if (g.used.length) kids.push(h('div', { class: 'lgroup' }, '使用中'), g.used.map(c => listItem(c, hmap[c.id], c.id === id)));
    if (g.stopped.length) kids.push(h('div', { class: 'lgroup' }, '已停止'), g.stopped.map(c => listItem(c, hmap[c.id], c.id === id)));
    if (!kids.length) kids.push(h('div', { class: 'lnone' }, '没有找到'));
    fill(listBox, kids);
  }

  // ---- 详情头的按钮 ----

  async function act(kind, ch, fn) {
    if (busy) return;
    busy = kind;
    renderDetail();
    try {
      const r = await fn({ signal });
      if (kind === 'check') {
        mergeHealth(ch.id, r);
        if (r && r.connected) notify(`能连通，${ms(r.latency_ms)}`, { kind: 'ok' });
        else notify('没有连通。确认已经登录，或者稍后再试一次', { kind: 'bad' });
      } else showDone(r, kind === 'start' ? '已启动' : '已停止');
    } catch (e) {
      if (!isAbort(e)) showError(e);
    }
    if (signal.aborted) return;
    busy = '';
    await data.refresh('core');
    if (!signal.aborted) render();
  }

  function headActs(ch) {
    const s = displayState(ch, hmap[ch.id]);
    const out = [];
    if (s !== 'stopped') {
      out.push(btn(busy === 'check' ? '正在检测' : '检测连通', { icon: 'bolt', disabled: !!busy, onClick: () => act('check', ch, o => api.status(ch.id, o)) }));
      out.push(btn(busy === 'stop' ? '正在停止' : '停止', { icon: 'stop', disabled: !!busy, onClick: () => act('stop', ch, o => api.stop(ch.id, o)) }));
    } else {
      out.push(btn(busy === 'start' ? '正在启动' : '启动', { icon: 'play', disabled: !!busy, onClick: () => act('start', ch, o => api.start(ch.id, o)) }));
    }
    return out;
  }

  // ---- 详情 ----

  function renderDetail() {
    const ch = current();
    if (!ch) {
      closeSub();
      fill(dcol, empty({
        title: '这条通道已经不在了',
        action: btn('回到通道列表', { onClick: () => navigate('#/channels') }),
      }));
      return;
    }
    const health = hmap[ch.id];
    // 运行中的通道要先知道「还没验证」与否才能分派
    if (isActive(ch) && !health && !query.tab) {
      if (!asked.has(ch.id) && healthLoaded) fetchHealth(ch.id);
      if (!asked.has(ch.id) || fetching.has(ch.id)) {
        closeSub();
        fill(dcol, pageState('loading'));
        return;
      }
    }
    // 出错页里「换配置文件」后启动失败时要停在本页显示错误，不按通道的新状态跳走（C-08）
    if (!query.tab && sub && sub.id === ch.id && sub.view && sub.view.pinned && sub.view.pinned(ch, health)) return;
    const module = pickView(ch, health, query.tab, sub?.id === ch.id ? sub.module : null);
    if (!sub ||sub.module !== module || sub.id !== ch.id) openSub(module, ch, health);
    else {
      if (sub.restore) fill(sub.restore, restoreBar(ch));
      if (!sub.tabbed) return;
      sub.head.replaceWith(sub.head = detailHead(ch, { status: headStatus(ch, health), acts: headActs(ch) }));
      if (sub.view && sub.view.channel) sub.view.channel(ch, health, core && core.system);
    }
  }

  const fetching = new Set();
  async function fetchHealth(id) {
    asked.add(id);
    fetching.add(id);
    try { mergeHealth(id, await api.channelHealth(id, { signal })); } catch (e) { if (isAbort(e)) return; }
    fetching.delete(id);
    if (!signal.aborted) render();
  }

  function closeSub() {
    if (!sub) return;
    const old = sub;
    sub = null;
    old.ctrl.abort();
    try { if (old.view && old.view.unmount) old.view.unmount(); } catch (e) { console.error('[unmount]', old.module, e); }
  }

  async function openSub(module, ch, health) {
    closeSub();
    const my = { module, id: ch.id, ctrl: new AbortController(), view: null, tabbed: !WHOLE.has(module) };
    sub = my;
    const gen = ++subGen;
    let host = dcol;
    // 「恢复上一次设置」放在详情头下面
    my.restore = OWN_RESTORE.has(module) ? null : h('div', { class: 'ch-restore-slot' }, restoreBar(ch));
    if (my.tabbed) {
      my.head = detailHead(ch, { status: headStatus(ch, health), acts: headActs(ch) });
      const tab = TAB_MODULE[query.tab] ? query.tab : 'overview';
      host = h('div', { class: 'ch-body', id: 'channel-tab-panel', role: 'tabpanel', 'aria-labelledby': `channel-tab-panel-tab-${tab}` });
      fill(dcol, my.head, my.restore, channelTabs(tab, k => navigate(`#/channels/${encodeURIComponent(ch.id)}?tab=${k}`)), host);
    } else if (my.restore) {
      // 登录页自己画详情头：它的内容排在提示条后面，详情头用 order 提到提示条前面（channels.css）
      my.restore.classList.add('ch-lift');
      host = h('div', { class: 'ch-whole' });
      fill(dcol, my.restore, host);
    } else fill(dcol);
    fill(host, pageState('loading'));
    let mod;
    try { mod = await load(module); } catch (e) {
      if (gen !== subGen) return;
      console.error('[channel]', module, e);
      fill(host, pageState('error', { title: '这一页没能打开', onRetry: () => location.reload() }));
      return;
    }
    if (gen !== subGen || signal.aborted) return;
    fill(host);
    try {
      my.view = mod.mount(host, {
        params: { id: ch.id }, query, signal: my.ctrl.signal, navigate, data,
        channel: ch, health, system: core && core.system,
      }) || null;
    } catch (e) {
      console.error('[mount]', module, e);
      fill(host, pageState('error', { title: '这一页没能打开', onRetry: () => location.reload() }));
    }
  }

  // ---- 整页 ----

  function render() {
    if (signal.aborted) return;
    if (!core || !core.channels) {
      if (coreError) fill(root, title, pageState('error', { onRetry: () => data.refresh('core') }));
      return;
    }
    if (!channels().length) {
      closeSub();
      fill(root, title, pageState('empty', {
        title: '还没有通道',
        text: '把客户给你的安装包或配置文件拖进来，或者点下面的按钮。',
        action: btn('添加通道', { cls: 'pri lg', icon: 'plus', onClick: () => navigate('?sheet=add') }),
      }));
      return;
    }
    const first = body.parentNode !== root;
    if (first) fill(root, title, body);
    const id = selectedId();
    if (id) lastId = id;
    renderList();
    if (first) {
      listBox.scrollTop = lastScroll;
      const on = listBox.querySelector('.litem.on');
      if (on && on.scrollIntoView) on.scrollIntoView({ block: 'nearest' });
    }
    renderDetail();
  }

  data.subscribe('core', snap => {
    coreError = snap.error;
    if (snap.value) core = snap.value;
    render();
  }, { signal });
  data.subscribe('health', snap => {
    if (!snap.value) return;
    healthLoaded = true;
    for (const [id, v] of Object.entries(snap.value)) mergeHealth(id, v);
    render();
  }, { signal });

  return {
    // 只改 query（换分页）：模块没变就交给它自己，变了由 renderDetail 重新挂
    update(q) {
      query = q || {};
      const ch = current();
      if (sub && ch && sub.view && sub.view.update && pickView(ch, hmap[ch.id], query.tab) === sub.module) sub.view.update(query);
      render();
    },
    unmount() { closeSub(); unbind(); document.body.classList.remove('channel-workspace-mode'); },
  };
}
