# Agent Web 工作台

日期：2026-11-15。独立的「俺寻思🤔」用户态宿主，不修改 DSH Web GUI，不把会话和模型策略写进内核。

## 运行

在项目目录：

```sh
cargo build --workspace --release --bins
./target/release/ithink-web --port 4173
```

Windows 局部工具链：

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\dev.ps1 web
```

打开 **http://127.0.0.1:4173**，点击左下角 **设置**：

1. 选择 **API 模型**。
2. 填 Endpoint，例如 `https://api.deepseek.com`，或服务商完整的 `/chat/completions` 地址。
3. 填模型名称和 API Key。DeepSeek 当前文档模型为 `deepseek-flash`（默认）和 `deepseek-v4-pro`。
4. 可选点 **测试连接**，它会发一次最多 32 输出 token 的真实请求，可能产生少量费用。测试不保存；测试后新输入的密钥会清空，保存前需重新填写。
5. **保存并应用**，关闭设置后直接聊天。

默认不会调用模型，也不会偷偷回退到假模型。没有密钥的本地 OpenAI 兼容服务可使用回环 HTTP 地址；非本机服务必须 HTTPS。当前支持 **Chat Completions 流式 SSE**；若兼容服务忽略 `stream` 并返回完整 JSON，适配器会自动回退到非流式解析。不是 Anthropic Messages、OpenAI Responses 或任意私有模型协议。

可直接运行 [发布版宿主](<../target/release/ithink-web.exe>)；旁边需要 [真实模型适配插件](<../target/release/ithink-model-provider.exe>) 和 [循环/工具/离线示例插件](<../target/release/ithink-loop-fixture.exe>)。

启动参数：

- `--port 4173`：仅监听 `127.0.0.1`。`--port 0` 自动选择空闲端口，第一行输出确切 URL。
- `--offline`：明确选择离线演示，不使用真实模型；假模型回显也会按块经 SSE 投递，便于观察流式界面。
- `--data-dir PATH`：非敏感设置目录；默认当前目录下 `.ithink`。
- `--no-persist`：设置仅内存，用于测试。

不要把服务开放到公网或局域网。端口被占用会启动失败，不接管别人的进程。

## 工作界面

- 左侧新建/切换对话；中间多轮消息与回答；底部输入和停止。
- 同一对话一次运行一条消息，不同对话可以并行。
- Enter 发送，Shift+Enter 换行；中文输入法组合状态不会误发送。
- 对话草稿独立保留，轮询不覆盖输入或抢焦点。
- 工具过程、上下文和模型返回的推理字段默认折叠；不伪造推理过程。
- 顶部显示所选模型和本机连接状态；移动端侧栏为抽屉。
- 模型回答和推理内容通过同源 SSE `/api/stream` 实时追加；最终状态仍以任务轮询为准。
- 连接中断显示错误并重试，不清空草稿和最后状态。

离线模式的回显文本会按块流式显示，但它不是模型生成。当前真实工具仅有无副作用的整数加法 `add`。模型可选择调用它，结果回注后继续生成。**还不是拥有文件/命令/浏览器权限的通用编码 Agent。**

同一会话接续最近一次已完成任务的原始上下文，不重复拼接历史；失败/取消任务不作为下一回合的隐式成功记录。上下文超限不会静默截断或自动生成摘要。

## 模型与参数

按 2026-11 的 DeepSeek 文档，当前模型均为 1M 上下文、最大 384K 输出：

| 模型 | 说明 |
|---|---|
| `deepseek-flash` | 默认；支持非思考/思考模式与图像输入 |
| `deepseek-v4-pro` | 支持非思考/思考模式；不支持图像输入 |

设置界面覆盖以下 DeepSeek Chat Completions 参数；未填写的字段不发送，交回服务端默认值：

- **思考模式 `thinking`**：自动（不发送，服务端默认开启）/ 开启 `{"type":"enabled"}` / 关闭 `{"type":"disabled"}`。其他 OpenAI 兼容服务建议保持自动。
- **思考强度 `reasoning_effort`**：留空、`low`、`high`、`max`；关闭思考时不发送。`minimal`/`medium`/`xhigh` 由服务端映射。
- **`temperature`**：0–2，思考模式下不生效。
- **`top_p`**：大于 0 且不超过 1；各模型在思考/非思考模式下的生效规则不同，以文档为准。
- **`max_tokens`**：1–393216，留空使用服务端默认（非思考 8K，思考 64K，`max` 强度 128K）。
- **`stop`**：最多 16 个停止序列，每行一个。
- **`response_format`**：普通文本或 JSON 对象；JSON 模式仍需在系统提示中要求输出 JSON。
- **`safety_check`**：可选开启基础风控 `high`。
- **工具调用**：由 Agent Loop 提供 `tools`，适配器固定发送 `tool_choice: "auto"`；思考模式下不使用 `required` 或指定工具，避免文档所述的 400。

`reasoning_content` 只在思考模式返回。界面默认折叠推理内容；带 `tools` 的后续请求按文档要求回传历史 `reasoning_content`，不带 `tools` 时省略。模型默认按流式请求；服务端忽略流式时自动回退完整 JSON。

本地传输限制仍然存在：单帧上限 1 MiB，模型请求和响应各自上限 1 MiB，Web 任务上下文默认容量 131072 units。因此 1M 上下文和 384K 输出是服务端模型能力，不是本工作台当前能完整承载的窗口；超出本地限制会明确报错，而不是静默截断。

## 配置与密钥

- Endpoint、模型、系统提示及采样/思考参数持久化到本机设置目录；保存采用同目录临时文件 + 原子替换。
- **API Key 只在服务及模型进程内存中。** 不写入设置文件、浏览器 localStorage/sessionStorage、任务上下文或内核日志；重启服务后需重填。
- 设置 API 只返回 `api_key_set`，不回显真实密钥。空密钥表示保留当前密钥，显式勾选清除才删除。
- **更换端点的 origin（协议、主机或有效端口）会清除旧密钥。** 防止“测试新地址”时把旧提供商密钥发给新站点；新服务需明确输入新密钥。
- 密钥通过一次性私有 stdin bootstrap 交给模型进程，不放 argv/env 或公开插件调用中；bootstrap 的 Debug 输出隐藏内容。
- HTTP 错误只返回安全的状态描述，不反射服务商错误正文、请求 URL 或 Authorization；不跟随重定向。
- 模型请求可取消：停止会中止 HTTP future、关闭连接并回收本机并发名额。**不能保证提供商已经生成的内容不计费，也不能撤销外部副作用。**
- 运行中不允许切换设置，需先停止或等待完成。连接测试最多占用一个名额，最长 10 秒。

这些保护不等于操作系统凭据保险箱或恶意插件沙箱。不要把真实密钥写进聊天内容、系统提示、端点路径或日志。

## 分层

- [页面](<../web/index.html>)、[样式](<../web/style.css>)、[脚本](<../web/app.js>)：无框架原生 HTML/CSS/JS，无 CDN。
- [HTTP 入口](<../demos/web/src/main.rs>)：静态资源、受限 API、同源检查、退出信号。
- [聊天宿主](<../demos/web/src/web_host.rs>)：会话和草稿以外的服务状态、任务归属、多轮初始上下文；独立线程驱动 Runtime。
- [设置管理](<../demos/web/src/web_settings.rs>)：非敏感配置持久化及密钥 origin 绑定。
- [模型适配器](<../ithink-core/src/userland/openai.rs>)：提供商消息转换、HTTP/TLS、响应与工具调用校验。
- [模型插件](<../plugins/model-provider/src/main.rs>)：独立进程，最多 8 个异步模型请求，按公开 API 返回提供商中立 ModelReply。

`openai` 是 core 的可选 feature；Web 宿主是独立包。默认 core 构建不引入 reqwest/Tokio。静态资源编译进二进制，修改后需重新构建并重启，不是热更新服务。

## API

| 方法 | 路径 | 含义 |
|---|---|---|
| GET | `/api/health` | 服务存活及当前模式 |
| GET | `/api/state` | 会话、各回合、任务、步骤、上下文、插件状态 |
| GET | `/api/stream` | 同源 SSE：模型内容与推理增量（不进入任务上下文） |
| GET | `/api/settings` | 不含密钥的当前配置 |
| POST | `/api/settings` | 校验、保存并应用配置 |
| POST | `/api/settings/test` | 测试候选配置，不保存 |
| POST | `/api/conversations` | `{}` 新会话 |
| POST | `/api/conversations/{id}/messages` | `{ "content": "..." }` |
| POST | `/api/tasks/{id}/cancel` | `{}` 停止任务 |
| POST | `/api/shutdown` | `{}` 优雅退出并清理插件 |

旧 `/api/tasks` 保留作离线回归场景测试，不是前端主接口。

写操作必须携带精确 Origin（例如 `http://127.0.0.1:4173`）和 `Content-Type: application/json`。浏览器自动提供 Origin；脚本显式添加。没有开放 CORS。静态入口允许从其他页面点链接打开，API 同源保护不因此放宽。

## 已知边界

- 会话/消息/草稿当前在内存；刷新页面后服务消息仍在，**重启服务会清空**。不是持久化聊天数据库。
- 单条消息最多 2000 Unicode 字符，一次进程最多 128 会话/256 消息。
- 模型文本/推理通过 SSE 实时显示，最终状态、工具事件和上下文仍由 700ms 轮询确认。SSE 是尽力而为的实时通道，断线会自动重连；最终以任务完成状态为准。
- SSE 连接会占用一个本机 HTTP 工作线程；默认 8 个线程，适合单机少量页面。增量事件不写入内核 journal，也不进入模型上下文。
- 最大 HTTP 头 8 KiB、正文 128 KiB；拒绝重复头、chunked、流水线和 Expect。读取绝对期限 3 秒、响应写入期限 1 秒，有界线程/连接队列。
- 模型响应最多 1 MiB；输出截断、错误工具格式、未知工具等返回明确失败，不执行半截工具调用。
- 没有账号、TLS 服务端、用户隔离、插件市场、ASR/TTS 或附件上传。仅适合本机原型，不是生产 Web 服务。

## 测试

```sh
cargo test --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Web 集成测试会启动真实宿主，并依赖同一 `target` 目录下的 `ithink-loop-fixture` 与 `ithink-model-provider`。请使用上面的 `--workspace` 命令，不要只跑 `-p ithink-web`。

可选浏览器测试：

```powershell
npm.cmd install --prefix .tools/web-test --no-save --package-lock=false --ignore-scripts playwright-core
node demos/web/tests/web_smoke.cjs
```

[测试脚本](<../demos/web/tests/web_smoke.cjs>) 自动启动隔离的临时宿主与本机 mock API，不触碰用户当前预览、不调用真实外部模型。默认本机 Chrome，可设 `ITHINK_BROWSER` 为 Edge 路径，`ITHINK_WEB_BIN` 指定被测二进制。浏览器依赖/截图位于忽略的本地工具目录。
