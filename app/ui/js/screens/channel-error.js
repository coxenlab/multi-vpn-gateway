// 通道 · 出错（界面 08）：先说出错原因和该做的事，再列受影响的规则，日志默认收起。
// - 原因：GET /cause（探测）。接口不可用或拿不到时，显示「连不上，原因还不清楚」加「查看日志」。
// - 重试：POST start。
// - 换配置文件（只有 openvpn / wireguard）：PUT config-file 后 POST start。start 失败时停在本页显示它返回的错误，
//   不按通道的新状态跳走（返回值的 pinned() 为真时，上层分派留在本页，DESIGN C-08）。
// - 换一个版本（只有 easyconnect）：PATCH {ec_ver} 换成下一个 usable_here 的版本；返回的通道没有 container_id
//   或仍是 error 时，PATCH 只保存了版本号，接着 POST start（DESIGN 5.10）。
// 渲染整条详情列（详情头加内容）；ctx.head === false 时不画详情头，由上层画。
import { h, fill, btn, st, card, lb, verdict, ic, pageState } from '../ui.js';
import { api, isAbort, isUnavailable } from '../api.js';
import { createLatest, isStale } from '../latest.js';
import { humanize, showError, showDone, notify } from '../feedback.js';
import { causeText, ACTION, FATE, PHRASE, stateKind, stateText } from '../terms.js';
import { channelRules, ruleFate, displayState } from '../derive.js';
import { findChannel, detailHead, placeholder } from './channel-unverified.js';
import { restoreBar } from './channel-edit.js';
import { realLines } from './channel-logs.js';
import { configProblem } from './add.js';

// 能换配置文件的类型，和选文件时的扩展名提示
export const FILE_TYPES = Object.freeze({ openvpn: '.ovpn,.conf', wireguard: '.conf' });
const VERSION_TYPES = new Set(['easyconnect']);

// 每种原因后面跟的一句话
const EXPLAIN = Object.freeze({
  runtime_down: '运行环境停了，通道没法连接。重新启动运行环境以后再试。',
  image_pull_failed: '客户端没有下载下来，可能是网络不稳。点「重试」再下载一次。',
  crash_loop: '客户端一启动就退出了。可以先重试；一直这样的话，看看下面的日志。',
  kicked: '同一个账号在别的地方登录了，这边被挤下线。重新登录就能恢复。',
  unknown: '看看下面的日志，或点「重试」再连一次。',
});
const TYPE_HINT = Object.freeze({
  openvpn: '配置文件换过的话，点「换配置文件」。',
  wireguard: '配置文件换过的话，点「换配置文件」。',
  easyconnect: '服务器升级过的话，点「换一个版本」。',
});
const UNKNOWN = Object.freeze({ code: 'unknown', action: 'view_logs' });
const LOG_FOLDED = 4;

// 下一个能在本机用的版本：排在当前版本后面的第一个（到末尾从头找），跳过当前版本；没有时返回 null
export function nextVersion(versions, current) {
  const tags = [];
  for (const v of versions || []) if (v && v.usable_here && v.tag && !tags.includes(v.tag)) tags.push(v.tag);
  const at = tags.indexOf(current);
  for (let i = 1; i <= tags.length; i++) {
    const t = tags[(at + i + tags.length) % tags.length];
    if (t !== current) return t;
  }
  return null;
}

// PATCH 返回的通道还要不要再启动：没有 container_id（只保存了版本号），或者仍是出错
export const needsStart = ch => !ch || !ch.container_id || ch.status === 'error';
export const failurePinned = (failure, ch, health) => !!failure && displayState(ch, health) === 'error';

// 一行日志 → {time, text, err}。行首是时间（或日期加时间）时拆出来
const TIME = /^(?:\d{4}-\d{2}-\d{2}[ T])?(\d{1,2}:\d{2}:\d{2})(?:[.,]\d+)?(?:Z|[+-]\d{2}:?\d{2})?\s+(.*)$/;
export function logLine(line) {
  const s = String(line ?? '');
  const m = TIME.exec(s);
  const text = m ? m[2] : s;
  return { time: m ? m[1] : '', text, err: /失败|错误|error|fail|fatal/i.test(text) };
}

// 原因接口的返回 → {code, action}；不可用、没有原因时当作不清楚
export const toCause = r => (!r || isUnavailable(r) || !r.code ? UNKNOWN : { code: r.code, action: r.action || null });

export function mount(root, ctx) {
  const id = ctx.params.id;
  const { signal } = ctx;
  const causeLatest = createLatest(signal);
  const logsLatest = createLatest(signal);
  let ch = null;
  let health = ctx.health;
  let system = null;
  let cause = null;       // null 表示还在查
  let failure = null;     // 启动失败后留在本页显示的错误 {title, text}
  let busy = null;        // 'retry' / 'stop' / 'file' / 'version' / 'runtime'
  let fileOk = true;      // 换配置文件接口不可用时隐藏按钮
  let lines = null;       // null 表示还没读到
  let logsFailed = false;
  let unfolded = false;
  let built = false;
  let head = null;

  const picker = h('input', { type: 'file', hidden: true, 'aria-hidden': 'true', tabindex: '-1' });
  picker.addEventListener('change', () => {
    const f = picker.files && picker.files[0];
    picker.value = '';
    if (f) replaceFile(f);
  });
  const verdictSlot = h('div', { style: 'display:contents' });
  const restoreSlot = h('div', { style: 'display:contents' });
  const rulesBody = h('div');
  const logsPre = h('pre', { class: 'logs' });
  const logsToggle = h('button', { type: 'button', class: 'more', hidden: true, onClick: () => { unfolded = !unfolded; renderLogs(); } });

  function build() {
    head = ctx.head === false ? null : renderHead();
    fill(root, h('div', { class: 'cs' },
      head, restoreSlot, verdictSlot,
      card([lb('受影响的规则'), rulesBody]),
      card([lb('日志', logsToggle), logsPre], { cls: 'cs-logs' }),
      picker));
    built = true;
    fill(restoreSlot, restoreBar(ch));
    renderVerdict();
    renderRules();
    renderLogs();
  }

  function renderHead() {
    const state = cause?.code === 'runtime_unknown' ? 'unknown' : displayState(ch, health);
    return detailHead(ch, st(stateKind(state), stateText(state)), state === 'unknown' ? [] : [
      btn(busy === 'retry' ? '正在重试' : '重试', { icon: 'refresh', disabled: !!busy, onClick: retry }),
      btn(busy === 'stop' ? '正在停止' : '停止', { icon: 'stop', disabled: !!busy, onClick: stop }),
    ], { editable: state !== 'unknown' });
  }

  function actionButtons(c) {
    if (displayState(ch, health) === 'unknown' || c.code === 'runtime_unknown') {
      return [btn(ACTION.view_logs, { onClick: () => { unfolded = true; renderLogs(); logsPre.scrollIntoView({ block: 'nearest' }); } })];
    }
    const out = [];
    const add = (text, busyText, kind, onClick) => out.push(btn(busy === kind ? busyText : text, { cls: out.length ? '' : 'pri', disabled: !!busy, onClick }));
    if (c.action === 'restart_runtime') add(ACTION.restart_runtime, '正在启动', 'runtime', restartRuntime);
    if (c.action === 'relogin') add(ACTION.relogin, '', 'relogin', () => ctx.navigate(`#/channels/${encodeURIComponent(id)}?tab=login`));
    if (FILE_TYPES[ch.vpn_type] && fileOk) {
      add('换配置文件', '正在替换', 'file', () => { picker.accept = FILE_TYPES[ch.vpn_type]; picker.click(); });
    }
    if (VERSION_TYPES.has(ch.vpn_type)) add('换一个版本', '正在换版本', 'version', switchVersion);
    if (c.action === 'view_logs') add(ACTION.view_logs, '', 'logs', () => { unfolded = true; renderLogs(); logsPre.scrollIntoView({ block: 'nearest' }); });
    if (!out.length && c.action === 'retry') add(ACTION.retry, '正在重试', 'retry', retry);
    return out;
  }

  function renderVerdict() {
    if (!built) return;
    const c = cause || UNKNOWN;
    let title, text;
    const unknown = displayState(ch, health) === 'unknown' || c.code === 'runtime_unknown';
    if (unknown) { title = '状态待确认'; text = '正在确认运行状态。'; }
    else if (failure) { title = failure.title; text = failure.text; }
    else if (!cause) { title = '连不上'; text = '正在查找原因。'; }
    else { title = causeText(c.code); text = [EXPLAIN[c.code] || EXPLAIN.unknown, TYPE_HINT[ch.vpn_type]].filter(Boolean).join(''); }
    const acts = unknown || cause || failure ? actionButtons(c) : [];
    fill(verdictSlot, verdict(unknown ? 'wait' : 'bad', { panel: true, title, text, action: acts.length ? h('div', { class: 'cs-acts' }, acts) : null }));
  }

  function renderRules() {
    if (!built) return;
    const rules = channelRules(ch);
    if (!rules.length) { fill(rulesBody, h('div', { class: 'dim' }, '这条通道还没有规则。')); return; }
    const routingOff = !!(system && system.routing_off === true);
    let blocked = false;
    const rows = rules.map(r => {
      const fate = ruleFate({ ...r, enabled: r.enabled !== false && r.enabled !== 0 }, ch, { routingOff, state: 'error', blocked: ch.blocked === true });
      if (fate === 'blocked') blocked = true;
      const t = FATE[fate] || FATE.waiting;
      return h('div', { class: 'row' },
        h('span', { class: 'mono cell', title: r.pattern }, r.pattern), h('span', { class: 'sp' }),
        h('span', { class: ('fate ' + t.cls).trim() }, fate === 'blocked' ? ic('block', 13) : fate === 'channel' ? ic('check', 12, 2) : null, t.text));
    });
    fill(rulesBody, h('div', { class: 'rows cs-rules' }, rows),
      blocked ? h('div', { class: 'dim cs-foot' }, '通道恢复后自动放行。阻止是为了不让内网地址误走公网。') : null);
  }

  function renderLogs() {
    if (!built) return;
    if (logsFailed || !lines || !lines.length) {
      logsToggle.hidden = true;
      fill(logsPre, h('span', { class: 't' }, logsFailed ? '没能读取日志' : lines ? '还没有日志' : '正在读取日志'));
      return;
    }
    logsToggle.hidden = lines.length <= LOG_FOLDED;
    logsToggle.textContent = unfolded ? '收起' : '展开';
    const shown = unfolded ? lines : lines.slice(-LOG_FOLDED);
    const out = [];
    shown.forEach((line, i) => {
      const l = logLine(line);
      if (i) out.push('\n');
      if (l.time) out.push(h('span', { class: 't' }, l.time), '  ');
      out.push(l.err ? h('span', { class: 'e' }, l.text) : l.text);
    });
    fill(logsPre, out);
    logsPre.scrollTop = logsPre.scrollHeight;
  }

  function sync() {
    if (!built) return;
    if (head) { const next = renderHead(); head.replaceWith(next); head = next; }
    fill(restoreSlot, restoreBar(ch));
    renderVerdict();
  }

  async function loadCause() {
    try { cause = toCause(await causeLatest.run(s => api.cause(id, { signal: s }))); } catch (e) {
      if (isStale(e)) return;
      cause = UNKNOWN;
    }
    sync();
  }

  async function loadLogs() {
    try {
      const r = await logsLatest.run(s => api.logs(id, 200, { signal: s }));
      lines = realLines(r && r.lines).map(String);
      logsFailed = false;
    } catch (e) {
      if (isStale(e)) return;
      logsFailed = true;
    }
    renderLogs();
  }

  // 会启动通道的操作：失败时把错误留在本页（failure），不提示后就消失
  async function run(kind, fn) {
    if (busy) return;
    busy = kind;
    failure = null;
    let failTitle = '没有成功';
    sync();
    try {
      await fn(t => { failTitle = t; });
    } catch (e) {
      if (isAbort(e)) return;
      const m = humanize(e, { fallback: failTitle });
      if (m) failure = { title: failTitle, text: m.title === failTitle ? m.text : [m.title, m.text].filter(Boolean).join('。') };
      loadCause();
      loadLogs();
    } finally {
      if (!signal.aborted) { busy = null; sync(); ctx.data.refresh('core'); }
    }
  }

  function retry() {
    return run('retry', async fail => {
      fail('重试没有成功');
      await api.start(id, { signal });
    });
  }

  function replaceFile(file) {
    if (!file.size) { notify('这个文件是空的，换一份再试', { kind: 'bad' }); return undefined; }
    const big = configProblem(file);
    if (big) { notify(`${big.title}。${big.text}`, { kind: 'bad' }); return undefined; }
    return run('file', async fail => {
      fail('配置文件没能换上');
      const content = await file.text();
      const r = await api.setConfigFile(id, content, file.name, { signal });
      if (isUnavailable(r)) { fileOk = false; notify(PHRASE.unavailable, { kind: 'bad' }); return; }
      fail('换了配置文件，但没能连上');
      await api.start(id, { signal });
      notify('已换上新的配置文件，正在重新连接', { kind: 'ok' });
    });
  }

  function switchVersion() {
    return run('version', async fail => {
      fail('没能换版本');
      const r = await api.vpnVersions(ch.vpn_type, { signal });
      const tag = nextVersion(r && r.versions, ch.ec_ver);
      if (!tag) { notify('没有别的能用的版本了', { kind: 'bad' }); return; }
      const updated = await api.update(id, { ec_ver: tag }, { signal });
      if (needsStart(updated)) {
        fail(`换成了 ${tag} 版本，但没能连上`);
        await api.start(id, { signal });
      }
      showDone(updated, `已换成 ${tag} 版本`);
    });
  }

  async function restartRuntime() {
    if (busy) return;
    busy = 'runtime';
    sync();
    try {
      await api.runtimeStart({ signal });
      loadCause();
    } catch (e) {
      if (!isAbort(e)) showError(e, { fallback: '运行环境没能启动' });
    } finally {
      if (!signal.aborted) { busy = null; sync(); ctx.data.refresh('core'); }
    }
  }

  async function stop() {
    if (busy) return;
    busy = 'stop';
    sync();
    try {
      await api.stop(id, { signal });
    } catch (e) {
      if (!isAbort(e)) showError(e, { fallback: '没能停止' });
    } finally {
      if (!signal.aborted) { busy = null; sync(); ctx.data.refresh('core'); }
    }
  }

  ctx.data.subscribe('core', snap => {
    const next = findChannel(snap.value, id);
    if (snap.value && snap.value.system) system = snap.value.system;
    if (!next) {
      if (!built || (snap.value && Array.isArray(snap.value.channels))) { built = false; head = null; fill(root, placeholder(snap, ctx)); }
      return;
    }
    ch = next;
    if (!built) build(); else { sync(); renderRules(); }
  }, { signal });
  if (!built) fill(root, pageState('loading'));
  loadCause();
  loadLogs();

  return {
    update() {},
    unmount() {},
    // 启动失败的错误还在显示：上层分派时留在本页，不按通道的新状态跳走
    pinned: (next = ch, nextHealth = health) => {
      if (!failurePinned(failure, next, nextHealth)) failure = null;
      return !!failure;
    },
    channel(next, nextHealth) { ch = next; health = nextHealth; sync(); renderRules(); },
  };
}
