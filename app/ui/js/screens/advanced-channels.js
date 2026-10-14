// 设置 · 高级 · 各通道的运行情况（界面 22 的二级页）：分流入口、每条通道、恢复中的备用资源、残留。
// 功能和原来一样：启动 / 停止通道、修复分流、清理残留、看原始日志。原始日志原样显示，里面的内部用词是有意保留的。
// 推导是纯函数，顶层不碰 document / window。
import { h, fill, card, btn, st, chip, pageState } from '../ui.js';
import { openSheet } from '../sheet.js';
import { api, isAbort } from '../api.js';
import { poll } from '../poll.js';
import { typeLabel } from '../terms.js';
import { showError, notify } from '../feedback.js';
import { pageHead, confirmSheet, healNow, ageText } from './settings-advanced.js';

const EVERY = 8000;
const DIAG_EVERY = 20000; // 逐条通道进去检查，开销大，间隔拉长
const enc = encodeURIComponent;

// 每一项的名字和说明（不显示内部名称）
// types: 通道类型的中文名表（/api/vpn-types 的 key → label），拿不到时用内置的
export function roleView(x, types = {}) {
  if (x.role === 'infra') {
    return /^vpncore-/.test(String(x.name || ''))
      ? { title: '内网连通检查', sub: '检测通道连通时临时用到' }
      : { title: '分流入口', sub: '所有分流都经过它' };
  }
  if (x.role === 'channel') return { title: x.channel_name || '未命名通道', sub: typeLabel(x.vpn_type, types[x.vpn_type]), channel: true };
  if (x.role === 'replacement') return { title: '通道恢复用的备份', sub: '在通道详情里处理' };
  return { title: '残留', sub: '不属于任何通道，可以放心清理' };
}

const STATE = Object.freeze({
  running: { kind: 'ok', text: '运行中' },
  exited: { kind: 'off', text: '已停止' },
  restarting: { kind: 'wait', text: '正在重启' },
  created: { kind: 'wait', text: '刚创建' },
  paused: { kind: 'off', text: '已暂停' },
  dead: { kind: 'bad', text: '已退出' },
  missing: { kind: 'off', text: '不存在' },
});

// 状态和补充说明。d 是这条通道在 /api/diag 里的结果，可以没有
export function stateView(x, d) {
  let s = STATE[x.state] || { kind: 'plain', text: '看不出来' };
  const notes = [];
  if (x.crash_loop) s = { kind: 'bad', text: '反复重启' };
  if (x.state === 'running' && x.uptime_secs != null) notes.push(`已运行 ${ageText(x.uptime_secs)}`);
  if (x.crash_loop) notes.push(`重启了 ${Number(x.restart_count) || 0} 次`);
  else if (Number(x.restart_count) > 0) notes.push(`累计重启 ${Number(x.restart_count)} 次`);
  if (x.exit_code != null && x.exit_code !== 0) notes.push(`退出码 ${x.exit_code}`);
  if (x.state === 'missing' && x.role === 'channel' && x.channel_status && x.channel_status !== 'stopped') notes.push('点「启动」会重新建好');
  if (d && x.state === 'running') {
    if (d.ec_client_alive === false) notes.push('客户端没在运行');
    if (d.tunnel_up === false) notes.push('隧道没起来');
    if (d.wg_handshake_age != null && Number(d.wg_handshake_age) > 180) notes.push(`${ageText(d.wg_handshake_age)}没有握手`);
    if (d.ec_kick_age != null && Number(d.ec_kick_age) < 7200) notes.push(`${ageText(d.ec_kick_age)}前被服务端踢下线`);
  }
  return { ...s, note: notes.join('，') };
}

// 每一项能做的操作：heal / start / stop / clean / logs / open
export function actionsOf(x) {
  const out = [];
  const exists = x.state !== 'missing';
  if (x.role === 'infra') {
    if (!/^vpncore-/.test(String(x.name || ''))) out.push('heal');
  } else if (x.role === 'channel') {
    const stopped = x.channel_status === 'stopped' || x.channel_status === 'error' || x.state === 'missing' || x.state === 'exited';
    out.push(stopped ? 'start' : 'stop');
  } else if (x.role === 'replacement') {
    if (x.channel_id) out.push('open');
  } else {
    out.push('clean');
  }
  if (exists) out.push('logs');
  return out;
}

export function countsText(list) {
  const xs = Array.isArray(list) ? list : [];
  const ch = xs.filter(x => x.role === 'channel');
  const orphans = xs.filter(x => x.role === 'orphan').length;
  const bad = xs.filter(x => x.crash_loop || (x.exit_code != null && x.exit_code !== 0)).length;
  const parts = [`${ch.length} 条通道，${ch.filter(x => x.state === 'running').length} 条在运行`];
  if (bad) parts.push(`${bad} 项有问题`);
  if (orphans) parts.push(`${orphans} 项残留`);
  return parts.join('，');
}

export function mount(root, ctx) {
  const { signal, navigate } = ctx;
  let list = null;
  let failed = false;
  let diag = new Map();
  let busy = '';
  let sig = '';
  let sheet = null;
  let types = {};

  const sum = h('span', { class: 'dim adv-count' });
  const body = h('div', { class: 'adv-body' });
  fill(root, h('div', { class: 'adv' },
    pageHead('高级', '#/settings/advanced', '各通道的运行情况', sum),
    body));

  function actionBtn(a, x) {
    const dis = !!busy;
    const doing = busy === a + ':' + x.name;
    if (a === 'heal') return btn(doing ? '正在修复' : '修复', { cls: 'sm', disabled: dis, onClick: () => run(a, x, () => healNow({ signal })) });
    if (a === 'start') return btn(doing ? '正在启动' : '启动', { cls: 'sm', disabled: dis, onClick: () => run(a, x, async () => { await api.start(x.channel_id, { signal }); notify(`「${x.channel_name}」已启动，需要的话重新登录`, { kind: 'ok' }); }) });
    if (a === 'stop') return btn(doing ? '正在停止' : '停止', { cls: 'sm ghost', disabled: dis, onClick: () => run(a, x, async () => { await api.stop(x.channel_id, { signal }); notify(`「${x.channel_name}」已停止`, { kind: 'ok' }); }) });
    if (a === 'clean') return btn(doing ? '正在清理' : '清理', { cls: 'sm danger', disabled: dis, onClick: () => clean(x) });
    if (a === 'open') return btn('去通道', { cls: 'sm', onClick: () => navigate(`#/channels/${enc(x.channel_id)}`) });
    return btn('日志', { cls: 'sm ghost', onClick: () => showLogs(x) });
  }

  function render() {
    const next = JSON.stringify([list, failed, [...diag], busy, types]);
    if (next === sig) return;
    sig = next;
    if (!list) { sum.textContent = ''; fill(body, failed ? pageState('error', { onRetry: () => poller.kick() }) : pageState('loading')); return; }
    sum.textContent = countsText(list);
    const rows = list.map(x => {
      const rv = roleView(x, types);
      const sv = stateView(x, x.role === 'channel' ? diag.get(String(x.channel_id)) : null);
      const idle = x.state !== 'running';
      return h('div', { class: 'tr adv-ct-row' },
        h('div', { class: 'two' }, rv.channel ? chip(rv.title, { idle }) : h('span', { class: 'cell adv-t', title: rv.title }, rv.title), rv.sub ? h('small', { class: 'cell', title: rv.sub }, rv.sub) : null),
        h('div', { class: 'two' }, st(sv.kind, sv.text), sv.note ? h('small', { class: 'cell', title: sv.note }, sv.note) : null),
        h('div', { class: 'adv-acts adv-r' }, actionsOf(x).map(a => actionBtn(a, x))));
    });
    fill(body, card([
      h('div', { class: 'tr th adv-ct-row' }, h('div', null, '名称'), h('div', null, '状态'), h('div', { class: 'adv-r' }, '操作')),
      rows.length ? rows : h('div', { class: 'adv-none dim' }, '还没有任何通道。'),
    ], { cls: 'tcard adv-ct' }));
  }

  async function load(s) {
    try {
      const r = await api.containers({ signal: s });
      list = r && Array.isArray(r.containers) ? r.containers : [];
      failed = false;
    } catch (e) {
      if (isAbort(e)) return;
      if (!list) failed = true;
    }
    if (!busy) render();
  }

  // 拿不到时只是少几条补充说明，不报错
  async function loadDiag(s) {
    try {
      const r = await api.diag({ signal: s });
      diag = new Map((r && Array.isArray(r.channels) ? r.channels : []).map(x => [String(x.id), x]));
    } catch (e) {
      if (isAbort(e)) return;
    }
    if (!busy) render();
  }

  async function run(a, x, fn) {
    if (busy) return;
    busy = a + ':' + x.name;
    render();
    try { await fn(); } catch (e) { if (!isAbort(e)) showError(e); }
    if (signal.aborted) return;
    busy = '';
    sig = '';
    await poller.kick();
    render();
  }

  async function clean(x) {
    if (busy) return;
    const ok = await confirmSheet({ title: '清理残留', ok: '清理', danger: true, text: '它不属于任何通道，清理以后不影响现有的通道。' });
    if (ok) run('clean', x, async () => { await api.containerRemove(x.name, { signal }); notify('已清理', { kind: 'ok' }); });
  }

  // 原始日志放在弹层里，原样显示
  function showLogs(x) {
    const pre = h('pre', { class: 'logs adv-logs', tabindex: '0', 'aria-label': '原始日志' }, '正在读取');
    const rv = roleView(x, types);
    const my = new AbortController();
    const again = btn('刷新', { cls: 'sm', icon: 'refresh', onClick: () => fetchLogs() });
    sheet = openSheet({
      title: `${rv.title} 的原始日志`, sub: '最近 300 行，给排查问题用。', width: 'w680',
      body: pre,
      foot: [again, h('span', { class: 'sp' }), btn('关闭', { onClick: () => sheet && sheet.close('close') })],
      onClose: () => { sheet = null; my.abort(); },
    });
    async function fetchLogs() {
      again.disabled = true;
      try {
        const r = await api.containerLogs(x.name, 300, { signal: my.signal });
        const lines = r && Array.isArray(r.lines) ? r.lines : [];
        pre.textContent = lines.length ? lines.join('\n') : '还没有日志';
        pre.scrollTop = pre.scrollHeight;
      } catch (e) {
        if (isAbort(e)) return;
        pre.textContent = '没能读取日志';
        showError(e);
      }
      again.disabled = false;
    }
    fetchLogs();
  }

  api.vpnTypes({ signal }).then(list => {
    types = Object.fromEntries((Array.isArray(list) ? list : []).filter(t => t && t.key).map(t => [t.key, t.label]));
    render();
  }, () => {});
  render();
  const poller = poll(load, EVERY, { signal });
  poll(loadDiag, DIAG_EVERY, { signal });
  return {
    update() {},
    unmount() { if (sheet) sheet.close('close'); },
  };
}
