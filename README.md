# gpt2apirs

> 把 ChatGPT 官网能力接入 OpenAI 兼容 API 的 Rust 实现。
> MIT License · Rust 2024 · PostgreSQL · axum · wreq

---

## 当前状态

| 项 | 数值 |
|---|---|
| 测试用例 | **493 个，全部通过** |
| 实现代码 | **84 文件 / 21,228 行** |
| 测试代码 | **22 文件 / 4,575 行** |
| 数据库 | PostgreSQL 18.0（4 张表：accounts / system_settings / app_meta / call_records）|

---

## 已验证的能力

以下全部**打真实上游**验证过，不是「编译通过」级别的声明。

| 能力 | 状态 | 证据 |
|---|:---:|---|
| 起服务（.env → 配置 → 日志 → 池 → 迁移 → HTTP） | ✅ | 启动 10 步日志全过 |
| `POST /v1/images/generations`（`b64_json`） | ✅ | 14.7s / 1,882,214 字节 PNG / 1312×1199 |
| `POST /v1/images/generations`（`url`） | ✅ | 96.9s / 2,273,901 字节 / 1402×1122 |
| `GET /v1/models` | ✅ | 文本 + 图片两个模型，带 `kind`；只宣告真正支持的 |
| `GET /v1/models/{id}` | ✅ | 未知 id → 404 `model_not_found`（与列表口径一致）|
| `POST /v1/messages/count_tokens` | ✅ | 真机三方对齐：预估 77 = 非流式 77 = 流式 77 |
| `x-api-key` 头 | ✅ | 与 Bearer 等价；进后台仍然 401 |
| 流式 usage 与小费口径 | ✅ | 修掉「guard 提示词算进账单」：chat / responses / messages 流式非流式全部对齐 |
| `GET /images/{path}` 静态服务 | ✅ | 路径穿越防护 + MIME 正确 |
| `GET /health` `/health/ready` `/version` | ✅ | 就绪探针 DB 不可用返回 503 |
| PostgreSQL 迁移 | ✅ | 可重复执行，幂等 |
| 日志脱敏 | ✅ | token / dx / base64 全掩码 |
| 全局 deadline（170s） | ✅ | 七段预算，选号等待也受限 |

### 图片与文本（M4）

| 能力 | 状态 | 证据 |
|---|:---:|---|
| **上游图片上传** | ✅ | 277 字节 PNG → **2.0s** → `file_id` |
| **`POST /v1/images/edits`** | ✅ | 1.7 MB 参考图 → **28.6s / 31.9s 复验** → 1,753,629 字节 PNG / 1402×1122 |
| 图生图 usage | ✅ | input image tokens 2439 / output 1634 |
| mask alpha 合成 | ✅ | 5 个单测（尺寸缩放 / 灰度 mask / 空输入 / 非图片）|
| **文本 SSE 提取** | ✅ | 真实样本 20,324 字节 → 51 字回复，6 条系统提示全挡住 |
| **`POST /v1/chat/completions`（非流式）** | ✅ | **5.9s / 6.6s** / usage `19+44=63` |
| **`POST /v1/chat/completions`（流式）** | ✅ | 4 条 SSE 事件 → 拼出「银装素裹」 |
| **文本不扣图片配额** | ✅ | 配额 23 / 4 **前后不变**，成功计数各 +1 |
| tiktoken 真实计数 | ✅ | `hello world`=2、英文长句=10、中文长句=**17** |
| **`GET/PUT /api/settings`** | ✅ | 12 项数值 + 1 项字符串；未知键拒收；越界 999 读回 **3** |
| **内容粗筛可配置生效** | ✅ | 配置关键词后 400 `content_policy_violation`，还原后放行 |
| 断网时正确降级 | ✅ | 上游不可达返回 502 `upstream_error`，**配额不被误扣** |
| **`POST /v1/responses`** | ✅ | 非流式 **6.8s** / usage `28+67=95`；流式 6 个事件 |
| **`POST /v1/messages`** | ✅ | 非流式 **5.1s** / usage `28+42`；流式 6 个 Anthropic 事件 |
| 三套错误格式分离 | ✅ | Anthropic 路径 `{"type":"error"…}`；OpenAI 路径 `{"error":{…}}` |
| `tools: []` 不误拒 | ✅ | 空数组放行（200），非空才 400 `tools_not_supported` |
| **真流式（chat）** | ✅ | 首字 **2.8~3.2s**（之前 = 总耗时 6~8s）|
| **结果缓存** | ✅ | 同一请求 **6.6s → 0.01s**；换问题仍走上游 |
| **并行 n>1** | ✅ | n=2 → 2 张图 / **30.1s**（串行需 ~55s）/ 两账号各扣 1 |
| **失败分类** | ✅ | 10 种类型，内容策略/验证码**不换号**（换也没用）|
| **换号重试** | ✅ | 次数走 `image_max_account_attempts`；空图不标账号失效 |
| **富文本标签处理** | ✅ | guard + 窄剥离，实测 **99 → 10**；4/6 题干净，残余打 WARN（ADR-011）|

### 账号池（M3，已完整落地）

| 能力 | 状态 | 证据 |
|---|:---:|---|
| 导入 / 去重 | ✅ | 同一 token 二次导入 → 只有 1 行 |
| **导入合并（补 refresh_token）** | ✅ | 二次导入带 RT → `merged=1`，`RT: missing → valid` |
| 凭证三态投影 | ✅ | `AT=valid` / `RT=missing` / `usable=usable` |
| **同步上游真实配额** | ✅ | 3.2s 拿到 `quota=25`、`plan=free`、`restore_at` |
| **选号 → 出图（全链路）** | ✅ | **HTTP 200 / 20.1s / 1,735,730 字节 PNG / 1402×1122** |
| **测试出图端点** | ✅ | **HTTP 200 / 21.7s / 1,746,951 字节 / 1312×1199** |
| **配额扣减 + 成功计数** | ✅ | `quota` 25→24→23，`成功/失败` 0→1→2 |
| 限流号不被选中 | ✅ | `503` `account_not_available`（精准错误） |
| 空图不计为成功 | ✅ | 上游配额耗尽时记 `fail++`，不是 `success++` |
| 失效判定三重保护期 | ✅ | 首次失效 `invalid_count=1`，状态不变 |
| 批量同步 / 刷 AT 进度 | ✅ | `operation_id` + 轮询 |
| OAuth PKCE 起始 | ✅ | 授权 URL 含 `code_challenge_method=S256` + `offline_access` |
| **OAuth 授权码交换** | ✅ **真机成功** —— 拿到 `refresh_token`，`RT=valid` |
| **用 RT 刷新 AT** | ✅ **真机成功** —— `refresh-token` 3.1s，AT 到期时间被续期 |
| 账号身份去重（防重复行） | ✅ 三级识别：token → upstream_account_id → email |
| 合并时保住历史计数 | ✅ 吸收重复行并累加 `success/fail`，配额不相加 |
| 导出（三件套校验） | ✅ | 缺 RT/id_token 时跳过并列明 |

---

## 快速开始

### 1. 准备

```bash
# 最小依赖：Rust nightly + NASM（Windows 上 BoringSSL 需要）+ PostgreSQL 18
cp .env.example .env   # 或直接编辑 .env
```

`.env`（**必须与可执行文件同目录**，例如 `target/debug/.env`）:

```ini
DATABASE_URL=postgres://user:pass@host:5432/db?sslmode=disable
AUTH_KEY=换成一个足够长的随机串
DATA_DIR=../../data
HOST=0.0.0.0
PORT=8000
```

### 2. 起服务

```bash
cargo run -p gpt2apirs-cli --features tls
```

> `--features tls` **不能省**。不开它编译出来的是存根客户端，第一次请求才会报
> 「gpt2apirs-http 未启用 tls feature」。
>
> Windows 上 BoringSSL 对环境敏感（VS 生成器找不到 cl、Ninja 下把 cl 当汇编器）。
> 直接跑 `powershell -File tools\build.ps1`，它把 vcvars、Ninja、NASM 和工具链文件都配好。

### 3. 导入账号

账号池启动时是空的，`/v1/images/generations` 会返回 **503**，必须自己导入：

```bash
curl -X POST http://127.0.0.1:8000/api/accounts \
  -H "Authorization: Bearer $AUTH_KEY" \
  -H "Content-Type: application/json" \
  -d '[{"access_token": "eyJhbGciOi..."}]'
```

然后**必须同步一次配额**，否则 `quota` 是未知的 0，不会被选中：

```bash
curl -X POST http://127.0.0.1:8000/api/accounts/sync \
  -H "Authorization: Bearer $AUTH_KEY" \
  -H "Content-Type: application/json" -d '{"ids":[]}'
```

### 4. 出图

```bash
curl -X POST http://127.0.0.1:8000/v1/images/generations \
  -H "Authorization: Bearer $AUTH_KEY" \
  -H "Content-Type: application/json" \
  -d '{"prompt":"a red apple","response_format":"url"}'
```

---

## 管理后台

浏览器打开 `http://127.0.0.1:8000/admin`，用 `.env` 里的 `ADMIN_KEY` 登录。

页面是手写 HTML/CSS/JS，**编译期打进二进制**：不需要 Node、不需要额外静态目录，部署仍只有一个可执行文件。
八个标签页：概览（成功率 / P50 / P95 / 错误 Top 5）、账号池、调用日志、代理分组、图片、设置、API Keys、备份。

### 登录口令与 API key 是两套

| 用途 | 凭证 | 存放 |
|---|---|---|
| 登录后台、调 `/api/*` | `ADMIN_KEY` | `.env`；登录后换成会话 cookie |
| 调 `/v1/*` | 后台里创建的 API key | 数据库，只存 sha256 |

两者**不能互换**：登录口令调 `/v1/*` 一律 401，API key 进后台也一律 401。
升级前只有 `AUTH_KEY` 的部署，启动时会自动把它迁成名为 `legacy` 的 API key，避免断流；之后请到后台建新 key 并吊销它。

### 桌面账号池 GUI（可选）

`tools/account_gui.py` 是一个单文件 Tkinter GUI，**只用 Python 标准库**：

```bat
python tools\account_gui.py
```

填服务地址与 `AUTH_KEY` → 「保存并连接」→ 粘贴 token → 「导入」→ 「同步配额」。
详见 [tools/README.md](tools/README.md)。

---

## 对外接口

### OpenAI 兼容

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/v1/models` | 模型列表（文本 + 图片，带 `kind`；只列真正支持的） |
| GET | `/v1/models/{id}` | 单个模型；未知 id 返回 404 `model_not_found` |
| POST | `/v1/images/generations` | 文生图（`b64_json` / `url`；`output_format` = png / jpeg / webp / gif，gif 配 `frame_count` / `frame_duration_ms`；`size` 只是提示词倾向，见已知限制） |
| POST | `/v1/images/edits` | **图生图**（multipart / JSON，支持 mask；同样支持 `output_format`） |
| POST | `/v1/chat/completions` | **文本对话**（流式 + 非流式，真实 token 计数） |
| POST | `/v1/responses` | **OpenAI Responses API**（`input` / `instructions`）|
| POST | `/v1/search` | **联网搜索**（返回答案 + 来源，URL 已去追踪参数）|
| POST | `/v1/messages` | **Anthropic Messages API**（`system` / `max_tokens`，事件流格式对齐）|
| POST | `/v1/messages/count_tokens` | 只算 token 不取号，口径与 `/v1/messages` 的 `usage.input_tokens` 同一函数 |

> `/v1/*` 的 API key 两种头都收：`Authorization: Bearer <key>` 与 `x-api-key: <key>`（后者给 Anthropic 客户端）。

### 上游失败的分类与状态码

同一类失败在流式与非流式上报同一个错误码；状态码按「谁的问题」分：

| 错误码 | 状态码 | 含义 | 该怎么做 |
|---|---:|---|---|
| `upstream_invalid_request` | 400 | 请求本身有问题（参数/模型/上下文超长） | 改请求，重试无用 |
| `content_policy_violation` | 400 | 触发上游内容策略 | 改提示词，换号无用 |
| `rate_limited` | 429 | 账号被限流 | 退避后重试，会换号 |
| `account_quota_exhausted` | 429 | 账号额度耗尽 | 换号或等恢复 |
| `account_token_invalid` / `upstream_blocked` | 502 | 账号侧问题（**不是调用方 key 的问题**） | 会自动换号并标异常 |
| `upstream_empty_reply` | 502 | 上游正常结束但没吐文本 | 可重试，不换号 |
| `upstream_protocol_error` / `upstream_network_error` | 502 | 协议不符 / 网络失败 | 可重试 |
| `turnstile_required` / `arkose_required` | 502 | 上游要人机验证 | 换号无用，需人工处理 |

> 账号问题故意不回 401/403：那会让调用方以为自己传错了 key。
> 鉴权失败的响应体也按路径选格式：`/v1/messages*` 是 Anthropic 的 `{"type":"error",…}`，其余 `/v1/*` 是 OpenAI 的 `{"error":{…}}`。

### 账号池管理

| 方法 | 路径 | 说明 |
|---|---|---|
| GET / POST / DELETE | `/api/accounts` | 列表 / 导入 / 批量删除 |
| GET | `/api/accounts/{id}` | 单个账号详情（与列表同一份视图，未知 id → 404）|
| GET | `/api/accounts/stats` | 统计 |
| POST | `/api/accounts/update` | 更新（状态 / 套餐 / 邮箱 / 代理 / 配额） |
| POST | `/api/accounts/sync` | 批量同步配额 → `operation_id` |
| POST | `/api/accounts/refresh-token` | 批量刷新 AT → `operation_id` |
| GET | `/api/accounts/operations/{id}` | 批量操作进度 |
| POST | `/api/accounts/{id}/test` | **真跑一张图**（消耗配额） |
| POST | `/api/accounts/oauth/{start,finish}` | OAuth PKCE 登录 |
| POST | `/api/accounts/export` | 导出（`json` / `txt`） |

### 管理后台（后台会话或 `ADMIN_KEY`）

| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/admin/login` | 登录，下发 `HttpOnly` 会话 cookie（12 小时）|
| GET | `/admin/session` | 会话信息 + 有效 key 数 |
| POST | `/admin/logout` | 退出 |
| GET / POST | `/api/keys` | 列 key / 建 key（明文只返回一次）|
| DELETE | `/api/keys/{id}` | 吊销（立刻失效）|
| GET | `/api/logs` | 调用日志，支持 `status=2xx`、`path=`、`account=`、`hours=`、分页 |
| GET | `/api/metrics` | 近 N 小时成功率、P50 / P95、错误 Top 5 |
| GET | `/api/images` | 图片列表（含账号、提示词、尺寸）|
| GET | `/api/images/stats` | 张数与总字节 |
| DELETE | `/api/images/{id}` | 删记录并删磁盘文件 |
| GET / POST | `/api/proxy-groups` | 代理分组列表 / 新建 |
| PUT / DELETE | `/api/proxy-groups/{id}` | 改组地址（同步组内账号）/ 删组（先摘成员）|
| POST | `/api/accounts/{id}/proxy-group` | 账号加入分组（`group_id: null` 表示移出）|
| POST | `/api/backup` | 打备份包到 `data/backups/`（含账号、设置、key 哈希、图片）|
| GET | `/api/backups` | 已有备份列表 |
| POST | `/api/restore` | 合并恢复（只补缺失，不覆盖已有行）|

### 运行设置

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/api/settings` | 读全部设置：**14 项数值** + **2 项字符串**（含 `rich_text_policy`）|
| PUT | `/api/settings` | 改设置：**先全校验 → 再落库 → 热重载**；未知键拒收（`unknown_setting_key`）|

> `PUT` 的校验顺序是刻意的：半途失败不会留下改了一半的设置。
> 数值越界会被**钳制**而不是报错（写 `999` 读回 `3`）。

### 运维

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/health` | 详细检查，总是 200 |
| GET | `/health/ready` | 就绪探针，DB 不可用 503 |
| GET | `/version` | 版本 |
| GET | `/images/{*path}` | 静态图片 |

---

## 设计要点

| 决策 | 说明 |
|---|---|
| **只支持 PostgreSQL** | 放弃 SQLite 双后端（ADR-008 修订）；双后端会逼出最低共同分母的 SQL |
| 配置分两层 | `.env`（引导层，不可热改）+ `system_settings` 单行 JSONB（运行层，可热改） |
| **`DATA_DIR` 相对可执行文件** | 服务的 CWD 不可靠（`/`、`C:\Windows\System32`），绝不按 PWD 解析 |
| 全局 deadline | 一次请求总时长上限 170s，贯穿七段流水线 |
| 账号池主键是 **uuid** | 不用 `access_token`，避免 token 轮换导致账号身份漂移 |
| 槽位用 **RAII** | `AccountLease` 持 `OwnedSemaphorePermit`，`?`/panic/取消都自动归还 |
| 状态 **2 层** | 业务状态持久化 + 凭证可用性派生（JWT 的 `exp` 是纯函数，不落库） |
| 失效判定三重保护期 | 新号 10 分钟 / 首次不算 / 30 秒确认，默认标异常而非删除 |

---

## 文档

| 文档 | 内容 |
|---|---|
| [docs/m4-image-edits-verification.md](docs/m4-image-edits-verification.md) | **M4 图生图验收**：上传三步 / 多模态 payload / 真机 28.6s |
| [docs/m4-text-stream.md](docs/m4-text-stream.md) | **M4.15 文本 SSE 实测**：真实下发完整消息而非补丁 / 三条过滤规则 |
| [docs/m4-chat-verification.md](docs/m4-chat-verification.md) | **M4.16 文本对话验收**：tiktoken 实测值 / 配额不变量 / 粗筛边界 |
| [docs/m4-text-endpoints-verification.md](docs/m4-text-endpoints-verification.md) | **M4.17/M4.18 验收**：Responses 与 Anthropic 格式 / 三套错误体 / 工具拒绝 |
| [docs/m5-m4-batch-verification.md](docs/m5-m4-batch-verification.md) | **M5.2/M5.6/M4.19 验收**：并行 / 失败分类重试 / 真流式 / 缓存 |
| [docs/rich-text-policy.md](docs/rich-text-policy.md) | **富文本标签处理**：13 种标签实测 / 约束与窄剥离 / 12 个单测 |
| [docs/m3-account-pool-verification.md](docs/m3-account-pool-verification.md) | M3 账号池全量验收：14 模块 / 12 个真 bug |
| [docs/account-pool-design.md](docs/account-pool-design.md) | 账号池设计：11 条决策 + 数据模型 + 状态机 + 任务分解 |
| [docs/account-pool-research.md](docs/account-pool-research.md) | 账号池研究：字段 / 状态 / 导入 / 切换策略 |
| [docs/account-pool-features.md](docs/account-pool-features.md) | 管理面按钮清单（28 项对照） |
| [docs/build-verification.md](docs/build-verification.md) | **构建验证**：BoringSSL 四个坑 / 全特性实测 / tools\build.ps1 |
| [docs/m3-batch1-verification.md](docs/m3-batch1-verification.md) | M3 第 1 批：模型 / 凭证 / 仓储 |
| [docs/m5-m4-verification.md](docs/m5-m4-verification.md) | 全局 deadline + `/v1/models` |
| [docs/m2-m4-verification.md](docs/m2-m4-verification.md) | 协议下沉 + `/v1/images/generations` |
| [docs/m1-design.md](docs/m1-design.md) | M1 基础设施设计 |
| [docs/todo.md](docs/todo.md) | 任务清单（117 条，45 完成） |
| [docs/decisions.md](docs/decisions.md) | ADR |
| [tools/README.md](tools/README.md) | GUI 使用说明 |

---

## 测试怎么跑

```bash
cargo test --workspace        # 离线用例：参数校验 / 解析 / 鉴权 / 盘面逻辑
```

**生成链路**的离线回归需要 `tls`（默认特性下 HTTP 客户端是存根），走脚本一次配好环境：

```bat
:: 文本链路端到端（本地 mock 上游，不出环回地址）
powershell -File tools\build.ps1 -CargoArgs test,-p,gpt2apirs-generate,--features,tls,--test,mock_upstream

:: 三个口径的 usage 必须一致（count_tokens = 非流式 = 流式）
powershell -File tools\build.ps1 -CargoArgs test,-p,gpt2apirs-api,--features,tls,--test,mock_upstream_usage

:: 图片七步流水线（含下载与落盘）
powershell -File tools\build.ps1 -CargoArgs test,-p,gpt2apirs-generate,--features,tls,--test,mock_upstream_image

:: 故障分类：401 / 429 / 配额 / 内容策略 / SSE 断流
powershell -File tools\build.ps1 -CargoArgs test,-p,gpt2apirs-generate,--features,tls,--test,mock_faults
powershell -File tools\build.ps1 -CargoArgs test,-p,gpt2apirs-api,--features,tls,--test,mock_faults_api

:: 换号重试：第一个账号 401，第二个必须接手
powershell -File tools\build.ps1 -CargoArgs test,-p,gpt2apirs-api,--features,tls,--test,mock_switch_account_live

:: 管理后台冒烟：真浏览器登录，逐页检查渲染与控制台异常（需要先起服务）
node tools\admin_e2e.mjs --key <ADMIN_KEY>

:: 图文对话：图片真的上传，且会话 payload 是多模态
powershell -File tools\build.ps1 -CargoArgs test,-p,gpt2apirs-api,--features,tls,--test,mock_upstream_image_input
```

后四条需要 `DATABASE_URL`（账号池读库），缺则自动跳过。
换号用例造的账号必须带 future `exp` 的 JWT，否则池子的凭证准入会跳过它们。

---

## 已知限制

| 限制 | 说明 |
|---|---|
| 生图 `size` 不精确 | 上游没有尺寸参数，只能写进提示词当倾向：实测请求 1792×1024 出图 1659×948，**纵横比对、总像素由上游决定**；`auto`/空表示不干预，非法值 400。要精确尺寸得走 Codex 链路（未实现） |
| 生图 `quality` 同样只是提示词 | 上游没有质量参数，`low`/`medium`/`high` 会写进提示词；但它**确实影响计费**（按档位算 token），实测 high 档 1659×948 = 6338 tokens |
| 生图 `stream: true` 不支持 | 上游没有逐块图片流（SSE 只给指针，字节另行下载）；要进度请用 `/api/image-tasks` |
| `temperature` / `top_p` / `max_tokens` 接受但忽略 | 上游 web 会话没有这些旋钮；`modalities` 例外，非 text 直接 400 |
| Codex 模型 | `codex-gpt-image-2` 等走另一条上游链路，未实现，对外不宣告 |
| 单实例假设 | 选号游标在进程内存，多实例部署槽位不共享 |
| 账号必须有额度 | `quota=0` 时不会出图；上游说 0 就是 0，等 `restore_at` 或换号 |
| 只导 access_token 时 | 无 `refresh_token`，AT 一过期即 `不可用`，只能重导 |

---

## 许可证

MIT

## 免责声明

本项目通过逆向研究接入 ChatGPT 官网能力，并非 OpenAI 官方服务。
接口可能随上游变化失效，并可能导致账号受限、临时或永久封禁。
**请勿使用重要、常用或高价值账号。**

使用者须自行了解技术、账号与合规风险，遵守 OpenAI 服务条款及当地法律法规。
