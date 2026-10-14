// 添加规则弹层（界面 19）：粘贴地址、选通道。域名还是网段由后台识别，这里只做预览计数；
// 提交后按返回的 added / rejected 说清加了几条、哪些没加上（R-11）。
// splitTokens、classifyToken、preview、summarize 是纯函数，顶层不碰 document / window。
import { h, btn, bul, verdict } from '../ui.js';
import { openSheet } from '../sheet.js';
import { api, isAbort } from '../api.js';
import { showDone, humanize } from '../feedback.js';
import { nameOf } from './rules.js';

// 每行一个；空白、逗号、分号也当分隔（和旧界面一样）
export const splitTokens = raw => String(raw ?? '').split(/[\s,;]+/).map(t => t.trim()).filter(Boolean);

const DANGER = /[,"'\\\s\u0000-\u001f]/;

function ipv4(a) {
  const p = a.split('.');
  return p.length === 4 && p.every(x => /^(0|[1-9]\d{0,2})$/.test(x) && Number(x) <= 255);
}
function ipv6(a) {
  if (!a.includes(':') || !/^[0-9a-fA-F:.]+$/.test(a)) return false;
  try { new URL(`http://[${a}]/`); return true; } catch { return false; }
}

// 预览用的识别，对照核心 webutil::classify：返回 {kind: 'domain'|'ip', pattern} 或 null。以后台的结果为准
export function classifyToken(tok) {
  const t = String(tok ?? '');
  if (!t) return null;
  const [addr, mask, extra] = t.split('/');
  const base = addr.split('%')[0];
  if (ipv4(addr) || ipv6(base)) {
    if (addr.includes('%') || extra !== undefined || DANGER.test(t)) return null;
    const v6 = addr.includes(':');
    if (mask === undefined) return { kind: 'ip', pattern: `${addr}/${v6 ? 128 : 32}` };
    if (!/^\d{1,3}$/.test(mask) || Number(mask) > (v6 ? 128 : 32)) return null;
    return { kind: 'ip', pattern: t };
  }
  if (DANGER.test(t)) return null;
  const host = t.split('://').pop().split('/')[0].split('@').pop().split(':')[0]
    .replace(/^(\+\.|\*\.)/, '').replace(/^\.+|\.+$/g, '').toLowerCase();
  if (!host || Array.from(host).length > 253 || host.includes('..')) return null;
  return { kind: 'domain', pattern: host };
}

// 预览：{tokens, domains, ips, invalid}。同一条（识别后相同）只算一次
export function preview(raw) {
  const tokens = splitTokens(raw);
  const seen = new Set();
  let domains = 0, ips = 0, invalid = 0;
  for (const t of tokens) {
    const c = classifyToken(t);
    if (!c) { invalid++; continue; }
    const k = c.kind + ' ' + c.pattern;
    if (seen.has(k)) continue;
    seen.add(k);
    if (c.kind === 'domain') domains++; else ips++;
  }
  return { tokens, domains, ips, invalid };
}

export function previewText(p) {
  if (!p.tokens.length) return '';
  const parts = [];
  if (p.domains) parts.push(`${p.domains} 个域名`);
  if (p.ips) parts.push(`${p.ips} 个网段`);
  const head = parts.length ? `识别到 ${parts.join('、')}` : '没有认出域名或网段';
  return p.invalid ? `${head}，${p.invalid} 条认不出` : head;
}

// 提交结果：r 是 POST /rules 的返回（added: {domain, ip}，rejected: [原文]），sent 是发出去的条数。
// 返回 {added, rejected, existed, text}
export function summarize(r, sent) {
  const a = (r && r.added) || {};
  const added = (Number(a.domain) || 0) + (Number(a.ip) || 0);
  const rejected = Array.isArray(r && r.rejected) ? r.rejected.map(String) : [];
  const existed = Math.max(0, (Number(sent) || 0) - added - rejected.length);
  const parts = [];
  if (added) parts.push(`已添加 ${added} 条`);
  if (existed) parts.push(`${existed} 条已经有了`);
  if (rejected.length) parts.push(`${rejected.length} 条没加上`);
  return { added, rejected, existed, text: parts.join('，') || '没有添加规则' };
}

export function mount(root, ctx) {
  const { signal, data } = ctx;
  const core = data.get('core').value;
  let channels = (core && core.channels) || [];
  let chosen = ctx.query.channel || (channels.length === 1 ? channels[0].id : null);
  let busy = false;
  let result = null;   // 上一次提交里没加上的，显示在输入框上方

  const area = h('textarea', {
    class: 'field rl-area', rows: 4, placeholder: 'wiki.corp.example\n10.20.18.0/24', 'aria-label': '地址，每行一个', spellcheck: 'false', autofocus: true,
  });
  const picks = h('div', { class: 'rl-picks', role: 'radiogroup', 'aria-label': '走哪条通道' });
  const msg = h('div', { class: 'rl-msg' });
  const count = h('span', { class: 'dim rl-count' });
  const cancel = btn('取消', { onClick: () => sheet.close() });
  const ok = btn('添加', { cls: 'pri', onClick: submit });

  const sheet = openSheet({
    title: '添加规则', width: 'w520', root,
    body: [msg, h('div', { class: 'ft rl-ft0' }, '地址，每行一个'), area, h('div', { class: 'ft' }, '走哪条通道'), picks],
    foot: [count, h('span', { class: 'sp' }), cancel, ok],
    onConfirm: submit,
    onClose: () => ctx.close(),
  });
  area.addEventListener('input', sync);

  function drawPicks() {
    picks.replaceChildren(...(channels.length ? channels.map(ch => {
      const on = ch.id === chosen;
      return h('button', {
        type: 'button', class: 'btn rl-pick' + (on ? ' soft on' : ''), role: 'radio', 'aria-checked': on ? 'true' : 'false',
        title: nameOf(ch), onClick: () => { chosen = ch.id; drawPicks(); sync(); },
      }, bul(nameOf(ch)), h('span', { class: 'cell' }, nameOf(ch)));
    }) : [h('span', { class: 'dim' }, '还没有通道，先添加一条通道。')]));
  }

  function sync() {
    const p = preview(area.value);
    count.textContent = previewText(p);
    const n = p.domains + p.ips;
    ok.textContent = busy ? '正在添加' : n ? `添加 ${n} 条` : '添加';
    ok.disabled = busy || !n || !channels.some(c => c.id === chosen);
    cancel.disabled = busy;
    area.disabled = busy;
    msg.replaceChildren(...(result ? [result] : []));
  }

  async function submit() {
    const p = preview(area.value);
    if (busy || !(p.domains + p.ips) || !chosen) return;
    const sent = new Set(p.tokens).size;
    busy = true;
    result = null;
    sync();
    let r;
    try {
      r = await api.addRules(chosen, p.tokens, undefined, { signal });
    } catch (e) {
      if (isAbort(e)) return;
      busy = false;
      const m = humanize(e);
      const saved = e.body?.saved === true;
      result = verdict(saved ? 'wait' : 'bad', { title: saved ? '已保存，还没同步到入口' : '规则没有加上', text: saved ? '无需重复添加，可在顶栏重试同步。' : m ? m.title : '' });
      if (saved) data.refresh('core');
      sync();
      return;
    }
    busy = false;
    const s = summarize(r, sent);
    data.refresh('core');
    if (!s.rejected.length) {
      showDone(r, s.text);
      sheet.close();
      return;
    }
    // 有没加上的：留在弹层里，输入框只留下它们，方便改了再加
    area.value = s.rejected.join('\n');
    result = verdict(s.added || s.existed ? 'wait' : 'bad', {
      title: s.text, text: `认不出是域名还是网段：${s.rejected.join('、')}`,
    });
    sync();
    area.focus();
  }

  drawPicks();
  sync();
  // 直接打开弹层时通道列表可能还没取到；之后通道有增减也跟着变
  let names = JSON.stringify(channels.map(c => [c.id, c.name]));
  data.subscribe('core', snap => {
    const next = (snap.value && snap.value.channels) || null;
    if (!next) return;
    const sig = JSON.stringify(next.map(c => [c.id, c.name]));
    channels = next;
    if (!chosen && channels.length === 1) chosen = channels[0].id;
    if (sig !== names) { names = sig; drawPicks(); }
    sync();
  }, { signal });

  return {
    update(query) {
      if (query && query.channel && query.channel !== chosen) { chosen = query.channel; drawPicks(); sync(); }
    },
    unmount() { sheet.close(); },
  };
}
