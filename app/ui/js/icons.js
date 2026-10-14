// 图标：16×16 线稿，取自原型第四版。svg() 是纯函数，只拼常量字符串；ic() 生成元素。
const P = {
  channels: '<circle cx="3.5" cy="8" r="1.8"/><circle cx="12.5" cy="3.8" r="1.8"/><circle cx="12.5" cy="12.2" r="1.8"/><path d="M5.2 7.2l5.6-2.6M5.2 8.8l5.6 2.6"/>',
  rules: '<path d="M3 2.5v4a3 3 0 003 3h4.5M3 6.5v7"/><path d="M9 7l2.5 2.5L9 12"/><circle cx="3" cy="2.5" r=".4"/>',
  conns: '<path d="M4.5 13V3.2M2 5.6l2.5-2.6L7 5.6M11.5 3v9.8M9 10.4l2.5 2.6 2.5-2.6"/>',
  gear: '<circle cx="8" cy="8" r="2.1"/><path d="M8 1.6v1.7M8 12.7v1.7M1.6 8h1.7M12.7 8h1.7M3.5 3.5l1.2 1.2M11.3 11.3l1.2 1.2M12.5 3.5l-1.2 1.2M4.7 11.3l-1.2 1.2"/>',
  home: '<rect x="2" y="2" width="5" height="5" rx="1.4"/><rect x="9" y="2" width="5" height="5" rx="1.4"/><rect x="2" y="9" width="5" height="5" rx="1.4"/><rect x="9" y="9" width="5" height="5" rx="1.4"/>',
  plus: '<path d="M8 3v10M3 8h10"/>',
  search: '<circle cx="7" cy="7" r="4.2"/><path d="M10.2 10.2l3.2 3.2"/>',
  back: '<path d="M10 3L5 8l5 5"/>',
  chev: '<path d="M6 3.5L10.5 8 6 12.5"/>',
  down: '<path d="M4 6.2l4 4 4-4"/>',
  updown: '<path d="M5 6.2L8 3.4l3 2.8M5 9.8l3 2.8 3-2.8"/>',
  play: '<path d="M5 3.2v9.6L12.6 8z" fill="currentColor" stroke-linejoin="round"/>',
  stop: '<rect x="4" y="4" width="8" height="8" rx="1.6" fill="currentColor"/>',
  pause: '<path d="M5.5 3.5v9M10.5 3.5v9" stroke-width="2"/>',
  refresh: '<path d="M13 8a5 5 0 11-1.6-3.7"/><path d="M13.2 2.6v2.9h-2.9"/>',
  bolt: '<path d="M8.8 1.8L3.6 9h3.8l-.6 5.2L12.4 7H8.4z" stroke-linejoin="round"/>',
  block: '<circle cx="8" cy="8" r="5.8"/><path d="M4 12L12 4"/>',
  warn: '<path d="M8 2.2l6.2 11H1.8z" stroke-linejoin="round"/><path d="M8 6.6v3M8 11.6h.01"/>',
  info: '<circle cx="8" cy="8" r="6"/><path d="M8 7.4v3.6M8 5h.01"/>',
  check: '<path d="M3.4 8.4l3 3 6.2-6.6"/>',
  x: '<path d="M4 4l8 8M12 4l-8 8"/>',
  okc: '<circle cx="8" cy="8" r="6.2" fill="currentColor" stroke="none"/><path class="ic-knock" d="M5 8.2l2.1 2.1L11 6.1"/>',
  badc: '<circle cx="8" cy="8" r="6.2" fill="currentColor" stroke="none"/><path class="ic-knock" d="M5.7 5.7l4.6 4.6M10.3 5.7l-4.6 4.6"/>',
  list: '<path d="M5.5 4h8M5.5 8h8M5.5 12h8M2.5 4h.01M2.5 8h.01M2.5 12h.01"/>',
  map: '<circle cx="3.5" cy="4" r="1.6"/><circle cx="12.5" cy="12" r="1.6"/><path d="M5.1 4H9a2 2 0 012 2v4.4"/>',
  target: '<circle cx="8" cy="8" r="5.6"/><circle cx="8" cy="8" r="2"/>',
  doc: '<path d="M4 1.8h5l3 3V13a1.2 1.2 0 01-1.2 1.2H4A1.2 1.2 0 012.8 13V3A1.2 1.2 0 014 1.8z"/><path d="M9 1.8v3h3"/>',
  tray: '<path d="M8 2v7.4M5.2 6.8L8 9.6l2.8-2.8"/><path d="M2.5 10.5v2a1.2 1.2 0 001.2 1.2h8.6a1.2 1.2 0 001.2-1.2v-2"/>',
  export: '<path d="M8 9.6V2.2M5.2 4.8L8 2l2.8 2.8"/><path d="M2.5 9v3.5a1.2 1.2 0 001.2 1.2h8.6a1.2 1.2 0 001.2-1.2V9"/>',
  copy: '<rect x="5.2" y="5.2" width="8.3" height="8.3" rx="1.8"/><path d="M10.8 5.2V4a1.5 1.5 0 00-1.5-1.5H4A1.5 1.5 0 002.5 4v5.3A1.5 1.5 0 004 10.8h1.2"/>',
  trash: '<path d="M2.8 4.2h10.4M6 4.2V2.8h4v1.4M4.2 4.2l.6 9a1 1 0 001 1h4.4a1 1 0 001-1l.6-9"/>',
  kbd: '<rect x="1.5" y="4" width="13" height="8.4" rx="1.8"/><path d="M4.2 6.8h.01M6.7 6.8h.01M9.2 6.8h.01M11.7 6.8h.01M5 9.8h6"/>',
  clock: '<circle cx="8" cy="8" r="6"/><path d="M8 4.6V8l2.3 1.5"/>',
  globe: '<circle cx="8" cy="8" r="6"/><path d="M2 8h12M8 2c2 2 2 10 0 12M8 2c-2 2-2 10 0 12"/>',
  power: '<path d="M8 1.8v5.4"/><path d="M4.4 3.9a5.4 5.4 0 107.2 0"/>',
};

export const ICONS = Object.freeze(Object.keys(P));

export function svg(name, size = 16, sw = 1.35) {
  return '<svg width="' + Number(size) + '" height="' + Number(size) + '" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="' + Number(sw) +
    '" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true" focusable="false">' + (P[name] || '') + '</svg>';
}

export function ic(name, size, sw) {
  const t = document.createElement('template');
  t.innerHTML = svg(name, size, sw);
  return t.content.firstChild;
}
