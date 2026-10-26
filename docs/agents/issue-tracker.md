# Issue 追踪：本地 Markdown

本仓的问题（issues）与规格（specs）以 Markdown 文件形式存放在根 `.scratch/` 下。`app/`、`tools/` 共用此入口；从子目录工作时仍按仓根解析下列路径。旧 `D:\novelcrafter` 三仓只查证历史；本配置不授权创建远端或 GitHub Issues。

## 约定

- 一个特性一个目录：`.scratch/<feature-slug>/`
- 规格文件为 `.scratch/<feature-slug>/spec.md`
- 实施工单每票一个文件：`.scratch/<feature-slug>/issues/<NN>-<slug>.md`，从 `01` 编号，禁止用单文件合并全部工单
- 工单顶部同时记录 `Status:`、`Label:` 与 `Implementation:`；分诊角色和完成状态见 `triage-labels.md`
- `Status` 是调度与完成判定的唯一来源。用 `node tools/ticket_status.ts --check <票据...>` 只读检查 Label 漂移；用 `--write <单张票据>` 从 Status 派生 Label，写入只改目标票的 Label 行并保持原换行。
- 评论与会话历史追加到文件底部的 `## Comments` 标题下

工单目录属 `.scratch/`（不入 git），批次收口后可整目录清理。

执行前读规格、工单、前置和决策门；`needs-info` 或未解除的门保持等待。验收与根仓提交完成后，同批把 `Status`/`Label` 置 `completed`、更新 `Implementation`、勾选验收项并在 `Comments` 记录证据；只补实现说明或只改一个状态字段不算收口。规格有多票时如实汇总进度，未裁定子票保留等待状态。

## 当某个 skill 说「发布到 issue 追踪处」

在 `.scratch/<feature-slug>/` 下新建文件（目录不存在则先创建）。

若是实施工单，按 `issues/<NN>-<slug>.md` 新建独立票，不用一个总文件替代多票。

## 当某个 skill 说「取回相关工单」

读取指定路径的文件。用户通常会直接给出路径或工单编号。

## Wayfinding 操作

供 `/wayfinder` 使用。**地图（map）**是一个文件，**子票**是每票一个文件。

下列探索子票的 `claimed`/`resolved` 是 wayfinder 协议，区别于实施工单的分诊与 `completed`，不交给写作票据调度器执行。

- **地图**：`.scratch/<effort>/map.md`（承载 Notes / Decisions-so-far / Fog 正文）。
- **子票**：`.scratch/<effort>/issues/NN-<slug>.md`，从 `01` 编号，正文写该票要解决的问题。`Type:` 行记录票型（`research`/`prototype`/`grilling`/`task`）；`Status:` 行记录 `claimed`/`resolved`。
- **阻塞**：顶部附近的 `Blocked by: NN, NN` 行。所列文件全部 `resolved` 时该票解除阻塞。
- **前沿（frontier）**：扫描 `.scratch/<effort>/issues/`，找出开放、未阻塞、未认领的票；编号最小者优先。
- **认领**：开工前先置 `Status: claimed` 并保存。
- **解决**：在 `## Answer` 标题下追加答案，置 `Status: resolved`，再把上下文指针（要点＋链接）追加到 `map.md` 的 Decisions-so-far。
