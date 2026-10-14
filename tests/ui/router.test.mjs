import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile, access } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { parse, parseQuery, match, build, withQuery, resolveOpen, splitQuery, plan, planSheet } from '../../app/ui/js/router.js';
import { ROUTES, LOADERS, SHEETS, SHEET_KEYS, resolve, sheetModule, load } from '../../app/ui/js/routes.js';

const JS = new URL('../../app/ui/js/', import.meta.url);

test('parse：空地址当作总览，去掉多余斜杠，拆出 query', () => {
  assert.deepEqual(parse(''), { path: '/home', params: {}, query: {} });
  assert.deepEqual(parse('#/'), { path: '/home', params: {}, query: {} });
  assert.deepEqual(parse('#/channels/demo-1/?tab=rules'), { path: '/channels/demo-1', params: {}, query: { tab: 'rules' } });
  assert.deepEqual(parse('#//rules//test?q=a.example&sheet=rule-add'), { path: '/rules/test', params: {}, query: { q: 'a.example', sheet: 'rule-add' } });
  assert.deepEqual(parse('channels'), { path: '/channels', params: {}, query: {} });
  assert.deepEqual(parseQuery('?q=%E5%86%85%E7%BD%91&x='), { q: '内网', x: '' });
});

test('match：按段匹配，参数解码；段数不同或解码失败不匹配', () => {
  const table = [{ pattern: '/channels' }, { pattern: '/channels/:id' }];
  assert.equal(match(table, '/channels').entry, table[0]);
  assert.deepEqual(match(table, '/channels/a%20b').params, { id: 'a b' });
  assert.equal(match(table, '/channels/a/b'), null);
  assert.equal(match(table, '/channels/%E0%A4%A'), null);
  assert.equal(match(table, '/rules'), null);
});

test('build 与 withQuery：空值去掉，null 删除已有项', () => {
  assert.equal(build('/rules', { view: 'flow', q: '', x: null }), '#/rules?view=flow');
  assert.equal(withQuery('#/rules?view=flow&sheet=rule-add&channel=c1', { sheet: null, channel: null }), '#/rules?view=flow');
  assert.equal(withQuery('#/home', { sheet: 'add' }), '#/home?sheet=add');
});

test('resolveOpen：外壳和页面内的跳转目标', () => {
  assert.equal(resolveOpen('#/channels/c1?tab=rules', '?sheet=add'), '#/channels/c1?tab=rules&sheet=add');
  assert.equal(resolveOpen('#/home', '#/channels/c1?tab=login'), '#/channels/c1?tab=login');
  assert.equal(resolveOpen('#/home', '/settings'), '#/settings');
  assert.equal(resolveOpen('', '#/settings'), '#/settings');
  assert.equal(resolveOpen('#/conns', ''), '#/conns');
});

test('splitQuery：sheet 和弹层自己的参数不算页面的 query', () => {
  assert.deepEqual(splitQuery({ view: 'flow', sheet: 'rule-add', channel: 'c1' }, SHEET_KEYS),
    { page: { view: 'flow' }, sheet: { sheet: 'rule-add', channel: 'c1' } });
  assert.deepEqual(splitQuery({ tab: 'rules' }, SHEET_KEYS), { page: { tab: 'rules' }, sheet: {} });
});

test('plan：换模块或参数整页重挂，只改 query 时更新，完全一样不动', () => {
  const a = { module: 'channels', params: { id: 'c1' }, query: { tab: 'rules' } };
  assert.equal(plan(null, a), 'mount');
  assert.equal(plan(a, { ...a, params: { id: 'c2' } }), 'mount');
  assert.equal(plan(a, { ...a, module: 'rules' }), 'mount');
  assert.equal(plan(a, { ...a, query: { tab: 'logs' } }), 'update');
  assert.equal(plan(a, { ...a, query: { tab: 'rules' } }), 'keep');
  assert.equal(plan({ ...a, query: { a: '1', b: '2' } }, { ...a, query: { b: '2', a: '1' } }), 'keep');
});

test('planSheet：打开、关闭、更新、不动', () => {
  assert.equal(planSheet('', '', {}, {}), 'keep');
  assert.equal(planSheet('', 'add', {}, { sheet: 'add' }), 'open');
  assert.equal(planSheet('add', '', { sheet: 'add' }, {}), 'close');
  assert.equal(planSheet('add', 'rule-add', {}, {}), 'open');
  assert.equal(planSheet('rule-add', 'rule-add', { channel: 'a' }, { channel: 'b' }), 'update');
  assert.equal(planSheet('rule-add', 'rule-add', { channel: 'a' }, { channel: 'a' }), 'keep');
});

test('路由表：DESIGN 4.1 的每个地址都能解析到模块和侧栏项', () => {
  const cases = [
    ['/home', {}, 'home', 'home'],
    ['/channels', {}, 'channels', 'channels'],
    ['/channels/c1', { tab: 'login' }, 'channels', 'channels'],
    ['/rules', {}, 'rules', 'rules'],
    ['/rules', { view: 'list' }, 'rules', 'rules'],
    ['/rules', { view: 'flow' }, 'rules-flow', 'rules'],
    ['/rules/test', { q: 'a.example' }, 'rules-test', 'rules'],
    ['/conns', {}, 'conns', 'conns'],
    ['/settings', {}, 'settings', 'settings'],
    ['/settings/advanced', {}, 'settings-advanced', 'settings'],
    ['/settings/advanced/mirrors', {}, 'advanced-mirrors', 'settings'],
    ['/settings/advanced/images', {}, 'advanced-images', 'settings'],
    ['/settings/advanced/channels', {}, 'advanced-channels', 'settings'],
    ['/settings/advanced/logs', {}, 'advanced-logs', 'settings'],
  ];
  for (const [path, query, module, nav] of cases) {
    const r = resolve(path, query);
    assert.ok(r, path);
    assert.equal(r.module, module, path);
    assert.equal(r.nav, nav, path);
  }
  assert.deepEqual(resolve('/channels/c1').params, { id: 'c1' });
  assert.equal(resolve('/nope'), null);
  assert.equal(sheetModule('add'), 'add');
  assert.equal(sheetModule('rule-add'), 'rule-add');
  assert.equal(sheetModule('toString'), null);
  assert.equal(sheetModule(undefined), null);
  assert.deepEqual(SHEETS['rule-add'].keys, ['channel']);
});

test('界面模块按路由动态加载：routes.js 不静态导入界面模块', async () => {
  const src = await readFile(new URL('routes.js', JS), 'utf8');
  assert.doesNotMatch(src, /^\s*import\s[^(]*['"]\.\/screens\//m);
  for (const [name, loader] of Object.entries(LOADERS)) {
    assert.match(loader.toString(), new RegExp(`import\\('\\./screens/${name}\\.js'\\)`), name);
  }
  const used = new Set(ROUTES.flatMap(r => (typeof r.module === 'function' ? [r.module({}), r.module({ view: 'flow' })] : [r.module])));
  for (const s of Object.values(SHEETS)) used.add(s.module);
  for (const m of used) assert.ok(Object.hasOwn(LOADERS, m), m);
});

test('每个界面模块文件都在，并导出 mount', async () => {
  for (const name of Object.keys(LOADERS)) {
    await access(fileURLToPath(new URL(`screens/${name}.js`, JS)));
    const mod = await load(name);
    assert.equal(typeof mod.mount, 'function', name);
  }
  await assert.rejects(load('nope'));
});
