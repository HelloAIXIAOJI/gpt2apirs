#!/usr/bin/env node
/**
 * 本地登录器：邮箱 + 密码 + TOTP 种子 -> OpenAI 平台 OAuth token
 *
 * 用法:
 *   node login.mjs --email a@b.com --password 'xxx' --totp BASE32SEED
 *   node login.mjs --file creds.json          # {"email":"...","password":"...","totp":"..."}
 *   node login.mjs --file creds.json --out tokens.json
 *   node login.mjs --file creds.json --proxy 127.0.0.1:7890   # 指定出口（推荐：登录用干净 IP）
 *   node login.mjs --file creds.json --headful               # 显示窗口（推荐：headless 会被 CF 识别）
 *
 * 输出: 一段可直接粘到后台「粘贴导入」的 JSON（access_token / refresh_token / id_token / email）
 *
 * 依赖: 本机已装 Chrome 或 Edge；Node 22+（自带 fetch / WebSocket）。无需 npm install。
 */
import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import crypto from "node:crypto";

const CLIENT_ID = "app_2SKx67EdpoN0G6j64rFvigXD";
const REDIRECT_URI = "https://platform.openai.com/auth/callback";
const UA = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const STEP_TIMEOUT_MS = 45_000;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const log = (...a) => console.log(...a);
const fail = (msg) => { console.error("失败：" + msg); process.exit(1); };

// ---------- 参数 ----------
function arg(name) {
  const i = process.argv.indexOf("--" + name);
  return i >= 0 ? process.argv[i + 1] : undefined;
}
let creds = {
  email: arg("email"),
  password: arg("password"),
  totp: arg("totp"),
};
const proxy = arg("proxy"); // 可选：登录走指定代理，例如 127.0.0.1:7890
// 默认 headful（可见窗口）：Cloudflare 会识别 headless，headless 模式在本机实测被 403
const headful = process.argv.includes("--headful");
const file = arg("file");
if (file) {
  if (!existsSync(file)) fail("找不到凭据文件 " + file);
  creds = { ...creds, ...JSON.parse(readFileSync(file, "utf8")) };
}
if (!creds.email || !creds.password || !creds.totp) {
  fail("缺少参数：需要 --email / --password / --totp，或 --file creds.json（含这三个键）");
}

// ---------- 工具 ----------
const b64url = (buf) => Buffer.from(buf).toString("base64").replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
function totp(seed) {
  const ab = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
  const clean = String(seed).toUpperCase().replace(/[^A-Z2-7]/g, "");
  let bits = "";
  for (const ch of clean) {
    const v = ab.indexOf(ch);
    if (v < 0) throw new Error("TOTP 种子不是合法 base32：" + ch);
    bits += v.toString(2).padStart(5, "0");
  }
  const key = Buffer.from(bits.match(new RegExp(".{8}", "g")).map((b) => parseInt(b, 2)));
  const counter = Buffer.alloc(8);
  counter.writeBigUInt64BE(BigInt(Math.floor(Date.now() / 1000 / 30)));
  const h = crypto.createHmac("sha1", key).update(counter).digest();
  const off = h[h.length - 1] & 0x0f;
  return ((h.readUInt32BE(off) & 0x7fffffff) % 1e6).toString().padStart(6, "0");
}

function findBrowser() {
  const candidates = [
    process.env.CHROME_PATH,
    "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
    "C:\\Program Files (x86)\\Google\\Chrome\\Application\\chrome.exe",
    "C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe",
    "C:\\Program Files\\Microsoft\\Edge\\Application\\msedge.exe",
    "/usr/bin/google-chrome",
    "/usr/bin/chromium",
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  ].filter(Boolean);
  const found = candidates.find((p) => existsSync(p));
  if (!found) fail("没找到 Chrome/Edge，可用 CHROME_PATH 指定浏览器路径");
  return found;
}

// ---------- CDP ----------
async function cdp(port) {
  let version = null;
  for (let i = 0; i < 60; i++) {
    try { version = await (await fetch("http://127.0.0.1:" + port + "/json/version")).json(); break; } catch { await sleep(250); }
  }
  if (!version) fail("浏览器调试端口没起来");
  const target = await (await fetch("http://127.0.0.1:" + port + "/json/new?about:blank", { method: "PUT" })).json();
  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((r) => ws.addEventListener("open", r, { once: true }));
  let id = 0;
  const pending = new Map();
  const events = [];
  ws.addEventListener("message", (e) => {
    const m = JSON.parse(e.data);
    if (m.id && pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id); return; }
    if (m.method) events.push(m);
  });
  const send = (method, params = {}) =>
    new Promise((res) => { const mid = ++id; pending.set(mid, res); ws.send(JSON.stringify({ id: mid, method, params })); });
  const evaluate = async (expression) => {
    const r = await send("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true });
    return r.result && r.result.result ? r.result.result.value : undefined;
  };
  const waitFor = async (expression, timeoutMs = STEP_TIMEOUT_MS) => {
    const end = Date.now() + timeoutMs;
    while (Date.now() < end) { if (await evaluate(expression)) return true; await sleep(400); }
    return false;
  };
  const typeInto = async (selector, text) => {
    await send("Runtime.evaluate", { expression: "(() => { const el = document.querySelector(" + JSON.stringify(selector) + "); if (el) el.focus(); })()" });
    await send("Input.insertText", { text });
  };
  const clickText = (text) =>
    evaluate("(() => { const re = new RegExp('\\\\s+', 'g'); const els = Array.from(document.querySelectorAll('button, a, [role=button]')); const el = els.find((e) => ((e.innerText || '').replace(re, ' ')).trim().includes(" + JSON.stringify(text) + ")); if (!el) return false; el.click(); return true; })()");
  return { send, evaluate, waitFor, typeInto, clickText, close: () => { try { ws.close(); } catch {} }, events };
}

// ---------- 主流程 ----------
const browserPath = findBrowser();
const port = 9400 + Math.floor(Math.random() * 400);
const profileDir = mkdtempSync(join(tmpdir(), "openai-login-"));
const child = spawn(browserPath, [
  ...(headful ? [] : ["--headless=new"]),
  ...(proxy ? ["--proxy-server=" + proxy] : []),
  "--remote-debugging-port=" + port,
  "--user-data-dir=" + profileDir,
  "--no-first-run",
  "--no-default-browser-check",
  "--disable-gpu",
  "--disable-extensions",
  "about:blank",
], { stdio: "ignore" });

const cleanup = () => {
  try { child.kill(); } catch {}
  setTimeout(() => { try { rmSync(profileDir, { recursive: true, force: true }); } catch {} }, 500);
};
process.on("exit", cleanup);

try {
  const c = await cdp(port);
  await c.send("Page.enable");
  await c.send("Runtime.enable");
  await c.send("Network.enable", { maxPostDataSize: 1000000 });
  await c.send("Network.setUserAgentOverride", { userAgent: UA });

  // 自己的 PKCE
  const verifier = b64url(crypto.randomBytes(32));
  const challenge = b64url(crypto.createHash("sha256").update(verifier).digest());
  const state = "local-" + Date.now();
  const params = new URLSearchParams({
    issuer: "https://auth.openai.com",
    client_id: CLIENT_ID,
    audience: "https://api.openai.com/v1",
    redirect_uri: REDIRECT_URI,
    screen_hint: "login_or_signup",
    scope: "openid profile email offline_access",
    response_type: "code",
    response_mode: "query",
    state,
    nonce: "n-" + Date.now(),
    code_challenge: challenge,
    code_challenge_method: "S256",
  });

  log("① 打开登录页…");
  await c.send("Page.navigate", { url: "https://auth.openai.com/api/accounts/authorize?" + params.toString() });
  const emailVisible = await c.waitFor("!!document.querySelector('input[type=email], input[name=email]')");
  if (!emailVisible) {
    const shown = await c.evaluate("(document.body.innerText || '').replace(new RegExp('\\\\s+', 'g'), ' ').slice(0, 200)");
    fail("登录页没出现邮箱框。页面显示：" + shown + "\n（常见原因：Cloudflare 拦截，隔几分钟再试；或该出口被限制）");
  }

  log("② 提交邮箱…");
  await c.typeInto('input[type=email], input[name=email]', creds.email);
  await sleep(300);
  if (!(await c.clickText("继续"))) fail("没找到「继续」按钮");
  await sleep(3000);

  // 若出现「使用密码继续」就先点它
  if (await c.evaluate("Array.from(document.querySelectorAll('button, a')).some((e) => (e.innerText || '').includes('使用密码'))")) {
    await c.clickText("使用密码");
    await sleep(2000);
  }
  const pwdVisible = await c.waitFor("!!document.querySelector('input[type=password]')");
  if (!pwdVisible) {
    const shown = await c.evaluate("(document.body.innerText || '').replace(new RegExp('\\\\s+', 'g'), ' ').slice(0, 200)");
    fail("没到密码页。页面显示：" + shown + "\n（这个号可能没设密码，或需要邮箱验证码；请用浏览器手动登录后改用 Session JSON 导入）");
  }

  log("③ 提交密码…");
  await c.typeInto('input[type=password]', creds.password);
  await sleep(300);
  if (!(await c.clickText("继续"))) fail("没找到「继续」按钮");
  await sleep(3500);

  const pwdError = await c.evaluate("(document.body.innerText || '').includes('密码错误') || (document.body.innerText || '').toLowerCase().includes('incorrect')");
  if (pwdError) fail("密码不正确（或该账号要求先改密码）。为免触发风控，脚本不再重试。");

  const mfaVisible = await c.waitFor("!!document.querySelector('input[autocomplete*=\"one-time-code\"], input[inputmode=\"numeric\"], input[name=\"code\"], input[type=\"tel\"]')", 15000);
  if (!mfaVisible) fail("没到两步验证页（这个账号可能没开 TOTP，或流程有变化）");

  const code = totp(creds.totp);
  log("④ 提交 TOTP（" + code + "）…");
  await c.typeInto('input[autocomplete*="one-time-code"], input[inputmode="numeric"], input[name="code"], input[type="tel"]', code);
  await sleep(300);
  if (!(await c.clickText("继续"))) fail("没找到「继续」按钮");

  // 等回调 URL 出现（可能停在 platform.openai.com/auth/callback?code=…）
  let authCode = null;
  const end = Date.now() + STEP_TIMEOUT_MS;
  while (Date.now() < end && !authCode) {
    const href = await c.evaluate("location.href");
    if (typeof href === "string" && href.includes("code=")) {
      const u = new URL(href);
      if (u.searchParams.get("code")) authCode = u.searchParams.get("code");
    }
    if (!authCode) await sleep(500);
  }
  if (!authCode) {
    const href = await c.evaluate("location.href");
    fail("没拿到授权 code，当前页面：" + href);
  }
  log("⑤ 拿到授权 code，换取 token…");

  const form = new URLSearchParams({
    grant_type: "authorization_code",
    client_id: CLIENT_ID,
    code: authCode,
    redirect_uri: REDIRECT_URI,
    code_verifier: verifier,
  });
  const res = await fetch("https://auth.openai.com/oauth/token", {
    method: "POST",
    headers: { "content-type": "application/x-www-form-urlencoded", "user-agent": UA, accept: "application/json" },
    body: form.toString(),
  });
  const body = await res.text();
  if (res.status !== 200) fail("换 token 失败（HTTP " + res.status + "）：" + body.slice(0, 300));

  const tok = JSON.parse(body);
  const out = {
    email: creds.email,
    access_token: tok.access_token,
    refresh_token: tok.refresh_token,
    id_token: tok.id_token,
  };
  const outfile = arg("out");
  if (outfile) {
    writeFileSync(outfile, JSON.stringify(out, null, 2), "utf8");
    log("\n已写入 " + outfile);
  }
  log("\n=== 复制下面这一段，粘到后台「粘贴导入」===");
  log(JSON.stringify({ access_token: out.access_token, refresh_token: out.refresh_token, id_token: out.id_token, email: out.email }));
  log("\n有效期：" + Math.round((tok.expires_in ?? 0) / 86400) + " 天；refresh_token " + (out.refresh_token ? "已获取" : "未返回"));
  c.close();
} finally {
  cleanup();
}