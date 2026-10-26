# 入门功能卡附图（app/assets/guide）

## 命名
- 文件名：`guide-<卡id>.webp`（与 `GUIDE_GROUPS` 里卡片 `id` 对齐）
- 例：`guide-writing.webp`、`guide-ai-service.webp`

## 卡上字段
- `fig`：必填，灰底占位时的说明文字（已有）
- `img`：选填，相对本目录的文件名，如 `guide-writing.webp`；空则继续画占位

## 运行时路径
- 开发／热开：服务端提供 `GET /assets/guide/<文件名>`（只允许本目录内 webp/png/jpg）
- 进包：构建需 `--add-data app/assets/guide;assets/guide`（或等价），frozen 从 `_MEIPASS/assets/guide` 读
- 本目录勿放无关大图；优先 WebP，单张目标约 80～150 KB

## 勿提交
- `_raw/` 未压缩原稿请放 `.scratch/入门优化/图/_raw/`，不要进 `app/assets/guide`
