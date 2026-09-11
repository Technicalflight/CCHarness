# AGENTS.md — CCHarness 仓库工作约定

## 项目速览

- Tauri 2 桌面应用：React 18 + TypeScript（strict）前端在 `src/`，Rust 后端在 `src-tauri/src/`。
- 常用命令：`npm run tauri dev`（开发）、`npm run build`（tsc + vite）、`npm run typecheck`、`cd src-tauri && cargo test`（后端测试）。

## 铁律（可判定，违反即缺陷）

- **字节稳定性**：修改 `src/prefix.rs`、`chat.rs` 或任何进入请求体组装路径的代码时，必须保持 Zone S / Zone H 字节稳定——请求体由预序列化片段按字节拼接，任何"看似等价"的改动都可能打破上游前缀缓存（有测试守护，改动前先读 `src/prefix.rs` 的模块注释）。
- **工作流门同步**：新增工作流模式时，读取端（`workflow_of`）与写入端（`set_workflow_mode` 白名单）必须同步修改，漏一端就是运行时报"未知工作流模式"。
- **配置向后兼容**：`AppSettings` / `SessionMeta` 新增字段必须加 `#[serde(default)]`，旧配置文件才能无损加载。
- **同文件编辑串行**：对同一文件的多处修改必须逐个进行，并行编辑会互相覆盖。

## 完成定义（Done when）

- 后端改动：`cargo test` 全绿；前端改动：`npm run build` 通过；两者都通过才算完成，并附测试名或构建输出作为证据。
- 涉及遥测 / 前缀链路的改动：额外确认不会污染请求账本（图像等无 usage 的调用不得写入 RequestStat）。
