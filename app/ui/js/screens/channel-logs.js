// 通道 · 日志（界面 05）：最近 200 行，给排查问题用。重复的行由后台合并。「导出」存成文本文件。
import { h, fill, card, lb } from '../ui.js';
import { api, isAbort } from '../api.js';
import { poll } from '../poll.js';
import { day } from '../format.js';
import { showError } from '../feedback.js';

const TAIL = 200;
const EVERY = 8000;

const TIME = /^(\[?\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:[.,]\d+)?(?:Z|[+-]\d{2}:?\d{2})?\]?|\[?\d{2}:\d{2}:\d{2}(?:[.,]\d+)?\]?)\s*/;
const BAD = /\b(error|fatal|fail(?:ed|ure)?|panic)\b|失败|错误|出错|异常|拒绝/i;
const WARN = /\b(warn(?:ing)?|retry(?:ing)?|timeout|timed out)\b|警告|重试|超时|断开/i;

// 一行日志 → {time, text, tone}。tone 为 e（出错）/ w（提醒）/ ''
export function splitLine(line) {
  const s = String(line ?? '');
  const m = s.match(TIME);
  const time = m ? m[1] : '';
  const text = m ? s.slice(m[0].length) : s;
  const tone = BAD.test(text) ? 'e' : WARN.test(text) ? 'w' : '';
  return { time, text, tone };
}

// 后台拿不到日志时给的占位行（以 < 开头的英文说明）不当日志显示
export const realLines = lines => (Array.isArray(lines) ? lines : []).filter(l => !/^<[^>]*>$/.test(String(l).trim()));

// 导出的文件名：通道名里不能进文件名的字符换成 -
export const exportName = (name, ts = Date.now()) =>
  `${String(name || '通道').replace(/[\\/:*?"<>|\u0000-\u001f]+/g, '-').trim() || '通道'}-日志-${day(ts)}.txt`;

export function mount(root, ctx) {
  const { signal } = ctx;
  const id = ctx.params.id;
  let ch = ctx.channel;
  let lines = null;

  const pre = h('pre', { class: 'logs ch-logs', tabindex: '0', 'aria-label': '最近的日志' });
  const exp = h('button', { type: 'button', class: 'more', disabled: true, onClick: exportLogs }, '导出');
  fill(root, card([lb('最近的日志', exp), pre], { cls: 'ch-logcard' }));
  fill(pre, h('span', { class: 't' }, '正在读取'));
  let first = true;
  let failed = false;

  function render() {
    const atBottom = pre.scrollHeight - pre.scrollTop - pre.clientHeight < 8;
    if (!lines.length) {
      fill(pre, h('span', { class: 't' }, failed ? '没能读取日志' : '还没有日志'));
      exp.disabled = true;
      return;
    }
    exp.disabled = false;
    const kids = [];
    lines.forEach((l, i) => {
      const p = splitLine(l);
      if (i) kids.push('\n');
      if (p.time) kids.push(h('span', { class: 't' }, p.time), '  ');
      kids.push(p.tone ? h('span', { class: p.tone }, p.text) : p.text);
    });
    fill(pre, kids);
    if (first || atBottom) pre.scrollTop = pre.scrollHeight;
    first = false;
  }

  function exportLogs() {
    if (!lines || !lines.length) return;
    const url = URL.createObjectURL(new Blob([lines.join('\n') + '\n'], { type: 'text/plain;charset=utf-8' }));
    const a = h('a', { href: url, download: exportName(ch && ch.name) });
    document.body.append(a);
    a.click();
    a.remove();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
  }

  let shown = false;
  poll(async s => {
    try {
      const r = await api.logs(id, TAIL, { signal: s });
      lines = realLines(r && r.lines);
      failed = false;
      render();
    } catch (e) {
      if (isAbort(e)) return;
      if (!shown) { shown = true; showError(e); }
      if (lines === null) { lines = []; failed = true; render(); }
    }
  }, EVERY, { signal });

  return {
    update() {},
    channel(next) { ch = next; },
    unmount() {},
  };
}
