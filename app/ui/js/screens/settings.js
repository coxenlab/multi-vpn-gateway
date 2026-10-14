// 设置（界面 21）：启动相关的三个开关、接管网络（现在的网络去向、虚拟网卡和系统代理两个开关）、配合 Clash 的接入说明、
// 备份导出导入、当前版本、进入高级。
// /api/app-settings 和 /api/takeover 都是探测接口：前者 404 时隐藏「启动」卡片；后者 404 时退回现有的
// /api/entry/tun、/api/entry/system-proxy 两个手动开关。开发版（不允许宿主集成）不接管网络。
// 后台永远不装助手：「启用虚拟网卡」只由这里的按钮触发，会弹一次管理员密码框（DESIGN 5.8）。
// 导出函数都是纯函数，顶层不碰 document / window。
import { h, fill, card, lb, btn, tog, verdict, ic } from '../ui.js';
import { openSheet } from '../sheet.js';
import { api, isUnavailable, isAbort } from '../api.js';
import { poll } from '../poll.js';
import { PHRASE, TAKEOVER_ACTION, RETRY_FAILED } from '../terms.js';
import { takeoverTodo, domainsVia } from '../derive.js';
import { day } from '../format.js';
import { showError, showDone, notify, humanize, clean } from '../feedback.js';

const EVERY = 20000; // 和核心的接管循环同一间隔

// 两个开关的四种组合 ↔ 核心的方式名：都不开是 clash（全交给 Clash）
export const modeOf = (tun, proxy) => (tun ? (proxy ? 'tun_system_proxy' : 'tun') : (proxy ? 'system_proxy' : 'clash'));

// 两个开关想开成什么。核心给了 want 就用它；没有（旧核心）时按方式名推
export function wants(t) {
  const w = t && t.want;
  if (w && typeof w === 'object') return { tun: !!w.tun, proxy: !!w.system_proxy };
  const mode = t && t.mode;
  return { tun: mode === 'tun' || mode === 'tun_system_proxy', proxy: mode === 'system_proxy' || mode === 'tun_system_proxy' };
}

const proxyOurs = t => !!(t && t.system_proxy && t.system_proxy.enabled && t.system_proxy.ours);
const proxyForeign = t => !!(t && t.system_proxy && t.system_proxy.enabled && !t.system_proxy.ours);

// 「现在的网络」流向图：规则里的内网地址、规则里的域名、其余流量，各画一条现在实际走的链（path），按实际状态画，
// 不按开关画。state：ok 接住了 / wait 要用户自己确认或可能不通 / off 没有接住 / idle 本来就不归通道管家；
// how 是这一条的一句话说明，接住了的不用再显示。画法见 flowSvg。
const VIA = Object.freeze({ tun: '虚拟网卡', proxy: '系统代理', clash: 'Clash' });
const APP = '你的程序';
const DIRECT = '原来的网络';
const inward = via => [APP, via, '入口', '通道', '内网'];

export function netNow(t) {
  const tunOn = !!(t && t.tun && t.tun.enabled);
  const ours = proxyOurs(t);
  const clash = !!(t && t.clash_running);
  const via = domainsVia(t);
  const ip = tunOn ? { state: 'ok', path: inward(VIA.tun), how: '虚拟网卡接住，送进通道' }
    : ours ? { state: 'ok', path: inward(VIA.proxy), how: '系统代理接住，送进通道；不读系统代理的程序不走' }
      : clash ? { state: 'wait', path: [APP, VIA.clash, DIRECT], how: '只能靠 Clash，但 Clash 默认不接内网地址，多半打不开。打开虚拟网卡就能接住' }
        : { state: 'off', path: [APP, DIRECT], how: '没有接住，走原来的网络。打开虚拟网卡就能接住' };
  const domain = via === 'tun' ? { state: 'ok', path: inward(VIA.tun), how: '虚拟网卡接住，送进通道' }
    : via === 'system_proxy' ? { state: 'ok', path: inward(VIA.proxy), how: '系统代理接住，送进通道；不读系统代理的程序不走' }
      : via === 'clash' && clash ? { state: 'wait', path: inward(VIA.clash), how: '要在 Clash 里加过配置才会这样走，加没加这里看不到' }
        : via === 'clash' ? { state: 'off', path: [APP, DIRECT], how: 'Clash 没有运行，没有接住。启动 Clash，或者打开系统代理' }
          : { state: 'off', path: [APP, DIRECT], how: '没有接住，走原来的网络。打开系统代理，或者配合 Clash' };
  const rest = ours && clash ? { state: 'wait', path: [APP, DIRECT], how: '系统代理开着的时候，Clash 接不到这些流量' }
    : clash ? { state: 'idle', path: [APP, VIA.clash, '外网'], how: '通道管家不碰，照旧走 Clash' }
      : { state: 'idle', path: [APP, DIRECT], how: '通道管家不碰，照旧走原来的网络' };
  return [
    { key: 'ip', what: '规则里的内网地址', ...ip },
    { key: 'domain', what: '规则里的域名', ...domain },
    { key: 'rest', what: '其余的上网流量', ...rest },
  ];
}

// 流向图：把 netNow 的三条链画成一张节点连线图（SVG 字符串，内容都是本文件里的常量）。
// 四列：你的程序 → 接住它的那一环（虚拟网卡 / 系统代理 / Clash）→ 通道 → 终点（内网 / 外网 / 原来的网络）。
// 一类流量一种颜色；接住了的线上有流动的光点，没接住的是不动的虚线，要用户自己确认的是带光点的虚线。
const MAP_W = 456, NODE_W = 92, NODE_H = 40, ROW = 54, MAP_TOP = 34, MAP_PAD = 10;
const COL_X = [0, 121, 243, 364];
const NODE_SUB = Object.freeze({
  [APP]: '浏览器、命令行', [VIA.tun]: '只接规则里的地址', [VIA.proxy]: '自动代理', [VIA.clash]: '你自己的 Clash',
  通道: '经入口分流', 内网: '客户内网', 外网: '照常上网', [DIRECT]: '不经过通道',
});
const FLOW_NAME = Object.freeze({ ip: '内网地址', domain: '域名', rest: '其余流量' });

export function flowGraph(rows) {
  const paths = rows.map(r => ({ key: r.key, state: r.state, nodes: r.path.filter(n => n !== '入口') }));
  const used = new Set(paths.flatMap(p => p.nodes));
  const handlers = [VIA.tun, VIA.proxy, VIA.clash].filter(n => used.has(n));
  const ends = ['内网', '外网', DIRECT].filter(n => used.has(n));
  const count = Math.max(handlers.length, ends.length, 1);
  const y = i => MAP_TOP + i * ROW;
  const nodes = [{ id: APP, col: 0, y: MAP_TOP + ((count - 1) * ROW) / 2 }];
  handlers.forEach((id, i) => nodes.push({ id, col: 1, y: y(i) }));
  if (used.has('通道')) nodes.push({ id: '通道', col: 2, y: y(0) });
  ends.forEach((id, i) => nodes.push({ id, col: 3, y: y(i) }));
  const at = Object.fromEntries(nodes.map(n => [n.id, n]));
  // 同一段上有几类流量就错开几条线
  const share = {};
  for (const p of paths) p.nodes.slice(1).forEach((b, i) => { (share[`${p.nodes[i]}>${b}`] ||= []).push(p.key); });
  const edges = paths.flatMap(p => p.nodes.slice(1).map((b, i) => {
    const from = at[p.nodes[i]], to = at[b];
    const on = share[`${from.id}>${to.id}`];
    const dy = (on.indexOf(p.key) - (on.length - 1) / 2) * 7;
    const x1 = COL_X[from.col] + NODE_W, y1 = from.y + NODE_H / 2 + dy, x2 = COL_X[to.col], y2 = to.y + NODE_H / 2 + dy;
    const mid = (x1 + x2) / 2;
    return { key: p.key, state: p.state, d: `M${x1} ${y1}C${mid} ${y1} ${mid} ${y2} ${x2} ${y2}` };
  }));
  return { width: MAP_W, height: MAP_TOP + (count - 1) * ROW + NODE_H + MAP_PAD, nodes, edges };
}

export function flowSvg(rows) {
  const g = flowGraph(rows);
  const legend = rows.map((r, i) => `<g transform="translate(${i * 96} 9)"><path class="set-map-key k-${r.key}" d="M0 0h16"/>`
    + `<text class="set-map-legend" x="22" y="3.5">${FLOW_NAME[r.key]}</text></g>`).join('');
  const lines = g.edges.map(e => `<path class="set-map-base k-${e.key} ${e.state}" d="${e.d}"/>`
    + (e.state === 'off' ? '' : `<path class="set-map-run k-${e.key}" d="${e.d}"/>`)).join('');
  const boxes = g.nodes.map(n => `<g transform="translate(${COL_X[n.col]} ${n.y})">`
    + `<rect class="${n.id === DIRECT ? 'set-map-node out' : 'set-map-node'}" width="${NODE_W}" height="${NODE_H}" rx="9"/>`
    + `<text class="set-map-t" x="${NODE_W / 2}" y="17">${n.id}</text>`
    + `<text class="set-map-s" x="${NODE_W / 2}" y="31">${NODE_SUB[n.id] || ''}</text></g>`).join('');
  return `<svg class="set-map-svg" viewBox="0 0 ${g.width} ${g.height}" aria-hidden="true">${legend}${lines}${boxes}</svg>`;
}

// /api/system 明确说不允许宿主集成（开发版、隔离实例）
export const isDev = system => !!system && system.host_integrations_available === false;

// /api/takeover 的结果 → 「接管网络」卡片要显示的内容。
// now：「现在的网络」三行；tun / proxy：两个开关（on 是想开且能开，sub 是开关下面的小字，warn 是提醒色的一句）。
// actions 里的每一项对应一个按钮：install_tun 启用虚拟网卡，enable_system_proxy 打开系统代理，retry 重试。
// tip 为真时 hint 只是一句建议（已经用系统代理接管时的「启用虚拟网卡」）。
export function takeoverView(t, { dev = false } = {}) {
  if (dev || (t && t.allowed === false)) {
    return { locked: true, sub: PHRASE.devNoTakeover, now: [], tun: null, proxy: null, hint: '', tip: false, actions: [] };
  }
  const want = wants(t);
  const installed = !!(t && t.tun && t.tun.installed);
  const other = !!(t && t.other_app && (t.other_app.tun || t.other_app.system_proxy));
  const todo = takeoverTodo(t);
  const actions = [];
  if (todo && todo.fix === 'settings') {
    actions.push('install_tun');
    if (!todo.tip && !want.proxy) actions.push('enable_system_proxy');
  } else if (todo && todo.fix === 'system-proxy-ok') {
    actions.push('enable_system_proxy');
  } else if (todo && todo.fix === 'retry') {
    actions.push('retry');
  }
  let hint = todo ? todo.text : '';
  if (!todo && other) hint = TAKEOVER_ACTION.close_other_app.text;
  const clash = !!(t && t.clash_running);
  return {
    locked: false, sub: '', now: netNow(t), hint, tip: !!(todo && todo.tip), actions,
    tun: {
      on: want.tun && installed,
      sub: installed ? '只接规则里的地址，别的流量不动' : '只接规则里的地址，别的流量不动。第一次打开要输一次管理员密码',
    },
    proxy: {
      on: want.proxy,
      sub: proxyForeign(t) ? '系统代理现在指向别的地址，不是通道管家' : '没有 Clash 时用它接域名规则；不读系统代理的程序不走',
      warn: !clash ? '' : want.proxy ? 'Clash 正在运行：系统代理开着，Clash 接不到流量，平时靠它打开的网站会打不开'
        : 'Clash 正在运行：打开以后 Clash 接不到流量，用 Clash 的话不要开',
    },
  };
}

// Clash 在不在运行能看到，配置加没加看不到：始终把最后一步留给用户自己核实。
export function clashEntryView(t, detected) {
  const running = !!((detected && detected.running) || (t && t.clash_running));
  const title = '配合 Clash';
  if (running && proxyOurs(t)) return { state: 'off', title, text: 'Clash 正在运行，但系统代理开着，流量到不了它。', button: '查看配置' };
  if (running) return { state: 'wait', title, text: '域名规则要在 Clash 里加过一次配置才生效，加没加需要你在 Clash 里确认。', button: '查看配置' };
  return { state: 'off', title, text: 'Clash 没有运行。用 Clash 的话加一次配置，之后新加的规则会自动同步。', button: '查看配置' };
}

// /api/entry/setup-commands 的结果 → 通用代理地址（给用其他代理软件的人手动接入）。还没准备好时为空串
export function proxyAddr(r) {
  const port = r && r.port != null && r.port !== '?' ? String(r.port) : '';
  return port ? `127.0.0.1:${port}` : '';
}

// 退回旧接口时：/api/entry/tun 和 /api/entry/system-proxy 的结果 → 两个手动开关。不支持的一项为 null
export function fallbackView({ tun, proxy } = {}, { dev = false } = {}) {
  if (dev) return { dev: true, tun: null, proxy: null };
  const tv = tun && tun.supported
    ? { installed: !!tun.installed, on: !!(tun.installed && tun.enabled), sub: !tun.installed ? '还没启用，第一次要输一次管理员密码' : tun.enabled ? '已开启' : '已关闭' }
    : null;
  const pv = proxy && proxy.supported
    ? { on: !!(proxy.enabled && proxy.is_ours), sub: !proxy.enabled ? '已关闭' : proxy.is_ours ? '已开启' : '现在指向别的地址' }
    : null;
  return { dev: false, tun: tv, proxy: pv };
}

const ruleCount = c => (Array.isArray(c && c.domains) ? c.domains.length : 0) + (Array.isArray(c && c.ips) ? c.ips.length : 0);

export function backupSummary(channels, failed = false) {
  if (!Array.isArray(channels)) return failed ? '没能读取通道' : '正在读取';
  const rules = channels.reduce((n, c) => n + ruleCount(c), 0);
  return `${channels.length} 条通道，${rules} 条规则`;
}

// 外壳注入的版本号；没有注入或注入的就是「开发版」时显示开发版
export function versionText(v) {
  const s = typeof v === 'string' ? v.trim() : '';
  return `当前版本 ${s || '开发版'}`;
}

export const backupName = (ts = Date.now()) => `通道管家备份-${day(ts)}.json`;

// 读备份文件的文本。不是导出文件的格式时抛错，错误文字可以直接给用户看
export function parseBackup(text) {
  let doc;
  try { doc = JSON.parse(String(text ?? '')); } catch { doc = null; }
  if (!doc || typeof doc !== 'object' || Array.isArray(doc) || doc.kind !== 'vpnmgr-export' || !Array.isArray(doc.channels)) {
    throw new Error('这个文件不是通道管家的备份');
  }
  return doc;
}

// 导入结果 → 一句话。导入的通道都是已停止（现有行为）
export function importSummary(r) {
  const n = Array.isArray(r && r.imported) ? r.imported.length : 0;
  const skipped = Array.isArray(r && r.skipped) ? r.skipped : [];
  let text = n ? `已导入 ${n} 条通道，都是已停止，需要时再启动` : '没有导入任何通道';
  if (skipped.length) {
    const list = skipped.slice(0, 3).map(s => `${(s && s.name) || '没有名字的通道'}（${clean(s && s.reason) || '内容不对'}）`).join('、');
    text += `；跳过 ${skipped.length} 条：${list}${skipped.length > 3 ? ' 等' : ''}`;
  }
  return text;
}

// 设置页的一行：标题、小字说明、右边的控件
function sr(title, sub, ...ctl) {
  return h('div', { class: 'row' },
    h('div', { class: 'two sp' }, h('span', { class: 'set-t' }, title), sub ? h('small', null, sub) : null),
    ctl);
}

function download(name, text, type) {
  const url = URL.createObjectURL(new Blob([text], { type }));
  const a = h('a', { href: url, download: name, hidden: true });
  document.body.append(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

const settled = r => (r.status === 'fulfilled' ? r.value : null);

export function mount(root, ctx) {
  const { signal, data } = ctx;
  let system = null;
  let channels = null;
  let coreFailed = false;
  let app;              // undefined 读取中 / UNAVAILABLE / 设置对象 / null 读取失败
  let appBusy = '';     // 正在保存的开关
  let net = { kind: 'loading' }; // loading / takeover {t} / fallback {tun, proxy} / error
  let netBusy = '';     // mode / install / proxy_ok / tun / proxy
  let backupBusy = '';  // export / import
  let addr;             // undefined 读取中 / '' 还没准备好 / 地址 / null 读取失败
  let clash;            // undefined 读取中 / 探测结果 / null 探测不可用
  let sheet = null;
  const sigs = {};

  const slot = () => h('div', { class: 'set-slot' });
  const startSlot = slot();
  const netSlot = slot();
  const backupSlot = slot();
  const picker = h('input', { type: 'file', accept: '.json,application/json', hidden: true, 'aria-hidden': 'true', tabindex: '-1' });
  picker.addEventListener('change', () => {
    const f = picker.files && picker.files[0];
    picker.value = '';
    if (f) importFile(f);
  });

  fill(root, h('div', { class: 'set' },
    h('h1', { class: 'h1 set-h' }, '设置'),
    h('div', { class: 'grid set-cols' },
      h('div', { class: 'grid' }, startSlot, netSlot),
      h('div', { class: 'grid' },
        backupSlot,
        card([lb('版本'), h('div', { class: 'rows' }, sr(versionText(globalThis.__tongdaoAppVersion), ''))]),
        card(h('div', { class: 'rows' },
          h('a', { class: 'row set-adv', href: '#/settings/advanced' },
            h('div', { class: 'two sp' }, h('span', { class: 'set-t' }, '高级'), h('small', null, '运行环境、日志、问题诊断')),
            h('span', { class: 'dim3' }, ic('chev', 14)))), { cls: 'set-link' }))),
    picker));

  // 同一份内容不重建，轮询不会打断下拉框和按钮
  function paint(key, el, sig, build) {
    if (sigs[key] === sig) return;
    sigs[key] = sig;
    fill(el, build());
  }

  const dev = () => isDev(system);

  // ---- 启动 ----

  function renderStart() {
    if (isUnavailable(app)) { paint('start', startSlot, 'none', () => []); return; }
    paint('start', startSlot, JSON.stringify([app === undefined ? 'loading' : app, appBusy, dev()]), () => {
      if (app === null) {
        return card([lb('启动'), h('div', { class: 'rows' }, sr('没能读取', '稍后再试一次', btn('重试', { cls: 'sm', onClick: loadApp })))]);
      }
      const ready = !!app && !appBusy;
      const row = (key, title, sub, extra = {}) => sr(title, extra.sub ?? sub, tog({
        checked: !!(app && app[key]), label: title, disabled: !ready || extra.disabled,
        onChange: v => setApp(key, v),
      }));
      return card([lb('启动'), h('div', { class: 'rows' },
        row('launch_at_login', '开机时自动打开', '', dev() ? { disabled: true, sub: '开发版不能设置' } : {}),
        row('auto_connect', '打开后自动连接通道', '需要登录的通道会提醒你'),
        row('keep_running_on_close', '关闭窗口后继续运行', '留在菜单栏'))]);
    });
  }

  async function loadApp() {
    app = undefined;
    renderStart();
    try { app = await api.appSettings({ signal }); } catch (e) {
      if (isAbort(e)) return;
      app = null;
    }
    renderStart();
  }

  async function setApp(key, value) {
    if (appBusy || !app || isUnavailable(app)) return;
    appBusy = key;
    renderStart();
    try {
      const r = await api.setAppSettings({ [key]: value }, { signal });
      if (isUnavailable(r)) app = r;
      else app = { ...app, ...(r && typeof r === 'object' ? r : { [key]: value }) };
    } catch (e) {
      if (isAbort(e)) return;
      showError(e);
    }
    appBusy = '';
    renderStart();
  }

  // ---- 接管网络 ----

  function renderNet() {
    paint('net', netSlot, JSON.stringify([net, netBusy, dev(), addr === undefined ? 0 : addr, clash === undefined ? 0 : clash]), () => {
      const rows = [];
      if (net.kind === 'loading' || net.kind === 'error') {
        rows.push(net.kind === 'loading'
          ? sr('现在的网络', '正在读取')
          : sr('现在的网络', '没能读取', btn('重试', { cls: 'sm', onClick: () => poller.kick() })));
      } else if (net.kind === 'takeover') {
        const v = takeoverView(net.t, { dev: dev() });
        if (v.locked) rows.push(sr('接管', v.sub));
        else {
          const map = h('div', { class: 'set-map', role: 'img', 'aria-label': v.now.map(r => `${r.what}：${r.how}`).join('；') });
          map.innerHTML = flowSvg(v.now);
          rows.push(h('div', { class: 'set-now' },
            h('div', { class: 'set-now-h' }, '现在的网络'),
            map,
            v.now.filter(r => r.state === 'wait' || r.state === 'off').map(r => h('div', { class: `set-now-row ${r.state}` },
              h('span', { class: 'set-state', 'aria-hidden': 'true' }),
              h('span', { class: 'set-now-how' }, `${r.what}：${r.how}`)))));
          if (v.hint || v.actions.length) {
            rows.push(h('div', { class: v.tip ? 'row set-hint tip' : 'row set-hint' },
              h('div', { class: 'two sp' }, v.hint ? h('span', null, v.hint) : null),
              h('div', { class: 'set-acts' }, v.actions.map((a, i) => actionBtn(a, i === 0 && !v.tip)))));
          }
          rows.push(sr('虚拟网卡', v.tun.sub, tog({ checked: v.tun.on, label: '虚拟网卡', disabled: !!netBusy, onChange: toggleTun })));
          rows.push(clashRow(net.t));
          rows.push(h('div', { class: 'row set-tall' },
            h('div', { class: 'two sp' }, h('span', { class: 'set-t' }, '系统代理'), h('small', null, v.proxy.sub),
              v.proxy.warn ? h('small', { class: 'set-warn' }, v.proxy.warn) : null),
            tog({ checked: v.proxy.on, label: '系统代理', disabled: !!netBusy, onChange: toggleProxy })));
        }
      } else {
        const v = fallbackView(net, { dev: dev() });
        if (v.dev) rows.push(sr('方式', PHRASE.devNoTakeover));
        else {
          if (v.tun) {
            rows.push(sr('虚拟网卡', v.tun.sub, v.tun.installed
              ? tog({ checked: v.tun.on, label: '虚拟网卡', disabled: !!netBusy, onChange: on => fallbackSet('tun', on) })
              : actionBtn('install_tun', true)));
          }
          if (v.proxy) {
            rows.push(sr('系统代理', v.proxy.sub, tog({ checked: v.proxy.on, label: '系统代理', disabled: !!netBusy, onChange: on => fallbackSet('proxy', on) })));
          }
          if (!v.tun && !v.proxy) rows.push(sr('方式', PHRASE.unavailable));
        }
        rows.push(clashRow(null));
      }
      rows.push(addrRow());
      return card([lb('接管网络'), h('div', { class: 'rows' }, rows)]);
    });
  }

  // 通用代理地址：给第三方代理软件手动接入（DESIGN 5.8）
  const addrRow = () => sr('其他代理软件', addr === null ? '没能读取代理地址' : '把内网地址指向这个代理地址',
    addr === null ? null : h('span', { class: addr ? 'num set-val' : 'dim' }, addr === undefined ? '正在读取' : addr || '还没准备好'));

  async function loadAddr(s) {
    try {
      addr = proxyAddr(await api.entrySetup({ signal: s }));
    } catch (e) {
      if (isAbort(e)) return;
      if (!addr) addr = null;
    }
    renderNet();
  }

  function actionBtn(action, primary) {
    if (action === 'install_tun') {
      return btn(netBusy === 'install' ? '正在启用' : TAKEOVER_ACTION.install_tun.button, { cls: primary ? 'pri sm' : 'sm', disabled: !!netBusy, onClick: installTun });
    }
    if (action === 'retry') {
      return btn(netBusy === 'retry' ? '正在重试' : TAKEOVER_ACTION.retry_takeover.button, { cls: primary ? 'pri sm' : 'sm', disabled: !!netBusy, onClick: retryTakeover });
    }
    return btn(netBusy === 'mode' ? '正在打开' : '打开系统代理', { cls: primary ? 'pri sm' : 'sm', disabled: !!netBusy, onClick: () => toggleProxy(true) });
  }

  const clashRow = t => {
    const v = clashEntryView(t, clash && !isUnavailable(clash) ? clash : null);
    return h('div', { class: `row set-clash ${v.state}` },
      h('span', { class: 'set-state', 'aria-hidden': 'true' }),
      h('div', { class: 'two sp' }, h('span', { class: 'set-t' }, v.title), h('small', null, v.text)),
      btn(v.button, { cls: v.state === 'wait' ? 'pri sm' : 'sm', onClick: openClash }));
  };

  async function loadClash(s) {
    try { clash = await api.clashDetect({ signal: s }); } catch (e) {
      if (isAbort(e)) return;
      clash = null;
    }
    renderNet();
  }

  async function loadNet(s) {
    try {
      const t = await api.takeover({ signal: s });
      if (!isUnavailable(t)) {
        net = { kind: 'takeover', t };
      } else if (dev()) {
        net = { kind: 'fallback', tun: null, proxy: null };
      } else {
        const [tun, proxy] = await Promise.allSettled([api.tunGet({ signal: s }), api.systemProxyGet({ signal: s })]);
        if (s && s.aborted) return;
        if (tun.status === 'rejected' && proxy.status === 'rejected') throw tun.reason;
        net = { kind: 'fallback', tun: settled(tun), proxy: settled(proxy) };
      }
    } catch (e) {
      if (isAbort(e)) return;
      if (net.kind === 'loading') net = { kind: 'error' };
    }
    renderNet();
  }

  // 接管相关的写操作：期间按钮置灰，结束后立即重读一次
  async function netAction(kind, fn) {
    if (netBusy) return;
    netBusy = kind;
    renderNet();
    try { await fn(); } catch (e) {
      if (isAbort(e)) return;
      showError(e);
    }
    netBusy = '';
    renderNet();
    poller.kick();
  }

  const setMode = (mode, done) => netAction('mode', async () => {
    const r = await api.setTakeover({ mode }, { signal });
    if (!isUnavailable(r) && r && typeof r === 'object') net = { kind: 'takeover', t: r };
    showDone(r, done);
  });

  // 两个开关各管各的：改一个时另一个保持现在想开的样子
  const toggleTun = on => {
    const t = net.t;
    if (on && !(t && t.tun && t.tun.installed)) return installTun();
    return setMode(modeOf(on, wants(t).proxy), on ? '虚拟网卡已打开' : '虚拟网卡已关闭');
  };
  const toggleProxy = on => setMode(modeOf(wants(net.t).tun, on), on ? '系统代理已打开' : '系统代理已关闭');

  // 重试：按当前方式再保存一次，核心清掉重试间隔、立即按实时状态执行，返回执行之后的状态（F1）
  const retryTakeover = () => netAction('retry', async () => {
    const r = await api.setTakeover({ mode: net.t && net.t.mode ? net.t.mode : 'tun' }, { signal });
    if (isUnavailable(r)) { notify(PHRASE.unavailable); return; }
    if (r && typeof r === 'object') net = { kind: 'takeover', t: r };
    if (r && r.action === 'retry_takeover') notify(RETRY_FAILED, { kind: 'bad' });
    else showDone(r, '已重新接管');
  });

  // 第一次启用虚拟网卡会弹管理员密码框。点了取消（或没装好）时提示可以改用系统代理（T-03）。装好就算打开了这个开关
  const installTun = () => netAction('install', async () => {
    try { await api.tunInstall({ signal }); } catch (e) {
      if (isAbort(e)) throw e;
      const m = humanize(e, { fallback: '虚拟网卡没有启用' });
      notify(`${m ? m.title : '虚拟网卡没有启用'}。可以改开系统代理。`, { kind: 'bad', ms: 8000 });
      return;
    }
    if (net.kind === 'takeover') {
      // 装好以后立即应用一次，不等后台的下一轮
      const r = await api.setTakeover({ mode: modeOf(true, wants(net.t).proxy) }, { signal });
      if (!isUnavailable(r) && r && typeof r === 'object') net = { kind: 'takeover', t: r };
    } else {
      await api.tunSet(true, { signal });
    }
    notify('虚拟网卡已启用', { kind: 'ok' });
  });

  const fallbackSet = (what, on) => netAction(what, async () => {
    const r = what === 'tun' ? await api.tunSet(on, { signal }) : await api.systemProxySet(on, { signal });
    showDone(r, on ? '已打开' : '已关闭');
  });

  // ---- 配合 Clash 的接入说明 ----

  function openClash() {
    const code = h('pre', { class: 'logs set-code', tabindex: '0', 'aria-label': 'Clash 配置片段' }, '正在读取');
    let text = '';
    const copy = btn('复制', { cls: 'sm', icon: 'copy', disabled: true, onClick: copySnippet });
    const pac = h('div', { class: 'rows' });
    const verge = btn('下载', { cls: 'sm', icon: 'export', onClick: downloadVerge });
    const done = btn('关闭', { onClick: () => sheet && sheet.close('close') });
    done.setAttribute('autofocus', ''); // 打开时焦点落在「关闭」，不落在配置片段上

    sheet = openSheet({
      title: '配合 Clash', sub: '在你现在用的 Clash 里加一次，之后新加的规则会自动同步。', width: 'w680',
      body: h('div', { class: 'set-sheet' },
        h('div', { class: 'lb set-lb' }, '配置片段', h('span', { class: 'sp' }), copy),
        code,
        h('div', { class: 'rows set-more' }, sr('用 Clash Verge', '也可以下载一份扩展配置，在 Clash Verge 里导入', verge)),
        pac),
      foot: [h('span', { class: 'sp' }), done],
      onClose: () => { sheet = null; },
    });
    const my = sheet;

    api.snippet({ signal }).then(r => {
      if (sheet !== my) return;
      text = typeof r === 'string' ? r : '';
      code.textContent = text || '没能读取';
      copy.disabled = !text;
    }, e => { if (!isAbort(e) && sheet === my) code.textContent = '没能读取，关掉再打开试试'; });

    // 通用代理地址在设置页的「接管网络」卡片里；这里只补自动代理配置地址
    api.entrySetup({ signal }).then(r => {
      if (sheet !== my) return;
      if (r && r.pac_url) fill(pac, sr('自动代理配置地址', '支持自动代理配置的程序可以直接填它', h('span', { class: 'num set-val' }, String(r.pac_url))));
    }, () => {});

    async function copySnippet() {
      try {
        await navigator.clipboard.writeText(text);
        notify('已复制', { kind: 'ok' });
      } catch {
        const range = document.createRange();
        range.selectNodeContents(code);
        const sel = window.getSelection();
        sel.removeAllRanges();
        sel.addRange(range);
        notify('没能自动复制，已经选中，按 ⌘C 复制', { kind: 'info' });
      }
    }

    async function downloadVerge() {
      verge.disabled = true;
      try {
        const r = await api.mergeProfile({ signal });
        download('通道管家-Clash-Verge.yaml', typeof r === 'string' ? r : '', 'text/yaml;charset=utf-8');
      } catch (e) {
        if (!isAbort(e)) showError(e);
      }
      verge.disabled = false;
    }
  }

  // ---- 备份 ----

  function renderBackup() {
    const summary = backupSummary(channels, coreFailed);
    paint('backup', backupSlot, JSON.stringify([summary, backupBusy]), () => card([lb('备份'), h('div', { class: 'rows' },
      sr(summary, '换电脑时用', h('div', { class: 'set-acts' },
        btn(backupBusy === 'export' ? '正在导出' : '导出', { cls: 'sm', disabled: !!backupBusy, onClick: askExport }),
        btn(backupBusy === 'import' ? '正在导入' : '导入', { cls: 'sm', disabled: !!backupBusy, onClick: () => picker.click() }))))]));
  }

  // 导出前先提示备份里有密码（S-04）
  function askExport() {
    if (backupBusy) return;
    const go = handle => { handle.close('close'); doExport(); };
    sheet = openSheet({
      title: '导出备份', sub: '换电脑时用它把通道和规则带过去。', width: 'w520',
      body: verdict('wait', {
        title: '备份里有密码',
        text: '不用登录窗口的通道，账号密码和密钥文件会一起写进备份，拿到这个文件的人就能连上。请放在安全的地方，不要发给别人。要在登录窗口里登录的通道不带密码，备忘录也不带。',
      }),
      foot: [h('span', { class: 'sp' }), btn('取消', { onClick: () => sheet && sheet.close('close') }), btn('导出', { cls: 'pri', onClick: () => sheet && go(sheet) })],
      onConfirm: go,
      onClose: () => { sheet = null; },
    });
  }

  async function doExport() {
    backupBusy = 'export';
    renderBackup();
    try {
      const doc = await api.exportConfig({ signal });
      download(backupName(), JSON.stringify(doc, null, 2) + '\n', 'application/json;charset=utf-8');
      const n = doc && Array.isArray(doc.channels) ? doc.channels.length : 0;
      notify(`已导出 ${n} 条通道`, { kind: 'ok' });
    } catch (e) {
      if (isAbort(e)) return;
      showError(e);
    }
    backupBusy = '';
    renderBackup();
  }

  async function importFile(file) {
    if (backupBusy) return;
    backupBusy = 'import';
    renderBackup();
    try {
      let doc;
      try { doc = parseBackup(await file.text()); } catch (e) {
        notify(e.message, { kind: 'bad' });
        return;
      }
      const r = await api.importConfig(doc, { signal });
      const n = Array.isArray(r && r.imported) ? r.imported.length : 0;
      notify(importSummary(r), { kind: n ? 'ok' : 'info', ms: 8000 });
      data.refresh('core');
    } catch (e) {
      if (isAbort(e)) return;
      showError(e);
    } finally {
      if (!signal.aborted) { backupBusy = ''; renderBackup(); }
    }
  }

  // ---- 数据 ----

  data.subscribe('core', snap => {
    const v = snap.value || {};
    channels = v.channels ?? channels;
    system = v.system ?? system;
    coreFailed = !!snap.error;
    renderStart();
    renderNet();
    renderBackup();
  }, { signal });

  renderStart();
  renderNet();
  renderBackup();
  loadApp();
  const poller = poll(async s => {
    await Promise.all([loadNet(s), loadClash(s)]);
    if (!addr) await loadAddr(s);
  }, EVERY, { signal });

  return {
    update() {},
    unmount() { if (sheet) sheet.close('close'); },
  };
}
