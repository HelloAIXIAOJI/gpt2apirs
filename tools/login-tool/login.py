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

    # 驱动版本要和浏览器大版本匹配；取不到版本号时，从报错里解析后重试一次
    def launch(vm):
        return uc.Chrome(options=make_options(), headless=bool(a.headless),
                         use_subprocess=True, version_main=vm)

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

        box.send_keys(creds["email"]); time.sleep(0.5)
        for b in driver.find_elements(By.CSS_SELECTOR, "button"):
            if "继续" in b.text or "Continue" in b.text:
                b.click(); break
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
            die("没到密码页。这个账号可能没设密码，或需要邮箱验证码；请浏览器手动登录后用 Session JSON 导入")

        pwd.send_keys(creds["password"]); time.sleep(0.5)
        for b in driver.find_elements(By.CSS_SELECTOR, "button"):
            if "继续" in b.text or "Continue" in b.text:
                b.click(); break
        time.sleep(4)
        print("③ 已提交密码", flush=True)

        body_text = driver.find_element(By.TAG_NAME, "body").text
        if "密码错误" in body_text or "incorrect" in body_text.lower():
            die("密码不正确（为免触发风控不重试）")

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
            die("没到两步验证页（账号可能没开 TOTP）")

        code = totp(creds["totp"])
        code_input.send_keys(code); time.sleep(0.5)
        for b in driver.find_elements(By.CSS_SELECTOR, "button"):
            if "继续" in b.text or "Continue" in b.text:
                b.click(); break
        print("④ 已提交 TOTP（%s）" % code, flush=True)

        auth_code = None
        for _ in range(90):
            time.sleep(1)
            m = re.search(r"[?&]code=([^&]+)", driver.current_url)
            if m:
                auth_code = m.group(1); break
        if not auth_code:
            die("没拿到授权 code，当前页面：" + driver.current_url[:120])
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