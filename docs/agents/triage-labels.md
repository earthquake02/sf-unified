# 分诊标签

工程 skill 以五个规范分诊角色表述。本文件把角色映射到本仓 issue 追踪处实际使用的标签字符串。

| mattpocock/skills 中的角色 | 本仓使用的标签字符串 | 含义 |
| --- | --- | --- |
| `needs-triage` | `needs-triage` | 维护者需要评估此问题 |
| `needs-info` | `needs-info` | 等待报告者补充信息 |
| `ready-for-agent` | `ready-for-agent` | 已充分规格化，可由 AFK 智能体执行 |
| `ready-for-human` | `ready-for-human` | 需要人类实现 |
| `wontfix` | `wontfix` | 不予处理 |

当某个 skill 提到某角色（如「打上 AFK 就绪分诊标签」）时，使用右列对应的标签字符串。

`Status` 是唯一权威字段，`Label` 是兼容派生字段。新建或变更单张票据后运行 `node tools/ticket_status.ts --write <票据>`；批量旧票不自动重写。只读核对使用 `--check`，发现漂移只报告文件名和期望值，不修改票面，也不改变调度结果。

## 实施工单的状态

上述五项是分诊角色；本仓另外使用 `completed` 表示已经验收并提交的实施票。新建或改动实施工单时，顶部 `Status:` 与 `Label:` 同步使用相应字符串，实施进展另记 `Implementation:`。收口同时置 `Status: completed` 与 `Label: completed`，并回写验收及 Comments 证据；不能把 `ready-for-agent` 当作完成状态。

wayfinder 探索子票的 `claimed`/`resolved` 不属于分诊标签表，按 `issue-tracker.md` 的地图协议消费。

如需改用自有词表，编辑右列即可。
