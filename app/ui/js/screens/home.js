// 总览（界面 01）；没有通道时同一路由渲染首次使用（界面 02）。
// 「需要处理」「已自动处理」和通道卡片的推导是导出的纯函数，顶层不碰 document / window，node 直接测试。
// 不显示「已阻止」数字（DESIGN 5.7：第一版没有数据来源）。
import { h, fill, card, lb, btn, st, tog, bul, ic, empty, pageState } from '../ui.js';
import { api, isUnavailable, isAbort } from '../api.js';
import { displayState, summary, chipState, channelRules, takeoverTodo } from '../derive.js';
import { stateText, stateKind, causeText, actionText, PHRASE, RETRY_FAILED } from '../terms.js';
import { rateParts, bytes, ms, clock } from '../format.js';
import { area } from '../chart.js';
import { showError, showDone, notify } from '../feedback.js';
import { poll } from '../poll.js';

const enc = encodeURIComponent;
const ruleOn = r => !!r && r.enabled !== false && r.enabled !== 0;
const nameOf = ch => (ch && ch.name) || '未命名通道';

// ---- 需要处理 ----

// 出错原因的动作 → 按钮。restart_runtime / retry 直接执行，relogin / view_logs 跳到通道详情
function causeAction(action, id) {
  const label = actionText(action) || actionText('view_logs');
  if (action === 'restart_runtime') return { label, run: 'runtime' };
  if (action === 'retry') return { label, run: 'start', id };
  if (action === 'relogin') return { label, go: `#/channels/${enc(id)}?tab=login` };
  return { label, go: `#/channels/${enc(id)}?tab=logs` };
}

// 接管网络那一条的按钮（fix 见 derive.takeoverTodo）
function takeoverAction(todo, takeover) {
  if (todo.fix === 'system-proxy-ok') return { label: todo.button, run: 'system-proxy-ok' };
  if (todo.fix === 'retry') return { label: todo.button, run: 'retry-takeover', mode: takeover.mode || 'auto' };
  if (todo.fix === 'settings') return { label: todo.button, go: '#/settings' };
  return null;
}

// 返回 [{key, tone: wait|bad, name?, title, text, action: {label, go|run, id?} | null}]。
// 顺序：运行环境 → 出错 → 待登录 → 还没验证 → 接管网络。全都没有时返回空数组（界面显示「没有需要处理的事」）。
// causes: {<cid>: /cause 的结果}；takeover: /api/takeover 的结果，接口不存在时为 UNAVAILABLE 或 null。
// 只是建议的那一条（已经用系统代理接管时的「启用虚拟网卡」）不在这里，见 takeoverTip。
export function needs({ channels = [], health = {}, system = null, causes = {}, takeover = null } = {}) {
  const head = [], bad = [], wait = [], unverified = [], tail = [];
  const chip = chipState(system);
  if (system?.config_application?.pending === true) {
    head.push({ key: 'config-pending', tone: 'wait', title: '规则还没同步', text: '已保存，还没同步到入口', action: { label: '重试', run: 'config' } });
  }
  // 运行环境没启动时顶栏已经有「启动」提示条，这一条不重复。
  // 「修复」重建的是分流入口（heal-proxy），对运行环境连不上外网没有用，这种情况只提示重新打开应用
  if (system && !['runtime', 'starting', 'dormant'].includes(chip)) {
    if (system.gave_up === true) {
      head.push({ key: 'runtime', tone: 'bad', title: '运行环境出问题了', text: '自动修复试了几次都没有成功', action: { label: '修复', run: 'heal' } });
    } else if (system.vm_egress_dead === true) {
      head.push({ key: 'runtime', tone: 'bad', title: '运行环境连不上外网', text: '退出通道管家再重新打开，运行环境会重新启动', action: null });
    }
  }
  for (const ch of channels || []) {
    const hl = health[ch.id];
    const state = displayState(ch, hl);
    const name = nameOf(ch);
    const blocked = ch.blocked === true || (hl && hl.blocked === true);
    // 分流总开关关着、或这条通道不参与分流时，它的规则本来就走原网络，不说被阻止
    const routed = !(system && system.routing_off === true) && ch.routing_enabled !== false;
    const held = blocked && routed ? channelRules(ch).filter(ruleOn).length : 0;
    const heldText = held ? `${held} 条规则暂时被阻止` : '';
    if (state === 'unknown') {
      wait.push({ key: 'unknown:' + ch.id, tone: 'wait', name, title: `${name} 状态待确认`,
        text: '暂时读不到运行状态，请稍后查看', action: { label: '查看', go: `#/channels/${enc(ch.id)}` } });
    } else if (state === 'error') {
      const c = causes[ch.id];
      const known = c && !isUnavailable(c) && typeof c === 'object';
      // 标题已经说了「连不上」，原因不清楚时副标题不再重复
      const reason = known && c.code && c.code !== 'unknown' ? causeText(c.code) : '原因还不清楚';
      bad.push({
        key: 'error:' + ch.id, tone: 'bad', name, title: `${name} 连不上`,
        text: heldText ? `${reason}，${heldText}` : reason,
        action: causeAction(known && c.action ? c.action : 'view_logs', ch.id),
      });
    } else if (state === 'waiting' && ch.login_method === 'headless') {
      // 不需要登录窗口的通道：没有登录这回事，只能去看看它的状态
      wait.push({
        key: 'wait:' + ch.id, tone: 'wait', name, title: `${name} 还没连通`,
        text: heldText || '还没有连上内网',
        action: { label: '查看', go: `#/channels/${enc(ch.id)}` },
      });
    } else if (state === 'waiting') {
      wait.push({
        key: 'wait:' + ch.id, tone: 'wait', name, title: `${name} 需要登录`,
        text: heldText || '登录以后才能访问内网',
        action: { label: '去登录', go: `#/channels/${enc(ch.id)}` },
      });
    } else if (state === 'unverified' && ch.login_method === 'headless') {
      unverified.push({
        key: 'unverified:' + ch.id, tone: 'wait', name, title: `${name} 还没验证`,
        text: '给它加一条域名或单独的地址，我们会用它来确认',
        action: { label: '添加规则', go: `?sheet=rule-add&channel=${enc(ch.id)}` },
      });
    } else if (state === 'unverified') {
      // 只有不需要登录窗口的通道能用规则里的地址来确认，其余的要填一个内网地址
      unverified.push({
        key: 'unverified:' + ch.id, tone: 'wait', name, title: `${name} 还没验证`,
        text: '登录以后，填一个连上才能打开的内网地址来确认',
        action: { label: '填写地址', go: `#/channels/${enc(ch.id)}` },
      });
    }
  }
  const t = takeover && !isUnavailable(takeover) ? takeoverTodo(takeover) : null;
  if (t && !t.tip) {
    tail.push({ key: 'takeover:' + takeover.action, tone: 'wait', title: t.title, text: t.text, action: takeoverAction(t, takeover) });
  }
  return [...head, ...bad, ...wait, ...unverified, ...tail];
}

// 接管网络的一句建议（不计入需要处理的数量）：{key, tone: 'tip', title, text, action}，没有时为 null
export function takeoverTip(takeover) {
  const t = takeover && !isUnavailable(takeover) ? takeoverTodo(takeover) : null;
  if (!t || !t.tip) return null;
  return { key: 'tip:' + takeover.action, tone: 'tip', title: t.title, text: t.text, action: takeoverAction(t, takeover) };
}

// ---- 已自动处理 ----

// 只认这三个来源；用户手动的操作是 src=audit，不算
const AUTO_SRC = new Set(['watchdog', 'manager', 'vm']);
// 事件名白名单（地图 08 第 2.5 节）→ 一句话。name 是事件 detail.cid 对应的通道名，可能没有
export const AUTO_EVENTS = Object.freeze({
  heal_done: () => '分流中断过一次，已自动修复',
  recovered: name => (name ? `${name} 已自动恢复` : '分流已自动恢复'),
  docker_reconnect: () => '和运行环境的连接断过一次，已自动重连', // wording-allow（核心的事件名）
  wake_detected: () => '电脑从睡眠中醒来，已自动检查一遍',
  vm_egress_recovered: () => '运行环境的外网出口已自动恢复',
  dns_cache_recovered: name => (name ? `${name} 的域名访问已自动恢复` : '通道的域名访问已自动恢复'),
  egress_guard_applied: () => '内网访问防护已自动生效',
});
export const REORDERED = '新规则和旧规则有重叠，已自动排好顺序';

const pad = n => String(n).padStart(2, '0');
// 当天显示 11:18，更早的显示 09-27
export function when(ts, now = Date.now()) {
  const t = Number(ts);
  if (!Number.isFinite(t) || t <= 0) return '';
  const d = new Date(t), n = new Date(now);
  if (d.getFullYear() === n.getFullYear() && d.getMonth() === n.getMonth() && d.getDate() === n.getDate()) return clock(t);
  return `${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

// 域名规则的写法规范化：去掉 *. / +. 前缀和末尾的点，转小写
const normDomain = p => String(p ?? '').trim().toLowerCase().replace(/^(\*|\+)\./, '').replace(/\.$/, '');
const suffixHit = (host, p) => host === p || host.endsWith('.' + p);

// 对当前的域名规则分别按插入顺序（规则 id）和按层级（标签数多的在前，同级保持顺序）排序，
// 拿每条规则自己的地址去求第一个命中的通道；有一条结果不同，就说明排序起了作用（DESIGN 5.1）。
// 停用的规则、已停止通道的规则不参与。
export function rulesReordered(channels = [], health = {}) {
  const rules = [];
  for (const ch of channels || []) {
    if (displayState(ch, health[ch.id]) === 'stopped') continue;
    for (const r of (ch && ch.domains) || []) {
      if (!ruleOn(r)) continue;
      const p = normDomain(r.pattern);
      if (p) rules.push({ id: Number(r.id) || 0, ch: ch.id, p, n: p.split('.').length });
    }
  }
  const byId = rules.slice().sort((a, b) => a.id - b.id);
  const byScope = byId.slice().sort((a, b) => b.n - a.n);
  const first = (list, host) => list.find(r => suffixHit(host, r.p));
  return rules.some(r => first(byId, r.p).ch !== first(byScope, r.p).ch);
}

// 返回 [{at, time, text}]，新的在前，同一句话只留最新一条，最多 limit 条。一条都没有时返回空数组（整块不显示）。
export function autoHandled({ events = [], channels = [], health = {}, now = Date.now(), limit = 5 } = {}) {
  const names = new Map((channels || []).map(ch => [ch.id, nameOf(ch)]));
  const out = [];
  // 由当前规则推导出来的，没有发生时间，时间一栏写「当前」，和其他行对齐
  if (rulesReordered(channels, health)) out.push({ at: null, time: '当前', text: REORDERED });
  const seen = new Set(out.map(x => x.text));
  const list = Array.isArray(events) ? events.slice().sort((a, b) => (Number(b.seq) || 0) - (Number(a.seq) || 0)) : [];
  for (const e of list) {
    if (!e || !AUTO_SRC.has(e.src) || !Object.hasOwn(AUTO_EVENTS, e.event)) continue;
    const cid = e.detail && typeof e.detail === 'object' ? e.detail.cid : null;
    const text = AUTO_EVENTS[e.event](cid != null ? names.get(cid) : null);
    if (seen.has(text)) continue;
    seen.add(text);
    out.push({ at: Number(e.ts_ms) || null, time: when(e.ts_ms, now), text });
  }
  return out.slice(0, limit);
}

// ---- 通道卡片 ----

// 已停止的通道不列。已连接的在前，按今天的流量从多到少；其余保持原来的顺序。
// traffic 是 /api/traffic/today 的结果；接口不存在或 available 为 false 时不显示流量。
export function channelRows({ channels = [], health = {}, traffic = null } = {}) {
  const tmap = traffic && !isUnavailable(traffic) && traffic.available !== false && traffic.channels && typeof traffic.channels === 'object'
    ? traffic.channels : null;
  const on = [], rest = [];
  for (const ch of channels || []) {
    const hl = health[ch.id];
    const state = displayState(ch, hl);
    if (state === 'stopped') continue;
    const row = { id: ch.id, name: nameOf(ch), state, latency: null, total: null, pct: 0 };
    if (state === 'connected') {
      row.latency = hl && hl.latency_ms != null ? hl.latency_ms : (ch.latency_ms ?? null);
      if (tmap) {
        const t = tmap[ch.id];
        row.total = t ? (Number(t.up) || 0) + (Number(t.down) || 0) : 0;
      }
      on.push(row);
    } else rest.push(row);
  }
  on.sort((a, b) => (b.total ?? -1) - (a.total ?? -1));
  const max = Math.max(0, ...on.map(r => r.total || 0));
  for (const r of on) r.pct = max > 0 && r.total ? Math.round((r.total / max) * 100) : 0;
  return [...on, ...rest];
}

// 标题下面的一句话
export function headline(online, todoCount) {
  return `${online} 条通道在线，` + (todoCount ? `${todoCount} 件事需要你处理` : '没有需要处理的事');
}

// ---- 首次使用的插图（静态，不含动态文字） ----

const FIRST_ART = '<svg width="240" height="100" viewBox="0 0 240 100" fill="none">' +
  '<path class="soft" d="M24 50h56c16 0 16-30 34-30h102"/><path class="main" d="M24 50h192"/><path class="soft" d="M24 50h56c16 0 16 30 34 30h102"/>' +
  '<circle class="src" cx="24" cy="50" r="11"/><circle class="end soft" cx="216" cy="20" r="7"/><circle class="end main" cx="216" cy="50" r="7"/><circle class="end soft" cx="216" cy="80" r="7"/></svg>';

// ---- 页面 ----

export function mount(root, ctx) {
  const { signal, navigate, data } = ctx;
  const s = { core: null, health: {}, conn: null, events: null, traffic: null, causes: {}, takeover: null, mode: '' };
  let refs = null;
  let routingBusy = false;
  const body = document.body;

  const channels = () => (s.core && s.core.value && s.core.value.channels) || null;
  const system = () => (s.core && s.core.value && s.core.value.system) || null;

  // 内容签名没变就不重建，按钮的焦点和按下状态不会被轮询打断
  function patch(el, sig, build) {
    if (el.dataset.sig === sig) return;
    el.dataset.sig = sig;
    fill(el, build());
  }

  async function runAction(a, el) {
    if (a.go) { navigate(a.go); return; }
    el.disabled = true;
    try {
      let r;
      if (a.run === 'heal') r = await api.healProxy({ signal });
      else if (a.run === 'config') r = await api.retryConfig({ signal });
      else if (a.run === 'runtime') r = await api.runtimeStart({ signal });
      else if (a.run === 'start') r = await api.start(a.id, { signal });
      else if (a.run === 'system-proxy-ok') r = await api.setTakeover({ system_proxy_ok: true }, { signal });
      else if (a.run === 'retry-takeover') r = await api.setTakeover({ mode: a.mode }, { signal });
      if (isUnavailable(r)) notify(PHRASE.unavailable);
      else if (a.run === 'retry-takeover' && r && r.action === 'retry_takeover') notify(RETRY_FAILED, { kind: 'bad' });
      else showDone(r, a.run === 'start' ? '已重新启动' : '已处理');
      // 接管的 PUT 返回执行之后的状态，直接用它
      if ((a.run === 'system-proxy-ok' || a.run === 'retry-takeover') && r && typeof r === 'object' && !isUnavailable(r)) s.takeover = r;
    } catch (e) {
      if (!isAbort(e)) showError(e);
    } finally {
      if (!signal.aborted) {
        el.disabled = false;
        data.refresh('core');
        side();
        takeoverPoll.kick();
      }
    }
  }

  function todoEl(item) {
    const el = h('div', { class: item.tone === 'bad' ? 'todo bad' : item.tone === 'tip' ? 'todo tip' : 'todo' },
      item.name ? bul(item.name, { size: 'md' }) : ic(item.tone === 'tip' ? 'info' : 'warn', 18, 1.6),
      h('div', { class: 'two' }, h('span', { class: 'cell', title: item.title }, item.title), item.text ? h('small', null, item.text) : null));
    if (item.action) {
      const b = btn(item.action.label, { cls: item.tone === 'wait' && item.action.go ? 'pri sm' : 'sm' });
      b.addEventListener('click', () => runAction(item.action, b));
      el.append(b);
    }
    return el;
  }

  function chRowEl(r) {
    const href = `#/channels/${enc(r.id)}`;
    if (r.state === 'connected') {
      return h('a', { class: 'row home-ch', href },
        bul(r.name, { size: 'md' }),
        h('div', { class: 'home-ch-main' },
          h('div', { class: 'home-ch-line' },
            h('b', { class: 'cell', title: r.name }, r.name),
            h('span', { class: 'dim num' }, ms(r.latency)),
            h('span', { class: 'sp' }),
            r.total != null ? h('span', { class: 'num' }, bytes(r.total)) : null),
          r.total != null ? h('div', { class: 'bar' }, h('i', { style: `width:${r.pct}%` })) : null));
    }
    return h('a', { class: 'row home-ch short', href },
      bul(r.name, { size: 'md' }),
      h('b', { class: 'cell', title: r.name }, r.name),
      h('span', { class: 'sp' }),
      st(stateKind(r.state), stateText(r.state)));
  }

  function bigParts(el, num, unit) {
    fill(el, num, unit ? h('small', null, unit) : null);
  }

  function buildFull() {
    const toggle = tog({
      xl: true, label: '分流',
      onChange: async checked => {
        routingBusy = true;
        toggle.disabled = true;
        try {
          await api.setRouting(!checked, { signal });
          await data.refresh('core');
        } catch (e) {
          if (!isAbort(e)) { toggle.checked = !checked; showError(e); }
        } finally {
          routingBusy = false;
          if (!signal.aborted) toggle.disabled = !system();
        }
      },
    });
    refs = {
      sub: h('p', { class: 'h1sub' }),
      toggle,
      online: h('div', { class: 'big' }),
      rules: h('div', { class: 'mid' }),
      lat: h('div', { class: 'mid' }),
      down: h('div', { class: 'big' }),
      up: h('div', { class: 'big' }),
      downChart: h('div', { class: 'chart-b' }),
      upChart: h('div', { class: 'chart-b' }),
      todo: h('div', { class: 'home-todos' }),
      auto: h('div', { class: 'home-auto', hidden: true }),
      autoRows: h('div', { class: 'rows' }),
      more: h('a', { class: 'more', href: '#/channels' }),
      chRows: h('div', { class: 'rows' }),
    };
    refs.auto.append(lb('已自动处理'), refs.autoRows);
    fill(root,
      h('h1', { class: 'h1' }, '总览'), refs.sub,
      h('div', { class: 'grid home-top' },
        card([lb('分流', refs.toggle), refs.online,
          h('div', { class: 'tri' },
            h('div', null, h('span', null, '规则'), refs.rules),
            h('div', null, h('span', null, '平均延迟'), refs.lat))], { cls: 'home-stat' }),
        card([lb('下载'), refs.down, refs.downChart], { cls: 'home-speed' }),
        card([lb('上传'), refs.up, refs.upChart], { cls: 'home-speed' })),
      h('div', { class: 'grid home-bottom' },
        card([h('div', { class: 'home-scroll' }, lb('需要处理'), refs.todo, refs.auto)], { cls: 'home-todo' }),
        card([h('div', { class: 'home-scroll' }, lb('通道', refs.more), refs.chRows)], { cls: 'home-channels' })));
  }

  function buildFirst() {
    const art = h('div', { class: 'home-art', 'aria-hidden': 'true' });
    art.innerHTML = FIRST_ART;
    const box = empty({
      title: '添加第一条通道',
      text: '把客户给你的安装包或配置文件拖进来，其余的交给我们。',
      action: btn('添加通道', { cls: 'pri lg', icon: 'plus', onClick: () => navigate('?sheet=add') }),
    });
    box.prepend(art);
    fill(root, box);
  }

  function renderSpeed() {
    if (!refs) return;
    const v = s.conn;
    const hist = (v && v.history) || [];
    const d = rateParts(v && v.down != null ? v.down : NaN);
    const u = rateParts(v && v.up != null ? v.up : NaN);
    bigParts(refs.down, d.num, d.unit);
    bigParts(refs.up, u.num, u.unit);
    refs.downChart.innerHTML = area(hist.map(p => p.down), { w: 300, h: 76 });
    refs.upChart.innerHTML = area(hist.map(p => p.up), { w: 300, h: 76 });
  }

  function update() {
    const chans = channels();
    const sys = system();
    const sum = summary(chans, s.health);
    const todo = needs({ channels: chans, health: s.health, system: sys, causes: s.causes, takeover: s.takeover });
    refs.sub.textContent = headline(sum.online, todo.length);
    bigParts(refs.online, String(sum.online), '条通道在线');
    bigParts(refs.rules, String(sum.rules), '条');
    if (sum.avgLatency == null) bigParts(refs.lat, '—'); else bigParts(refs.lat, String(sum.avgLatency), '毫秒');
    if (!routingBusy) {
      refs.toggle.checked = !!sys && sys.routing_off !== true;
      refs.toggle.disabled = !sys;
    }

    const tip = takeoverTip(s.takeover);
    patch(refs.todo, JSON.stringify([todo, tip]), () => [
      todo.length ? todo.map(todoEl) : h('div', { class: 'home-none' }, '没有需要处理的事'),
      tip ? todoEl(tip) : null]);

    const auto = autoHandled({ events: s.events && s.events.events, channels: chans, health: s.health });
    refs.auto.hidden = !auto.length;
    patch(refs.autoRows, JSON.stringify(auto), () => auto.map(a =>
      h('div', { class: 'row slim' }, h('span', { class: 'num dim3 home-when' }, a.time), h('span', { class: 'cell', title: a.text }, a.text))));

    const rows = channelRows({ channels: chans, health: s.health, traffic: s.traffic });
    refs.more.textContent = `全部 ${chans.length} 条`;
    patch(refs.chRows, JSON.stringify(rows), () => (rows.length ? rows.map(chRowEl) : h('div', { class: 'home-none' }, '所有通道都已停止')));
  }

  function render() {
    if (signal.aborted) return;
    const chans = channels();
    const mode = chans ? (chans.length ? 'full' : 'first') : (s.core && s.core.error ? 'error' : 'loading');
    body.classList.toggle('home-first', mode === 'first');
    if (mode !== s.mode) {
      s.mode = mode;
      refs = null;
      if (mode === 'full') buildFull();
      else if (mode === 'first') buildFirst();
      else if (mode === 'error') fill(root, h('div', { class: 'hrow' }, h('h1', { class: 'h1' }, '总览')), pageState('error', { onRetry: () => data.refresh('core') }));
      else fill(root, pageState('loading'));
    }
    if (mode === 'full') { update(); renderSpeed(); }
  }

  // 出错通道的原因、接管状态：探测接口，跟着通道列表刷新，同一时间只有一轮
  let sideBusy = false;
  async function side() {
    const chans = channels();
    if (sideBusy || signal.aborted || !chans) return;
    sideBusy = true;
    const errIds = chans.filter(ch => displayState(ch, s.health[ch.id]) === 'error').map(ch => ch.id);
    const causes = {};
    const jobs = errIds.map(id => api.cause(id, { signal }).then(r => { causes[id] = r; }, e => {
      if (!isAbort(e) && s.causes[id]) causes[id] = s.causes[id];
    }));
    await Promise.all(jobs);
    sideBusy = false;
    if (signal.aborted) return;
    s.causes = causes;
    render();
  }

  render();
  const takeoverPoll = poll(async pollSignal => {
    if (isUnavailable(s.takeover)) return;
    try { s.takeover = await api.takeover({ signal: pollSignal }); } catch (_) { return; }
    if (!signal.aborted) render();
  }, 30000, { signal });
  data.subscribe('core', snap => { s.core = snap; render(); side(); }, { signal });
  data.subscribe('health', snap => { if (snap.value) s.health = snap.value; render(); }, { signal });
  data.subscribe('connections', snap => { if (snap.value) s.conn = snap.value; else if (snap.error) s.conn = null; renderSpeed(); }, { signal, ms: 3000 });
  data.subscribe('events', snap => { s.events = snap.value && !isUnavailable(snap.value) ? snap.value : null; render(); }, { signal });
  data.subscribe('traffic', snap => { s.traffic = snap.value || null; render(); }, { signal });

  return {
    update() {},
    unmount() { body.classList.remove('home-first'); refs = null; },
  };
}
