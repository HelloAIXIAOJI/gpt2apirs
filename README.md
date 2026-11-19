<h1 align="center">GPT2API-RS</h1>

<p align="center">
把 ChatGPT 网页版封装成自托管的 OpenAI / Anthropic 兼容接口。<br>
一个二进制，自带账号池、图片归档与管理后台。
</p>

<p align="center">
  <a href="https://github.com/HelloAIXIAOJI/gpt2apirs/actions/workflows/test.yml"><img src="https://github.com/HelloAIXIAOJI/gpt2apirs/actions/workflows/test.yml/badge.svg" alt="tests"></a>
  <img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="license">
  <img src="https://img.shields.io/badge/rust-2024-orange.svg" alt="rust">
</p>

<p align="center">
  <a href="#快速开始">快速开始</a> ·
  <a href="#功能">功能</a> ·
  <a href="#效果展示">效果展示</a> ·
  <a href="#api">API</a> ·
  <a href="#配置">配置</a> ·
  <a href="#常见问题">常见问题</a>
</p>

> [!WARNING]
> 本项目通过分析 ChatGPT 网页端接口提供兼容 API，仅供个人学习与技术研究使用。
>
> - 请勿用于商业用途、批量滥用或转售服务。
> - 使用本项目有账号被限流或封禁的风险，请勿使用重要账号测试。

## 快速开始

需要 Rust（2024 edition）与 PostgreSQL。`--features tls` 会编译 BoringSSL，还要一套构建工具：

- Linux：`cmake ninja-build nasm perl pkg-config libclang-dev`
- macOS：`brew install cmake ninja nasm`
- Windows：VS「使用 C++ 的桌面开发」+ `choco install nasm`；装了 LLVM 时把 `LIBCLANG_PATH` 指到它的 `bin`

不想自己编 BoringSSL 的话，直接用 [Releases](https://github.com/HelloAIXIAOJI/gpt2apirs/releases) 里的二进制。

```bash
git clone https://github.com/HelloAIXIAOJI/gpt2apirs && cd gpt2apirs
cp .env.example .env               # 填 DATABASE_URL 与 ADMIN_KEY
cargo build --locked --release --features tls -p gpt2apirs-cli
./target/release/gpt2apirs
```

启动后打开 `http://127.0.0.1:8000/admin`，用 `ADMIN_KEY` 登录，导入账号并在「密钥」页创建 API key。

```bash
curl http://127.0.0.1:8000/v1/chat/completions \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model":"text","messages":[{"role":"user","content":"你好"}]}'
```

## 功能

OpenAI 与 Anthropic 双协议兼容，文本支持流式输出。

### 账号池

- 支持 token 与 OAuth 授权导入
- 账号过期自动刷新，配额定期同步
- 账号失效时自动换号

### 图片

- 生图与图生图
- 图片浏览、归档与打包下载

### 出口代理

- 内置代理内核，支持 Clash 订阅
- 支持节点测速与切换

### 管理后台

- 概览、账号、图片、任务、日志、备份、设置
- 运行参数热更新，无需重启

## 效果展示

| 概览 | 账号池 |
|:---:|:---:|
| ![概览](readme/img/overview.png) | ![账号池](readme/img/accounts.png) |

| 图片（浏览 / 归档） | 代理（节点 / 测速） |
|:---:|:---:|
| ![图片](readme/img/gallery.png) | ![代理](readme/img/proxy.png) |

## API

对外提供两个模型名：`text` 与 `image`。

| 接口 | 说明 |
|---|---|
| `POST /v1/chat/completions` | 文本对话 |
| `POST /v1/responses` | Responses 格式 |
| `POST /v1/messages` | Anthropic 格式 |
| `POST /v1/messages/count_tokens` | token 计数 |
| `POST /v1/images/generations` | 文生图 |
| `POST /v1/images/edits` | 图生图 |
| `POST /v1/search` | 联网检索 |
| `GET /v1/models` | 模型列表 |

## 配置

`.env` 与可执行文件同目录。

| 变量 | 必填 | 说明 |
|---|---|---|
| `DATABASE_URL` | 是 | PostgreSQL 连接串 |
| `ADMIN_KEY` | 是 | 后台登录口令 |
| `DATA_DIR` | 否 | 图片与归档目录 |
| `PUBLIC_BASE_URL` | 否 | 对外地址 |
| `CORS_ALLOW_ORIGINS` | 否 | 允许跨站调 `/v1/*` 的来源，逗号分隔；留空放开任意来源 |

## 常见问题

**第一次请求报「未启用 tls feature」**：编译时缺少 `--features tls`。

**编译报 `btls-sys` / BoringSSL 失败**：构建工具没齐，对着上一节的清单补。Windows 上如果报 `generator : Visual Studio xx` 不被支持，说明 VS 自带的 cmake 太老，装一份独立的 cmake 与 ninja 放到 PATH 前面。

**启动报 `DATABASE_URL` 缺失**：`.env` 不在可执行文件同目录。

**`/v1/*` 返回 401**：需使用「密钥」页创建的 API key，`ADMIN_KEY` 仅用于登录后台。

## 本地开发

CI 的测试任务不含 `tls`，跑的是存根客户端，所以下面两条不需要 BoringSSL 工具链：

```bash
cargo test --workspace
cargo check --workspace --all-targets
```

要连真实上游的检查再开 `tls`（必须先装好上一节那套工具链）：

```bash
cargo check --workspace --all-features --all-targets
```

`*_live` 用例没有 `DATABASE_URL` 会自动跳过，有的话会打真实上游。CI 另外用 `RUSTFLAGS=-D warnings`，本地建议同样设上。

## 发布构建

GH Actions 按 `linux-x86_64`、`linux-aarch64`（release + debug）与 `windows-x86_64`（release）各编一份，命令就是：

```bash
cargo build --locked --release --features tls -p gpt2apirs-cli
```

debug 包另设 `CARGO_PROFILE_DEV_DEBUG=0` 去掉调试符号，体积从 GB 级降到百 MB 级。
## 社区

[Linux.DO](https://linux.do/) 是世界上最大的中文技术社区。

## 许可证

MIT。
