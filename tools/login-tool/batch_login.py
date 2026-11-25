#!/usr/bin/env python3
"""
批量账密登录：读 accounts.json，逐个登录，把 token 写进 sessions.jsonl（一行一个）。

- 复用同一个浏览器进程；每个账号前清 cookie
- 输出: {"email","access_token","refresh_token","id_token","seconds"} 或 {"email","error","seconds"}
- 连续 CF 拦截达到阈值就停（避免把出口打到黑名单深处）
"""
import argparse, base64, hashlib, hmac, json, os, re, secrets, sys, time
from urllib.parse import urlencode
import urllib.request

def page_notice(driver, limit=14):
    """把页面上人眼看到的提示原样取出来（去空行、去重复行）。

    上游判定账号不可用时不会给授权 code，而是直接渲染一个说明页；
    那上面的文案就是唯一结论，丢掉它等于让人去猜。
    """
    try:
        text = driver.execute_script("return document.body ? document.body.innerText : ''") or ""
    except Exception:
        text = ""
    if not text.strip():
        try:
            from selenium.webdriver.common.by import By
            text = driver.find_element(By.TAG_NAME, "body").text or ""
        except Exception:
            return []
    lines, seen = [], set()
    for raw in text.splitlines():
        line = " ".join(raw.split())
        if not line or line in seen:
            continue
        seen.add(line)
        lines.append(line)
        if len(lines) >= limit:
            break
    return lines

def page_error_code(lines):
    """上游把结论写在 error_code 里；有它就不用再猜。"""
    for line in lines:
        m = re.search(r"error_code[:\s]*([A-Za-z_]+)", line)
        if m:
            return m.group(1)
    return None

def describe_page(driver):
    """拼一段能直接贴给人看的话：错误码 + 页面原文 + 地址。"""
    lines = page_notice(driver)
    try:
        url = driver.current_url
    except Exception:
        url = ""
    out = []
    code = page_error_code(lines)
    if code:
        out.append("上游错误码：%s" % code)
    if lines:
        out.append("上游页面原文：")
        out.extend("  " + line for line in lines)
    else:
        out.append("上游页面没有可见文字")
    out.append("页面：%s" % url[:180])
    return "\n".join(out)

def page_looks_final(driver):
    """当前页是不是「上游已经给结论」的说明页。

    判据是结构性的，不靠猜文案：登录流程每一页都有输入框，
    说明页一个都没有（只剩「重试」和诊断信息）。
    """
    lines = page_notice(driver)
    if page_error_code(lines):
        return True
    try:
        from selenium.webdriver.common.by import By
        boxes = driver.find_elements(By.CSS_SELECTOR, "input, textarea")
        if any(box.is_displayed() for box in boxes):
            return False
    except Exception:
        return False
    return len(lines) >= 2

def fill_input(driver, element, text, retries=3):
    """往受控输入框写值并校验；send_keys 偶尔写不进去（React 水合时序），失败改用 JS 原生 setter。"""
    from selenium.webdriver.common.by import By as _By
    for _ in range(retries):
        try:
            element.clear()
        except Exception:
            pass
        try:
            element.click()
            element.send_keys(text)
        except Exception:
            pass
        time.sleep(0.3)
        try:
            if (element.get_attribute("value") or "") == text:
                return True
        except Exception:
            pass
    # 兜底：JS 原生 setter + input/change 事件（React 受控组件认这个）
    try:
        driver.execute_script(
            """
            const el = arguments[0], v = arguments[1];
            const setter = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(el), 'value').set;
            setter.call(el, v);
            el.dispatchEvent(new Event('input', { bubbles: true }));
            el.dispatchEvent(new Event('change', { bubbles: true }));
            """,
            element, text)
        time.sleep(0.3)
        return (element.get_attribute("value") or "") == text
    except Exception:
        return False

def click_continue(driver):
    """点「继续」：精确匹配，避免误点「使用 Google 账户继续」这类社交按钮。"""
    from selenium.webdriver.common.by import By as _By
    for b in driver.find_elements(_By.CSS_SELECTOR, "button"):
        t = (b.text or "").strip()
        if t in ("继续", "Continue"):
            b.click()
            return True
    for b in driver.find_elements(_By.CSS_SELECTOR, 'form button[type="submit"], button[type="submit"]'):
        t = (b.text or "").strip()
        if any(k in t for k in ("Google", "Apple", "Microsoft", "电话", "使用")):
            continue
        b.click()
        return True
    return False


CLIENT_ID = "app_2SKx67EdpoN0G6j64rFvigXD"
REDIRECT_URI = "https://platform.openai.com/auth/callback"

def find_chrome():
    for p in [os.environ.get("CHROME_PATH"),
              r"C:\Program Files\Google\Chrome\Application\chrome.exe",
              r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
              os.path.expandvars(r"%LOCALAPPDATA%\Google\Chrome\Application\chrome.exe"),
              "/usr/bin/google-chrome", "/usr/bin/chromium",
              "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"]:
        if p and os.path.exists(p):
            return p
    return None


def b64url(b): return base64.urlsafe_b64encode(b).decode().rstrip("=")

def totp(seed):
    ab = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567"
    bits = "".join(format(ab.index(c), "05b") for c in re.sub(r"[^A-Z2-7]", "", seed.upper()))
    key = bytes(int(bits[i:i + 8], 2) for i in range(0, len(bits), 8))
    h = hmac.new(key, int(time.time() // 30).to_bytes(8, "big"), hashlib.sha1).digest()
    off = h[-1] & 0x0F
    return str((int.from_bytes(h[off:off + 4], "big") & 0x7FFFFFFF) % 1_000_000).zfill(6)

def authorize_url():
    verifier = b64url(secrets.token_bytes(32))
    challenge = b64url(hashlib.sha256(verifier.encode()).digest())
    params = {
        "issuer": "https://auth.openai.com", "client_id": CLIENT_ID,
        "audience": "https://api.openai.com/v1", "redirect_uri": REDIRECT_URI,
        "screen_hint": "login_or_signup", "scope": "openid profile email offline_access",
        "response_type": "code", "response_mode": "query",
        "state": "batch-" + str(int(time.time() * 1000)),
        "nonce": "n-" + str(int(time.time())), "code_challenge": challenge,
        "code_challenge_method": "S256",
    }
    return "https://auth.openai.com/api/accounts/authorize?" + urlencode(params), verifier

def exchange(code, verifier):
    data = urlencode({"grant_type": "authorization_code", "client_id": CLIENT_ID,
                      "code": code, "redirect_uri": REDIRECT_URI,
                      "code_verifier": verifier}).encode()
    req = urllib.request.Request("https://auth.openai.com/oauth/token", data=data,
                                 headers={"content-type": "application/x-www-form-urlencoded"})
    with urllib.request.urlopen(req, timeout=40) as r:
        return json.loads(r.read())

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--file", default="accounts.json")
    ap.add_argument("--out", default="sessions.jsonl")
    ap.add_argument("--limit", type=int, default=0)
    ap.add_argument("--start", type=int, default=0)
    ap.add_argument("--delay", type=float, default=6.0)
    ap.add_argument("--max-cf-fails", type=int, default=3)
    ap.add_argument("--headless", action="store_true")
    ap.add_argument("--skip-ok", action="store_true", help="跳过 --out 里已有 token 的账号")
    a = ap.parse_args()

    import sys as _s
    uc_path = os.environ.get("UC_PATH")
    if uc_path and os.path.isdir(uc_path):
        _s.path.insert(0, uc_path)
    import undetected_chromedriver as uc
    from selenium.webdriver.common.by import By

    with open(a.file, "r", encoding="utf-8") as f:
        accounts = json.load(f)
    done_emails = set()
    if a.skip_ok and os.path.exists(a.out):
        with open(a.out, "r", encoding="utf-8") as f:
            for line in f:
                try:
                    rec = json.loads(line)
                    if rec.get("access_token"):
                        done_emails.add(rec["email"])
                except Exception:
                    pass
        accounts = [x for x in accounts if x["email"] not in done_emails]
        print("[跳过] 已有 token 的账号 %d 个，本次处理 %d 个" % (len(done_emails), len(accounts)), flush=True)

    accounts = accounts[a.start:]
    if a.limit:
        accounts = accounts[:a.limit]

    chrome = os.environ.get("CHROME_PATH") or find_chrome()

    def make_options():
        o = uc.ChromeOptions()
        o.binary_location = chrome
        o.add_argument("--window-size=1280,900")
        # 窗口挪到屏幕外：保持 headful（CF 认 headless），但不挡用户视线
        o.add_argument("--window-position=-32000,-32000")
        # 屏幕外窗口默认会被判定遮挡并节流（React 水合/定时器停摆），必须显式关闭
        o.add_argument("--disable-backgrounding-occluded-windows")
        o.add_argument("--disable-background-timer-throttling")
        o.add_argument("--disable-renderer-backgrounding")
        return o

    def launch(vm):
        return uc.Chrome(options=make_options(), headless=bool(a.headless), use_subprocess=True, version_main=vm)

    def start_driver():
        try:
            return launch(156)
        except Exception as e:
            m = re.search(r"Current browser version is (\d+)\.", str(e))
            if not m:
                raise
            return launch(int(m.group(1)))

    def browser_dead(err):
        s = str(err).lower()
        return ("no such window" in s or "web view not found" in s
                or "invalid session id" in s or "chrome not reachable" in s
                or "disconnected" in s)

    driver = start_driver()

    done, fails, cf_fails, total_secs = 0, 0, 0, 0.0
    out = open(a.out, "a", encoding="utf-8")

    def login_one(acct):
        url, verifier = authorize_url()
        driver.execute_cdp_cmd("Network.clearBrowserCookies", {})
        driver.get(url)
        box = None
        for _ in range(40):
            try:
                box = driver.find_element(By.CSS_SELECTOR, 'input[type="email"], input[name="email"]')
                break
            except Exception:
                time.sleep(0.4)
        if box is None:
            body = driver.find_element(By.TAG_NAME, "body").text[:120].replace("\n", " ")
            raise RuntimeError("CF 拦截或无登录页：" + body[:60])

        fill_input(driver, box, acct["email"]); time.sleep(0.35)
        click_continue(driver)
        time.sleep(2.0)
        for el in driver.find_elements(By.CSS_SELECTOR, "button, a"):
            if "使用密码" in el.text:
                el.click(); time.sleep(1.2); break

        pwd = None
        for _ in range(30):
            try:
                pwd = driver.find_element(By.CSS_SELECTOR, 'input[type="password"]')
                break
            except Exception:
                time.sleep(0.4)
        if pwd is None:
            raise RuntimeError("没到密码页；" + describe_page(driver))
        fill_input(driver, pwd, acct["password"]); time.sleep(0.35)
        click_continue(driver)
        time.sleep(2.5)
        txt = driver.find_element(By.TAG_NAME, "body").text
        if "密码错误" in txt or "incorrect" in txt.lower():
            raise RuntimeError("密码错误；" + describe_page(driver))

        code_input = None
        for _ in range(30):
            for sel in ['input[autocomplete*="one-time-code"]', 'input[inputmode="numeric"]',
                        'input[name="code"]', 'input[type="tel"]']:
                try:
                    code_input = driver.find_element(By.CSS_SELECTOR, sel); break
                except Exception:
                    pass
            if code_input:
                break
            time.sleep(0.4)
        if code_input is None:
            raise RuntimeError("没到 TOTP 页；" + describe_page(driver))
        fill_input(driver, code_input, totp(acct["totp"])); time.sleep(0.35)
        click_continue(driver)

        auth_code = None
        for tick in range(60):
            time.sleep(0.7)
            m = re.search(r"[?&]code=([^&]+)", driver.current_url)
            if m:
                auth_code = m.group(1); break
            # 卡在说明页就别干等：命中就把上游原话带进 error 字段
            if tick >= 5 and tick % 4 == 0 and page_looks_final(driver):
                break
        if not auth_code:
            raise RuntimeError("没拿到 code；" + describe_page(driver))
        return exchange(auth_code, verifier)

    try:
        for idx, acct in enumerate(accounts, 1):
            if cf_fails >= a.max_cf_fails:
                print("[停止] 连续 %d 个账号被 Cloudflare 拦，先歇一会儿再跑" % cf_fails, flush=True)
                break
            t0 = time.time()
            try:
                try:
                    tok = login_one(acct)
                except Exception as e:
                    if browser_dead(e):
                        print("  [重启浏览器] 上一个窗口没了，重新拉起…", flush=True)
                        try:
                            driver.quit()
                        except Exception:
                            pass
                        driver = start_driver()
                        time.sleep(2)
                        t0 = time.time()
                        tok = login_one(acct)
                    else:
                        raise
                secs = time.time() - t0
                rec = {"email": acct["email"], "access_token": tok.get("access_token"),
                       "refresh_token": tok.get("refresh_token"), "id_token": tok.get("id_token"),
                       "seconds": round(secs, 1)}
                done += 1; total_secs += secs; cf_fails = 0
                print("[%d/%d] ok  %s  %.1fs  平均 %.1fs" % (idx, len(accounts), acct["email"], secs, total_secs / done), flush=True)
            except Exception as e:
                secs = time.time() - t0
                # 新加的页面原文是多行，压成一行再截：日志一行一条才看得下去
                msg = " | ".join(str(e).splitlines())[:400]
                if "CF" in msg or "拦截" in msg:
                    cf_fails += 1
                fails += 1
                rec = {"email": acct["email"], "error": msg, "seconds": round(secs, 1)}
                print("[%d/%d] 失败 %s  %.1fs  %s" % (idx, len(accounts), acct["email"], secs, msg), flush=True)
            out.write(json.dumps(rec, ensure_ascii=False) + "\n"); out.flush()
            time.sleep(a.delay)
    finally:
        out.close()
        try: driver.quit()
        except Exception: pass
        print("\n完成：成功 %d，失败 %d；平均每个成功账号 %.1f 秒" % (
            done, fails, (total_secs / done) if done else 0), flush=True)

if __name__ == "__main__":
    main()