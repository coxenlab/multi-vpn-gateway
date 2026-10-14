import test from 'node:test';
import assert from 'node:assert/strict';
import { createVncLifecycle } from '../../app/ui/js/login/vnc-lifecycle.js';

const flush = async () => { await new Promise(resolve => setImmediate(resolve)); };
function fixture(overrides = {}) {
  const doc = new EventTarget(); doc.hidden = false;
  const win = new EventTarget();
  const calls = { login: 0, renew: 0, release: [], open: 0, close: 0 };
  let time = 0, id = 0;
  const api = {
    login: async (_cid, { viewer }) => { calls.login++; return { viewer_id: viewer, viewer_ttl_seconds: 60 }; },
    renewLoginViewer: async () => { calls.renew++; },
    releaseLoginViewer: async (_cid, viewer) => { calls.release.push(viewer); },
    ...overrides,
  };
  const view = createVncLifecycle({
    channelId: () => 'test', isActive: () => true, doc, win, api,
    now: () => time, newId: () => `viewer-${++id}`,
    open: async ({ login, current }) => { await login(); if (current()) calls.open++; },
    close: () => { calls.close++; },
  });
  return { view, doc, win, calls, advance: ms => { time += ms; },
    hide(value) { doc.hidden = value; doc.dispatchEvent(new Event('visibilitychange')); } };
}

test('隐藏保留登录视图，租约内回来直接续期；离页立即释放', async t => {
  const f = fixture(); t.after(() => f.view.end());
  f.view.open(); await flush();
  const closes = f.calls.close;
  f.hide(true); f.advance(10000);
  assert.equal(f.calls.close, closes);
  assert.deepEqual(f.calls.release, []);
  f.hide(false); await flush();
  assert.equal(f.calls.login, 1);
  assert.equal(f.calls.open, 1);
  assert.equal(f.calls.renew, 1);
  f.win.dispatchEvent(new Event('pagehide'));
  assert.deepEqual(f.calls.release, ['viewer-1']);
  f.win.dispatchEvent(new Event('pageshow')); await flush();
  assert.equal(f.calls.login, 2);
});

test('隐藏超过租约或续期返回 404 时重新获取自己的视图', async t => {
  const f = fixture(); t.after(() => f.view.end());
  f.view.open(); await flush(); f.hide(true); f.advance(60001); f.hide(false); await flush();
  assert.equal(f.calls.login, 2);
  assert.deepEqual(f.calls.release, ['viewer-1']);
  const expired = fixture({ renewLoginViewer: async () => { throw { status: 404 }; } });
  t.after(() => expired.view.end());
  expired.view.open(); await flush(); expired.hide(true); expired.hide(false); await flush();
  assert.equal(expired.calls.login, 2);
});

test('隐藏期间登录请求完成可以保留；卸载后迟到的租约只能释放', async t => {
  let resolve;
  const f = fixture({ login: (_cid, { viewer }) => new Promise(r => { resolve = () => r({ viewer_id: viewer }); }) });
  t.after(() => f.view.end());
  f.view.open(); f.hide(true); resolve(); await flush();
  assert.equal(f.calls.open, 1);
  assert.deepEqual(f.calls.release, []);
  f.hide(false); await flush(); assert.equal(f.calls.renew, 1);
  f.view.open(); f.view.end(); resolve(); await flush();
  assert.equal(f.calls.open, 1);
  assert.deepEqual(f.calls.release, ['viewer-1', 'viewer-2']);
});
