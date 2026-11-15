# gpt2apirs

把 ChatGPT 网页版的能力（文本对话、生图、图生图、联网检索）转成 OpenAI / Anthropic 兼容接口，
自带账号池、配额同步与失败换号。适合给自建网关（如 New API）当上游用，也可以直接调用。

- **OpenAI 兼容**：`/v1/chat/completions`、`/v1/responses`、`/v1/images/generations`、`/v1/images/edits`、`/v1/models`
- **Anthropic 兼容**：`/v1/messages`、`/v1/messages/count_tokens`
- **真流式**：文本 SSE 边收边转，首字延迟不等于总耗时
- **账号池**：导入去重、OAuth 授权、用 refresh_token 续期、同步上游配额、按失败类型决定换不换号
- **管理后台**：总览 / 账号 / 日志 / 代理 / 图片 / 设置 / 密钥 / 备份

MIT · Rust 2024 · axum · PostgreSQL · BoringSSL

---

## 接口一览

| 接口 | 说明 |
|---|---|
| `POST /v1/chat/completions` | 文本对话，支持 `stream: true` 与 `n > 1` |
| `POST /v1/responses` | OpenAI Responses 格式，流式/非流式 |
| `POST /v1/messages` | Anthropic Messages 格式，流式/非流式 |
| `POST /v1/messages/count_tokens` | tiktoken 真实计数，与实际上报口径一致 |
| `POST /v1/images/generations` | 生图，`n > 1` 并行（多账号各扣一次配额） |
| `POST /v1/images/edits` | 图生图/改图，支持 mask |
| `GET /v1/models`、`GET /v1/models/{id}` | 只列真正支持的模型 |
| `POST /v1/search` | 联网检索 |
| `GET /health`、`GET /health/ready`、`GET /version` | 存活 / 就绪（DB 不可用返回 503）|
| `GET /api/*`、`GET /admin` | 管理后台（后台会话或 `ADMIN_KEY`）|

模型：文本 `gpt-5-5`，图片 `gpt-image-2`。上游还有 `codex-*` 系列，本项目未实现。

---

## 部署

### 1. 准备

依赖：Rust（nightly）、PostgreSQL 18、NASM（编译 BoringSSL 用）。

```bash
cp .env.example .env   # 然后改成你自己的值
```

### 2. 编译

```bash
# Linux / macOS：先装 cmake、ninja、nasm
cargo build --release --features tls
```

Windows 上 BoringSSL 对环境敏感（VS 生成器找不到 cl、Ninja 下把 cl 当汇编器），**直接用脚本**，它会把 vcvars、Ninja、NASM 和工具链文件都配好：

```powershell
powershell -File tools\build.ps1 -CargoArgs build,--release,--features,tls
```

> `--features tls` 不能省。不开启时编译出来的是存根客户端，第一次请求会报「未启用 tls feature」。

### 3. 配置

`.env` 必须与可执行文件**同目录**（例如 `target/release/.env`）。

| 变量 | 必填 | 说明 |
|---|---|---|
| `DATABASE_URL` | 是 | `postgres://user:pass@host:5432/db?sslmode=disable` |
| `AUTH_KEY` | 是 | 调用 `/v1/*` 的口令，至少 8 字符 |
| `ADMIN_KEY` | 否 | 管理后台登录口令；不填时沿用 `AUTH_KEY` |
| `HOST` / `PORT` | 否 | 默认 `0.0.0.0:8000` |
| `DATA_DIR` | 否 | 图片落盘目录 |
| `PUBLIC_BASE_URL` | 否 | 对外基础地址，用于拼图片 URL |

`AUTH_KEY` 和 `ADMIN_KEY` 是两套凭据：前者进 `/v1/*`，后者进 `/admin`，互相不能代替。

### 4. 起服务

```bash
./gpt2apirs
```

### 5. 第一条请求

```bash
curl http://127.0.0.1:8000/v1/models -H "Authorization: Bearer $AUTH_KEY"

curl http://127.0.0.1:8000/v1/chat/completions \
  -H "Authorization: Bearer $AUTH_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model":"gpt-5-5","messages":[{"role":"user","content":"你好"}]}'
```

账号通过管理后台「账号」页导入（token 或 OAuth 授权），导入后会自动同步上游配额。

---

## 运行设置

管理后台「设置」页，或 `PUT /api/settings`。改完即时生效，不用重启。常用几项：

| 键 | 作用 |
|---|---|
| `refresh_account_interval_minute` | 账号状态轮询间隔 |
| `image_max_account_attempts` / `text_max_account_attempts` | 失败换号的最多尝试次数 |
| `image_account_concurrency` / `account_processing_concurrency` | 单账号 / 全局并发 |
| `image_task_concurrency` | 异步生图任务的并发 |
| `console_request_timeout_secs` | 单请求超时 |
| `chat_cache_ttl_secs` | 相同请求的结果缓存时长 |
| `log_retention_hours` / `image_retention_hours` | 日志与图片保留时长 |
| `chat_blocked_keywords` / `rich_text_policy` | 请求侧关键词拦截、富文本标签处理 |

越界值会被钳到合法区间，未知键直接拒收。

---

## 失败分类

上游失败会归到固定几类，客户端拿到的状态码据此决定，不换号的类型也一并说明：

| 分类 | 客户端状态码 | 换号 |
|---|---|---|
| 请求本身有问题（参数、模型不存在、超长） | 400 | 否 |
| 内容策略拦截 | 400 | 否（换号也没用）|
| 配额 / 限流 | 429 | 是 |
| 账号凭证失效、需要人工验证 | 401 / 403 / 503 | 是（并标记账号）|
| 上游协议/网络异常、空回复 | 502 | 是 |
| 没有可用账号 | 503 | — |

---

## 已知限制

- **Turnstile 求解未经验证**：能对真实挑战解出 token 并交给上游，但上游目前不校验该 token，
  所以「解出来」不等于「解对了」。上游一旦开始校验，会以 503 失败并标记账号需人工验证。
- **依赖上游网页接口**：上游随时可能改协议。失败分类和降级能兜住单次异常，兜不住整体改版。
- **按单实例设计**：异步生图任务存在进程内存里（重启会丢），账号池并发槽位与后台轮询也没有跨实例协调，
  多副本部署会让同一账号的并发翻倍、任务在别的实例上查不到。
- **账号 token 明文存库**：数据库泄露等于账号被拿走，请自行做好库的访问控制。
- **不做用户侧限流**：并发与额度请交给前面的网关（如 New API）。
- `temperature` / `top_p` / `max_tokens` 目前被忽略：上游链路不接受这些参数。

---

## 常见问题

**第一次请求报「未启用 tls feature」**：编译时漏了 `--features tls`。

**启动就报 `DATABASE_URL` 缺失**：`.env` 不在可执行文件同目录，或变量为空。程序不会从当前工作目录找。

**`/v1/*` 返回 401 但口令没错**：`AUTH_KEY` 与 `ADMIN_KEY` 是两套凭据；后台会话 cookie 不能用于 `/v1/*`。

**请求返回 503 `account_not_available`**：没有可用账号（全部失效或在冷却）。看后台「账号」页的状态。

**异步生图任务查不到**：服务重启过，任务记录在内存里，重启即丢。

---

## 开发与测试

```bash
cargo test --workspace
cargo check --workspace --all-targets
cargo check --workspace --all-features --all-targets
```

仓库带 GitHub Actions，六个独立工作流，可分别触发：测试、编译检查（默认/全特性）、编译（默认/全特性）、tls 链路 mock 测试。

需要数据库的测试在缺 `DATABASE_URL` 时会自行跳过；带 `mock_` 前缀的测试用进程内假上游，不消耗真实配额。

---

## 许可证

MIT。

本项目只是把网页接口转成兼容 API 的客户端实现，使用它可能违反上游服务条款，账号风险自负。
