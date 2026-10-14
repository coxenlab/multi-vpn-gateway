// C-07 的界面一侧：/cause 的返回（形状按隔离核心的真实输出）→ 出错页和总览用的原因与动作。
import test from 'node:test';
import assert from 'node:assert/strict';
import { toCause, failurePinned } from '../../app/ui/js/screens/channel-error.js';
import { UNAVAILABLE } from '../../app/ui/js/api.js';
import { causeText, actionText, FORBIDDEN } from '../../app/ui/js/terms.js';

test('启动失败提示只在通道仍出错时留住页面，恢复或待确认时解除', () => {
  const failure = { title: '启动失败' };
  assert.equal(failurePinned(failure, { status: 'error' }), true);
  for (const status of ['logged_in', 'running', 'stopped']) assert.equal(failurePinned(failure, { status }), false);
  assert.equal(failurePinned(failure, { status: 'error', container_state: 'unknown' }), false);
  assert.equal(failurePinned(failure, { status: 'error' }, { probe_failed: true }), false);
  assert.equal(causeText('runtime_unknown'), '状态待确认');
});

test('出错原因：四类有判据的原因各有一句话和动作；不认识、已停止、接口不存在时按原因不清楚处理', () => {
  const known = {
    runtime_down: 'restart_runtime', image_pull_failed: 'retry', crash_loop: 'retry', kicked: 'relogin',
  };
  const unknownText = causeText('unknown');
  for (const [code, action] of Object.entries(known)) {
    const c = toCause({ code, action, at: null });
    assert.deepEqual(c, { code, action });
    assert.notEqual(causeText(c.code), unknownText, code);
    assert.ok(actionText(c.action), action);
    assert.ok(!FORBIDDEN.test(causeText(c.code) + actionText(c.action)), code);
  }
  for (const r of [{ code: 'unknown', action: 'view_logs', at: null }, { code: null, action: null, at: null }, UNAVAILABLE, null]) {
    assert.deepEqual(toCause(r), { code: 'unknown', action: 'view_logs' });
  }
  assert.equal(unknownText, '连不上，原因还不清楚');
});
