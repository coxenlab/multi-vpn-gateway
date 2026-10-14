import test from 'node:test';
import assert from 'node:assert/strict';
import { area } from '../../app/ui/js/chart.js';

test('C-09：空数据画空状态，不补假点', () => {
  assert.match(area([]), /chart-none/);
  assert.match(area([null, 5, null]), /chart-none/);
});

test('有效点全被空值隔开时画空状态，不出空白图', () => {
  const out = area([1, null, 2, null, 3]);
  assert.match(out, /chart-none/);
  assert.doesNotMatch(out, /<svg/);
});

test('有一段连着两个点就画线，空值处断开', () => {
  const out = area([1, 2, null, 3]);
  assert.match(out, /<svg/);
  assert.equal((out.match(/M/g) || []).length >= 2, true);
});
