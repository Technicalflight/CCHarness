# AGENTS.md

面向 AI 编码智能体的仓库约定。与 README.md 分工：README 讲「这是什么」，本文件讲「在这里怎么改代码」。

## 项目基础信息

- 项目类型：Tauri 2 桌面应用（BYOM 多模型 AI 工作台），AGPL-3.0。
- 技术栈：React 18 + TypeScript 5（strict）+ Vite 5 ｜ Rust（edition 2021）+ Tauri 2。
- 目录：前端 `src/`，Rust 后端 `src-tauri/src/`；包管理器 npm（锁文件 package-lock.json，CI 用 `npm ci`）。
- 源码模块地图见 README.md「源码地图」；`docs/` 被 .gitignore 排除，需要入库的文件不要放进去。

## 开发与构建命令

- 安装依赖：`npm install`（改动依赖时保持 package-lock.json 同步提交）。
- 桌面开发：`npm run tauri dev`。
- 前端构建（含类型检查，即验收）：`npm run build`；仅类型检查：`npm run typecheck`。
- 后端测试：`cd src-tauri && cargo test`（全绿是后端唯一验收标准；不要在文档里硬编码测试数量，会随迭代失真）。
- 改动 Cargo.toml 的版本或依赖后，跑一次 cargo 命令刷新 Cargo.lock 并一并提交。

## 测试规范

- Rust 测试写在各模块的 `#[cfg(test)]` 内（参考 `worktree.rs`、`commands.rs` 的 `goal_tests`、`skills.rs`）；新增可判定行为必须带测试。
- 禁止跳过、忽略或删除失败测试；修复链条固定为：复现 → 定位原因 → 最小修复 → 重跑同一测试。
- 前端无测试框架：`tsc --noEmit` 通过 + `vite build` 成功即验收。
- 报告完成时附测试名或构建输出摘要；「计划了」「应该通过」不构成证据。

## 代码风格

- TypeScript：strict 模式，无 ESLint/Prettier——风格向同目录现有文件看齐；组件用函数式 + hooks，跨视图状态用 zustand。
- Rust：结构体 `AppSettings` / `SessionMeta` 新增字段必须加 `#[serde(default)]`，旧配置文件才能无损加载。
- 注释解释「为什么」；提交信息用 `type(scope): description`（feat / fix / docs / refactor / perf / test / chore / ci）。

## 操作边界

✅ 必须做

- 每次改动跑对应验收命令（上节），一个逻辑变更一个提交。
- 用户可见的行为变更（如新增功能、界面文案）同步更新 README.md 与 `site/guide/` 对应页面。
- 推送前 `git log --format='%an <%ae>' -1` 确认作者身份无本机信息泄露。

⚠️ 先读文件头注释、必要时先说明方案再改

- `src/prefix.rs`、`src-tauri/src/chat.rs`：请求体由预序列化片段按字节拼接，Zone S / Zone H 必须字节稳定——任何「看似等价」的重构都可能打破上游前缀缓存。
- 新增工作流模式：读取端 `workflow_of`、写入端 `set_workflow_mode` 白名单、`chat.rs` 指令注入、前端 `WF_LABEL` / `WF_ICON` / 工作流菜单 / Composer 选项，多处必须同步。
- 发版打 `v*` tag：版本号三处（`package.json`、`src-tauri/tauri.conf.json`、`src-tauri/Cargo.toml`）+ `.github/workflows/release.yml` 的 releaseBody + `site/index.html` 版本号须一致；推 tag 会触发三平台构建并自动发布 Release。

🚫 绝对禁止

- 修改 `.github/workflows/` 的 `permissions:` 段与依赖版本声明。
- 提交密钥 / 令牌 / .env、node_modules、dist、target。
- `git push --force`，以及未经用户明确要求的任何 push。
