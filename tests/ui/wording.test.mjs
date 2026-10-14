// V-07：界面文字不出现内部用词。扫描 app/ui 里除 vendor/ 以外的全部文件，
// 凡是含中日韩字符的行，出现 mihomo|docker|socks|probe|容器（不分大小写）就失败；标了 wording-allow 的行除外。
import test from 'node:test';
import assert from 'node:assert/strict';
import { readdir, readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import { FORBIDDEN } from '../../app/ui/js/terms.js';
import { humanize, clean, recentErrors, savedPending } from '../../app/ui/js/feedback.js';

test('已保存但未同步的写入结果不误报为保存失败', () => {
  assert.equal(savedPending({ rules_applied: false }), true);
  assert.equal(savedPending({ config_application: { pending: true } }), true);
  assert.equal(savedPending({ rules_applied: true }), false);
  const result = humanize(new ApiError({ status: 502, body: { saved: true } }));
  assert.equal(result.title, '已保存，还没同步到入口');
});
import { ApiError } from '../../app/ui/js/api.js';

const ROOT = fileURLToPath(new URL('../../app/ui/', import.meta.url));
const CJK = /[⺀-鿿가-힯豈-﫿＀-￯]/;
const WORDS = /mihomo|docker|socks|probe|容器/i; // wording-allow

async function* walk(dir) {
  for (const e of await readdir(dir, { withFileTypes: true })) {
    const p = path.join(dir, e.name);
    if (e.isDirectory()) { if (path.relative(ROOT, p) !== 'vendor') yield* walk(p); }
    else yield p;
  }
}

// 返回违规的行：[{file, line, text}]
function scan(text, file = '') {
  const bad = [];
  text.split('\n').forEach((line, i) => {
    if (CJK.test(line) && WORDS.test(line) && !line.includes('wording-allow')) bad.push({ file, line: i + 1, text: line.trim() });
  });
  return bad;
}

test('扫描规则本身：含中文且含禁词才算，标记的行放过', () => {
  assert.equal(scan('// 通过 Docker 起').length, 1);
  assert.equal(scan("text: '检查 socks 出口'").length, 1);
  assert.equal(scan("text: '容器'").length, 1);
  assert.equal(scan('const docker = 1;').length, 0);
  assert.equal(scan("'通道已连接'").length, 0);
  assert.equal(scan('// 容器 wording-allow').length, 0);
  assert.equal(WORDS.source, FORBIDDEN.source);
});

test('app/ui（vendor/ 除外）没有内部用词', async () => {
  const bad = [];
  let files = 0;
  for await (const f of walk(ROOT)) {
    files++;
    bad.push(...scan(await readFile(f, 'utf8'), path.relative(ROOT, f)));
  }
  assert.ok(files > 20, '应该扫到界面文件');
  assert.deepEqual(bad, []);
});

test('错误转人话：后端原文命中禁词或不是中文时换成通用句子，原文只留在内存里', () => {
  const docker = humanize(new ApiError({ status: 500, reason: 'docker 连接不可用', raw: '{"error":"docker 连接不可用"}' })); // wording-allow
  assert.ok(!FORBIDDEN.test(docker.title + docker.text));
  assert.equal(docker.title, '操作没有完成');
  assert.equal(recentErrors().at(-1).raw, '{"error":"docker 连接不可用"}'); // wording-allow

  const english = humanize(new ApiError({ status: 500, reason: 'put_file failed: broken pipe' }));
  assert.equal(english.title, '操作没有完成');

  const conflict = humanize(new ApiError({ status: 409, reason: 'replacement pending' }));
  assert.equal(conflict.title, '有别的操作正在进行，请稍后再试');

  const zh = humanize(new ApiError({ status: 409, reason: '请先启动通道' }));
  assert.equal(zh.title, '请先启动通道');

  const rt = humanize(new ApiError({ status: 503, kind: 'runtime', reason: 'docker unavailable' })); // wording-allow
  assert.equal(rt.title, '运行环境还没准备好');
  assert.ok(!FORBIDDEN.test(rt.text));

  const failed = humanize(new ApiError({ status: 200, kind: 'failed', reason: '' }));
  assert.equal(failed.title, '操作没有完成');

  assert.equal(humanize(Object.assign(new Error('x'), { name: 'AbortError' })), null);
  assert.equal(humanize(null), null);
  assert.equal(clean('SOCKS5 探活失败'), ''); // wording-allow
  assert.equal(clean('已保存'), '已保存');
});
