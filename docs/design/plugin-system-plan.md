# CCHarness 插件系统开发计划

> 版本：v1（2026-09-09）· 状态：规划
> 原则：**声明式优先、无任意代码执行、复用既有扩展点**。插件是"打包 + 分发 + 统一管理"，不是新的运行时。

---

## 1. 现状盘点（插件系统要统一的东西）

| 已有扩展点 | 形态 | 能力 | 缺口 |
|---|---|---|---|
| 技能 Skills | `~/.ccharness/skills/*.md` + 项目级覆盖 | inject=auto 注入 Zone S；inject=command → `/命令` | 单文件、无依赖、无版本、市场只能装技能 |
| MCP 服务 | stdio / http，`mcp__<id>__<tool>` 命名空间 | 工具调用 + 审批卡 + 信任门控 | 手动逐个配置，与技能/市场割裂 |
| 技能市场 | skillhub 远端列表 → 写入 skills 目录 | 拉取/安装/计数 | 只支持技能，没有包结构与元数据 |
| 智能体工具 | 内置只读/写工具 + 审批 | 写日志、审阅 diff | 不可扩展新工具 |
| 前端界面 | 固定视图 + 命令面板 | — | 无 UI 扩展点 |

**定位**：插件 = 一个版本化的 zip 包（`.ccplug`），内含清单 + 若干可声明产物（技能、MCP 服务、命令、设置、UI 挂载点），由宿主统一安装/启停/卸载/更新。

---

## 2. 插件包格式（`.ccplug`）

```
my-plugin.ccplug (zip)
├── plugin.json            # 清单（唯一可信来源）
├── skills/*.md            # 可选：技能（沿用现有 frontmatter 规范）
├── mcp/*.json             # 可选：MCP 服务声明
├── assets/                # 可选：图标 / README
└── CHANGELOG.md           # 可选
```

### plugin.json 清单

```json
{
  "id": "com.example.dbtools",       // 反域名式，全局唯一，安装目录名
  "name": "DB Tools",
  "version": "1.2.0",                // semver
  "description": "数据库常用工具集",
  "author": "example",
  "engine": "^0.1",                  // 宿主版本兼容范围
  "permissions": {                   // 声明式权限（P1 起强制展示）
    "mcp": true,                     // 是否注册 MCP 子进程
    "network": ["https://api.example.com/*"],  // 网络白名单（接 urlguard）
    "workspace": "project"           // none | project：可访问的技能/文件层级
  },
  "contributes": {
    "skills": ["skills/*.md"],
    "mcp": ["mcp/db.json"],
    "commands": ["/dbquery"],        // 由 skills(inject=command) 自动生成，此处仅声明展示
    "settings": [                    // 插件设置项（渲染进设置页）
      { "key": "defaultLimit", "type": "number", "default": 20, "title": "默认查询条数" }
    ]
  }
}
```

**硬约束（与现有架构对齐）**：
- 技能正文 8KB / 总量 32KB 上限沿用 `skills.rs` 现值；
- MCP 服务安装后**默认 disabled + untrusted**，首用必须走现有信任/审批门控；
- 网络访问走现有 `urlguard` 白名单逻辑，声明之外一律拒绝；
- 清单校验失败 = 拒绝安装（fail-closed）。

---

## 3. 安装目录与生命周期

```
~/.ccharness/
├── plugins/
│   └── com.example.dbtools/
│       ├── plugin.json          # 解包后的清单
│       ├── skills/...
│       └── .installed.json      # 安装元数据（时间/来源/校验和）
├── plugins.d/registry.json      # 启用状态 + 插件设置值
└── skills/                      # 技能目录不变——插件技能以符号链接/挂载方式并入扫描
```

生命周期：`install（解包+校验）→ enable / disable（秒级，不动文件）→ uninstall（整目录删除）→ update（版本号比对 + 覆盖安装）`。

**冲突规则**：插件技能与手动技能同名时，优先级 = 项目技能 > 插件技能 > 全局技能；两个插件同名技能按 id 字典序，UI 提示冲突。

---

## 4. 分阶段计划

### P0 · 设计定稿（0.5 天）
- [ ] 清单 schema 字段冻结（本文档 §2）
- [ ] 权限模型定稿（mcp / network / workspace 三类，声明式，无可执行权限）
- [ ] 冲突与优先级规则评审

### P1 · 内核：插件注册表（Rust，2~3 天）
- [ ] 新模块 `plugins.rs`：manifest 解析 + 校验（必填字段、semver、路径穿越检查、glob 白名单）
- [ ] `install/uninstall/enable/disable`：zip 解包（复用现有 `zip` crate）、校验和记录、原子写入（临时目录 + rename）
- [ ] 技能并入：`skills::scan()` 扩展扫描 `plugins/*/skills`（respect enabled 状态与优先级）
- [ ] MCP 并入：`mcp_status`/配置加载时把 enabled 插件的 `mcp/*.json` 合入服务列表（默认 disabled）
- [ ] Tauri 命令：`plugin_list / plugin_install_zip / plugin_toggle / plugin_uninstall / plugin_get_settings / plugin_set_settings`
- [ ] 单测：清单校验、路径穿越、冲突解析

### P2 · 本地安装 + 市场扩展（1~2 天）
- [ ] 本地安装入口：文件选择器（dialog 插件 file 模式）+ 拖拽导入
- [ ] skillhub 后端：列表项支持 `type: skill | plugin`，插件条目返回 `.ccplug` 下载地址 + 版本
- [ ] 更新检查：启动时对比远端 latest 与本地版本（网络走 urlguard）

### P3 · UI：插件管理页（2 天）
- [ ] 侧边栏新增「插件」视图（或并入设置页一级 Tab）：
  - 已安装列表（图标/名称/版本/来源/启用开关）
  - 权限摘要展示（mcp/network/workspace 徽章）
  - 卸载（二次确认）/ 更新按钮
- [ ] 市场页加「插件」Tab：卡片式列表 + 安装进度
- [ ] 插件设置项渲染进管理页（declared settings → 表单）

### P4 · 前端扩展点注册表（2~3 天，可选先行）
- [ ] 前端 `pluginRegistry`：插件可声明 UI 挂载点
  - `preview-panel.tab`：向功能区预览面板贡献自定义标签页（iframe 指向本地服务或内置页面）
  - `command-palette.entry`：命令面板条目
  - `composer.slash`：/命令补全面板分组展示
- [ ] 挂载点按 capability 粒度授权：只有 enabled 插件的声明被渲染；无任意 JS 注入（iframe + 声明式配置）

### P5 · 生态与安全强化（持续）
- [ ] 插件签名（ed25519）+ 校验失败降级提示
- [ ] 引用计数审批：MCP 子进程首次调用仍走审批卡（现状已满足）
- [ ] 插件健康度：市场侧安装数/评分；本地崩溃/报错上报开关

---

## 5. 里程碑与验收

| 里程碑 | 内容 | 验收标准 |
|---|---|---|
| M1（P0+P1） | 内核可用 | 手动放一个 .ccplug 到安装入口，技能出现在 / 补全，MCP 服务出现在 MCP 页且默认关闭 |
| M2（P2） | 可分发 | 从市场一键安装/更新插件；本地 zip 拖入安装成功 |
| M3（P3） | 可管理 | 全生命周期（装/启停/卸载/改设置）全程 UI 完成且即时生效 |
| M4（P4） | 可扩展 UI | 至少 1 个示例插件贡献预览面板标签页 |

## 6. 风险与对策

1. **权限蔓延**：坚持声明式三类权限，拒绝"任意代码"扩展点；UI 扩展仅 iframe/配置。
2. **提示词膨胀**：插件技能沿用 8KB/32KB 上限 + Zone S 分区，市场侧标注注入体积。
3. **供应链**：默认 untrusted + 审批门控 + 网络白名单 + 后续签名。
4. **复杂度失控**：P4 之前不引入任何脚本运行时；先跑通"包 = 声明集合"的完整闭环。

## 7. 待定问题（需要拍板）

- [ ] 插件是否允许贡献**新的智能体内置工具**（当前方案：不允许，一律走 MCP 子进程）？
- [ ] 市场插件是否需要审核/白名单，还是开放上传？
- [ ] 引擎兼容（`engine` 字段）的版本粒度：minor 即可还是需要精确补丁号？
