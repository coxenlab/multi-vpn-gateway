// 通道 · 还没验证（界面 06）：客户端起来了，但没有可以用来确认的内网地址（/health 的 awaiting_target）。
// 不显示「已连接」。给这条通道加一条规则（打开添加规则），或者填一个内网地址：
// 把它存成这条通道的验证地址（PATCH，不触发重建），再 GET /status 检测一次。
// 渲染整条详情列（详情头加内容）；ctx.head === false 时不画详情头，由上层画。
import { h, fill, btn, st, bul, card, lb, field, verdict, pageState } from '../ui.js';
import { api, isAbort } from '../api.js';
import { humanize, showError } from '../feedback.js';
import { typeLabel, stateText } from '../terms.js';
import { moreMenu, restoreBar } from './channel-edit.js';

// 用户填的地址 → 验证地址。没写协议时补 http://；含空白、逗号，或解析不出主机时返回 null
export function toTargetUrl(text) {
  const s = String(text ?? '').trim();
  if (!s || /[\s,，]/.test(s)) return null;
  let u;
  try { u = new URL(/^[a-z][a-z0-9+.-]*:\/\//i.test(s) ? s : 'http://' + s); } catch { return null; }
  if ((u.protocol !== 'http:' && u.protocol !== 'https:') || !u.hostname) return null;
  return u.href;
}

// 原子验证：先检测候选地址，只有成功才保存；失败或执行故障不改原值。
// 请求已发出后离开页面，服务端仍可能完成检测并保存成功结果。
// fake api 可以替换，node 直接测试。返回 saved 明确表示候选地址是否已经保存。
export async function tryTarget(client, id, url, { signal } = {}) {
  const r = await client.verifyTarget(id, url, { signal });
  if (!r || r.probe_failed === true || r.connected == null) {
    const error = new Error(String(r?.probe_reason || '检测没有完成')); // wording-allow
    error.reason = String(r?.probe_reason || '检测没有完成'); // wording-allow
    error.body = r || null;
    throw error;
  }
  const target = r.target || url;
  return { connected: r.connected === true, target, saved: r.connected === true };
}

// 能用规则里的地址来确认的通道：出口绑在隧道上的无头类型。其余类型只能填验证地址（F2）
export const ruleCanVerify = ch => !!ch && ch.login_method === 'headless';

// 检测失败不等于地址一定写错：客户端隧道尚未建立时，同一个正确的内网地址也会失败。
export const targetFailureText = () =>
  '没有通过，地址没有保存。可能是客户端隧道还没连通，也可能是这个地址无法从内网访问。';

export const findChannel = (core, id) => ((core && core.channels) || []).find(c => c && c.id === id) || null;

// 详情头：标记、名称、类型、状态，右边是这一屏的按钮（06、08 共用）
export function detailHead(ch, status, actions, { editable = true } = {}) {
  return h('div', { class: 'dhead' }, bul(ch.name, { size: 'lg' }),
    h('div', { class: 't' }, h('b', { class: 'cell', title: ch.name }, ch.name), h('span', { class: 'dim' }, typeLabel(ch.vpn_type) + '　'), status),
    h('div', { class: 'acts' }, actions, editable ? moreMenu(ch) : null));
}

// 页面没法画时的三种样子：还在加载、读取失败、通道已经不在
export function placeholder(snap, ctx) {
  if (!snap || (!snap.value && !snap.error)) return pageState('loading');
  if (!snap.value || !Array.isArray(snap.value.channels)) return pageState('error', { onRetry: () => ctx.data.refresh('core') });
  return pageState('empty', { title: '这条通道已经不在了', action: btn('回到通道列表', { onClick: () => ctx.navigate('#/channels') }) });
}

export function mount(root, ctx) {
  const id = ctx.params.id;
  const { signal } = ctx;
  let ch = null;
  let busy = null; // 'check' / 'stop'
  let built = false;
  let head = null;

  const input = field({ placeholder: '例如 oa.park.example', label: '内网地址', onEnter: check });
  const checkBtn = btn('检测', { onClick: check });
  const result = h('div', { class: 'cs-result', hidden: true, role: 'status' });
  const restoreSlot = h('div', { style: 'display:contents' });

  function build() {
    input.value = ch.probe_url || '';
    head = ctx.head === false ? null : renderHead();
    const byRule = ruleCanVerify(ch);
    fill(root, h('div', { class: 'cs' },
      head, restoreSlot,
      verdict('info', byRule ? {
        panel: true, title: '还不知道连上了没有', text: '给这条通道加一条域名，或者一个单独的地址（网段不能用来确认），我们会用它来确认。',
        action: h('div', { class: 'cs-acts' }, btn('添加规则', { cls: 'pri', onClick: () => ctx.navigate('?sheet=rule-add&channel=' + encodeURIComponent(id)) })),
      } : { panel: true, title: '还不知道连上了没有', text: '在登录窗口里登录以后，填一个连上才能打开的内网地址，我们用它来确认。' }),
      card([lb(byRule ? '或者填一个内网地址' : '内网地址'), h('div', { class: 'cs-url' }, input, checkBtn),
        h('div', { class: 'dim cs-foot' }, '要填连上以后才能打开的地址。'), result])));
    built = true;
    fill(restoreSlot, restoreBar(ch));
  }

  function renderHead() {
    const acts = [];
    if (ch.login_method !== 'headless') {
      acts.push(btn('重新打开窗口', { icon: 'refresh', disabled: !!busy, onClick: () => ctx.navigate(`#/channels/${encodeURIComponent(id)}?tab=login`) }));
    }
    acts.push(btn(busy === 'stop' ? '正在停止' : '停止', { icon: 'stop', disabled: !!busy, onClick: stop }));
    return detailHead(ch, st('wait', stateText('unverified')), acts);
  }

  function sync() {
    if (head) { const next = renderHead(); head.replaceWith(next); head = next; }
    fill(restoreSlot, restoreBar(ch));
    checkBtn.disabled = !!busy;
    checkBtn.lastChild.textContent = busy === 'check' ? '正在检测' : '检测';
  }

  function showResult(kind, text) {
    fill(result, st(kind, text));
    result.hidden = false;
  }

  async function check() {
    if (busy) return;
    const url = toTargetUrl(input.value);
    if (!url) { showResult('bad', '地址写得不对，例如 oa.park.example 或 http://oa.park.example/'); return; }
    busy = 'check';
    sync();
    result.hidden = true;
    try {
      const r = await tryTarget(api, id, url, { signal });
      if (r.connected) showResult('ok', `已经连上了，用 ${r.target} 确认过`);
      else showResult('bad', targetFailureText());
      ctx.data.refresh('core');
      ctx.data.refresh('health');
    } catch (e) {
      if (isAbort(e)) return;
      const m = humanize(e, { fallback: '这个地址没能保存' });
      if (m) showResult('bad', m.text ? `${m.title}。${m.text}` : m.title);
    } finally {
      if (!signal.aborted) { busy = null; sync(); }
    }
  }

  async function stop() {
    if (busy) return;
    busy = 'stop';
    sync();
    try {
      await api.stop(id, { signal });
      ctx.data.refresh('core');
    } catch (e) {
      if (!isAbort(e)) showError(e, { fallback: '没能停止' });
    } finally {
      if (!signal.aborted) { busy = null; sync(); }
    }
  }

  ctx.data.subscribe('core', snap => {
    const next = findChannel(snap.value, id);
    if (!next) { if (!built || (snap.value && Array.isArray(snap.value.channels))) { built = false; head = null; fill(root, placeholder(snap, ctx)); } return; }
    ch = next;
    if (!built) build(); else sync();
  }, { signal });
  if (!built) fill(root, pageState('loading'));

  return { update() {}, unmount() {} };
}
