import test from 'node:test';
import assert from 'node:assert/strict';
import { recommendedMemory, needsMemoryRestart, runningMemory } from '../../app/ui/js/runtime-memory.js';
import { needs } from '../../app/ui/js/screens/home.js';
test('扩容推荐以真实运行量翻倍，并受 16 GiB 和物理内存一半约束', () => {
  assert.equal(recommendedMemory(2,64),4);
  assert.equal(recommendedMemory(6,16),8);
  assert.equal(recommendedMemory(10,64),16);
  assert.equal(recommendedMemory(8,16),null);
  assert.equal(recommendedMemory(2,null),null);
  assert.equal(runningMemory({}),null);
});
test('保存值与实际值不同才提示重启，只有真实 OOM 才出现首页提示', () => {
  const system = { runtime_restart_available: true, memory_pressure:{memory_gib:2,oom_kills:0} };
  assert.equal(needsMemoryRestart(4,system),true);
  assert.equal(needsMemoryRestart(2,system),false);
  assert.equal(needsMemoryRestart(4,{}),false);
  assert.ok(!needs({system}).some(x=>x.key==='memory-low'));
  system.memory_pressure.oom_kills=1;
  const rows = needs({system, channels:[{id:'a',status:'error'}],causes:{a:{code:'memory_low',action:'restart_runtime_bigger'}}});
  assert.equal(rows.filter(x=>x.key==='memory-low').length,1);
  assert.ok(!rows.some(x=>x.key==='error:a'));
});
