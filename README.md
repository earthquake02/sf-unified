# 故事工坊

中文本地小说写作应用：无构建单文件程序页（`app/index.html`）＋Rust 本地服务与壳窗口（`app/server-rs/`，axum＋Tauri 同一进程单 exe），数据全在本机、BYOK 直连你自己的 AI 端点、离线可用。

## 快速开始

- **使用**：双击 `故事工坊.exe` → 首次在控制台页选一个数据文件夹 → 开始写作。关掉窗口即停。功能与用法见 [app/说明书.md](app/说明书.md)。
- **AI**：设置 → AI 服务 → 填任意 OpenAI 兼容端点的 Base 与 Key。Key 以 DPAPI 密文存在本机数据文件夹，不入任何导出。
- **开发核验**：`node tools/dev_page.mjs`（现有 exe 服务仓内页面＋隔离数据夹，页面改动刷新即生效）。

## 构建

```
node tools/release_rust.mjs
```

产出 `release/rust/故事工坊.exe`（Windows 单 exe，便携直发；macOS 不在本仓范围）。升级＝替换 exe，数据文件夹不动。

## 目录一览

| 位置 | 内容 |
|---|---|
| `app/` | 程序本体：`index.html`（单文件应用）、`server-rs/`（Rust 服务＋壳）、`说明书.md`（应用手册）、提示词 JSON |
| `docs/` | `agents/`（协作协议）、`adr/`（决策记录） |
| `tools/` | 构建与验证工具（本地化，不入 git） |
| `data/` | 运行数据（不入 git） |
| `release/` | 构建产物（不入库） |

更多：[AGENTS.md](AGENTS.md)（协作规则）、[CONTEXT.md](CONTEXT.md)（术语表）。

## 许可证

[MIT](LICENSE)
