// 路由表。界面模块按路由用动态 import() 加载：某个界面模块写坏了，只有它自己的路由打不开。
// 每个界面模块导出 mount(root, ctx)，返回 {update(query), unmount()}。
import { match } from './router.js';

// 模块名 → 加载函数。每一项都写成字面量，方便检查文件是否存在
export const LOADERS = Object.freeze({
  home: () => import('./screens/home.js'),
  channels: () => import('./screens/channels.js'),
  rules: () => import('./screens/rules.js'),
  'rules-flow': () => import('./screens/rules-flow.js'),
  'rules-test': () => import('./screens/rules-test.js'),
  conns: () => import('./screens/conns.js'),
  settings: () => import('./screens/settings.js'),
  'settings-advanced': () => import('./screens/settings-advanced.js'),
  'advanced-mirrors': () => import('./screens/advanced-mirrors.js'),
  'advanced-images': () => import('./screens/advanced-images.js'),
  'advanced-channels': () => import('./screens/advanced-channels.js'),
  'advanced-logs': () => import('./screens/advanced-logs.js'),
  add: () => import('./screens/add.js'),
  'rule-add': () => import('./screens/rule-add.js'),
  // 下面这些由界面模块自己分派：通道详情按分页和状态分到 03–08，添加通道的第 15 步
  'channel-overview': () => import('./screens/channel-overview.js'),
  'channel-rules': () => import('./screens/channel-rules.js'),
  'channel-logs': () => import('./screens/channel-logs.js'),
  'channel-login': () => import('./screens/channel-login.js'),
  'channel-unverified': () => import('./screens/channel-unverified.js'),
  'channel-error': () => import('./screens/channel-error.js'),
  'add-progress': () => import('./screens/add-progress.js'),
});

// nav 是侧栏当前项；module 可以是函数，按 query 选模块（规则的列表 / 流向图）
export const ROUTES = Object.freeze([
  { pattern: '/home', nav: 'home', module: 'home' },
  { pattern: '/channels', nav: 'channels', module: 'channels' },
  { pattern: '/channels/:id', nav: 'channels', module: 'channels' },
  { pattern: '/rules', nav: 'rules', module: q => (q.view === 'flow' ? 'rules-flow' : 'rules') },
  { pattern: '/rules/test', nav: 'rules', module: 'rules-test' },
  { pattern: '/conns', nav: 'conns', module: 'conns' },
  { pattern: '/settings', nav: 'settings', module: 'settings' },
  { pattern: '/settings/advanced', nav: 'settings', module: 'settings-advanced' },
  { pattern: '/settings/advanced/mirrors', nav: 'settings', module: 'advanced-mirrors' },
  { pattern: '/settings/advanced/images', nav: 'settings', module: 'advanced-images' },
  { pattern: '/settings/advanced/channels', nav: 'settings', module: 'advanced-channels' },
  { pattern: '/settings/advanced/logs', nav: 'settings', module: 'advanced-logs' },
]);

// ?sheet=<名> → 弹层模块和归它的 query 项。弹层模块同样导出 mount(root, ctx)，root 是 #sheet-root
export const SHEETS = Object.freeze({
  add: Object.freeze({ module: 'add', keys: [] }),
  'rule-add': Object.freeze({ module: 'rule-add', keys: ['channel'] }),
});
// 所有弹层自己的 query 项，拆 query 时用
export const SHEET_KEYS = Object.freeze([...new Set(Object.values(SHEETS).flatMap(s => s.keys))]);

export const HOME = '#/home';

// 路径加 query → {entry, params, module, nav}；没有这条路由返回 null
export function resolve(path, query = {}) {
  const m = match(ROUTES, path);
  if (!m) return null;
  const module = typeof m.entry.module === 'function' ? m.entry.module(query) : m.entry.module;
  return { entry: m.entry, params: m.params, module, nav: m.entry.nav };
}

export const sheetModule = name => (name && Object.hasOwn(SHEETS, name) ? SHEETS[name].module : null);

export function load(module) {
  const loader = LOADERS[module];
  return loader ? loader() : Promise.reject(new Error('unknown module ' + module));
}
