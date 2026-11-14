#!/usr/bin/env node
// 管理后台端到端冒烟：用真浏览器登录，逐页检查渲染与控制台异常。
//
// 为什么要它：前端是手写 JS，没有类型检查，语法错误能让整页静默失效
// （历史上 app.js 解析失败过一次，页面只剩空壳）。node --check 只挡语法，
// 这个脚本挡「页面能打开但没数据」。
//
// 用法：先起服务，然后
//   node tools/admin_e2e.mjs --key <ADMIN_KEY>
// 参数：--base 默认 http://127.0.0.1:8000；--key 也可用环境变量 ADMIN_KEY；--headed 显示窗口。

import { spawn } from 'node:child_process';
import { existsSync } from 'node:fs';

const args = process.argv.slice(2);
const arg = (name, fallback) => {
  const index = args.indexOf('--' + name);
  if (index >= 0 && args[index + 1]) return args[index + 1];
  return fallback;
};
const BASE = arg('base', 'http://127.0.0.1:8000');
const KEY = arg('key', process.env.ADMIN_KEY || '');
const HEADED = args.includes('--headed');

if (!KEY) {
  console.error('缺少 ADMIN_KEY：用 --key 传，或设环境变量 ADMIN_KEY');
  process.exit(2);
}

const CANDIDATES = [
  'C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe',
  'C:\\Program Files\\Microsoft\\Edge\\Application\\msedge.exe',
  'C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe',
  'C:\\Program Files (x86)\\Google\\Chrome\\Application\\chrome.exe',
];
const BROWSER = CANDIDATES.find((path) => existsSync(path));
if (!BROWSER) {
  console.error('找不到 Edge 或 Chrome');
  process.exit(2);
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const DEVTOOLS_PORT = 9330 + Math.floor(Math.random() * 200);

const browser = spawn(BROWSER, [
  HEADED ? '--new-window' : '--headless=new',
  '--disable-gpu',
  '--no-first-run',
  '--no-default-browser-check',
  '--remote-debugging-port=' + DEVTOOLS_PORT,
  '--user-data-dir=' + (process.env.TEMP || '.') + '\\admin-e2e-' + DEVTOOLS_PORT,
  '--window-size=1440,1000',
  'about:blank',
], { stdio: 'ignore', detached: true });
browser.unref();

let socket = null;
const cleanup = () => {
  try { if (socket) socket.close(); } catch {}
  try { process.kill(-browser.pid); } catch { try { browser.kill(); } catch {} }
};
process.on('exit', cleanup);
process.on('SIGINT', () => { cleanup(); process.exit(130); });

async function connect() {
  for (let i = 0; i < 80; i++) {
    await sleep(250);
    try {
      const list = await (await fetch('http://127.0.0.1:' + DEVTOOLS_PORT + '/json/list')).json();
      const page = list.find((target) => target.type === 'page');
      if (page) return page.webSocketDebuggerUrl;
    } catch {}
  }
  throw new Error('浏览器调试端口没起来');
}

const exceptions = [];
let pendingId = 1;
const waiters = new Map();
const send = (method, params = {}) => new Promise((resolve) => {
  const id = pendingId++;
  waiters.set(id, resolve);
  socket.send(JSON.stringify({ id, method, params }));
});
const evaluate = async (expression) => {
  const reply = await send('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true });
  if (reply.result && reply.result.exceptionDetails) {
    throw new Error(reply.result.exceptionDetails.text || '页面求值异常');
  }
  return reply.result && reply.result.result ? reply.result.result.value : undefined;
};

// 每页一个断言：返回 { ok, detail }
const PAGES = [
  {
    tab: 'overview',
    name: '概览',
    check: `(() => {
      const cards = document.querySelectorAll('#metrics .card').length;
      const rows = document.querySelectorAll('#errors tbody tr').length;
      const hours = document.getElementById('metrics-hours');
      return { ok: cards >= 6 && Boolean(hours), detail: '指标卡 ' + cards + ' 张，错误行 ' + rows + '，时间范围可选' };
    })()`,
  },
  {
    tab: 'accounts',
    name: '账号池',
    check: `(() => {
      const cards = document.querySelectorAll('#account-stats .card').length;
      const rows = document.querySelectorAll('#accounts tbody tr').length;
      const cols = document.querySelectorAll('#accounts thead th').length;
      const bulk = ['bulk-sync', 'bulk-refresh', 'bulk-export', 'bulk-delete']
        .every((id) => document.getElementById(id));
      const page = (document.getElementById('account-page') || {}).textContent || '';
      const filters = document.getElementById('account-keyword') && document.getElementById('account-status');
      const ok = cards >= 5 && cols >= 10 && bulk && filters && page.includes('页');
      return { ok, detail: '统计卡 ' + cards + '，列 ' + cols + '，行 ' + rows + '，分页「' + page.trim() + '」' };
    })()`,
  },
  {
    tab: 'logs',
    name: '调用日志',
    check: `(() => {
      const rows = document.querySelectorAll('#logs tbody tr').length;
      const page = (document.getElementById('logs-page') || {}).textContent || '';
      const filters = document.getElementById('log-path') && document.getElementById('log-hours');
      return { ok: rows >= 1 && filters && page.includes('页'), detail: '日志行 ' + rows + '，分页「' + page.trim() + '」' };
    })()`,
  },
  {
    tab: 'proxies',
    name: '代理分组',
    check: `(() => {
      const rows = document.querySelectorAll('#groups tbody tr').length;
      const hint = (document.querySelector('#group-hint') || {}).textContent || '';
      return { ok: rows >= 0, detail: '分组行 ' + rows + (hint.trim() ? '，提示有内容' : '') };
    })()`,
  },
  {
    tab: 'images',
    name: '图片',
    check: `(() => {
      const cards = document.querySelectorAll('#images > *').length;
      const summary = (document.querySelector('#images-summary') || {}).textContent || '';
      const page = (document.getElementById('images-page') || {}).textContent || '';
      return { ok: summary.length > 0 && page.includes('页'), detail: '图片块 ' + cards + '，分页「' + page.trim() + '」，摘要「' + summary.trim().slice(0, 24) + '」' };
    })()`,
  },
  {
    tab: 'settings',
    name: '设置',
    check: `(() => {
      const rows = document.querySelectorAll('#settings-groups tbody tr').length;
      const groups = document.querySelectorAll('#settings-groups h3').length;
      const save = document.getElementById('settings-save');
      return { ok: rows >= 15 && groups >= 4 && Boolean(save), detail: '设置行 ' + rows + '，分组 ' + groups };
    })()`,
  },
  {
    tab: 'keys',
    name: 'API Keys',
    check: `(() => {
      const rows = document.querySelectorAll('#keys tbody tr').length;
      return { ok: rows >= 1, detail: 'key 行 ' + rows };
    })()`,
  },
  {
    tab: 'backup',
    name: '备份',
    check: `(() => {
      const rows = document.querySelectorAll('#backups tbody tr').length;
      return { ok: rows >= 0, detail: '备份行 ' + rows };
    })()`,
  },
];

// 交互检查：渲染对了不代表按钮能用，这里点一遍关键按钮
const INTERACTIONS = [
  {
    name: '概览切换时间范围',
    steps: [
      "document.querySelector('#tabs button[data-tab=\"overview\"]').click()",
      "(() => { const select = document.getElementById('metrics-hours'); select.value = '1'; select.dispatchEvent(new Event('change')); return true; })()",
      "new Promise((resolve) => setTimeout(() => resolve(document.querySelectorAll('#metrics .card').length >= 6), 1200))",
    ],
  },
  {
    name: '日志重置清空筛选',
    steps: [
      "document.querySelector('#tabs button[data-tab=\"logs\"]').click()",
      "(() => { document.getElementById('log-path').value = '/v1/images'; document.getElementById('log-reset').click(); return true; })()",
      "new Promise((resolve) => setTimeout(() => resolve(document.getElementById('log-path').value === ''), 800))",
    ],
  },
  {
    name: '账号池打开并取消编辑弹窗',
    steps: [
      "document.querySelector('#tabs button[data-tab=\"accounts\"]').click()",
      "new Promise((resolve) => setTimeout(resolve, 900))",
      "(() => { const row = document.querySelector('#accounts tbody tr'); const selects = row.querySelectorAll('select'); const action = selects[selects.length - 1]; action.value = 'edit'; action.dispatchEvent(new Event('change')); return true; })()",
      "new Promise((resolve) => setTimeout(() => resolve(document.getElementById('account-dialog').open === true), 500))",
    ],
  },
  {
    name: '编辑弹窗取消后关闭',
    steps: [
      "(() => { document.getElementById('edit-cancel').click(); return true; })()",
      "new Promise((resolve) => setTimeout(() => resolve(document.getElementById('account-dialog').open === false), 500))",
    ],
  },
  {
    name: '分组账号数联动到账号池',
    steps: [
      // 池里可能一个分组都没有，先造一个临时分组，用完删掉
      "document.querySelector('#tabs button[data-tab=\"proxies\"]').click()",
      "new Promise((resolve) => setTimeout(resolve, 600))",
      "fetch('/api/proxy-groups',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({name:'e2e-temp-group',url:''})}).then((r) => r.status)",
      "new Promise((resolve) => setTimeout(() => { renderers.proxies(); resolve(true); }, 200))",
      "new Promise((resolve) => setTimeout(resolve, 1200))",
      "(() => { const link = document.querySelector('#groups tbody a'); if (link) link.click(); return true; })()",
      "new Promise((resolve) => setTimeout(() => { const select = document.getElementById('account-group'); const view = document.querySelector('[data-view=\"accounts\"]'); globalThis.__linkOk = Boolean(view && !view.hidden && select && select.value !== ''); resolve(globalThis.__linkOk); }, 1800))",
      "fetch('/api/proxy-groups').then((r) => r.json()).then((payload) => Promise.all((payload.data || []).filter((g) => g.name === 'e2e-temp-group').map((g) => fetch('/api/proxy-groups/' + g.id, { method: 'DELETE' })))).then((r) => (globalThis.__linkOk !== false))",
    ],
  },
  {
    name: '设置恢复默认按钮可用',
    steps: [
      "document.querySelector('#tabs button[data-tab=\"settings\"]').click()",
      "new Promise((resolve) => setTimeout(resolve, 1000))",
      "(() => { const row = document.querySelector('#settings-groups tbody tr'); const input = row.querySelector('input, select, textarea'); input.value = '99999'; row.querySelector('button').click(); return true; })()",
      "new Promise((resolve) => setTimeout(() => { const row = document.querySelector('#settings-groups tbody tr'); const input = row.querySelector('input, select, textarea'); resolve(String(input.value) !== '99999'); }, 400))",
    ],
  },
];

async function main() {
  const url = await connect();
  const { WebSocket } = await import('node:worker_threads').then(() => ({ WebSocket: globalThis.WebSocket }));
  socket = new WebSocket(url);
  await new Promise((resolve, reject) => {
    socket.addEventListener('open', resolve, { once: true });
    socket.addEventListener('error', () => reject(new Error('WebSocket 连接失败')), { once: true });
  });
  socket.addEventListener('message', (event) => {
    const message = JSON.parse(event.data);
    if (message.id && waiters.has(message.id)) {
      waiters.get(message.id)(message);
      waiters.delete(message.id);
      return;
    }
    if (message.method === 'Runtime.exceptionThrown') {
      const details = message.params.exceptionDetails || {};
      const text = (details.exception && details.exception.description) || details.text || '未知异常';
      exceptions.push({ url: details.url || '', text: String(text).split('\n')[0].slice(0, 200) });
    }
  });

  await send('Page.enable');
  await send('Runtime.enable');
  await send('Emulation.setDeviceMetricsOverride', { width: 1440, height: 1000, deviceScaleFactor: 1, mobile: false });

  await send('Page.navigate', { url: BASE + '/admin' });
  await sleep(1200);
  const loggedIn = await evaluate(
    "fetch('/admin/login',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({key:" + JSON.stringify(KEY) + "})}).then(r=>r.status)"
  );
  if (loggedIn !== 200) throw new Error('登录失败，HTTP ' + loggedIn);

  await send('Page.navigate', { url: BASE + '/admin' });

  // 页面自己会先查会话再渲染，固定等待会抢跑：等它明确进入已登录态
  let session = '';
  for (let attempt = 0; attempt < 40; attempt += 1) {
    await sleep(250);
    session = await evaluate("document.getElementById('session-state').textContent");
    if (session && session.includes('已登录')) break;
  }
  const failures = [];
  console.log('会话：' + session);
  console.log('');
  console.log('页面          断言结果');
  console.log('------------  ----------------------------------------------');

  for (const page of PAGES) {
    exceptions.length = 0;
    await evaluate("document.querySelector('#tabs button[data-tab=\"" + page.tab + "\"]').click()");
    // 页面会并发拉若干接口，固定等待不可靠：轮询到断言成立或超时
    let verdict = { ok: false, detail: '等待超时' };
    for (let attempt = 0; attempt < 32; attempt += 1) {
      await sleep(250);
      try {
        verdict = await evaluate(page.check);
      } catch (error) {
        verdict = { ok: false, detail: String(error.message || error) };
      }
      if (verdict && verdict.ok) break;
    }
    const clean = exceptions.length === 0;
    const ok = verdict.ok && clean;
    const note = verdict.detail + (clean ? '' : ' | 异常：' + exceptions.map((item) => item.text).join(' / '));
    console.log(page.name.padEnd(12, ' ') + '  ' + (ok ? 'PASS' : 'FAIL') + '  ' + note);
    if (!ok) failures.push(page.name);
  }

  console.log('');
  console.log('交互检查      结果');
  console.log('------------  ----------------------------------------------');
  for (const item of INTERACTIONS) {
    exceptions.length = 0;
    let passed = false;
    try {
      let last;
      for (const step of item.steps) last = await evaluate(step);
      passed = Boolean(last);
    } catch (error) {
      passed = false;
    }
    const clean = exceptions.length === 0;
    const ok = passed && clean;
    console.log(item.name.padEnd(14, ' ') + (ok ? 'PASS' : 'FAIL') + (clean ? '' : '  异常：' + exceptions.map((e) => e.text).join(' / ')));
    if (!ok) failures.push(item.name);
  }

  console.log('');
  if (failures.length) {
    console.log('失败页签：' + failures.join('、'));
    process.exitCode = 1;
  } else {
    console.log('全部通过');
  }
}

main()
  .catch((error) => {
    console.error('冒烟脚本自身出错：' + (error && error.message ? error.message : error));
    process.exitCode = 1;
  })
  .finally(cleanup);
