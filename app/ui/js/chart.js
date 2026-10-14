// 唯一的折线图。纯函数，返回 SVG 字符串；数值只拼数字，坐标文字经转义。
// points：数字数组，null 表示这一段没有数据（断开，不补点）。没有任何一段连着两个有效点时画空状态。
import { esc } from './ui.js';

const n1 = v => (Math.round(v * 10) / 10).toString();

// labels 给三段文字（左、中、右）时在图下方画时间轴，图按比例缩放；不给时图铺满外框（总览卡片底部）。
export function area(points, { w = 300, h = 76, max, labels, empty = '还没有数据' } = {}) {
  const pts = Array.isArray(points) ? points.map(v => (typeof v === 'number' && Number.isFinite(v) ? Math.max(0, v) : null)) : [];
  const valid = pts.filter(v => v !== null);
  if (valid.length < 2) return '<div class="chart-none">' + esc(empty) + '</div>';

  const top = Math.max(max || 0, ...valid) || 1;
  const step = pts.length > 1 ? w / (pts.length - 1) : w;
  const y = v => h - (v / top) * h * 0.85;

  // 按 null 切成若干段，每段一条线加一块浅色填充
  const runs = [];
  let cur = [];
  pts.forEach((v, i) => {
    if (v === null) { if (cur.length) runs.push(cur); cur = []; return; }
    cur.push([i * step, y(v)]);
  });
  if (cur.length) runs.push(cur);
  if (!runs.some(r => r.length > 1)) return '<div class="chart-none">' + esc(empty) + '</div>';

  let fill = '', line = '';
  for (const r of runs) {
    const d = r.map((p, i) => (i ? 'L' : 'M') + n1(p[0]) + ' ' + n1(p[1])).join(' ');
    line += d + ' ';
    if (r.length > 1) fill += d + ' L' + n1(r[r.length - 1][0]) + ' ' + h + ' L' + n1(r[0][0]) + ' ' + h + 'Z ';
  }

  const axis = Array.isArray(labels) && labels.length === 3;
  const vh = axis ? h + 18 : h;
  let text = '';
  if (axis) {
    const anchor = ['start', 'middle', 'end'];
    text = labels.map((t, i) => '<text x="' + n1(i * w / 2) + '" y="' + (h + 15) + '" text-anchor="' + anchor[i] + '">' + esc(t) + '</text>').join('');
  }
  return '<svg class="area' + (axis ? ' axis' : '') + '" viewBox="0 0 ' + w + ' ' + vh + '"' + (axis ? '' : ' preserveAspectRatio="none"') + ' aria-hidden="true">' +
    (fill ? '<path class="fill" d="' + fill.trim() + '"/>' : '') +
    '<path class="line" d="' + line.trim() + '"' + (axis ? '' : ' vector-effect="non-scaling-stroke"') + '/>' + text + '</svg>';
}
