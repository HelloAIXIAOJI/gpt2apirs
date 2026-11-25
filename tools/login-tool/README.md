# 本地登录器（login-tool）

在**管理员自己的电脑**上跑登录，输出可直接粘进后台「粘贴导入」的 JSON。服务端不跑浏览器。

## 文件

- `login.py` ✅ 单个账号：undetected_chromedriver + 本地 Chrome，能过 Cloudflare。
- `batch_login.py` 批量：读 `accounts.json`，逐个登录写 `sessions.jsonl`（一行一个）。
- `parse_accounts.mjs` 把 `邮箱----密码----TOTP种子----AT` 的文本解析成 `accounts.json`。
- `login.mjs` Node + CDP 版，**会被 CF 识别**，仅作诊断。

## 前置

- Python 3.10~3.13（3.14 上 pip 版 uc 因 `distutils` 移除会 import 失败；可设 `UC_PATH` 指向兼容副本）
- `pip install undetected-chromedriver selenium`
- 本机有 Chrome / Chromium（`CHROME_PATH` 可指定）

## 单个账号

```bash
python login.py --file creds.json --out tokens.json
python login.py --file creds.json --proxy 127.0.0.1:7890
```

## 批量

```bash
node parse_accounts.mjs 账号.txt accounts.json      # 解析账号文件
python batch_login.py --file accounts.json --out sessions.jsonl --delay 3
python batch_login.py --skip-ok --out sessions.jsonl   # 断点续跑：跳过已有 token 的
```

实测：单账号约 **12 秒**，100 个约 20 分钟，全程没触发 Cloudflare。

## 两个坑（都已在代码里处理）

1. **窗口被遮挡会被浏览器节流**：Chromium 判定窗口不可见后会停掉定时器和渲染，React 水合不完成，点「继续」毫无反应也不报错。脚本已加 `--disable-backgrounding-occluded-windows` 等参数，并把窗口放到屏幕外，不挡你视线。
2. **受控输入框偶尔写不进去**：`send_keys` 在 React 水合前打字会丢，提交时报「必填项」。脚本用 `fill_input()` 写后校验 value，失败改用 JS 原生 setter 兜底。

另外「继续」按钮按**精确文本**匹配，避免误点「使用 Google 账户继续」。

## 失败时会把上游原话报出来

```
失败：上游没给授权 code，停在了说明页
上游错误码：account_deactivated
上游页面原文：
  身份验证错误
  你没有账户，因为该账户已被删除或停用。如果你认为这是错误，请通过我们的帮助中心 help.openai.com 联系我们。
  重试
  诊断信息
页面：https://auth.openai.com/mfa-challenge/...
```

以前只说「没拿到 code」，等于把唯一有用的信息扔掉 —— 用户分不清是密码错了、种子错了，还是号已经废了。

现在一旦页面变成**说明页**（没有可输入的控件，只剩「重试」和诊断信息），脚本立刻停止等待并把页面文字原样打印：

- 错误码：`innerText` 里常常没有（折叠在「诊断信息」里），会再翻一遍 DOM 源码取 `error_code`；
- 页面原文：按行去重后原样输出，未做任何改写；
- 同时把整页存成 `fail-*.html`，需要时可以直接看原始页面；
- 判定是**结构性**的（有没有真正能输入的控件），不靠匹配文案，所以封号、停用、限流、地区限制、需要人工验证这些情况都能覆盖。

批量脚本（`batch_login.py`）同样处理：整段提示会写进该账号的 `error` 字段，`sessions.jsonl` 里一眼能看出哪个号是被上游废掉的。

## 失败与风控

- 页面显示「正在进行安全验证」→ 出口 IP 被 CF 拦：换出口或等一会儿，脚本直接退出不重试。
- 密码错误：直接退出，**不重试**（上游有 `max_verify_attempts`）。
- 账号没设密码/没开 TOTP：会停在密码页或验证页 → 手动登录后用 Session JSON 导入。
- `account_deactivated`：上游已经把号删了/停用了，**这个号救不回来**，直接从池子里去掉。
- 驱动报 SSL EOF：uc 默认要联网确认驱动版本，被挡时会这样。设 `CHROMEDRIVER_PATH` 指本机那份即可。
- 别在电脑上点/关那个浏览器窗口，关掉会导致 `no such window`（脚本会自动重启浏览器并重试当前账号）。

## 安全

`creds.json`、`tokens.json`、`sessions.jsonl` 都是明文凭据，用完删掉，别提交仓库、别外传。