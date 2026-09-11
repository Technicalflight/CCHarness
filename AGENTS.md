# AGENTS.md — CCHarness 仓库工作约定

## 项目速览

- Tauri 2 桌面应用：React 18 + TypeScript（strict）前端在 `src/`，Rust 后端在 `src-tauri/src/`。
- 常用命令：`npm run tauri dev`（开发）、`npm run build`（tsc + vite）、`npm run typecheck`、`cd src-tauri && cargo test`（后端测试，全绿是唯一后端验收标准）。

## 铁律（可判定，违反即缺陷）

- **字节稳定性**：修改 `src/prefix.rs`、`chat.rs` 或任何进入请求体组装路径的代码时，必须保持 Zone S / Zone H 字节稳定——请求体由预序列化片段按字节拼接，任何"看似等价"的改动都可能打破上游前缀缓存（有测试守护，改动前先读 `src/prefix.rs` 的模块注释）。
- **工作流门同步**：新增工作流模式时，以下位置缺一不可，漏一处就是运行时报"未知工作流模式"或界面缺入口：读取端 `workflow_of`、写入端 `set_workflow_mode` 白名单、`chat.rs` 指令注入、前端 `WF_LABEL` / `WF_ICON` / 工作流菜单与 Composer 选项。
- **配置向后兼容**：`AppSettings` / `SessionMeta` 新增字段必须加 `#[serde(default)]`，旧配置文件才能无损加载。
- **同文件编辑串行**：对同一文件的多处修改必须逐个进行，并行编辑会互相覆盖。
- **发布同步**：打版本 tag 前必须同步全部版本声明处（见下节），遗漏任何一处都会造成 Release 页版本与实际构建不一致。

## 发布规则（打 tag 前逐项核对）

版本号存在于 3 处构建面 + 2 个发布面，推 `v*` tag 会触发三平台构建并自动发布 GitHub Release（`.github/workflows/release.yml`），无需手动建 Release。tag 前必须全部同步并已提交：

1. `package.json` → `version`；
2. `src-tauri/tauri.conf.json` → `version`（应用内侧边栏与更新检查读这里）；
3. `src-tauri/Cargo.toml` → `version`（跑一次 `cargo check` 刷新 Cargo.lock 一并提交）；
4. `.github/workflows/release.yml` → `releaseBody` 顶部新增本版更新说明；
5. `site/` 官网 → 版本号与受影响章节（推送 main 后自动部署 GitHub Pages）；
6. `README.md` → 用户可见的行为变更（如新增工作流模式）同步更新。

## 禁止事项

- 禁止跳过或忽略失败的测试。修复链条固定为：复现 → 定位原因 → 最小修复 → 重跑同一检查；**无诊断的盲目重试不构成修复**。
- 禁止一次提交夹带无关改动（一个逻辑变更一个提交）。
- 禁止把密钥、令牌、本机绝对路径等敏感信息写入代码、配置、文档或提交信息。
- 禁止未经用户明确要求执行 `git push --force`；推送永远 opt-in，不推断。
- 禁止改动 `.github/workflows` 的权限配置（`permissions:` 段），最小权限是既定约定。

## 完成定义（Done when）

- [ ] 后端改动 `cargo test` 全绿；前端改动 `npm run build` 通过；两者都通过才算完成。
- [ ] 证据随行：报告完成时附测试名或构建输出摘要；「计划了」「应该通过」不构成证据。
- [ ] 涉及遥测 / 前缀链路的改动：确认不会污染请求账本（图像等无 usage 的调用不得写入 RequestStat）。
- [ ] 用户可见的行为变更：README 与 `site/guide/` 对应章节已先行同步。
- [ ] 提交信息说明改了什么、为什么；推送前 `git log --format='%an <%ae>' -1` 核对作者身份无本机信息泄露。

## 最终报告格式

完成任务后按此结构汇报：改动文件清单 → 验证证据（测试 / 构建输出摘要）→ 提交哈希 → 是否已推送 → 遗留风险或后续建议。
