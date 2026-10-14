// 测试地址（界面 18）：输入一个地址，看它会从哪里出去。命中的通道和地址在看板上高亮，其余变淡。
// 结果按现在的规则推算：按地址访问的连接可能先按域名匹配（DESIGN 5.3），所以结果不写「一定」。
// testView 是纯函数，顶层不碰 document / window。
import { h, fill, card, lb, btn, st, bul, ic, path, verdict, isEnter } from '../ui.js';
import { api, isUnavailable, isAbort } from '../api.js';
import { createLatest, isStale } from '../latest.js';
import { displayState, domainsVia, ruleInactive, rulesOff, ruleUnsupported } from '../derive.js';
import { PHRASE, DOMAINS_OFF, RULES_OFF, FATE } from '../terms.js';
import { ms } from '../format.js';
import { humanize } from '../feedback.js';
import { watchRules, watchTakeover, stateView, nameOf, fixButton, NO_PROXY_APPS } from './rules.js';
import { board, flowGroups, flowState } from './rules-flow.js';

const DEBOUNCE = 500;

// /api/rules/test 的结果 → 界面要显示的内容。
// 返回 {state: empty|unavailable|bad-input|error|result, tone, title, mark, sub, hops, hit, addr, note}
export function testView(res, { q = '', channels = [], health = {}, system = null, takeover = null, error = null } = {}) {
  if (!String(q).trim()) return { state: 'empty' };
  if (error) {
    if (Number(error.status) === 400) return { state: 'bad-input', title: '只能测一个域名或地址', text: '不能是网段，也不能一次填多个。' };
    const m = humanize(error) || { title: '没能测试这个地址', text: '' };
    return { state: 'error', title: m.title, text: m.text };
  }
  if (isUnavailable(res)) return { state: 'unavailable' };
  if (!res || typeof res !== 'object') return { state: 'empty' };

  const rule = res.rule || null;
  const known = res.channel ? (channels || []).find(c => c.id === res.channel.id) : null;
  const ch = known || (res.channel ? { ...res.channel } : null);
  const name = ch ? nameOf(ch) : '';
  const hl = ch ? health[ch.id] : null;
  const fs = ch ? flowState(ch, hl, system) : null;
  const ruleHop = rule ? { title: '命中规则', sub: rule.pattern, tone: 'brand' } : null;
  const out = { state: 'result', note: null, fix: null, hit: null, addr: null };
  const live = takeover && !isUnavailable(takeover) ? takeover : null;
  // 域名规则现在没有生效（DESIGN 5.8）：命中了规则也不说「能通」，写明没有生效，给能让它生效的动作
  const testedRule = { kind: res.kind, pattern: res.normalized || q, ...rule };
  const unsupported = ruleUnsupported(testedRule, live);
  const dead = unsupported ? { text: '当前接管方式不支持 IPv6，不能确认此地址经过通道。', fix: null }
    : ruleInactive(testedRule, live) ? rulesOff(live) : null;
  const inactiveText = unsupported ? FATE.unsupported.text : live?.effective === 'none' ? RULES_OFF : DOMAINS_OFF;

  if (dead) {
    out.note = dead.text;
    out.fix = dead.fix;
  } else if (live && live.effective === 'tun' && domainsVia(live) === 'system_proxy' && res.kind === 'domain') {
    out.note = `域名规则现在由系统代理处理，${NO_PROXY_APPS.join('、')} 这类不读系统代理的程序不按这个结果走。`;
  }

  if (dead && (res.result === 'channel' || res.result === 'blocked') && ch) {
    return {
      ...out, tone: 'brand', title: `规则指向 ${name}`, mark: name, sub: { kind: 'wait', text: inactiveText },
      hops: [ruleHop, { title: name, sub: inactiveText, tone: 'wait' }].filter(Boolean),
      hit: ch.id, addr: rule ? rule.pattern : null,
    };
  }
  if (res.result === 'channel' && ch) {
    const r = res.reachable;
    let sub;
    if (r && r.ok === true) sub = { kind: 'ok', text: r.latency_ms != null ? `能通，${ms(r.latency_ms)}` : '能通' };
    else if (r && r.ok === false) sub = { kind: 'bad', text: '通道连着，但这个地址现在连不上' };
    else sub = { kind: fs.kind, text: fs.text };
    return {
      ...out, tone: 'brand', title: `走 ${name}`, mark: name, sub,
      hops: [ruleHop, { title: name, sub: fs.text, tone: fs.kind === 'ok' ? 'brand' : fs.kind === 'bad' ? 'bad' : fs.kind === 'wait' ? 'wait' : 'ink-3' }].filter(Boolean),
      hit: ch.id, addr: rule ? rule.pattern : null,
    };
  }
  if (res.result === 'blocked') {
    return {
      ...out, tone: 'bad', title: '暂时阻止', mark: name || null,
      sub: { kind: 'bad', text: ch ? `${name}${displayState(ch, hl) === 'error' ? '出错' : '连不上'}，先拦下，不走原网络` : '先拦下，不走原网络' },
      hops: [ruleHop, ch ? { title: name, sub: fs.kind === 'bad' ? fs.text : '暂时阻止', tone: 'bad' } : null].filter(Boolean),
      hit: ch ? ch.id : null, addr: ch && rule ? rule.pattern : null,
    };
  }
  // 走原网络：没命中规则，或分流总开关关着。已停止通道的规则不生效，核心不把它们算作命中（rule、channel 都是 null）
  const off = !!(system && system.routing_off);
  const why = off ? '分流已关闭' : '没有命中生效的规则';
  const hops = [{ title: why, skip: true }, { title: '原网络', sub: '不经过任何通道', tone: 'ink-3' }];
  return { ...out, tone: 'off', title: '走原网络', mark: null, sub: { kind: 'off', text: why }, hops, hit: 'direct', addr: null };
}

export function mount(root, ctx) {
  const { signal, navigate, data } = ctx;
  let q = String(ctx.query.q ?? '');
  let res = null;
  let error = null;
  let busy = false;
  let asked = null;   // 上一次发出去的地址
  let takeover = null;
  let timer = null;
  const latest = createLatest(signal);

  const input = h('input', {
    type: 'text', value: q, placeholder: '输入一个域名或地址', 'aria-label': '要测试的地址',
    autocomplete: 'off', spellcheck: 'false', autofocus: true,
  });
  input.addEventListener('input', () => {
    clearTimeout(timer);
    timer = setTimeout(() => go(input.value), DEBOUNCE);
  });
  input.addEventListener('keydown', e => {
    if (isEnter(e)) { e.preventDefault(); clearTimeout(timer); go(input.value, true); }
  });

  const left = h('div', { class: 'rl-scroll rl-tboard' });
  const side = h('div', { class: 'rl-side' });
  fill(root,
    h('div', { class: 'hrow rl-head' }, h('h1', { class: 'h1' }, '规则'), h('span', { class: 'sp' }),
      h('label', { class: 'field rl-q' }, ic('target', 14), input),
      btn('结束测试', { onClick: () => navigate('#/rules') })),
    h('div', { class: 'rl-test' }, left, side));
  input.focus();

  const st0 = watchRules(data, signal, () => render());
  watchTakeover(signal, t => { takeover = t; render(); });

  // 地址写进地址栏（替换，不增加历史），由 update 发请求
  function go(v, force) {
    const t = String(v ?? '').trim();
    if (force && t === q.trim()) { asked = null; run(); return; }
    navigate('#/rules/test' + (t ? '?q=' + encodeURIComponent(t) : ''), { replace: true });
  }

  async function run() {
    const t = q.trim();
    if (t === asked) return;
    asked = t;
    error = null;
    res = null;
    if (!t) { latest.cancel(); busy = false; render(); return; }
    busy = true;
    render();
    try {
      res = await latest.run(s => api.rulesTest(t, { signal: s }));
    } catch (e) {
      if (isStale(e) || isAbort(e)) return;
      error = e;
    }
    busy = false;
    render();
  }

  function render() {
    if (signal.aborted) return;
    const state = stateView(st0, { onRetry: () => data.refresh('core'), navigate });
    const v = testView(res, { q, channels: st0.channels || [], health: st0.health, system: st0.system, takeover, error });
    const lit = !busy && v.state === 'result';
    if (state) fill(left, state);
    else fill(left, board(flowGroups(st0.channels, { health: st0.health, system: st0.system, takeover }), { hit: lit ? v.hit : null, addr: lit ? v.addr : null, cols: 2 }));
    fill(side, ...sideView(v));
  }

  function sideView(v) {
    if (busy) return [card([lb('结果'), h('div', { class: 'dim rl-wait' }, '正在查…')])];
    if (v.state === 'empty') return [card([lb('结果'), h('div', { class: 'dim rl-wait' }, '输入一个域名或地址，看它会从哪条通道出去。')])];
    if (v.state === 'unavailable') return [verdict('info', { panel: true, title: PHRASE.unavailable, text: '可以在列表里按地址搜索，看它归哪条通道。' })];
    if (v.state === 'bad-input' || v.state === 'error') return [verdict('bad', { panel: true, title: v.title, text: v.text })];
    const mk = v.mark ? bul(v.mark, { size: 'lg', idle: v.tone === 'off' }) : h('span', { class: 'bul lg idle', 'aria-hidden': 'true' }, ic('globe', 22));
    return [
      card([lb('结果'),
        h('div', { class: 'rl-res' }, mk, h('div', { class: 'two' },
          h('span', { class: 'rl-res-t cell', title: v.title }, v.title), h('small', null, st(v.sub.kind, v.sub.text)))),
        v.note ? h('div', { class: 'rl-warn', role: 'note' }, ic('info', 14, 1.6), h('span', null, v.note),
          fixButton(v.fix, { takeover, signal, navigate, onDone: t => { takeover = t; render(); } })) : null]),
      card([lb('为什么'), h('div', { class: 'rl-why' }, path(v.hops))], { cls: 'rl-grow' }),
    ];
  }

  render();
  run();

  return {
    update(query) {
      q = String((query && query.q) ?? '');
      if (document.activeElement !== input && input.value.trim() !== q.trim()) input.value = q;
      run();
    },
    unmount() { clearTimeout(timer); latest.cancel(); },
  };
}
