'use strict';

const $ = (selector) => document.querySelector(selector);
const $$ = (selector) => Array.from(document.querySelectorAll(selector));

function toast(message, isError) {
  const node = $('#toast');
  node.textContent = message;
  node.style.background = isError ? '#cf222e' : '#1f2328';
  node.hidden = false;
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => { node.hidden = true; }, 4000);
}

async function api(path, options) {
  const response = await fetch(path, Object.assign({ credentials: 'same-origin' }, options || {}));
  if (response.status === 401) {
    showLogin();
    throw new Error('未登录');
  }
  const text = await response.text();
  let body = null;
  try { body = text ? JSON.parse(text) : null; } catch (_) { body = text; }
  if (!response.ok) {
    const message = (body && body.error && body.error.message) || (body && body.detail && body.detail.message) || text;
    throw new Error(message || ('HTTP ' + response.status));
  }
  return body;
}

function showLogin() {
  $('#login-panel').hidden = false;
  $('#tabs').hidden = true;
  $('#views').hidden = true;
  $('#logout').hidden = true;
  $('#session-state').textContent = '未登录';
}

function showApp(summary) {
  $('#login-panel').hidden = true;
  $('#tabs').hidden = false;
  $('#views').hidden = false;
  $('#logout').hidden = false;
  $('#session-state').textContent = '已登录 · 可用 key ' + (summary && summary.api_keys != null ? summary.api_keys : '?') + ' 把';
}

async function checkSession() {
  try {
    const summary = await api('/admin/session');
    showApp(summary);
    await openTab('overview');
    return true;
  } catch (_) {
    return false;
  }
}

$('#login-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  const key = $('#login-key').value.trim();
  if (!key) return;
  const error = $('#login-error');
  error.hidden = true;
  try {
    await api('/admin/login', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ key: key })
    });
    $('#login-key').value = '';
    await checkSession();
  } catch (failure) {
    error.textContent = '登录失败：' + failure.message;
    error.hidden = false;
  }
});

$('#logout').addEventListener('click', async () => {
  try { await api('/admin/logout', { method: 'POST' }); } catch (_) {}
  showLogin();
});

const renderers = {};

async function openTab(name) {
  $$('#tabs button').forEach((button) => button.classList.toggle('active', button.dataset.tab === name));
  $$('#views section').forEach((section) => { section.hidden = section.dataset.view !== name; });
  const render = renderers[name];
  if (!render) return;
  try { await render(); } catch (failure) { toast(failure.message, true); }
}

$$('#tabs button').forEach((button) => button.addEventListener('click', () => openTab(button.dataset.tab)));

function cell(text) {
  const td = document.createElement('td');
  td.textContent = text == null ? '' : String(text);
  return td;
}

function row(values) {
  const tr = document.createElement('tr');
  values.forEach((value) => tr.appendChild(value instanceof Node ? value : cell(value)));
  return tr;
}

renderers.overview = async () => {
  const metrics = await api('/api/metrics?hours=24');
  const cards = [
    ['总请求', metrics.total],
    ['成功', metrics.ok],
    ['失败', metrics.failed],
    ['成功率', (metrics.success_rate * 100).toFixed(1) + '%'],
    ['P50', metrics.p50_ms == null ? '-' : metrics.p50_ms.toFixed(0) + ' ms'],
    ['P95', metrics.p95_ms == null ? '-' : metrics.p95_ms.toFixed(0) + ' ms']
  ];
  const host = $('#metrics');
  host.textContent = '';
  cards.forEach(([label, value]) => {
    const card = document.createElement('div');
    card.className = 'card';
    const k = document.createElement('div'); k.className = 'k'; k.textContent = label;
    const v = document.createElement('div'); v.className = 'v'; v.textContent = value;
    card.appendChild(k); card.appendChild(v);
    host.appendChild(card);
  });
  const body = $('#errors tbody');
  body.textContent = '';
  (metrics.top_errors || []).forEach((item) => body.appendChild(row([item.code, item.hits])));
  if (!(metrics.top_errors || []).length) body.appendChild(row(['（无失败）', '']));
};

renderers.accounts = async () => {
  const payload = await api('/api/accounts');
  const items = payload.items || payload.data || [];
  const body = $('#accounts tbody');
  body.textContent = '';
  items.forEach((item) => {
    body.appendChild(row([
      item.email || '(无邮箱)',
      item.plan,
      item.status_label || item.status,
      item.quota,
      (item.success_count || 0) + ' / ' + (item.fail_count || 0),
      item.proxy || '(直连)'
    ]));
  });
  if (!items.length) body.appendChild(row(['（池子为空，去导入账号）', '', '', '', '', '']));
  $('#accounts-hint').textContent = '共 ' + items.length + ' 个账号';
};

$('#sync-accounts').addEventListener('click', async () => {
  try {
    const result = await api('/api/accounts/sync', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: '{}' });
    toast('已提交同步：' + JSON.stringify(result).slice(0, 120));
    await renderers.accounts();
  } catch (failure) { toast(failure.message, true); }
});

renderers.logs = async () => {
  const status = $('#log-status').value;
  const payload = await api('/api/logs?limit=50&status=' + encodeURIComponent(status));
  const body = $('#logs tbody');
  body.textContent = '';
  (payload.data || []).forEach((item) => {
    const account = item.account_id ? String(item.account_id).slice(0, 8) : '-';
    body.appendChild(row([
      (item.created_at || '').replace('T', ' ').slice(0, 19),
      item.method,
      item.path,
      item.status,
      item.duration_ms + ' ms',
      account,
      item.error_code || ''
    ]));
  });
  if (!(payload.data || []).length) body.appendChild(row(['（暂无记录）', '', '', '', '', '', '']));
};

$('#refresh-logs').addEventListener('click', () => renderers.logs().catch((failure) => toast(failure.message, true)));
$('#log-status').addEventListener('change', () => renderers.logs().catch((failure) => toast(failure.message, true)));

renderers.images = async () => {
  const [stats, payload] = await Promise.all([api('/api/images/stats'), api('/api/images?limit=60')]);
  $('#images-summary').textContent = '共 ' + stats.count + ' 张 / ' + (stats.bytes / 1048576).toFixed(1) + ' MB';
  const host = $('#images');
  host.textContent = '';
  (payload.data || []).forEach((item) => {
    const figure = document.createElement('figure');
    const image = document.createElement('img');
    image.loading = 'lazy';
    image.src = '/' + item.rel_path;
    image.alt = item.prompt || item.rel_path;
    const caption = document.createElement('figcaption');
    caption.textContent = item.width + '×' + item.height + ' · ' + Math.round(item.bytes / 1024) + ' KB';
    const remove = document.createElement('button');
    remove.textContent = '删除';
    remove.className = 'danger';
    remove.addEventListener('click', async () => {
      if (!confirm('删除这张图片？文件会一起删掉。')) return;
      try {
        await api('/api/images/' + item.id, { method: 'DELETE' });
        toast('已删除');
        await renderers.images();
      } catch (failure) { toast(failure.message, true); }
    });
    caption.appendChild(document.createElement('br'));
    caption.appendChild(remove);
    figure.appendChild(image);
    figure.appendChild(caption);
    host.appendChild(figure);
  });
  if (!(payload.data || []).length) host.textContent = '（还没有生成的图片）';
};

renderers.settings = async () => {
  const payload = await api('/api/settings');
  const entries = [].concat(payload.numerics || payload.numeric || [], payload.strings || []);
  const body = $('#settings tbody');
  body.textContent = '';
  entries.forEach((spec) => {
    const input = document.createElement('input');
    input.value = spec.value;
    input.dataset.key = spec.key;
    const bounds = spec.min != null || spec.max != null
      ? (spec.min != null ? spec.min : '-∞') + ' .. ' + (spec.max != null ? spec.max : '+∞')
      : ('长度 ≤ ' + (spec.max_len != null ? spec.max_len : '?'));
    const range = (spec.unit ? bounds + ' ' + spec.unit : bounds)
      + (spec.restart_required ? ' · 需重启' : ' · 热生效');
    const save = document.createElement('button');
    save.textContent = '保存';
    save.addEventListener('click', async () => {
      const numeric = spec.min != null || spec.max != null || spec.kind === 'integer' || spec.kind === 'float';
      const value = numeric ? Number(input.value) : input.value;
      const values = {};
      values[spec.key] = value;
      try {
        await api('/api/settings', {
          method: 'PUT',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ values: values })
        });
        toast('已保存 ' + spec.key);
        $('#settings-hint').textContent = '最近保存：' + spec.key + ' = ' + input.value;
      } catch (failure) { toast(failure.message, true); }
    });
    const holder = document.createElement('td');
    holder.appendChild(input);
    body.appendChild(row([spec.key, holder, range, save]));
  });
};

renderers.keys = async () => {
  const payload = await api('/api/keys');
  const body = $('#keys tbody');
  body.textContent = '';
  (payload.data || []).forEach((item) => {
    const revoke = document.createElement('button');
    revoke.textContent = '吊销';
    revoke.className = 'danger';
    revoke.disabled = Boolean(item.revoked_at);
    revoke.addEventListener('click', async () => {
      if (!confirm('吊销后该 key 立刻失效，确定？')) return;
      try {
        await api('/api/keys/' + item.id, { method: 'DELETE' });
        toast('已吊销');
        await renderers.keys();
      } catch (failure) { toast(failure.message, true); }
    });
    body.appendChild(row([
      item.name,
      item.prefix,
      (item.created_at || '').slice(0, 19).replace('T', ' '),
      item.last_used_at ? String(item.last_used_at).slice(0, 19).replace('T', ' ') : '从未',
      item.revoked_at ? '已吊销' : '有效',
      revoke
    ]));
  });
  if (!(payload.data || []).length) body.appendChild(row(['（还没有 key）', '', '', '', '', '']));
};

$('#key-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  const name = $('#key-name').value.trim();
  if (!name) return;
  try {
    const created = await api('/api/keys', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ name: name })
    });
    $('#key-name').value = '';
    const hint = $('#new-key');
    hint.hidden = false;
    hint.textContent = '新 key（只显示这一次，请立刻复制）：' + created.plaintext;
    await renderers.keys();
  } catch (failure) { toast(failure.message, true); }
});

checkSession();
