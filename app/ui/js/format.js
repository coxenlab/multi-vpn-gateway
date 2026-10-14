// 数字和时间的显示格式。纯函数，顶层不碰 document / window。

const UNITS = ['B', 'KB', 'MB', 'GB', 'TB'];

// 1536 → {num: '1.5', unit: 'KB'}。小于 10 保留一位小数，末尾的 .0 去掉
export function bytesParts(n) {
  let v = Number(n);
  if (!Number.isFinite(v) || v < 0) return { num: '—', unit: '' };
  let i = 0;
  while (v >= 1024 && i < UNITS.length - 1) { v /= 1024; i++; }
  const num = i === 0 ? String(Math.round(v)) : (v < 10 ? v.toFixed(1).replace(/\.0$/, '') : String(Math.round(v)));
  return { num, unit: UNITS[i] };
}
export const bytes = n => { const p = bytesParts(n); return p.unit ? `${p.num} ${p.unit}` : p.num; };

// 每秒字节数 → {num, unit: 'MB/秒'}
export function rateParts(bps) {
  const p = bytesParts(bps);
  return { num: p.num, unit: p.unit ? p.unit + '/秒' : '' };
}
export const rate = bps => { const p = rateParts(bps); return p.unit ? `${p.num} ${p.unit}` : p.num; };

// 延迟毫秒；没有值时显示「—」
export const ms = v => (v == null || !Number.isFinite(Number(v)) ? '—' : `${Math.round(Number(v))} 毫秒`);

export const count = n => (Number.isFinite(Number(n)) ? Number(n).toLocaleString('zh-CN') : '—');

const pad = n => String(n).padStart(2, '0');

// 毫秒时间戳 → 本地时间 11:18
export function clock(ts) {
  const d = new Date(Number(ts));
  return Number.isNaN(d.getTime()) ? '—' : `${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

// 毫秒时间戳 → 本地日期 2026-09-29（导出文件的日期区间用）
export function day(ts = Date.now()) {
  const d = new Date(Number(ts));
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

// 多久以前：12 秒前 / 3 分钟前 / 2 小时前 / 超过一天显示日期
export function ago(ts, now = Date.now()) {
  const t = Number(ts);
  if (!Number.isFinite(t) || t <= 0) return '—';
  const s = Math.max(0, Math.round((now - t) / 1000));
  if (s < 5) return '刚刚';
  if (s < 60) return `${s} 秒前`;
  if (s < 3600) return `${Math.floor(s / 60)} 分钟前`;
  if (s < 86400) return `${Math.floor(s / 3600)} 小时前`;
  return day(t);
}

// 秒数 → 4 小时 12 分
export function duration(sec) {
  const s = Math.max(0, Math.floor(Number(sec) || 0));
  if (s < 60) return `${s} 秒`;
  if (s < 3600) return `${Math.floor(s / 60)} 分钟`;
  if (s < 86400) return `${Math.floor(s / 3600)} 小时 ${Math.floor((s % 3600) / 60)} 分`;
  return `${Math.floor(s / 86400)} 天 ${Math.floor((s % 86400) / 3600)} 小时`;
}
