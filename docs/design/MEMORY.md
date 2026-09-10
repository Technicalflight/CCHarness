# Memory Index

- [插件系统开发计划](plugin-system-plan.md) — .ccplug 包格式与 plugin.json 清单、声明式三类权限（mcp/network/workspace）、目录布局与生命周期、P0–P5 分阶段（内核→分发→管理 UI→前端扩展点→签名）、M1–M4 里程碑
- [CCHarness 缓存设计方向](cache-hit-mechanism.md) — 前缀纪律为地基 + 收窄的精确缓存：三区模型、DigestChain、BoundaryCompactor、AuxMemo 白名单，M1–M4 实施顺序
- [AuxMemo v2 与提示词增强](auxmemo-and-enhance.md) — M4 落地：L1 LRU + L2 磁盘加密缓存（workspaceNS 隔离）、origin/billed/saved 分层账本、Composer 提示词增强与 Telemetry 账本的页面功能设计
