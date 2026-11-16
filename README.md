# 俺寻思🤔 · I`think🤔

一个独立、内核式的 Agent harness。**轻、快、模块化；内核提供机制，用户态决定策略。** 代码标识使用 `ithink`，包名暂为 `ithink-kernel`，尚未发布到包仓库。

## 当前状态

**0.1 原型已实现，尚非生产版本。** 使用 Rust，核心不依赖模型协议、会话、记忆或 Router/Worker 编排。命令行演示仍使用假能力；可选 Web 工作台已支持配置 OpenAI 兼容 API 并真实对话。首次启动不自动联网，填好设置后才调用所选模型，也可手动切换离线演示。

| 部分 | 已实现 |
|---|---|
| 通信调度器 | 主题订阅、顺序编号、直接事件投递，有界进程消息队列 |
| 主体管理 | AID、0–3 工作层级、显式能力、父子归属，连接身份不可自报 |
| 任务管理器 | 任务归属、显式初始上下文、查询、完成、子树取消 |
| 上下文 | 原文历史 + 当前视图、压力通知、带版本检查的替换、配对与固定项校验、回放 |
| 插件装载 | 独立进程、版本握手、依赖检查、运行时加载/替换/卸载，失败保留旧实例 |
| 系统桥 | 宿主按服务/操作授予能力，后端由插件提供，原生结果/错误原样封装返回 |
| Agent Loop（用户态） | 模型 → 工具 → 结果回写 → 继续/结束；多任务驱动、取消、步数限制、压缩暂停与恢复 |
| Web 工作台（可选宿主） | 侧栏会话、多轮聊天、输入/停止、折叠工具过程；配置端点/模型/密钥、真实连接测试与 API 调用 |

## 开始运行

通用 Rust 环境（1.85 或更新）：

```sh
cargo test --all-targets
cargo build --bins
cargo run --bin ithink-demo
cargo run --bin ithink-loop-demo

# 可选 Agent 工作台：http://127.0.0.1:4173
cargo build --features web --bins
cargo run --features web --bin ithink-web
```

本工作区已装有**项目局部** Rust 工具链，没有修改系统 PATH。在 Windows 上可直接：

```powershell
.\dev.ps1 test
.\dev.ps1 lint
.\dev.ps1 fmt
.\dev.ps1 demo
.\dev.ps1 loop-demo
.\dev.ps1 web
.\dev.ps1 release
```

需要 Windows MSVC 构建工具。本机初次链接时系统临时目录导致 `LNK1104`，辅助脚本将构建临时目录设在项目内解决；它不修改系统环境配置。

如果本机 PowerShell 拒绝直接执行辅助脚本，可仅对此次进程运行：

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\dev.ps1 test
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\dev.ps1 demo
```

这不会更改全局执行策略。

演示会：启动一个后端插件 → 请求它 → 热替换成新版本 → 再次请求 → 卸载。进程集成测试还覆盖主代理通过公开 API 启动工作代理、显式传递初始上下文和接收结果。

## 结构

```text
src/
  api.rs             公共类型与请求
  kernel.rs          三个部件的状态机、权限、系统请求路由
  context.rs         上下文原文日志、活动视图、替换与回放
  protocol.rs        有大小限制的逐行 JSON 协议
  plugin.rs          内核启动插件与握手
  runtime.rs         事件循环、热加载、宿主 API
  process_tree.rs    Windows Job / Unix 进程组清理
  sdk.rs             只使用公开协议的有界轮询 Rust 插件客户端
  agent_loop.rs      可替换的用户态模型/工具循环状态机
  loop_plugin.rs     同一插件内多任务循环驱动
  web_host.rs        可选会话/聊天宿主，不是内核模块
  web_settings.rs    主机模型配置；持久化非密钥参数
  openai.rs          可选 OpenAI 兼容 Chat Completions 适配器
  bin/               演示宿主和用户态示例插件
 tests/
  process_plugins.rs 真正启动独立进程的内核集成测试
  agent_loop.rs      循环状态机与真实上下文回归测试
  agent_loop_process.rs 模型/工具/循环进程端到端测试
  web_host.rs        本机 HTTP 与任务 API 回归测试
  chat_api.rs        真实模型进程与本机 mock API 回归
  web_smoke.cjs      隔离宿主+mock API 的真实浏览器测试
web/                 无框架 Agent 聊天工作界面
```

- [设计草案（历史讨论，已修正适用范围）](<design/内核草案.md>)
- [首版实现约定与边界](<design/实现约定.md>)
- [插件 API、授权与热加载说明](<design/插件协议.md>)
- [Agent Loop 接口、运行方式与边界](<design/AgentLoop.md>)
- [Agent 工作台运行方式、API 设置与限制](<design/Web调试台.md>)
- [本机测试结果、演示和未验证项](<design/验证记录.md>)

## 已明确的边界

1. **进程插件不是安全沙箱。** 插件约定使用内核 API；恶意原生程序仍可能绕开 API 自行访问操作系统。Windows Job 和 Unix 进程组只做生命周期清理，不能当访问隔离。
2. **不内置压缩策略。** 内核通知压力、校验替换，失败不截断历史。计量单位由调用方提供，不能宣称是提供方精确 token 数，也不保证缓存命中。
3. **取消是协作式。** 任务结束会使关联待办请求失败，并通知后端取消；已发生的系统副作用不可回滚。无任务绑定的系统调用不会因为任意任务结束而被取消。
4. **热替换不自动迁移进程内状态。** 新实例获得新 AID；旧实例待办请求明确失败。新实例完成握手之后仍可能再次崩溃，握手不是永久健康保证。
5. **日志目前在内存中。** 单个上下文日志可序列化并确定回放，但还没有整个内核的磁盘恢复、日志轮换或常驻主代理自动重启。
6. 已在本机 Windows x64 测试。Unix 有条件编译实现，尚未在本机执行；冷启动、常驻内存和大规模负载仍需独立基准测试。
7. **许可证为 AGPL-3.0-or-later。** 见 [LICENSE](LICENSE)。它要求通过网络提供修改版服务时向用户提供对应源码；具体合规责任在部署方。Cargo 仍设置 `publish = false`，尚未发布到包仓库。

## 许可证

本项目以 **GNU Affero General Public License v3.0 或更新版本**（AGPL-3.0-or-later）发布，完整文本见 [LICENSE](LICENSE)。

```text
Copyright (C) 2026  ithink contributors

This program is free software: you can redistribute it and/or modify
it under the terms of the GNU Affero General Public License as published by
the Free Software Foundation, either version 3 of the License, or
(at your option) any later version.
```

分发修改版或把它作为网络服务对外提供时，AGPL 第 13 条要求让使用者能取得对应源码。这属于法律义务，不是本项目的技术限制；部署方需自行确认合规。
