// 临时验证：把 main.ts 里标签增删的下标运算抽出来跑一遍，
// 对比「修复前的算法」和「修复后的算法」，确认修复真的解决跳标签问题。
// 运行：node scripts/verify-tab-logic.mjs

/** 修复前的 closeTab 下标处理 */
function closeOld(list, active, target) {
  const index = list.indexOf(target);
  if (index < 0) return { list, active };
  const next = list.filter((t) => t !== target);
  if (active >= next.length) active = Math.max(0, next.length - 1);
  return { list: next, active };
}

/** 修复后的 closeTab 下标处理 */
function closeNew(list, active, target) {
  const index = list.indexOf(target);
  if (index < 0) return { list, active };
  const next = list.filter((t) => t !== target);
  if (index < active) active--;
  if (active >= next.length) active = Math.max(0, next.length - 1);
  return { list: next, active };
}

const cases = [
  { list: ['A', 'B', 'C'], active: 2, target: 'A', expect: 'C' }, // 关左侧标签，应仍停在 C
  { list: ['A', 'B', 'C'], active: 1, target: 'A', expect: 'B' },
  { list: ['A', 'B', 'C'], active: 0, target: 'B', expect: 'A' }, // 关右侧标签
  { list: ['A', 'B', 'C'], active: 0, target: 'A', expect: 'B' }, // 关自己，右移一位
  { list: ['A', 'B', 'C'], active: 2, target: 'C', expect: 'B' }, // 关自己（最后一个），左移一位
  { list: ['A', 'B'], active: 1, target: 'A', expect: 'B' },
  { list: ['A'], active: 0, target: 'A', expect: null }, // 全关光
];

let failed = 0;
console.log('场景                                      修复前   修复后   期望');
console.log('-'.repeat(74));
for (const c of cases) {
  const label = `[${c.list}] active=${c.active} 关闭 ${c.target}`.padEnd(42);
  const before = closeOld([...c.list], c.active, c.target);
  const after = closeNew([...c.list], c.active, c.target);
  const nameOld = before.list[before.active] ?? null;
  const nameNew = after.list[after.active] ?? null;
  const okNew = nameNew === c.expect;
  const okOld = nameOld === c.expect;
  if (!okNew) failed++;
  console.log(
    `${label}${String(nameOld).padEnd(9)}${String(nameNew).padEnd(9)}${String(c.expect).padEnd(9)}` +
    `${okOld ? '' : '(旧错) '}${okNew ? 'OK' : '<<< 新算法仍然错'}`,
  );
}

console.log('-'.repeat(74));
console.log(failed === 0 ? '修复后的下标算法全部通过。' : `修复后仍有 ${failed} 个场景失败！`);
process.exit(failed === 0 ? 0 : 1);
