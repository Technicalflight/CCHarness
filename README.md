<div align="center">

<img src="src-tauri/icons/128x128.png" width="96" alt="CCHarness logo"/>

# CCHarness

### 把多个大模型装进同一个桌面工作台

**自带模型 · 缓存是一级指标 · 智能体干活，掌控在你**

![Platform](https://img.shields.io/badge/Platform-Windows%20%7C%20macOS%20%7C%20Linux-blue)
![Tauri](https://img.shields.io/badge/Tauri-2-24C8D8?logo=tauri&logoColor=white)
![React](https://img.shields.io/badge/React-18-61DAFB?logo=react&logoColor=black)
![TypeScript](https://img.shields.io/badge/TypeScript-strict-3178C6?logo=typescript&logoColor=white)
![Rust](https://img.shields.io/badge/Rust-12k%2B%20lines-DEA584?logo=rust&logoColor=white)

</div>

> [!IMPORTANT]
> CCHarness 处于早期开发阶段（v0.1.x），数据格式与接口随时可能变化。本仓库当前为**私有项目**，未经作者许可请勿分发。

CCHarness 是一个自带模型（BYOM）的桌面工作台：一个界面管理多家 Provider，流式多会话对话，六种工作流模式按任务切换，真实智能体循环直接读写你的工作区——并且把**前缀缓存命中率**当作一级指标内置在遥测面板里。

---

## 下载安装

前往 [Releases](https://github.com/Technicalflight/CCHarness/releases) 页面下载对应平台的安装包（自 v0.1.0 起提供三平台构建）：

| 平台 | 文件 | 说明 |
|:--|:--|:--|
| Windows | `.exe`（NSIS 安装向导）或 `.msi` | x64 |
| macOS | `.dmg` | Apple Silicon + Intel 通用二进制 |
| Linux | `.deb` / `.rpm` / `.AppImage` | Debian 系 / 红帽系 / 免安装运行 |

安装包未做代码签名：Windows 首次运行可能出现 SmartScreen 提示（点「仍要运行」）；macOS 首次打开需在「系统设置 → 隐私与安全性」中放行。应用内置**更新检查**——点击侧边栏底部的版本号或在设置页开启启动自动检查，发现新版本会提示前往 Releases 下载。

## 为什么选择 CCHarness？

| | |
|:--|:--|
| 🧭 **桌面优先** | 原生桌面应用而非浏览器标签页：直接读写工作区文件、`@` 文件引用补全、系统级通知、`Ctrl+K` 命令面板，会话与配置全部留在本机。 |
| 💰 **缓存是一级指标** | 上游前缀缓存命中 token 约 1/10 价格。CCHarness 在**请求侧制造可缓存性**（而非在响应侧猜测相似性），命中率曲线、逐请求账本、miss 归因全部内置进遥测面板。 |
| 🔄 **工作流引擎** | 智能体 / 规划 / 目标 / 深度推理 / 生图 / 自定义状态机——六种模式一个下拉切换，权限边界、指令注入、工具面随模式自动收敛。 |
| 🔒 **Local-first** | 会话、API Key、记忆全部落在本机：Key 经系统级加密（Windows DPAPI / macOS 钥匙串 / Linux 密钥环），请求直连你配置的 Provider，不经过任何第三方中转。 |

## 从提示词到补丁

1. **描述任务** —— 在输入框直接说；`@` 引用工作区文件，拖入或粘贴图片多模态提问，草稿可一键「✨ 优化」为更清晰的提示词（幂等调用走本地缓存，不重复计费）。
2. **模型调用工具** —— 智能体模式拥有 16+ 工具（读 / 写 / 编辑 / 命令 / 网页 / MCP / 记忆 / 截屏 / 子代理委派）；写与改默认逐项审批，切换免审批档会弹出**风险确认对话框**（列明可能删除文件 / 清空数据等行为，勾选责任条款才能启用，档位常驻红色警示）。
3. **快照留痕** —— 每次 write / edit 保留前后快照，行级 diff 在审阅面板过目；完整工具调用记录（含被拒绝的调用）可回溯。
4. **你审阅，你决定** —— 批准或拒绝每一处改动；开启 Worktree 隔离后，所有写操作重定向到 git 工作树，主工作区在合并前不被触碰，随时整体丢弃重来。

---

## 不止一个聊天窗口

会话即工作流，一个下拉切换：

| 模式 | 行为 |
|---|---|
| 🤖 **智能体** | 完整工具面：读写文件、执行命令、委派子智能体 |
| 📋 **规划** | 只读调研，输出 ```plan 方案卡；批准前不修改任何文件，批准后自动切回智能体执行 |
| 🎯 **目标** | 只锁目标与验收标准：每轮输出 ```goal 清单（✅/⬜ + 可核验证据），全部达成输出 GOAL_DONE，前端有界自动继续 |
| 🧠 **深度推理** | ToT 思维树：三路候选方案并行生成，judge 模型评审择优后执行 |
| 🎨 **生图** | 走 `/images/generations` 端点的独立图像生成会话，模型列表自动过滤 |
| 🔀 **自定义状态机** | 可视化编辑的声明式工作流：每状态独立指令 + 工具面 + 条件分支 + 并行扇出，支持断点续跑 |

### 目标模式：对齐 Codex `/goal` 的完整状态机

- **五态生命周期**：`active → paused / achieved / unmet / budget_limited`，持久化进会话元数据，重启存活；
- **`/goal` 命令面**：`/goal <目标>` 创建 · `/goal` 查看摘要 · `/goal pause / resume / clear`；
- **工具化目标管理**：模型可调用 `get_goal / create_goal / update_goal` 三个工具自主读取与推进目标，但**只能**标记 `achieved / unmet`——暂停 / 恢复 / 清除是用户专属权限；
- **五段式目标模板**（Objective / Scope / Constraints / Done when / Stop if）内置于创建引导；
- **证据审计**：8 条完成判定规则——✅ 必须附可核验证据（文件路径 / 命令输出 / 测试名），代理信号不算数，存疑即 ⬜；
- **预算软停止**：会话成本达到上限时，下一次自动继续变为一次性收尾指令（完成手头原子操作 + 最终清单 + 总结），而非硬中断。

### 多智能体协作

- **竞技场**：一条提示词同时发给 N 个模型并行作答，泳道并排对比，逐轮 token / 缓存命中 / 成本一目了然；
- **圆桌群聊**：多成员顺序发言，主持人 LLM 负责调度发言顺序与话题收敛；
- **子智能体**：具名角色档案（独立 Provider / 模型 / 系统提示词），把独立调研 / 分析子任务委派给后台——独立上下文、实时进度流式显示、完整过程落盘可查；状态机工作流支持并行扇出多子代理会师。

### 右侧工作区面板

| 面板 | 能力 |
|---|---|
| 📁 文件 | 工作区文件树浏览与预览，写工具成功后自动刷新 |
| 🌿 Git | 暂存 / 提交 / 分支 / 远程 / 推拉完整闭环，行级 diff，无需离开应用 |
| 🌐 浏览器 | 内嵌实时预览本地 dev server，一键切换**手机视口模拟**（不重载页面） |
| ✅ 任务 | 智能体长任务的 `todo_write` 计划清单实时同步 |

另有会话置顶 / 归档分组、消息分支、外部会话导入、Markdown 导出、压缩上下文（70% 自动触发 + 手动）。

## 你的模型，你做主

CCHarness 不绑定任何模型厂商。内置适配 DeepSeek、智谱 GLM、Kimi、OpenAI、Anthropic、Ollama 本地模型，以及任意 OpenAI 兼容端点：连接测试、一键获取模型列表、按模型配置定价与上下文窗口；品牌图标内联 [@lobehub/icons](https://lobehub.com/icons) 官方 SVG。

## 扩展生态

- **MCP**：接入任意 Model Context Protocol 服务器，工具自动并入模型工具面，连接状态可视化管理；
- **Skills**：本地技能 + 内置技能市场（SkillHub），模型可用 `load_skill` 工具按需自主加载；
- **子智能体档案**：在「子智能体」页为不同角色配置专属模型与提示词；
- **注入防护**：外部内容（网页 / MCP 返回）默认经过 Guardrails 注入检测与数据围栏包裹，中英文注入短语库可自定义扩充。

## 记忆系统

- **向量长期记忆**：OpenAI 兼容 embeddings 端点 + 余弦相似度召回，按工作区隔离，FIFO 容量控制；
- **自动反思**：回合结束后自动提炼 0–3 条记忆写入向量库，全程静默；
- **提示词增强**：AuxMemo 精确缓存（L1 内存 LRU + L2 加密磁盘），相同草稿零 API 调用零计费。

## 评测

内置 Benchmark 视图：8 类基准用例（算术 / 逻辑 / 中文 / 指令 / 代码 / 概括 / 建议 / 常识），关键字判分 + 可选 **LLM-as-judge** 评审（通过 / 评分 / 理由），历史记录保留 50 轮。

## 请求侧缓存工程（本项目的差异化设计）

CCHarness 把智能放在**请求侧**：制造可缓存性，而不是在响应侧猜测相似性。每个出站请求按字节分三区：

```
Zone S  冻结区   system prompt，会话内一次写定，字节永不变化
Zone H  历史区   append-only：已发送内容以预序列化字节入史，任何组件不得改写
Zone T  尾区     本轮新增（用户消息），下一轮落入 H 成为稳定前缀
```

- 请求体由**预序列化片段按字节拼接**组装，保证 provider 看到逐字节相同的前缀——这是上游前缀缓存命中的前提；
- SHA-256 增量指纹链跟踪前缀演化；模型切换等事件开新**纪元**，遥测把「预期重建」与「意外劣化」分开统计；
- **miss 分歧定位**：provider 报 0 命中而本地链完整时，按请求形态自动归因（链断裂 → 前缀回退 → 尾区过大 → 上游丢失），附处置建议；
- **辅助调用精确缓存（AuxMemo）**：标题生成 / 提示词增强等幂等调用走 L1 + L2 缓存，命中不产生 API 调用与计费；
- **明确不做**：语义缓存、主循环响应重放、跨工作区共享。

## 安全护栏

| 层 | 机制 |
|---|---|
| 权限档位 | 只读 / 需审批 / 自动写入三级；自动写入需**风险确认对话框**（责任条款勾选），档位红色标识 |
| 审批卡 | 每次写操作弹出预览与 diff，120 秒超时即拒绝（fail-closed） |
| Worktree 隔离 | 写操作重定向 git 工作树，合并前主工作区零触碰 |
| 注入检测 | 外部内容围栏包裹 + 伪造围栏标记转义 |
| SSRF 防护 | 默认拒绝环回 / 内网 / 链路本地地址，本地模型服务需显式白名单 |

## Local-first

| 数据 | 存储 | 保护 |
|---|---|---|
| 会话 | 本机应用数据目录 | 原子写入（tmp + rename），损坏自动备份重建 |
| API Key | 本机应用数据目录 | 系统级加密落盘：Windows DPAPI / macOS 钥匙串 + AES-GCM / Linux 密钥环（Secret Service）+ AES-GCM；密钥环不可用时降级明文并明确提示 |
| 辅助调用缓存 | L1 内存 LRU + L2 磁盘 | 工作区隔离、静态加密、8 MiB 有界 |
| 出站请求 | 直连你配置的 Provider | 不经过任何第三方中转 |

## 架构

```mermaid
flowchart LR
    UI["React UI<br/>会话 / 竞技场 / 工作流 / 遥测 / 审阅"] --> CMD["Tauri 命令层<br/>Rust"]
    CMD --> PREFIX["前缀状态机<br/>Zone S / H / T + SHA-256 指纹链"]
    CMD --> CHAT["Provider HTTP / SSE<br/>流式解析 · 用量与成本"]
    CHAT --> MODELS["DeepSeek / GLM / Kimi<br/>OpenAI / Anthropic / Ollama"]
    CMD --> TOOLS["工具执行<br/>审批门控 · Worktree · 快照 diff"]
    CMD --> MEMO["记忆系统<br/>向量库 · 反思 · AuxMemo"]
    CMD --> SESSIONS["会话持久化<br/>原子写入 · 目标状态机"]
    CMD --> TELE["遥测与账本<br/>命中率 · 成本 · miss 归因"]
```

技术栈：Tauri 2 + React 18 + TypeScript（strict）/ Rust 后端约 12,000 行 + 前端约 11,700 行，98+ 个后端单元测试覆盖前缀字节稳定性、SSRF、Git 面板、worktree 往返等核心链路。

## 开发

环境要求：Node ≥ 22、Rust ≥ 1.77、Windows / macOS / Linux。

```bash
npm install          # 前端依赖
npm run tauri dev    # 开发模式（自动起 vite + cargo）
npm run tauri build  # 打包安装程序
```

单独验证：

```bash
npm run typecheck    # tsc --noEmit
npm run build        # tsc + vite build
cd src-tauri && cargo test    # 后端单元测试
node scripts/gen-icons.mjs    # 重新生成图标
```

目录结构：

```
src/                    React 前端
  components/           Sidebar / Composer / 面板 / 命令面板 / 宠物
  views/                会话 / 竞技场 / 工作流 / 子智能体 / 评测 / 遥测 / 设置
  lib/                  Tauri API 封装、格式化、配色、图标
  store.ts              zustand 全局状态
src-tauri/              Rust 后端
  src/prefix.rs         三区前缀状态机 + 指纹链（含字节稳定性测试）
  src/chat.rs           Provider HTTP / SSE / 图像生成 / 用量与成本
  src/commands.rs       Tauri 命令层（会话编排 / 工具循环 / 目标状态机）
  src/auxmemo.rs        辅助调用精确缓存（L1 LRU + L2 加密磁盘 + 账本）
  src/memvector.rs      向量长期记忆（embeddings + 余弦召回）
  src/guard.rs          注入检测（Guardrails）
  src/urlguard.rs       SSRF 防护（含测试）
  src/worktree.rs       git worktree 隔离生命周期
  src/git_panel.rs      Git 面板命令（暂存/提交/分支/远程）
  src/bench.rs          Benchmark + LLM-as-judge
  src/sessions.rs       会话持久化 + Markdown 导出
```

## 项目状态

- 当前版本 v0.1.1，处于快速迭代期，更新日志见 [Releases](https://github.com/Technicalflight/CCHarness/releases)；
- v0.1.1 新增：应用内更新检查（侧边栏版本号 / 启动自动检查）、macOS / Linux 下的 Key 等价保护（钥匙串 / 密钥环保存主密钥 + AES-256-GCM 加密落盘）；
- 路线图：RAG 知识库管线、A2A 协议接入。

## Built on open source

感谢 [Tauri](https://tauri.app)、[React](https://react.dev)、[zustand](https://zustand.docs.pmnd.rs)、[react-markdown](https://github.com/remarkjs/react-markdown)、[@lobehub/icons](https://lobehub.com/icons) 与 Rust 生态的众多 crate。

本项目在 AI 模型协作下开发。

## 许可

Copyright (c) 2026 CCHarness Authors. All rights reserved.

本项目暂未选择开源许可证，**保留所有权利**：未经作者书面许可，不得复制、修改或分发本仓库内容。
