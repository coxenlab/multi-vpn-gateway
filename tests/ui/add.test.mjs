// F5 第 5 条：添加通道时选的配置文件超过核心接收的上限就不读，直接提示文件太大。
import test from 'node:test';
import assert from 'node:assert/strict';
import { CONFIG_LIMIT, configProblem } from '../../app/ui/js/screens/add.js';
import { configText, installOutcome, installFailureText, installAttemptMatches,
  installationError, requireInstallAttempt } from '../../app/ui/js/screens/add-progress.js';
import { FORBIDDEN } from '../../app/ui/js/terms.js';

// 文件替身：记下有没有被读过
function fakeFile(size) {
  const f = { name: 'office.conf', size, reads: 0, async text() { f.reads++; return '[Interface]\n'; } };
  return f;
}
const draft = file => ({ spec: { inputs: [{ key: 'config_file', type: 'file' }] }, file });

test('配置文件的上限低于核心收请求体的上限（2 MB）', () => {
  assert.ok(CONFIG_LIMIT > 0 && CONFIG_LIMIT <= 1024 * 1024);
  assert.equal(configProblem(fakeFile(CONFIG_LIMIT)), null);
  const p = configProblem(fakeFile(CONFIG_LIMIT + 1));
  assert.equal(p.title, '配置文件太大了');
  assert.ok(!FORBIDDEN.test(p.title + p.text));
  assert.equal(configProblem(null), null);
});

test('配置文件超过上限时不读取，直接按失败处理', async () => {
  const big = fakeFile(CONFIG_LIMIT + 1);
  await assert.rejects(configText(draft(big)), e => e.status === 413 && /配置文件太大了/.test(e.reason));
  assert.equal(big.reads, 0);

  const small = fakeFile(300);
  assert.equal(await configText(draft(small)), '[Interface]\n');
  assert.equal(small.reads, 1);

  // 不是配置文件（安装包）或者没有文件：不读
  const pkg = fakeFile(CONFIG_LIMIT + 1);
  assert.equal(await configText({ spec: { inputs: [{ key: 'package', type: 'file' }] }, file: pkg }), undefined);
  assert.equal(pkg.reads, 0);
  assert.equal(await configText({ spec: { inputs: [] }, file: null }), undefined);
});

test('通用安装只有明确成功才进入登录，失败和中断保留明确原因', () => {
  assert.equal(installOutcome({ phase: 'queued' }), 'waiting');
  assert.equal(installOutcome({ phase: 'running' }), 'waiting');
  assert.equal(installOutcome({ phase: 'succeeded', exit_code: 0 }), 'succeeded');
  assert.equal(installOutcome({ phase: 'manual', exit_code: 20 }), 'manual');
  assert.equal(installOutcome({ phase: 'failed', exit_code: 7 }), 'failed');
  assert.equal(installOutcome({ phase: 'interrupted' }), 'failed');
  assert.match(installFailureText({ phase: 'failed', log: '准备\ndpkg: dependency failed' }), /dependency failed/);
  assert.match(installFailureText({ phase: 'interrupted' }), /不会自动重装/);
  assert.equal(installAttemptMatches('attempt-a', { attempt_id: 'attempt-a', phase: 'running' }), true);
  assert.equal(installAttemptMatches('attempt-a', { attempt_id: 'attempt-b', phase: 'succeeded' }), false);
  assert.equal(installAttemptMatches('', { attempt_id: 'attempt-a' }), false);
  assert.throws(() => requireInstallAttempt('attempt-a', { attempt_id: 'attempt-b', phase: 'succeeded' }),
    e => e.installation === true && e.step === 1 && /另一项任务/.test(e.reason));
  const queryError = installationError(new Error('temporary'), '暂时读不到安装状态');
  assert.equal(queryError.installation, true);
  assert.equal(queryError.fallback, '暂时读不到安装状态');
});
