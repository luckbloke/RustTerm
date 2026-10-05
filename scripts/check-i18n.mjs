// 校验脚本：不做代码求值（沙箱禁止子进程），改为静态解析 i18n.ts，
// 检查中英词条是否对齐，以及代码/HTML 引用的键是否都存在。
//
//   node scripts/check-i18n.mjs
//
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const src = readFileSync(join(root, 'src', 'i18n.ts'), 'utf8');
const main = readFileSync(join(root, 'src', 'main.ts'), 'utf8');
const html = readFileSync(join(root, 'index.html'), 'utf8');

/** 取出 `const NAME ... = { ... }` 的对象体（跳过类型标注里的花括号）。 */
function sliceObject(name) {
  const decl = src.indexOf(`const ${name}`);
  if (decl < 0) throw new Error(`未找到 ${name}`);
  const assign = src.indexOf('= {', decl);
  if (assign < 0) throw new Error(`${name} 没有对象字面量`);
  const open = assign + 2;
  let depth = 0;
  for (let i = open; i < src.length; i++) {
    if (src[i] === '{') depth++;
    else if (src[i] === '}') {
      depth--;
      if (depth === 0) return src.slice(open + 1, i);
    }
  }
  throw new Error(`${name} 未闭合`);
}

/** 解析顶层键，并报告是否展开了 staticText。 */
function parseObject(body) {
  const keys = new Set();
  let spreadsStatic = false;
  for (const line of body.split('\n')) {
    if (/^\s*\.\.\.staticText\s*,?\s*$/.test(line)) { spreadsStatic = true; continue; }
    // 顶层键固定缩进两格，避免匹配到嵌套对象里的字段
    const m = /^ {2}([A-Za-z][A-Za-z0-9_]*)\s*:/.exec(line);
    if (m) keys.add(m[1]);
  }
  return { keys, spreadsStatic };
}

const statics = parseObject(sliceObject('staticText'));
const zh = parseObject(sliceObject('zh'));
const en = parseObject(sliceObject('en'));

for (const [name, dict] of [['zh', zh], ['en', en]]) {
  if (!dict.spreadsStatic) throw new Error(`${name} 没有展开 staticText`);
  for (const k of statics.keys) dict.keys.add(k);
}

const problems = [];
for (const k of zh.keys) if (!en.keys.has(k)) problems.push(`en 缺少词条: ${k}`);
for (const k of en.keys) if (!zh.keys.has(k)) problems.push(`zh 缺少词条: ${k}`);

/** 判断某个键在两个文件里出现时是字符串还是函数。 */
function kindOf(body, key) {
  const re = new RegExp(`^ {2}${key}\\s*:\\s*(.)`, 'm');
  const m = re.exec(body);
  if (!m) return null;
  return m[1] === '(' || m[1] === '{' ? 'function' : 'string';
}
for (const k of zh.keys) {
  const a = kindOf(sliceObject('zh'), k);
  const b = kindOf(sliceObject('en'), k);
  if (a && b && a !== b) problems.push(`词条类型不一致: ${k} (zh=${a}, en=${b})`);
}

const used = new Set();
for (const m of main.matchAll(/\bt\(\s*'([A-Za-z][A-Za-z0-9_]*)'/g)) used.add(m[1]);
for (const m of main.matchAll(/(?:showPrompt|showConfirm|showAlert)\(\s*'([A-Za-z][A-Za-z0-9_]*)'\s*,\s*'([A-Za-z][A-Za-z0-9_]*)'/g)) {
  used.add(m[1]);
  used.add(m[2]);
}
for (const m of html.matchAll(/data-i18n(?:-title|-placeholder)?="([A-Za-z][A-Za-z0-9_]*)"/g)) used.add(m[1]);

// errorText 内部用 return t('...') 取文案，也要算作引用
for (const m of src.matchAll(/return\s+t\(\s*'([A-Za-z][A-Za-z0-9_]*)'/g)) used.add(m[1]);
for (const m of src.matchAll(/^\s*case '[^']*':\s*\n\s*return t\(\s*'([A-Za-z][A-Za-z0-9_]*)'/gm)) used.add(m[1]);
for (const m of src.matchAll(/t\(\s*'(err[A-Za-z0-9_]*)'/g)) used.add(m[1]);
for (const k of used) if (!zh.keys.has(k)) problems.push(`代码/HTML 引用了不存在的键: ${k}`);

const unused = [...zh.keys].filter((k) => !used.has(k)).sort();

console.log(`staticText ${statics.keys.size} 条，zh ${zh.keys.size} 条，en ${en.keys.size} 条，代码/HTML 引用 ${used.size} 个键`);
if (unused.length) console.log(`\n未被引用的词条（${unused.length}）: ${unused.join(', ')}`);
if (problems.length) {
  console.log('\n问题:');
  for (const p of problems) console.log('  - ' + p);
} else {
  console.log('\n中英词条完全对齐，引用的键全部存在。');
}
process.exit(problems.length ? 1 : 0);
