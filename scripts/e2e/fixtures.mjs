// 端到端验收的夹具：一个 1000 篇的库。
//
// 从 `.tmp/gen-quick-vault.mjs` 搬过来的（B-33：验收资产不能只活在 `.tmp/`，
// 那里 `.gitignore` 一盖就没了，下次改这块只能重写一份判据未必等价的脚本）。
//
// 刻意包含四类难看的名字：中文、大小写混排、子目录、以及 `foo/bar` 这种
// 用户真的会输进来的路径。另外每个打分规则都配了一个**只差一点**的诱饵，
// 这样「首位命中正确」才有意义——如果候选里只有一篇能匹配，测的是筛选
// 而不是排序。
//
// # 幂等
//
// 每次全量重写：上一轮跑剩下的文件会让 `totalFiles` 漂移，而「1000 篇」是
// 验收口径（B-32：第一次绿、第二次红的脚本比没有脚本更糟）。
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'

import { QUICK_VAULT, RESCAN_VAULT } from './env.mjs'

/**
 * 兜底：这些脚本会 `unlink` / `rmSync`，绝不允许被指向真人的库。
 *
 * 判据是「在系统临时目录下」，不是「路径含 `.tmp`」——后者在别人机器上
 * 未必成立，而前者恰好也排除了仓库里的 `.tmp`（那里删一个文件 390ms）。
 */
export function assertTempVault(root) {
  const temp = path.resolve(os.tmpdir()).toLowerCase()
  const abs = path.resolve(root).toLowerCase()
  const rel = path.relative(temp, abs)
  if (rel === '' || rel.startsWith('..') || path.isAbsolute(rel)) {
    throw new Error(`夹具路径必须在系统临时目录（${os.tmpdir()}）下：${root}`)
  }
}

/**
 * 递归删掉库里所有 `.md`，**不删普通目录**。
 *
 * 幂等性的全部来源（B-32）：上一轮恢复出来的 `c 2.md`、`f 2.md` 会改变这一轮
 * 的落点（撞名判定看的是磁盘），于是「应该恢复成 c 2.md」变成「c 3.md」——
 * 断言没错，起点错了。这类失败最贵：它看起来像实现坏了。
 *
 * 名字以 `.md` 结尾的**目录**要整块删：夹具会用「同名目录」模拟"这个名字被
 * 占了"，只删文件的话下一轮 `writeFileSync('a/b/f.md')` 直接撞 `EISDIR`。
 */
export function sweepNotes(dir) {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    if (entry.name === '.knowisle') continue
    const abs = path.join(dir, entry.name)
    if (!entry.isDirectory()) {
      if (entry.name.endsWith('.md')) fs.unlinkSync(abs)
      continue
    }
    if (entry.name.endsWith('.md')) fs.rmSync(abs, { recursive: true, force: true })
    else sweepNotes(abs)
  }
}

/**
 * 只清回收站（文件 + 清单），不碰笔记。
 *
 * 回收目录里躺的是**正文**，不是元数据：上一轮留下的 `...-dup.md` 会让这一轮
 * 「trash 里恰好 1 份」的断言直接红在一个正确的实现上（B-32）。它与笔记的重写
 * 是两件事，所以单独一个函数——`prepareQuickVault` 需要「1000 篇 + 空回收站」，
 * 而 `resetVault` 只想要其中一半。
 *
 * **不删 `.knowisle/` 本身**：应用开着时 `index.sqlite` 被连接占着，Windows 上
 * 会抛 `EBUSY`。回收目录与清单是普通文件，应用不持有，可以直接清。
 */
export function clearTrash(root) {
  assertTempVault(root)
  for (const dir of ['trash', 'trash-manifest']) {
    fs.rmSync(path.join(root, '.knowisle', dir), { recursive: true, force: true })
  }
}

/**
 * 把夹具库拉回起点：删 `.md`、清回收站、重写指定的文件。
 *
 * @param files `{ 'a/b/c.md': '正文' }`，相对路径 → 内容。
 */
export function resetVault(root, files = {}, { clearTrash: wipeTrash = false } = {}) {
  assertTempVault(root)
  fs.mkdirSync(root, { recursive: true })
  sweepNotes(root)
  if (wipeTrash) clearTrash(root)
  for (const [rel, content] of Object.entries(files)) {
    const abs = path.join(root, rel)
    fs.mkdirSync(path.dirname(abs), { recursive: true })
    fs.writeFileSync(abs, content, 'utf8')
  }
}

/** 每篇样本的内容。刻意短：验收需要「整篇都渲染出来」，才能由 DOM 反算绝对偏移。 */
const body = (title, dir, n) => `# ${title}

这是 ${dir} 下的第 ${n} 篇样本笔记。

- 条目一
- 条目二

正文最后一行。
`

function pad(i) {
  return String(i).padStart(4, '0')
}

/** 夹具里的全部文件（`dir` 是相对目录，`name` 是文件名）。 */
function manifest() {
  const files = []
  const batch = (dir, count, makeName, special = []) => {
    for (const name of special) files.push({ dir, name })
    for (let i = 0; i < count - special.length; i += 1) {
      files.push({ dir, name: makeName(i) })
    }
  }

  batch('inbox', 60, (i) => (i % 2 === 0 ? `收件箱-${pad(i)}.md` : `inbox-${pad(i)}.md`))
  batch('notes', 100, (i) => (i % 2 === 0 ? `note-${pad(i)}.md` : `会议记录-${pad(i)}.md`), [
    'foobar.md',
  ])
  batch('notes/sub-a', 60, (i) => (i % 2 === 0 ? `sub-a-${pad(i)}.md` : `项目笔记-${pad(i)}.md`))
  batch('notes/sub-b', 60, (i) => (i % 2 === 0 ? `sub-b-${pad(i)}.md` : `阅读摘录-${pad(i)}.md`))
  batch('docs', 80, (i) => (i % 2 === 0 ? `guide-${pad(i)}.md` : `规范-${pad(i)}.md`), ['readme.md'])
  batch('archive', 80, (i) => (i % 2 === 0 ? `archive-${pad(i)}.md` : `旧稿-${pad(i)}.md`))
  batch('recipes', 60, (i) => (i % 2 === 0 ? `recipe-${pad(i)}.md` : `食谱-${pad(i)}.md`), [
    'almond-dessert/menu.md',
  ])
  batch('projects', 159, (i) =>
    i % 3 === 0 ? `Ki-Store-${pad(i)}.md` : i % 3 === 1 ? `DESIGN-${pad(i)}.md` : `project-${pad(i)}.md`,
  )
  batch('笔记', 100, (i) => (i % 2 === 0 ? `第${pad(i)}篇-读书摘要.md` : `第${pad(i)}篇-会议记录.md`), [
    '知识管理.md',
  ])
  batch('杂项', 60, (i) => (i % 2 === 0 ? `杂项-${pad(i)}.md` : `随想-${pad(i)}.md`), ['认知识别.md'])
  batch('中文/长篇阅读', 60, (i) => `长篇-${pad(i)}.md`)
  batch('foo', 40, (i) => `foo-${pad(i)}.md`, ['bar.md'])
  batch('alpha', 40, (i) => `Alpha-${pad(i)}.md`, ['API-Design.md'])
  batch('beta', 40, (i) => `beta-${pad(i)}.md`, ['api-design.md'])
  batch('bar', 1, () => 'unused.md', ['notes.md'])

  return files
}

/**
 * 清空一个 Vault，**但保留 `.knowisle/`**。
 *
 * # 为什么不能整目录删
 *
 * 应用开着的时候，索引库 `index.sqlite` 被 SQLite 的连接占着，Windows 上
 * `rm -rf` 会在它身上抛 `EBUSY`——脚本第二次跑就崩，红得像是实现坏了（B-32）。
 * 保留索引也不影响幂等：下面每次都会重写全部笔记，mtime 一变，增量同步
 * 就会把它们全部重读一遍，跟冷建没有区别。
 *
 * 唯一不能保留的场景是「必须从零建库」——那种验收要在应用**没打开这个库**
 * 的时候跑，或者换一个新的临时目录。
 */
export function wipeVault(root) {
  if (!fs.existsSync(root)) return
  for (const entry of fs.readdirSync(root, { withFileTypes: true })) {
    if (entry.name === '.knowisle') continue
    fs.rmSync(path.join(root, entry.name), { recursive: true, force: true })
  }
}

export function prepareQuickVault() {
  const root = QUICK_VAULT
  const files = manifest()
  if (files.length !== 1000) {
    throw new Error(`夹具数量不对：${files.length}，应为 1000`)
  }

  wipeVault(root)
  // 回收站也一起清：`wipeVault` 保留了 `.knowisle/`，而回收目录里躺的是正文。
  // 「trash 里恰好 1 份」这类断言（文件树 / 回收站脚本）在上一轮的残留面前必红。
  clearTrash(root)
  for (const [index, file] of files.entries()) {
    const abs = path.join(root, file.dir, file.name)
    fs.mkdirSync(path.dirname(abs), { recursive: true })
    fs.writeFileSync(abs, body(path.basename(file.name, '.md'), file.dir, index), 'utf8')
  }

  // ── 全文搜索的探针 ──────────────────────────────────────────────────────
  //
  // 只追加到既有文件上，**不新增 .md**：1000 篇是其它验收的口径，
  // 多一篇就会让「前 50 个候选」「totalFiles」这些数字漂移。
  //
  // - API-Design.md：全角/半角对照（证明不做宽度折叠），以及 5 行同词
  //   （证明每篇最多 3 条命中）。
  // - foo/bar.md：`Foo` 与 `foo`（证明 ASCII 大小写折叠）。
  // - notes/foobar.md：410 行左右的长文，验收「跳转后该行滚到视口」需要一个
  //   真的需要滚动的文档——短笔记里所有行本来就在视口内，那条断言会退化成恒真。
  const probes = {
    'alpha/API-Design.md': [
      '',
      '探针：全角ＡＢＣ 与半角ABC 并存。',
      '重复命中探针 一',
      '重复命中探针 二',
      '重复命中探针 三',
      '重复命中探针 四',
      '重复命中探针 五',
    ],
    'foo/bar.md': ['', '探针：Foo 大写，foo 小写。'],
    'notes/foobar.md': [
      '',
      ...Array.from({ length: 400 }, (_, i) => `长文填充行 ${i + 1}`),
      '',
      '长文探针在此',
    ],
  }
  for (const [rel, lines] of Object.entries(probes)) {
    fs.appendFileSync(path.join(root, rel), lines.join('\n') + '\n', 'utf8')
  }

  // ── 两个坏文件（「坏文件不连坐」） ──────────────────────────────────────
  //
  // 它们进得了扫描树（扩展名是 .md），但进不了全文索引：
  //   - GBK：非 UTF-8 → `InvalidEncoding`，跳过并计数。
  //   - 超大：> 2MB 的索引上限 → 跳过并计数。
  // 两个文件都带**独一无二**的标记，用来断言「它们真的没被索引」——
  // 只看"其它文件搜得到"是不够的，那不能排除"坏文件也被静默索引了"。
  fs.mkdirSync(path.join(root, 'bad'), { recursive: true })
  fs.writeFileSync(
    path.join(root, 'bad', 'gbk-note.md'),
    Buffer.from(
      '47424b20b1e0c2ebb5c4bec9b1cabcc70d0ad6d0cec4c4dac8dda3bad6aacab6b9dcc0eda1a30d0a67626b2d6f6e6c792d6d61726b65720d0a',
      'hex',
    ),
  )
  fs.writeFileSync(
    path.join(root, 'bad', 'huge-note.md'),
    `huge-only-marker\n${'x'.repeat(2 * 1024 * 1024 + 1024)}\n`,
    'utf8',
  )

  return { root, count: files.length }
}

/* ───────────────────── 「重新扫描」的夹具（ADR-017） ───────────────────── */

/**
 * 造一个「重扫前后会变、但变得有据可查」的库。
 *
 * # 每个目录都对应一条判据，不是随便凑的
 *
 * | 目录 | 子项 | 默认展开 | 服务于 |
 * |---|---|---|---|
 * | `inbox/` | 2 | 是（depth 0 且 ≤ 8） | 判据 1 的反面：新文件落在已展开的目录里，看见它不需要任何展开逻辑 |
 * | `notes/` | 20 | 否（> 8） | 判据 3：手动展开 → 重扫 → 仍展开；也是当前笔记的家 |
 * | `deep/` | 2（`sub/` 折叠） | 是 | 判据 3 的反面：没动过的目录不许被重扫弹开 |
 * | `bulk/` | 2000 | 否 | 判据 7：重扫会让索引失效，2000 篇的同步窗口足够宽（本机实测 ≈ 0.8 s），宽到能在里面开搜索 |
 *
 * 判据 1 要的「新目录」不在夹具里——它由脚本自己 `mkdir` 出来，
 * 因为「树里先是没它、点完重扫才有」才是那条判据的全部内容。
 *
 * # 回收站直接造，不走 UI
 *
 * 判据 5 要「停在第 2 页」，而每页 200 条。用 UI 删 201 篇要跑 201 次
 * 「右键 → 删除 → 确认 + 整库重扫」，那是分钟级且每步都可能被前一步的
 * 状态影响。回收站的磁盘契约很窄（`trash/<ms>-<名>` + `trash-manifest/<entry>.json`），
 * 直接照它写 250 条既快又确定。
 */
export function prepareRescanVault() {
  const root = RESCAN_VAULT
  assertTempVault(root)
  fs.mkdirSync(root, { recursive: true })
  // 整块清（不只是 `.md`）：上一轮的 `新来的/` 是个空目录，留着会让
  // 「树里没有这个目录」这条前置断言在第二次运行时恒假（B-32）。
  wipeVault(root)
  clearTrash(root)

  const files = {
    'inbox/one.md': '# 第一封\n\n收件箱里的第一封。\n',
    'inbox/two.md': '# 第二封\n\n收件箱里的第二封。\n',
    'notes/alpha.md': '# alpha\n\n当前笔记：重扫的对账对象。\n',
    'notes/beta.md': '# beta\n\n非当前笔记：用来验「外部删除后树里消失」。\n',
    'notes/gamma.md': '# gamma\n\n改名的那一篇。\n',
    'deep/other.md': '# other\n\n第二层之外的一篇。\n',
    'deep/sub/leaf.md': '# leaf\n\n`deep/sub` 默认折叠（depth 1）。\n',
  }
  for (let i = 0; i < 17; i += 1) {
    files[`notes/note-${String(i).padStart(2, '0')}.md`] = `# note-${i}\n\n第 ${i} 篇。\n`
  }
  for (let i = 0; i < 2000; i += 1) {
    files[`bulk/bulk-${String(i).padStart(3, '0')}.md`] = `# bulk-${i}\n\n索引同步的填充篇。\n`
  }
  for (const [rel, content] of Object.entries(files)) {
    const abs = path.join(root, rel)
    fs.mkdirSync(path.dirname(abs), { recursive: true })
    fs.writeFileSync(abs, content, 'utf8')
  }

  // 250 条回收站记录：每页 200，「加载更多」之后才有第 2 页。
  const trashDir = path.join(root, '.knowisle', 'trash')
  const manifestDir = path.join(root, '.knowisle', 'trash-manifest')
  fs.mkdirSync(trashDir, { recursive: true })
  fs.mkdirSync(manifestDir, { recursive: true })
  const base = 1_700_000_000_000
  for (let i = 0; i < 250; i += 1) {
    const stamp = base + i
    const name = `旧稿-${String(i).padStart(3, '0')}.md`
    const entry = `${stamp}-${name}`
    fs.writeFileSync(path.join(trashDir, entry), `# 旧稿 ${i}\n`, 'utf8')
    fs.writeFileSync(
      path.join(manifestDir, `${entry}.json`),
      JSON.stringify({ origin: `notes/${name}`, deletedAt: stamp }),
      'utf8',
    )
  }

  return { root, count: Object.keys(files).length }
}
