// hash 路由的纯函数：解析、匹配、拼地址、判断切换时要整页重挂还是只更新。
// 顶层不碰 document / window，node 可直接导入。

// '#/channels/abc?tab=rules' → {path: '/channels/abc', params: {}, query: {tab: 'rules'}}
// 空地址当作 /home；末尾的斜杠去掉。params 由 match 填。
export function parse(hash) {
  let s = String(hash ?? '');
  if (s.startsWith('#')) s = s.slice(1);
  const qi = s.indexOf('?');
  let path = qi >= 0 ? s.slice(0, qi) : s;
  const query = parseQuery(qi >= 0 ? s.slice(qi + 1) : '');
  if (!path.startsWith('/')) path = '/' + path;
  path = path.replace(/\/+/g, '/');
  if (path.length > 1) path = path.replace(/\/$/, '');
  if (path === '/') path = '/home';
  return { path, params: {}, query };
}

export function parseQuery(s) {
  const query = {};
  const text = String(s ?? '').replace(/^\?/, '');
  if (!text) return query;
  for (const [k, v] of new URLSearchParams(text)) query[k] = v;
  return query;
}

const decode = s => { try { return decodeURIComponent(s); } catch { return null; } };

// table: [{pattern: '/channels/:id', ...}]。返回 {entry, params}，匹配不上返回 null
export function match(table, path) {
  const segs = String(path).split('/').filter(Boolean);
  for (const entry of table) {
    const pat = entry.pattern.split('/').filter(Boolean);
    if (pat.length !== segs.length) continue;
    const params = {};
    let ok = true;
    for (let i = 0; i < pat.length; i++) {
      if (pat[i].startsWith(':')) {
        const v = decode(segs[i]);
        if (v == null || v === '') { ok = false; break; }
        params[pat[i].slice(1)] = v;
      } else if (pat[i] !== segs[i]) { ok = false; break; }
    }
    if (ok) return { entry, params };
  }
  return null;
}

// 拼地址。query 里值为 null / undefined / '' 的项去掉
export function build(path, query = {}) {
  const q = new URLSearchParams();
  for (const [k, v] of Object.entries(query)) if (v != null && v !== '') q.set(k, String(v));
  const qs = q.toString();
  return '#' + path + (qs ? '?' + qs : '');
}

// 在当前地址上改 query：patch 里值为 null 的项删除
export function withQuery(hash, patch) {
  const r = parse(hash);
  return build(r.path, { ...r.query, ...patch });
}

// 外壳和页面内的跳转目标：'#/…' 直接用；'?sheet=add' 这种只带 query 的并到当前地址上
export function resolveOpen(current, target) {
  const t = String(target ?? '').trim();
  if (t.startsWith('?')) return withQuery(current, parseQuery(t));
  if (t.startsWith('#')) return build(parse(t).path, parse(t).query);
  if (t.startsWith('/')) return build(parse(t).path, parse(t).query);
  return withQuery(current, {});
}

// 把 query 拆成页面自己的和弹层的：sheet 以及 sheetKeys 里的项（如添加规则的 channel）归弹层
export function splitQuery(query, sheetKeys = []) {
  const page = {}, sheet = {};
  for (const [k, v] of Object.entries(query || {})) {
    if (k === 'sheet' || sheetKeys.includes(k)) sheet[k] = v; else page[k] = v;
  }
  return { page, sheet };
}

const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);
const sorted = o => Object.fromEntries(Object.entries(o || {}).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)));

// 从上一个状态切到下一个状态时，页面该怎么办。
// prev/next: {module, params, query}，query 是页面自己的部分（splitQuery 的 page）；返回 'mount' | 'update' | 'keep'
export function plan(prev, next) {
  if (!prev || prev.module !== next.module || !same(sorted(prev.params), sorted(next.params))) return 'mount';
  return same(sorted(prev.query), sorted(next.query)) ? 'keep' : 'update';
}

// 弹层：prev/next 为弹层名（没有时为空）。返回 'open' | 'close' | 'update' | 'keep'
export function planSheet(prevName, nextName, prevQuery, nextQuery) {
  if (!nextName) return prevName ? 'close' : 'keep';
  if (prevName !== nextName) return 'open';
  return same(sorted(prevQuery), sorted(nextQuery)) ? 'keep' : 'update';
}
