# 本地登录器（login-tool）

在**管理员自己的电脑**上跑一次登录，输出一段 JSON，粘到后台「粘贴导入」。
服务端不需要浏览器，也不占内存。

## 用哪个脚本

- `login.py` ✅ **推荐**：undetected_chromedriver + 本地 Chrome/Chromium，实测能过 Cloudflare 并跑通完整登录。
- `login.mjs`：Node + Chrome DevTools Protocol，**会被 CF 识别**，留作诊断用。

原因：Playwright/Puppeteer/裸 CDP 的连接会被 Cloudflare 检测到；uc 用打补丁的 chromedriver 驱动，不暴露这些痕迹。同机同 IP 对照实验，只有 uc 能过。

## 前置

- Python 3.10~3.13（3.14 上 pip 版 uc 会因 `distutils` 移除而 import 失败；可设 `UC_PATH` 指向兼容副本）
- `pip install undetected-chromedriver selenium`
- 本机有 Chrome / Chromium（`CHROME_PATH` 可指定路径）
- 账号有：邮箱、密码、TOTP 种子

## 用法

```bash
# creds.json: {"email":"...","password":"...","totp":"BASE32SEED"}
python login.py --file creds.json --out tokens.json
python login.py --file creds.json --proxy 127.0.0.1:7890      # 走指定出口
python login.py --file creds.json --headless                  # 不推荐，headless 更易被识别
```

输出：

```json
{"access_token":"eyJhbGciOi...","refresh_token":"rt.1.AAD...","id_token":"eyJhbGciOi...","email":"a@b.com"}
```

粘到后台「账号池 → 粘贴导入」即可（导入器认 `access_token` / `refresh_token` / `id_token` / `email`，已存在会合并）。

## 它做了什么

1. 起一个可见窗口的 Chrome（uc 驱动），打开我们自己的 platform OAuth 授权 URL（自带 PKCE）
2. 邮箱 → 若出现「使用密码继续」就点它 → 密码 → 现算 TOTP 提交
3. 从回调 URL 抠 `code`
4. `code` + PKCE verifier → `/oauth/token` 换 AT / RT / ID token
5. 关浏览器

AT 约 10 天有效，RT 长期有效，后台自动续期，正常不需要反复跑。

## 失败与风控

- **页面显示「正在进行安全验证」**：出口 IP 被 CF 拦。换出口（`--proxy`）或等几分钟到几小时；脚本直接退出，不重试。
- **密码错误**：直接退出，**不重试**（服务端有 `max_verify_attempts`）。
- **没到密码页 / 没到 TOTP 页**：账号可能没设密码/没开 TOTP，或页面流程变了 → 手动登录后用 Session JSON 导入。
- 别批量循环调用；失败后至少等 10 分钟，一个号一天最多一两次。
- 驱动版本不匹配时脚本会自动解析浏览器版本并重试一次。

## 安全

`creds.json`、`tokens.json` 都是明文凭据，用完删掉，别提交仓库、别外传。