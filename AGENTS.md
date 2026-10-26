# 故事工坊 · Agent 操作说明书

单页规则，所有会话适用；应用功能见 `app/说明书.md`（唯一应用手册），术语见 `CONTEXT.md`。

## 项目形态

- **程序页**：`app/index.html`——无构建、BYOK 的单文件应用。
- **本地服务与壳**：`app/server-rs/`（Rust 单 exe：axum 服务＋Tauri 壳窗口同一进程），经 `/api/fs` 等端点读写用户指定的数据文件夹。端点契约摘要见 `app/说明书.md` §二。
- **提示词资产**：`app/storyforge-提示词全库.json` 等为程序数据，非文档。

## 构建与验证

| 动作 | 命令 |
|---|---|
| 构建 Windows exe | `node tools/release_rust.mjs` → `release/rust/故事工坊.exe` |
| 快速核验（不构建） | `node tools/dev_page.mjs`（exe 服仓内页面＋隔离数据夹，刷新即生效） |
| 类型门 | `node tools/check_types.ts`（基线 `tools/check_types.baseline.json` 只降不升；改到哪个函数就给哪个函数补 JSDoc 注解） |
| 全量回归（最小集） | `node tools/run_all.ts` |

**Windows 单平台铁律**：发布形态永久为 Windows 单 exe；不做任何 macOS 交叉构建，mac 产物不进本仓、不阻塞发布（ADR 0002）。

## 硬性纪律

1. **删除一律走回收站**（PowerShell `Microsoft.VisualBasic.FileIO.FileSystem` 带 `RecycleOption`），禁止 `rm/del` 直删；批量删除先出清单、边删边落日志；「计划非空但现状为空」类异常立即停下人工核查。
2. **提交**：显式 `git add <具体文件>`，禁止 `git add -A` / `git add .`；提交前 `git diff --check`；保持 `.gitattributes` 的 `* -text`。
3. **隐私挡板**：默认不推送远端（先生明确说「推送」才 push）；不索取/打印/落盘 token；机密（`*.xy-config.json`、`*.lx-config.json`、任意层级 `_工坊/机密/`）任何情况下不得入库。
4. **数据边界**：`data/books/`、`data/sources/`、`release/`、运行态数据夹 `D:\故事工坊\` 永不入 git。
5. **不追踪历史**：旧 SHA、旧工单、旧文档一律不可引用；查证历史去备份盘（ADR 0001）。
6. **含 Windows 路径的脚本**先落 `.py` 文件再执行（Git Bash heredoc 内联会吃反斜杠）；脚本写完文件必回读核对；`SyntaxWarning` 即停。
7. **上下文防爆**：`app/index.html`（约 1.16 MB）与 `app/storyforge-提示词全库.json`（约 392 KB）禁无锚点全量读——先 Grep 定位再局部读（单次 ≤120 行），或写一次性扫描脚本输出精简汇总。

## 目录一览

| 位置 | 职责 |
|---|---|
| `app/` | 程序：`index.html`、`server-rs/`、`说明书.md`、提示词 JSON |
| `docs/` | `agents/`（协作协议三件）、`adr/`（决策记录） |
| `tools/` | 构建/验证工具（原位本地化，不入 git） |
| `.scratch/` | 本地工单区（不入 git），入口见 `docs/agents/issue-tracker.md` |
| `data/` | 运行数据（books/sources 不入库） |
| `release/` | 构建产物（不入库） |

## 协作入口

- **工单**：读 `docs/agents/issue-tracker.md`，统一使用根 `.scratch/<feature>/` 的本地 Markdown。
- **分诊**：读 `docs/agents/triage-labels.md`。
- **术语与决策**：`CONTEXT.md` 是术语唯一来源；决策记录在 `docs/adr/`，记新决策依 domain-modeling 规则新增 ADR。
- 功能、提示词和交互入口变更同步 `app/说明书.md`。
