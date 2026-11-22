#!/usr/bin/env node
/* 单字名提及扫描：从 app/index.html 抽取判定函数做可重复回归（不依赖活书路径）。
   覆盖：单字「冥」能中、「冥王星」排除、「智辰」不记到「辰」、「红了」不中、
   「老林／小林」算人名、「明亮。」不因句末标点而中、多字设定次数不回退。
   run: node tools/sim_mention_single.mjs */
import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import {fileURLToPath} from 'node:url';

const REPO = fileURLToPath(new URL('../', import.meta.url));
const APP_PAGE = path.join(REPO, 'app', 'index.html');

const srcHtml = fs.readFileSync(APP_PAGE, 'utf8');
const startMark = '/* ---------- 单字名提及判定 ----------';
const start = srcHtml.indexOf(startMark);
const sceneEnd = srcHtml.indexOf('/* ---------- 模板引擎');
if (start < 0 || sceneEnd < 0) {
  console.error('FAIL 无法从 index.html 抽取单字名判定源');
  process.exit(1);
}
// 抽判定模块一直到 sceneMentions 结束（模板引擎之前），再附上 evolCountMentions / 检测入口。
// 缓存表在模块之前声明，抽取时补上，避免 mentionLexInstall 触发 invalidate 时未定义。
let source = 'const _menScanCache=new Map();const _sceneMenCache=new Map();var _graphCoCache=null;\n' + srcHtml.slice(start, sceneEnd);
const grabFn = (name) => {
  const re = new RegExp('function ' + name + '\\(');
  const i = srcHtml.search(re);
  if (i < 0) throw new Error('missing ' + name);
  let n = 0, j = srcHtml.indexOf('{', i);
  for (let k = j; k < srcHtml.length; k++) {
    const ch = srcHtml[k];
    if (ch === '{') n++;
    else if (ch === '}') {
      n--;
      if (n === 0) return srcHtml.slice(i, k + 1);
    }
  }
  throw new Error('unclosed ' + name);
};
for (const name of ['codexDetectNames', 'codexDetectHit', 'evolCountMentions', 'conflictExcerptAround', 'mentionFirstAccepted']) {
  if (!source.includes('function ' + name + '(')) source += '\n' + grabFn(name) + '\n';
}

const ctx = vm.createContext({
  console,
  Set, Map, Int32Array, Object, Array, String, Number, Boolean, JSON, Math,
  fetch: undefined,
  S: {ui: {novelId: 'n1'}, data: {novels: [{id: 'n1'}], scenes: [], codexEntries: []}},
  entriesOf: () => [],
  scenePos: () => [],
  scenePlainText: s => String(s && s.content || ''),
  htmlToText: s => String(s || ''),
  codexAiIncludeOf: e => (e && e.aiInclude) || 'onDetect',
  unitListTitle: s => s && s.title,
});
vm.runInContext(source, ctx, {filename: 'mention-single.js'});
const run = code => vm.runInContext(code, ctx);

const lexDir = path.join(REPO, 'app', 'assets', 'mention');
const freqText = fs.readFileSync(path.join(lexDir, 'char-freq.txt'), 'utf8');
const compText = fs.readFileSync(path.join(lexDir, 'compounds.txt'), 'utf8');
run('mentionLexInstallText(' + JSON.stringify(freqText) + ',' + JSON.stringify(compText) + ')');

let pass = 0, fail = 0;
const ok = (cond, name, extra) => {
  if (cond) { pass++; console.log('  PASS ' + name); }
  else { fail++; console.log('  FAIL ' + name + (extra !== undefined ? ' [' + extra + ']' : '')); }
};

ok(run('mentionCharTier("明")') === 'high', '分层 明=high');
ok(run('mentionCharTier("林")') === 'high', '分层 林=high');
ok(run('mentionCharTier("红")') === 'high', '分层 红=high');
ok(run('mentionCharTier("岁")') === 'high', '分层 岁=high');
ok(run('mentionCharTier("的")') === 'function', '分层 的=function');
ok(run('mentionCharTier("冥")') === 'rare', '分层 冥=rare');
ok(run('mentionCharTier("辰")') === 'rare', '分层 辰=rare');
ok(run('mentionCharTier("矩")') === 'rare', '分层 矩=rare');
ok(run('_mentionLex.compounds.has("明亮")'), '词表含 明亮');
ok(run('_mentionLex.compounds.has("冥王星")'), '词表含 冥王星');
ok(run('_mentionLex.compounds.has("小林")'), '词表含 小林（将按小X 默认算人名）');
ok(run('MEN_TIME_WORDS.has("辰时")'), '封闭时间词含 辰时');
ok(run('MEN_BOOK_NEIGH_MIN===5'), '本书邻字阈值=5');

const setupBook = (entries, plains) => {
  const code = `(function(){
    S.data.codexEntries = ${JSON.stringify(entries)}.map(e=>Object.assign({novelId:'n1',trackAlias:true,aiInclude:'onDetect'},e));
    const plains = ${JSON.stringify(plains)};
    S.data.scenes = plains.map((content,i)=>({id:'s'+i,novelId:'n1',title:'小节'+(i+1),content}));
    entriesOf = () => S.data.codexEntries;
    scenePos = () => S.data.scenes.map(s=>({s}));
    scenePlainText = s => String(s.content||'');
    htmlToText = s => String(s||'');
    mentionScanInvalidate();
  })()`;
  run(code);
};

const countOf = (eid, text) => {
  setupBook(ctx.S.data.codexEntries, [text]); // keep entries
  return run(`(()=>{const rules=entriesOf('n1').map(e=>({id:e.id,names:mentionNamesOf(e).sort()})).filter(r=>r.names.length);
    const m=mentionMatcher(rules);
    const c=mentionMatchCounts(m, ${JSON.stringify(text)}, mentionFilterCtx('n1'));
    return c.get(${JSON.stringify(eid)})||0;
  })()`);
};

// --- 合成用例 ---
const E = [
  {id:'e-ming', name:'冥', aliases:['冥王']},
  {id:'e-ming2', name:'明', aliases:[]},
  {id:'e-hong', name:'红', aliases:[]},
  {id:'e-lin', name:'林', aliases:[]},
  {id:'e-ju', name:'矩', aliases:[]},
  {id:'e-chen', name:'辰', aliases:[]},
  {id:'e-zhi', name:'智辰', aliases:[]},
  {id:'e-jly', name:'金灵月', aliases:['灵月']},
  {id:'e-wm', name:'文明', aliases:[]},
];
setupBook(E, ['']);

const scan = (text) => run(`(()=>{
  const rules=entriesOf('n1').map(e=>({id:e.id,names:mentionNamesOf(e).sort()})).filter(r=>r.names.length);
  const m=mentionMatcher(rules);
  const c=mentionMatchCounts(m, ${JSON.stringify(text)}, mentionFilterCtx('n1'));
  const o={};
  for(const [k,v] of c)o[k]=v;
  return o;
})()`);

// 冥能中
let r = scan('冥走进房间。冥说：「走。」');
ok((r['e-ming']||0) === 2, '单字「冥」能中', r['e-ming']);

// 冥王星排除：单字「冥」被词表复合词丢掉。别名「冥王」仍是多字独立计数（现行行为，前缀「冥王星」里的「冥王」会命中）——本断言只查单字过滤。
r = run(`(()=>{
  const text='探测船飞过冥王星。';
  const rules=[{id:'e-ming',names:['冥']}];
  const m=mentionMatcher(rules);
  const ctx=mentionFilterCtx('n1');
  ctx.official=new Set();
  return mentionMatchCounts(m,text,ctx).get('e-ming')||0;
})()`);
ok(r === 0, '仅单字「冥」时「冥王星」不计', r);
r = scan('人们称呼他为冥王。');
ok((r['e-ming']||0) >= 1, '别名「冥王」仍计入', r['e-ming']);

// 智辰不记到辰
r = scan('智辰站在门口，辰也跟了上来。');
ok((r['e-zhi']||0) === 1, '「智辰」记到智辰', r['e-zhi']);
ok((r['e-chen']||0) === 1, '独立的「辰」仍算，智辰里的辰不算', r['e-chen']);
r = scan('只有智辰在场。');
ok((r['e-chen']||0) === 0, '仅「智辰」时辰为 0', r['e-chen']);
ok((r['e-zhi']||0) === 1, '仅「智辰」时智辰为 1', r['e-zhi']);

// 红了不中
r = scan('她的脸却红了，额头也红着。');
ok((r['e-hong']||0) === 0, '「红了／红着」不中', r['e-hong']);

// 老林／小林算人名（用户确认；相对研究稿「默认不算」的差异）
r = scan('老林走过来。');
ok((r['e-lin']||0) === 1, '「老林」算人名', r['e-lin']);
r = scan('小林笑了。');
ok((r['e-lin']||0) === 1, '「小林」算人名（即使用户词表有小林）', r['e-lin']);
r = scan('穿过一片小林。');
ok((r['e-lin']||0) === 1, '残差：一片小林也会召回（已接受）', r['e-lin']);

// 明亮。不因句末标点而中
r = scan('灯光很明亮。');
ok((r['e-ming2']||0) === 0, '「明亮。」不因句末标点而中', r['e-ming2']);
r = scan('需要说明。明天出发。打开照明。');
ok((r['e-ming2']||0) === 0, '说明／明天／照明不中', r['e-ming2']);

// 多字设定不回退：金灵月／灵月独立计数
r = scan('金灵月回眸。灵月没有说话。金灵月与灵月都在。');
// 金灵月 appears 2 times; 灵月 bare appears 2 times (「灵月没有」「与灵月」); inside 金灵月 the alias 灵月 also independently counts under current rules.
// Current official behavior: different names independent. 金灵月 x2, 灵月 as alias of same entry also matches inside 金灵月 (2) + bare (2) = 4?
// e-jly owns both 金灵月 and 灵月. mentionMatchCounts: same name no self-overlap; different names independent; owners weight added per pattern.
// So 金灵月 hits 2, 灵月 hits: inside each 金灵月 + bare 灵月. 2 interiors + 2 bare = 4. Total weight = 2+4 = 6.
// "多字设定次数不被这次改动回退" — compare with unfiltered multi-only counts.
const multiText = '金灵月回眸。灵月没有说话。金灵月与灵月都在。人类文明还很脆弱。';
r = scan(multiText);
const multiOnly = run(`(()=>{
  const text=${JSON.stringify(multiText)};
  const rules=[{id:'e-jly',names:['金灵月','灵月']},{id:'e-wm',names:['文明']}];
  const m=mentionMatcher(rules);
  const raw=mentionMatchSpans(m,text);
  const unfiltered=new Map();
  for(const s of raw)if(s.length>=2)unfiltered.set(s.eid,(unfiltered.get(s.eid)||0)+s.weight);
  const filtered=mentionMatchCounts(m,text,{official:new Set(['金灵月','灵月','文明']),compounds:_mentionLex.compounds});
  return {rawJly:unfiltered.get('e-jly')||0, rawWm:unfiltered.get('e-wm')||0, filJly:filtered.get('e-jly')||0, filWm:filtered.get('e-wm')||0};
})()`);
ok(multiOnly.rawJly === multiOnly.filJly && multiOnly.filJly > 0, '金灵月／灵月次数不回退', JSON.stringify(multiOnly));
ok(multiOnly.rawWm === multiOnly.filWm && multiOnly.filWm === 1, '「文明」次数不回退', JSON.stringify(multiOnly));
ok((r['e-wm']||0) === 1, '正文「文明」计入文明条目', r['e-wm']);
ok((r['e-ming2']||0) === 0, '「文明」里的「明」不记到明（更长设定名让位）', r['e-ming2']);

// 矩：规矩／矩阵不中；「矩问」能中
r = scan('做人要守规矩，不要只看矩阵。矩问：「谁？」');
ok((r['e-ju']||0) === 1, '规矩／矩阵排除，标点称谓「矩问」能中', r['e-ju']);

// 高亮／检测／出场／演进共用判定
setupBook(E, ['冥走进房间。灯光很明亮。老林来了。']);
ok(run(`codexDetectHit(S.data.codexEntries.find(e=>e.id==='e-ming'), '冥走进房间。')===true`), 'codexDetectHit 冥能中');
ok(run(`codexDetectHit(S.data.codexEntries.find(e=>e.id==='e-ming2'), '灯光很明亮。')===false`), 'codexDetectHit 明亮不中');
ok(run(`mentionTextHitsEntry(S.data.codexEntries.find(e=>e.id==='e-lin'), '老林来了。', mentionFilterCtx('n1'))===true`), '出场/命中 老林');
ok(run(`mentionTextHitsEntry(S.data.codexEntries.find(e=>e.id==='e-ming2'), '灯光很明亮。', mentionFilterCtx('n1'))===false`), '出场 明亮不中');

// evolCountMentions：多字最长优先不变，单字过滤一致
ok(run(`evolCountMentions('金灵月回眸。', ['金灵月','灵月'])>=1`), 'evol 多字仍计数');
const evolReg = run(`(()=>{
  const t='金灵月回眸。灵月没有说话。';
  const a=evolCountMentions(t,['金灵月','灵月']);
  const b=evolCountMentions(t,['金灵月','灵月','月'], mentionFilterCtx('n1'));
  return {a,b,same:a===b};
})()`);
ok(evolReg.same, 'evol 加入不相关单字「月」（无此设定语境）不改变多字次数', JSON.stringify(evolReg));
ok(run(`evolCountMentions('灯光很明亮。', ['明'], mentionFilterCtx('n1'))===0`), 'evol 「明亮。」中的明不计');
ok(run(`evolCountMentions('冥走进房间。', ['冥'], mentionFilterCtx('n1'))===1`), 'evol 单字冥计入');

// 矛盾摘录用名：不要抽到「明亮」里的明
const ex = run(`conflictExcerptAround('灯光很明亮。冥走进来。', ['明','冥'], 52, mentionFilterCtx('n1'))`);
ok(ex.mark === '冥', '矛盾摘录落到真正的单字命中', JSON.stringify(ex));

// 强制视为人名 / 视为复合词
run(`S.data.codexEntries.find(e=>e.id==='e-ming2').mentionForcePerson=true; mentionScanInvalidate();`);
r = scan('灯光很明亮。');
ok((r['e-ming2']||0) === 1, '本条强制视为人名时可计入明亮中的明', r['e-ming2']);
run(`S.data.codexEntries.find(e=>e.id==='e-ming2').mentionForcePerson=false;
     S.data.codexEntries.find(e=>e.id==='e-ming2').mentionCompoundWords=['冥夜'];
     S.data.novels[0].mentionExceptions={compound:['冥夜'],person:[]};
     mentionScanInvalidate();`);
r = scan('那是冥夜之前的事。');
ok((r['e-ming']||0) === 0, '用户复合词例外可排除「冥夜」', r['e-ming']);

// 建议别名提示：不强制
ok(run(`mentionNameNeedsAliasHint({name:'冥',aliases:[]})===true`), '单字且无别名时建议补别名');
ok(run(`mentionNameNeedsAliasHint({name:'冥',aliases:['卢修斯']})===false`), '已有两字以上别名则不再提示');
ok(run(`mentionNameNeedsAliasHint({name:'金灵月',aliases:[]})===false`), '多字名不提示');

// names 含单字
ok(run(`codexDetectNames({name:'冥',aliases:['卢修斯'],trackAlias:true}).indexOf('冥')>=0`), 'codexDetectNames 含单字名');

console.log(`ALL PASS: ${pass}/${pass + fail}`);
process.exit(fail ? 1 : 0);
