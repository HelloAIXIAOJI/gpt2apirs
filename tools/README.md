# tools/

## build.ps1 —— 统一构建入口

Windows 上编 `--features tls` 要过 BoringSSL，对环境有要求（VS 自带 Ninja、NASM、工具链文件）。
脚本一次性把这些配好：

```bat
powershell -File tools\build.ps1           :: = cargo build -p gpt2apirs-cli --features tls
powershell -File tools\build.ps1 -Release
powershell -File tools\build.ps1 -CargoArgs build,--workspace,--all-targets
```

只往 `target/` 写一个 `bssl-toolchain.cmake`，不动仓库其他文件；也不替你停正在跑的服务。
四个坑的报错原文与实测记录见 `docs/build-verification.md`。

## account_gui.py —— 账号池图形管理工具

单文件，**只用 Python 标准库**（tkinter + urllib），不需要 pip install。

```bat
python tools\account_gui.py
```

### 首次使用

1. 起服务：`gpt2apirs`（或 `cargo run -p gpt2apirs-cli --features tls`）
2. 打开 GUI，填：
   - **服务地址**：`http://127.0.0.1:8000`
   - **AUTH_KEY**：`.env` 里的 `AUTH_KEY`
3. 点「保存并连接」（配置存到 `%USERPROFILE%\.gpt2apirs\gui_config.json`）

### 加账号

把 **Access Token** 粘进文本框（一行一个），点「导入」。也可以：

- 点「从文件读取…」选 `.txt`（一行一个 token）或 `.json`（备份 / Session / CPA 格式）
- 整段 JSON 直接粘也行 —— 服务端会自动识别

导入后**必须点「同步配额」**，否则 `quota` 是未知的 0，不会被选中。

### 按钮说明

| 按钮 | 作用 | 注意 |
|---|---|---|
| 刷新列表 | 重新拉 `/api/accounts` | — |
| 同步配额 | 打上游 `/me` 拿真实配额与套餐 | 选中则只同步选中的；不选则同步「需要同步的」 |
| 刷新 AT | 用 refresh_token 换新 access_token | 必须选中；没有 RT 的号会失败 |
| 启用 / 禁用 | 改 `status` | 禁用后不会被选号 |
| 删除 | 从池里移除 | 不可撤销 |
| 测试出图 | **真实生成一张图** | 只选中 1 个；会消耗配额，耗时 1~2 分钟 |
| 导出 AT | 导出选中的 access_token | 需 AT/RT/id_token 三件套齐全，否则跳过 |
| 统计 | 各状态计数与总配额 | — |
| **OAuth 登录** | 走授权码流程拿 `refresh_token` | 链接只展示不自动打开；见上一节 |

### 界面里怎么看「这个号还能用吗」

| 列 | 判据 |
|---|---|
| 状态 | `正常` 才可被选号；`限流` = 配额 0；`异常` = 凭证失效 |
| 配额 | **必须 > 0**，否则不会出图 |
| AT | `有效` / `即将过期`（<24h）/ `无效` |
| RT | `有效` / `缺失`（AT 一过期就没救）/ `无效` |
| 可用性 | `可用` / `可恢复`（能刷新救回来）/ `不可用`（换号） |

### 拿到 refresh_token（OAuth 登录）

> **先说清楚：RT 无法从网页界面或浏览器里手动复制。**
> 它是 OAuth 授权码交换的产物，`chatgpt.com/api/auth/session` 只返回 `accessToken`。

> ✅ **已实测通过**：授权成功后拿到 refresh_token，`RT` 列显示 `有效`；
> 之后「刷新 AT」能在 3 秒内换出新 AT。
>
> 授权成功后**会自动同步一次配额**，不用再手动点「同步配额」。

点「OAuth 登录」打开对话框：

```text
① 获取授权链接   → 生成一条 auth.openai.com 的授权 URL
                   **不会自动打开浏览器**，只显示在框里，可点「复制链接」

② 浏览器打开该链接
   ⚠️ 要授权哪个号，就用那个号登录；浏览器已有别的号时请开【无痕窗口】

③ 登录后被重定向到
   https://platform.openai.com/auth/callback?code=XXXX&state=YYYY
   页面可能白屏或报错 —— 没关系，从【地址栏】复制整条 URL

④ 把整条 URL 粘回对话框 → 点「④ 完成授权」
   成功后账号直接入库（带 refresh_token）
```

**验证成功的标志**：刷新列表后，该账号的 `RT` 列显示 `有效`（不是 `缺失`）。
此后 AT 快过期时点「刷新 AT」即可自动续期，不用再手动换 token。

### 三个易错点

| 坑 | 说明 |
|---|---|
| 用错账号 | 授权链接会在**当前浏览器已登录的账号**上生效；多账号务必开无痕 |
| 回调页面白屏 | `platform.openai.com/auth/callback` 可能不渲染任何内容，但地址栏里 `code` 是有效的 |
| 会话 10 分钟过期 | 生成链接后拖太久会失效，重新点「获取授权链接」即可 |
### 常见困惑

| 现象 | 原因 |
|---|---|
| 导入成功但生图报 `503 no_available_account` | 没点「同步配额」，或该号 `quota=0` |
| 生图返回 200 但 `data` 为空 | **上游配额为 0**，本地门已过；看 `restore_at` 等恢复 |
| 「导入」显示**合并**若干 | 这些 token 已在池里，本次带的新字段（如 refresh_token）已补进去，不会产生重复账号 |
| 想给已有账号补 refresh_token | **直接再导一次**带 RT 的 JSON 即可（会走合并），不用先删 |
| 测试出图失败但同步正常 | 同步只证明 token 有效，测试才证明配额链路通 |

---

## 测试脚本

| 脚本 | 作用 | 需要联网 |
|---|---|:---:|
| `test_account_gui_smoke.py` | 构造全部控件 + 表格填充，验证不崩 | ❌ |
| `test_account_gui_api.py` | 打真实服务跑 36 项断言（导入/去重/更新/同步/删除/错误路径） | ✅ |

```bat
:: 冒烟（不需要服务）
python tools\test_account_gui_smoke.py

:: API 测试（需先起服务）
set GUI_TEST_BASE=http://127.0.0.1:8000
python tools\test_account_gui_api.py
```

> API 测试会创建带 `gpt2apirs-gui-test-` 前缀的临时账号，并在结束时删除，
> 不会碰真实账号。
