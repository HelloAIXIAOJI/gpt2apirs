#!/usr/bin/env python3
"""
本地登录器（undetected_chromedriver 版）

为什么用 uc 而不是 Node + CDP：
  Cloudflare 能识别 CDP 连接（我们实测无头/有头 Edge、patchright 都被 403），
  而 undetected_chromedriver 用打补丁的 chromedriver 驱动，实测能过。

用法：
  python login.py --file creds.json                 # 默认可见窗口（推荐）
  python login.py --file creds.json --out tokens.json
  python login.py --file creds.json --proxy 127.0.0.1:7890
  python login.py --email a@b.com --password 'xxx' --totp BASE32SEED

输出：一段可直接粘到后台「粘贴导入」的 JSON。
依赖：pip install undetected-chromedriver selenium requests
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

def page_error_code(lines, driver=None):
    """上游把结论写在 error_code 里；有它就不用再猜。

    innerText 里常常没有：错误码在「诊断信息」折叠块里，用户不点开就看不到。
    所以再翻一遍源码 —— DOM 里有，只是没渲染成可见文字。
    """
    for line in lines:
        m = re.search(r"error_code[:\s]*([A-Za-z_]+)", line)
        if m:
            return m.group(1)
    if driver is not None:
        try:
            m = re.search(r'error_code["\\\s:>]*([A-Za-z_]{4,})', driver.page_source)
            if m:
                return m.group(1)
        except Exception:
            pass
    return None

def describe_page(driver):
    """拼一段能直接贴给人看的话：错误码 + 页面原文 + 地址。"""
    lines = page_notice(driver)
    try:
        url = driver.current_url
    except Exception:
        url = ""
    out = []
    code = page_error_code(lines, driver)
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
    说明页一个都没有（只剩「重试」和诊断信息）。这样封号、停用、
    需要人工验证、地区限制……都覆盖得到。
    """
    lines = page_notice(driver)
    if page_error_code(lines, driver):
        return True
    # 数「真的能输入的控件」：React 页面里留着 opacity:0 的旧输入框，
    # Selenium 的 is_displayed() 认为它可见，据此判断会把说明页当成流程页。
    try:
        usable = driver.execute_script(
            "const els = [...document.querySelectorAll('input,textarea')];"
            "return els.filter(el => {"
            "  const s = getComputedStyle(el), r = el.getBoundingClientRect();"
            "  return el.type !== 'hidden' && s.display !== 'none' && s.visibility !== 'hidden'"
            "    && s.opacity !== '0' && r.width > 1 && r.height > 1;"
            "}).length;"
        )
    except Exception:
        usable = 0
    if usable:
        return False
    return len(lines) >= 2

def die_with_page(driver, reason, tag="notice"):
    """失败时把上游页面上的提示原样说出来。

    只说「没拿到 code」等于把唯一的信息扔掉：上游若给出结论，
    用户看一眼就知道该换号、该改密码、还是该等冷却。
    """
    dump_page(driver, tag)
    die(reason + "\n" + describe_page(driver))

def wait_next_totp_window():
    """等到下一个 30 秒窗口：MFA 重试时不能复用刚刚被拒的同一个码。"""
    cur = int(time.time()) // 30
    while int(time.time()) // 30 == cur:
        time.sleep(1)

def dump_page(driver, tag):
    """失败时把页面存一份：看不清页面就只能猜，猜是最贵的。"""
    try:
        path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "fail-%s.html" % tag)
        with open(path, "w", encoding="utf-8") as f:
            f.write("<!-- %s -->\n" % driver.current_url)
            f.write(driver.page_source)
        print("（页面已存 %s）" % path, flush=True)
    except Exception:
        pass

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

def b64url(b): return base64.urlsafe_b64encode(b).decode().rstrip("=")

def totp(seed):
    ab = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567"
    clean = re.sub(r"[^A-Z2-7]", "", str(seed).upper())
    bits = "".join(format(ab.index(c), "05b") for c in clean)
    key = bytes(int(bits[i:i + 8], 2) for i in range(0, len(bits), 8))
    h = hmac.new(key, int(time.time() // 30).to_bytes(8, "big"), hashlib.sha1).digest()
    off = h[-1] & 0x0F
    return str((int.from_bytes(h[off:off + 4], "big") & 0x7FFFFFFF) % 1_000_000).zfill(6)

def die(msg):
    print("失败：" + msg, file=sys.stderr)
    sys.exit(1)

def find_chrome():
    cands = [os.environ.get("CHROME_PATH"),
             r"C:\Program Files\Google\Chrome\Application\chrome.exe",
             r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
             os.path.expandvars(r"%LOCALAPPDATA%\Google\Chrome\Application\chrome.exe"),
             "/usr/bin/google-chrome", "/usr/bin/chromium",
             "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"]
    for p in cands:
        if p and os.path.exists(p):
            return p
    return None

def chrome_major(path):
    if not path:
        return None
    try:
        import subprocess
        out = subprocess.run([path, "--version"], capture_output=True, text=True, timeout=20).stdout
        m = re.search(r"(\d+)\.", out)
        return int(m.group(1)) if m else None
    except Exception:
        return None

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--file")
    ap.add_argument("--email"); ap.add_argument("--password"); ap.add_argument("--totp")
    ap.add_argument("--out")
    ap.add_argument("--proxy")
    ap.add_argument("--headless", action="store_true", help="不推荐：headless 更容易被 CF 识别")
    a = ap.parse_args()

    creds = {"email": a.email, "password": a.password, "totp": a.totp}
    if a.file:
        with open(a.file, "r", encoding="utf-8") as f:
            creds.update(json.load(f))
    if not all(creds.get(k) for k in ("email", "password", "totp")):
        die("需要 email / password / totp（命令行或 --file 的 JSON）")

    # Python 3.12+ 上 pip 版 uc 会因 distutils 移除而 import 失败，
    # 支持用 UC_PATH 指向一份兼容副本（例如 FlareSolverr 自带的 vendored 版）
    try:
        import undetected_chromedriver as uc
    except ImportError:
        extra = os.environ.get("UC_PATH")
        if extra and os.path.isdir(extra):
            sys.path.insert(0, extra)
        try:
            import undetected_chromedriver as uc
        except ImportError as e:
            die("缺依赖 undetected_chromedriver（%s）；Python 3.12+ 可设 UC_PATH 指向兼容副本" % e)
    from selenium.webdriver.common.by import By

    verifier = b64url(secrets.token_bytes(32))
    challenge = b64url(hashlib.sha256(verifier.encode()).digest())
    params = {
        "issuer": "https://auth.openai.com", "client_id": CLIENT_ID,
        "audience": "https://api.openai.com/v1", "redirect_uri": REDIRECT_URI,
        "screen_hint": "login_or_signup", "scope": "openid profile email offline_access",
        "response_type": "code", "response_mode": "query",
        "state": "local-" + str(int(time.time())), "nonce": "n-" + str(int(time.time())),
        "code_challenge": challenge, "code_challenge_method": "S256",
    }
    url = "https://auth.openai.com/api/accounts/authorize?" + urlencode(params)

    chrome = find_chrome()

    def make_options():
        # uc 不允许复用 options，每次启动都要新建
        o = uc.ChromeOptions()
        if chrome:
            o.binary_location = chrome
        o.add_argument("--window-size=1280,900")
        if a.proxy:
            o.add_argument("--proxy-server=" + a.proxy)
        return o

    # 驱动版本要和浏览器大版本匹配；取不到版本号时，从报错里解析后重试一次。
    # driver_executable_path 给了就直接用本机那一份：默认行为要联网确认驱动版本，
    # 那台机器下载被挡时会报 SSL EOF（实测），而缓存里往往已经有匹配的驱动。
    driver_path = os.environ.get("CHROMEDRIVER_PATH") or None

    def launch(vm):
        return uc.Chrome(options=make_options(), headless=bool(a.headless),
                         use_subprocess=True, version_main=vm,
                         driver_executable_path=driver_path)

    try:
        driver = launch(chrome_major(chrome))
    except Exception as e:
        m = re.search(r"Current browser version is (\d+)\.", str(e))
        if not m:
            raise
        print("（驱动版本不匹配，按浏览器真实版本 %s 重试）" % m.group(1), flush=True)
        driver = launch(int(m.group(1)))
    try:
        print("① 打开登录页…", flush=True)
        driver.get(url)
        box = None
        for _ in range(60):
            try:
                box = driver.find_element(By.CSS_SELECTOR, 'input[type="email"], input[name="email"]')
                break
            except Exception:
                time.sleep(0.5)
        if box is None:
            body = driver.find_element(By.TAG_NAME, "body").text[:160].replace("\n", " ")
            die("登录页没出现邮箱框。页面显示：" + body + "\n（常见原因：出口 IP 被 Cloudflare 拦，换出口或稍后再试）")

        fill_input(driver, box, creds["email"]); time.sleep(0.5)
        click_continue(driver)
        time.sleep(3)
        print("② 已提交邮箱", flush=True)

        for el in driver.find_elements(By.CSS_SELECTOR, "button, a"):
            if "使用密码" in el.text:
                el.click(); time.sleep(2); break

        pwd = None
        for _ in range(40):
            try:
                pwd = driver.find_element(By.CSS_SELECTOR, 'input[type="password"]')
                break
            except Exception:
                time.sleep(0.5)
        if pwd is None:
            die_with_page(driver, "没到密码页（可能没设密码，或需要邮箱验证码）", "no_password_page")

        fill_input(driver, pwd, creds["password"]); time.sleep(0.5)
        click_continue(driver)
        time.sleep(4)
        print("③ 已提交密码", flush=True)

        body_text = driver.find_element(By.TAG_NAME, "body").text
        if "密码错误" in body_text or "incorrect" in body_text.lower():
            die_with_page(driver, "密码不正确（为免触发风控不重试）", "bad_password")

        code_input = None
        for _ in range(40):
            for sel in ['input[autocomplete*="one-time-code"]', 'input[inputmode="numeric"]',
                        'input[name="code"]', 'input[type="tel"]']:
                try:
                    code_input = driver.find_element(By.CSS_SELECTOR, sel)
                    break
                except Exception:
                    pass
            if code_input:
                break
            time.sleep(0.5)
        if code_input is None:
            die_with_page(driver, "没到两步验证页（账号可能没开 TOTP）", "no_totp_page")

        code = totp(creds["totp"])
        fill_input(driver, code_input, code); time.sleep(0.5)
        click_continue(driver)
        print("④ 已提交 TOTP（%s）" % code, flush=True)

        auth_code = None
        # 有些号在 TOTP 之后还会再弹一道 MFA（mfa-challenge）：再填一次即可。
        # 必须等下一个 30 秒窗口，否则填进去的还是刚用过的那 6 位，必然再被拒。
        extra_mfa = 0
        for tick in range(90):
            time.sleep(1)
            # 卡在说明页就别干等 90 秒：前 4 秒是跳转过渡，不做判定。
            # 命中就直接把上游原话报出去（封号、停用都是这一条路）。
            if tick >= 4 and tick % 3 == 0 and page_looks_final(driver):
                die_with_page(driver, "上游没给授权 code，停在了说明页", "notice_page")
            m = re.search(r"[?&]code=([^&]+)", driver.current_url)
            if m:
                auth_code = m.group(1); break
            if "mfa-challenge" in driver.current_url and extra_mfa < 3:
                extra_mfa += 1
                wait_next_totp_window()
                box = None
                for sel in ['input[autocomplete*="one-time-code"]', 'input[inputmode="numeric"]',
                            'input[name="code"]', 'input[type="tel"]']:
                    try:
                        box = driver.find_element(By.CSS_SELECTOR, sel)
                        break
                    except Exception:
                        pass
                if box is not None:
                    fresh = totp(creds["totp"])
                    fill_input(driver, box, fresh); time.sleep(0.5)
                    click_continue(driver)
                    print("④+ 又一道 MFA，已再提交 TOTP（%s）" % fresh, flush=True)
                continue
        if not auth_code:
            die_with_page(driver, "没拿到授权 code", "no_auth_code")
        print("⑤ 拿到授权 code，换 token…", flush=True)

        data = urlencode({"grant_type": "authorization_code", "client_id": CLIENT_ID,
                          "code": auth_code, "redirect_uri": REDIRECT_URI,
                          "code_verifier": verifier}).encode()
        req = urllib.request.Request("https://auth.openai.com/oauth/token", data=data,
                                     headers={"content-type": "application/x-www-form-urlencoded"})
        try:
            with urllib.request.urlopen(req, timeout=40) as r:
                tok = json.loads(r.read())
        except Exception as e:
            die("换 token 失败：" + str(e)[:200])

        out = {"email": creds["email"], "access_token": tok.get("access_token"),
               "refresh_token": tok.get("refresh_token"), "id_token": tok.get("id_token")}
        if a.out:
            with open(a.out, "w", encoding="utf-8") as f:
                json.dump(out, f, ensure_ascii=False, indent=2)
            print("\n已写入 " + a.out)
        print("\n=== 复制下面这段，粘到后台「粘贴导入」===")
        print(json.dumps({k: out[k] for k in ("access_token", "refresh_token", "id_token", "email")}, ensure_ascii=False))
        print("\n有效期 %s 天；refresh_token %s" % (
            round((tok.get("expires_in") or 0) / 86400), "已获取" if out["refresh_token"] else "未返回"))
    finally:
        try:
            driver.quit()
        except Exception:
            pass

if __name__ == "__main__":
    main()