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

// 分组列表在账号页与分组页都要用，放这里避免两次请求
let groupCache = [];

async function loadGroups(force) {
  if (!force && groupCache.length) return groupCache;
  const payload = await api('/api/proxy-groups');
  groupCache = payload.data || [];
  return groupCache;
}

async function assignGroup(accountId, groupId) {
  await api('/api/accounts/' + accountId + '/proxy-group', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ group_id: groupId || null })
  });
  groupCache = [];
}

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

function metricCard(label, value) {
  const card = document.createElement('div');
  card.className = 'card';
  const k = document.createElement('div');
  k.className = 'k';
  k.textContent = label;
  const v = document.createElement('div');
  v.className = 'v';
  v.textContent = value;
  card.appendChild(k);
  card.appendChild(v);
  return card;
}

renderers.overview = async () => {
  const hours = Number($('#metrics-hours').value || 24);
  const [metrics, accountTotal] = await Promise.all([
    api('/api/metrics?hours=' + hours),
    api('/api/accounts?page=1&page_size=1').then((payload) => payload.total || 0).catch(() => 0),
  ]);

  const host = $('#metrics');
  host.textContent = '';
  [
    ['总请求', metrics.total],
    ['成功', metrics.ok],
    ['失败', metrics.failed],
    ['成功率', (metrics.success_rate * 100).toFixed(1) + '%'],
    ['P50', metrics.p50_ms == null ? '-' : metrics.p50_ms.toFixed(0) + ' ms'],
    ['P95', metrics.p95_ms == null ? '-' : metrics.p95_ms.toFixed(0) + ' ms'],
    ['账号总数', accountTotal],
  ].forEach(([label, value]) => host.appendChild(metricCard(label, value)));

  const body = $('#errors tbody');
  body.textContent = '';
  (metrics.top_errors || []).forEach((item) => body.appendChild(row([item.code, item.hits])));
  if (!(metrics.top_errors || []).length) body.appendChild(row(['（该时间段没有失败请求）', '']));

  $('#metrics-hint').textContent = (metrics.total ? '' : '该时间段没有请求 · ')
    + '更新于 ' + new Date().toLocaleTimeString('zh-CN');
};

$('#metrics-hours').addEventListener('change', () => {
  renderers.overview().catch((failure) => toast(failure.message, true));
});

$('#metrics-refresh').addEventListener('click', () => {
  renderers.overview().catch((failure) => toast(failure.message, true));
});

// 账号池：统计、筛选、批量、行内操作、导入、OAuth、异步进度

const JSON_TYPE = { 'Content-Type': 'application/json' };
const ACCOUNT_STATUS = [
  ['', '全部'], ['normal', '正常'], ['limited', '限流'], ['abnormal', '异常'], ['disabled', '禁用'],
];
const accountState = { page: 1, pageSize: 20, total: 0, status: '', plan: '', keyword: '', selected: new Set() };

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function accountQuery(page) {
  const params = new URLSearchParams();
  params.set('page', String(page || accountState.page));
  params.set('page_size', String(accountState.pageSize));
  if (accountState.status) params.set('status', accountState.status);
  if (accountState.plan) params.set('plan', accountState.plan);
  if (accountState.keyword) params.set('keyword', accountState.keyword);
  return params.toString();
}

async function loadAccountCounts() {
  return Promise.all(ACCOUNT_STATUS.map(([status]) => {
    const params = new URLSearchParams({ page: '1', page_size: '1' });
    if (status) params.set('status', status);
    if (accountState.keyword) params.set('keyword', accountState.keyword);
    return api('/api/accounts?' + params.toString()).then((payload) => payload.total || 0).catch(() => 0);
  }));
}

function accountStats(counts) {
  const host = $('#account-stats');
  host.textContent = '';
  ACCOUNT_STATUS.forEach(([status, label], index) => {
    const card = document.createElement('div');
    card.className = 'card';
    if (status) card.style.cursor = 'pointer';
    const k = document.createElement('div');
    k.className = 'k';
    k.textContent = label + '账号';
    const v = document.createElement('div');
    v.className = 'v';
    v.textContent = counts[index];
    card.appendChild(k);
    card.appendChild(v);
    if (status) {
      card.addEventListener('click', () => {
        accountState.status = accountState.status === status ? '' : status;
        $('#account-status').value = accountState.status;
        accountState.page = 1;
        renderers.accounts().catch((failure) => toast(failure.message, true));
      });
    }
    host.appendChild(card);
  });
}

function summarizeError(raw) {
  const text = String(raw || '');
  const start = text.indexOf('{');
  if (start >= 0) {
    try {
      const parsed = JSON.parse(text.slice(start));
      const message = (parsed.error && parsed.error.message)
        || (parsed.detail && parsed.detail.message)
        || parsed.message;
      if (message) return String(message).slice(0, 80);
    } catch (_) { /* 不是完整 JSON，退回截断 */ }
  }
  return text.length > 80 ? text.slice(0, 80) + '…' : text;
}

function relativeTimeText(value) {
  if (!value) return '-';
  const text = relativeTime(value);
  return text + '（' + String(value).slice(0, 19).replace('T', ' ') + '）';
}

function relativeTime(value) {
  if (!value) return '-';
  const then = new Date(value).getTime();
  if (Number.isNaN(then)) return String(value).slice(0, 19).replace('T', ' ');
  const diff = Date.now() - then;
  if (diff < 60000) return '刚刚';
  if (diff < 3600000) return Math.floor(diff / 60000) + ' 分钟前';
  if (diff < 86400000) return Math.floor(diff / 3600000) + ' 小时前';
  return Math.floor(diff / 86400000) + ' 天前';
}

function credentialCell(item) {
  const credential = item.credential || {};
  const td = document.createElement('td');
  const span = document.createElement('span');
  span.className = credential.usable === 'usable' ? 'muted' : 'error';
  span.textContent = (credential.access_label || credential.access || '-') + ' / ' + (credential.refresh_label || credential.refresh || '-');
  td.appendChild(span);
  if (item.can_refresh_access_token) {
    const flag = document.createElement('span');
    flag.className = 'muted';
    flag.textContent = ' · 可刷新';
    td.appendChild(flag);
  }
  return td;
}

function syncSelection() {
  $('#account-selected').textContent = accountState.selected.size ? '已选 ' + accountState.selected.size + ' 个' : '未选择';
}

async function runAccountAction(action, item) {
  try {
    if (action === 'edit') { openAccountDialog(item); return; }
    if (action === 'test') {
      toast('正在出图测试 ' + (item.email || item.id) + '…');
      const result = await api('/api/accounts/' + item.id + '/test', { method: 'POST', headers: JSON_TYPE, body: '{}' });
      const ok = result.status === 'success';
      toast(ok
        ? '出图成功 ' + result.elapsed_ms + 'ms，' + (result.bytes || 0) + ' 字节 ' + (result.size || '')
        : '出图失败：' + (result.message || '未知原因'), !ok);
      return;
    }
    if (action === 'refresh') {
      const result = await api('/api/accounts/refresh-token', { method: 'POST', headers: JSON_TYPE, body: JSON.stringify({ ids: [item.id] }) });
      toast('已提交刷新 AT');
      pollOperation(result.operation_id, '刷新 AT');
      return;
    }
    if (action === 'enable' || action === 'disable') {
      await api('/api/accounts/update', {
        method: 'POST',
        headers: JSON_TYPE,
        body: JSON.stringify({ id: item.id, status: action === 'enable' ? 'normal' : 'disabled' }),
      });
      toast(action === 'enable' ? '已启用' : '已禁用');
      await renderers.accounts();
      return;
    }
    if (action === 'delete') {
      if (!confirm('删除账号 ' + (item.email || item.id) + '？')) return;
      await api('/api/accounts', { method: 'DELETE', headers: JSON_TYPE, body: JSON.stringify({ ids: [item.id] }) });
      toast('已删除');
      accountState.selected.delete(item.id);
      await renderers.accounts();
    }
  } catch (failure) { toast(failure.message, true); }
}

function accountActions(item) {
  const select = document.createElement('select');
  const options = [['', '更多…'], ['edit', '编辑'], ['test', '测试出图'], ['refresh', '刷新 AT']];
  options.push(item.status === 'disabled' ? ['enable', '启用'] : ['disable', '禁用']);
  options.push(['delete', '删除']);
  options.forEach(([value, label]) => {
    const option = document.createElement('option');
    option.value = value;
    option.textContent = label;
    select.appendChild(option);
  });
  select.addEventListener('change', () => {
    const action = select.value;
    select.value = '';
    if (action) runAccountAction(action, item);
  });
  const td = document.createElement('td');
  td.appendChild(select);
  return td;
}

function accountRow(item, groups) {
  const check = document.createElement('td');
  const box = document.createElement('input');
  box.type = 'checkbox';
  box.checked = accountState.selected.has(item.id);
  box.addEventListener('change', () => {
    if (box.checked) accountState.selected.add(item.id); else accountState.selected.delete(item.id);
    syncSelection();
  });
  check.appendChild(box);

  const groupSelect = document.createElement('select');
  const none = document.createElement('option');
  none.value = '';
  none.textContent = '(不属于任何组)';
  groupSelect.appendChild(none);
  (groups || []).forEach((group) => {
    const option = document.createElement('option');
    option.value = group.id;
    option.textContent = group.name;
    groupSelect.appendChild(option);
  });
  groupSelect.value = item.proxy_group_id || '';
  groupSelect.addEventListener('change', async () => {
    try {
      await assignGroup(item.id, groupSelect.value);
      toast('已更新分组');
    } catch (failure) { toast(failure.message, true); }
  });

  const proxy = document.createElement('td');
  const proxyText = document.createElement('div');
  proxyText.textContent = item.proxy || '(直连)';
  proxyText.style.fontSize = '12px';
  proxy.appendChild(proxyText);
  proxy.appendChild(groupSelect);

  const failCell = document.createElement('td');
  failCell.textContent = (item.success_count || 0) + ' / ' + (item.fail_count || 0);
  if (item.fail_count > 0) failCell.className = 'error';

  const errorCell = document.createElement('td');
  errorCell.style.maxWidth = '240px';
  errorCell.style.wordBreak = 'break-all';
  errorCell.style.fontSize = '12px';
  if (item.last_error) {
    errorCell.className = 'error';
    errorCell.textContent = summarizeError(item.last_error);
    errorCell.title = item.last_error;
  } else {
    errorCell.className = 'muted';
    errorCell.textContent = '-';
  }

  const quotaCell = document.createElement('td');
  quotaCell.textContent = (item.quota_confirmed ? '' : '≈') + item.quota;
  quotaCell.title = item.quota_confirmed ? '已由远端确认' : '本地估算，未经确认';

  const tr = row([
    check,
    item.email || '(无邮箱)',
    item.plan,
    item.status_label || item.status,
    quotaCell,
    credentialCell(item),
    failCell,
    errorCell,
    proxy,
    accountActions(item),
  ]);
  tr.dataset.accountId = item.id;
  return tr;
}

async function pollOperation(id, label) {
  const host = $('#account-progress');
  host.hidden = false;
  host.textContent = label + '：已提交，等待进度…';
  for (let attempt = 0; attempt < 300; attempt += 1) {
    const progress = await api('/api/accounts/operations/' + id).catch(() => null);
    if (!progress) {
      host.textContent = label + '：进度记录已过期（操作可能仍在后台继续）';
      return;
    }
    const percent = progress.total ? Math.round((progress.processed / progress.total) * 100) : 100;
    let text = label + '：' + progress.processed + ' / ' + progress.total + '（' + percent + '%）成功 '
      + progress.succeeded + '，失败 ' + progress.failed;
    if (progress.errors && progress.errors.length) {
      const first = progress.errors[0];
      text += ' | 首个错误：' + String(first.message || first.error || JSON.stringify(first)).slice(0, 120);
    }
    host.textContent = text + (progress.done ? ' —— 已完成' : '');
    if (progress.done) {
      await renderers.accounts();
      return;
    }
    await sleep(800);
  }
  host.textContent = label + '：轮询超时，请手动刷新';
}

async function accountBatch(kind) {
  const ids = Array.from(accountState.selected);
  if (!ids.length && kind !== 'sync' && kind !== 'refresh') {
    toast('请先勾选账号', true);
    return;
  }
  try {
    if (kind === 'sync' || kind === 'refresh') {
      const path = kind === 'sync' ? '/api/accounts/sync' : '/api/accounts/refresh-token';
      const result = await api(path, { method: 'POST', headers: JSON_TYPE, body: JSON.stringify({ ids }) });
      toast('已提交 ' + result.total + ' 个账号');
      pollOperation(result.operation_id, kind === 'sync' ? '同步配额' : '刷新 AT');
      return;
    }
    if (kind === 'export') {
      const payload = await api('/api/accounts/export', { method: 'POST', headers: JSON_TYPE, body: JSON.stringify({ ids, format: 'json' }) });
      const blob = new Blob([JSON.stringify(payload, null, 2)], { type: 'application/json' });
      const url = URL.createObjectURL(blob);
      const link = document.createElement('a');
      link.href = url;
      link.download = 'accounts-export.json';
      link.click();
      URL.revokeObjectURL(url);
      toast('已导出');
      return;
    }
    if (kind === 'enable' || kind === 'disable') {
      const status = kind === 'enable' ? 'normal' : 'disabled';
      let done = 0;
      let failed = 0;
      for (const id of ids) {
        try {
          await api('/api/accounts/update', { method: 'POST', headers: JSON_TYPE, body: JSON.stringify({ id, status }) });
          done += 1;
        } catch (_) { failed += 1; }
      }
      toast('已处理 ' + done + ' 个' + (failed ? '，失败 ' + failed + ' 个' : ''));
      accountState.selected.clear();
      await renderers.accounts();
      return;
    }
    if (kind === 'delete') {
      if (!confirm('删除选中的 ' + ids.length + ' 个账号？此操作不可撤销。')) return;
      const result = await api('/api/accounts', { method: 'DELETE', headers: JSON_TYPE, body: JSON.stringify({ ids }) });
      toast('已删除 ' + (result.removed || 0) + ' 个账号');
      accountState.selected.clear();
      await renderers.accounts();
    }
  } catch (failure) { toast(failure.message, true); }
}

function openAccountDialog(item) {
  const dialog = $('#account-dialog');
  dialog.dataset.id = item.id;
  $('#edit-email').value = item.email || '';
  $('#edit-plan').value = ['free', 'plus', 'pro'].includes(item.plan) ? item.plan : 'unknown';
  $('#edit-status').value = ['normal', 'limited', 'abnormal', 'disabled'].includes(item.status) ? item.status : 'normal';
  $('#edit-quota').value = item.quota;
  $('#edit-quota-confirmed').checked = Boolean(item.quota_confirmed);
  $('#edit-proxy').value = item.proxy || '';
  $('#edit-error').hidden = true;
  if (dialog.showModal) dialog.showModal(); else dialog.setAttribute('open', '');
}

renderers.accounts = async () => {
  const [payload, groups, counts] = await Promise.all([
    api('/api/accounts?' + accountQuery()),
    loadGroups(true),
    loadAccountCounts(),
  ]);
  const items = payload.items || [];
  accountState.total = payload.total || 0;
  accountStats(counts);

  const body = $('#accounts tbody');
  body.textContent = '';
  items.forEach((item) => body.appendChild(accountRow(item, groups)));
  if (!items.length) body.appendChild(row(['（没有符合条件的账号）', '', '', '', '', '', '', '', '', '']));
  syncSelection();
  $('#account-all').checked = false;

  const pages = Math.max(1, Math.ceil(accountState.total / accountState.pageSize));
  $('#account-page').textContent = '第 ' + payload.page + ' / ' + pages + ' 页 · 共 ' + accountState.total + ' 个';
  $('#account-prev').disabled = payload.page <= 1;
  $('#account-next').disabled = payload.page >= pages;
  $('#accounts-hint').textContent = '共 ' + accountState.total + ' 个账号 · ' + groups.length + ' 个分组';
};

$('#account-search').addEventListener('click', () => {
  accountState.keyword = $('#account-keyword').value.trim();
  accountState.status = $('#account-status').value;
  accountState.plan = $('#account-plan').value;
  accountState.page = 1;
  renderers.accounts().catch((failure) => toast(failure.message, true));
});

$('#account-reset').addEventListener('click', () => {
  $('#account-keyword').value = '';
  $('#account-status').value = '';
  $('#account-plan').value = '';
  accountState.keyword = '';
  accountState.status = '';
  accountState.plan = '';
  accountState.page = 1;
  renderers.accounts().catch((failure) => toast(failure.message, true));
});

$('#account-status').addEventListener('change', () => $('#account-search').click());
$('#account-plan').addEventListener('change', () => $('#account-search').click());
$('#account-keyword').addEventListener('keydown', (event) => { if (event.key === 'Enter') $('#account-search').click(); });

$('#account-all').addEventListener('change', (event) => {
  const boxes = Array.from(document.querySelectorAll('#accounts tbody input[type=checkbox]'));
  accountState.selected.clear();
  boxes.forEach((box) => {
    box.checked = event.target.checked;
    if (box.checked) {
      const tr = box.closest('tr');
      if (tr && tr.dataset.accountId) accountState.selected.add(tr.dataset.accountId);
    }
  });
  syncSelection();
});

$('#account-prev').addEventListener('click', () => {
  if (accountState.page > 1) {
    accountState.page -= 1;
    renderers.accounts().catch((failure) => toast(failure.message, true));
  }
});

$('#account-next').addEventListener('click', () => {
  const pages = Math.max(1, Math.ceil(accountState.total / accountState.pageSize));
  if (accountState.page < pages) {
    accountState.page += 1;
    renderers.accounts().catch((failure) => toast(failure.message, true));
  }
});

[['bulk-sync', 'sync'], ['bulk-refresh', 'refresh'], ['bulk-enable', 'enable'],
 ['bulk-disable', 'disable'], ['bulk-export', 'export'], ['bulk-delete', 'delete']]
  .forEach(([id, kind]) => $('#' + id).addEventListener('click', () => accountBatch(kind)));

$('#import-submit').addEventListener('click', async () => {
  const hint = $('#import-hint');
  try {
    let text = $('#import-text').value.trim();
    const file = $('#import-file').files[0];
    if (file) text = (await file.text()).trim();
    if (!text) { hint.textContent = '没有内容可导入'; return; }
    hint.textContent = '导入中…';
    const result = await api('/api/accounts', { method: 'POST', headers: JSON_TYPE, body: JSON.stringify(text) });
    hint.textContent = '新增 ' + result.added + '，合并 ' + result.merged + '，跳过 ' + result.skipped;
    $('#import-text').value = '';
    $('#import-file').value = '';
    await renderers.accounts();
  } catch (failure) {
    hint.textContent = '';
    toast(failure.message, true);
  }
});

let oauthSession = '';
$('#oauth-start').addEventListener('click', async () => {
  try {
    const start = await api('/api/accounts/oauth/start', {
      method: 'POST',
      headers: JSON_TYPE,
      body: JSON.stringify({ email_hint: $('#oauth-email').value.trim() || null }),
    });
    oauthSession = start.session_id;
    const link = $('#oauth-link');
    link.hidden = false;
    link.textContent = start.authorize_url;
    $('#oauth-copy').hidden = false;
    $('#oauth-hint').textContent = '链接已生成：在浏览器打开登录，然后把回调地址粘到下面。';
  } catch (failure) { toast(failure.message, true); }
});

$('#oauth-copy').addEventListener('click', async () => {
  try {
    await navigator.clipboard.writeText($('#oauth-link').textContent);
    toast('链接已复制');
  } catch (_) { toast('复制失败，请手动选择文本', true); }
});

$('#oauth-finish').addEventListener('click', async () => {
  const callback = $('#oauth-callback').value.trim();
  if (!oauthSession) { toast('请先获取授权链接', true); return; }
  if (!callback) { toast('请粘贴回调地址', true); return; }
  try {
    const result = await api('/api/accounts/oauth/finish', {
      method: 'POST',
      headers: JSON_TYPE,
      body: JSON.stringify({ session_id: oauthSession, callback }),
    });
    $('#oauth-hint').textContent = '授权完成：新增 ' + result.added + '，合并 ' + result.merged;
    $('#oauth-callback').value = '';
    oauthSession = '';
    await renderers.accounts();
  } catch (failure) { toast(failure.message, true); }
});

$('#edit-cancel').addEventListener('click', () => {
  const dialog = $('#account-dialog');
  if (dialog.close) dialog.close(); else dialog.removeAttribute('open');
});

$('#edit-save').addEventListener('click', async () => {
  const dialog = $('#account-dialog');
  const error = $('#edit-error');
  error.hidden = true;
  const body = {
    id: dialog.dataset.id,
    email: $('#edit-email').value.trim(),
    plan: $('#edit-plan').value,
    status: $('#edit-status').value,
    proxy: $('#edit-proxy').value.trim(),
    quota: Number($('#edit-quota').value || 0),
    quota_confirmed: $('#edit-quota-confirmed').checked,
  };
  try {
    await api('/api/accounts/update', { method: 'POST', headers: JSON_TYPE, body: JSON.stringify(body) });
    if (dialog.close) dialog.close(); else dialog.removeAttribute('open');
    toast('已保存');
    await renderers.accounts();
  } catch (failure) {
    error.hidden = false;
    error.textContent = failure.message;
  }
});

const logsState = { page: 1, pageSize: 50 };

function logStatusClass(status) {
  if (status >= 500) return 'error';
  if (status >= 400) return 'error';
  return '';
}

renderers.logs = async () => {
  const params = new URLSearchParams({
    limit: String(logsState.pageSize),
    offset: String((logsState.page - 1) * logsState.pageSize),
    status: $('#log-status').value,
    hours: $('#log-hours').value,
  });
  const path = $('#log-path').value.trim();
  if (path) params.set('path', path);

  const payload = await api('/api/logs?' + params.toString());
  const items = payload.data || [];
  const body = $('#logs tbody');
  body.textContent = '';
  items.forEach((item) => {
    const when = cell(relativeTime(item.created_at));
    when.title = item.created_at || '';
    const status = cell(item.status);
    if (logStatusClass(item.status)) status.className = 'error';
    const account = cell(item.account_id ? String(item.account_id).slice(0, 8) : '-');
    account.className = 'mono';
    body.appendChild(row([
      when,
      item.method,
      item.path,
      status,
      item.duration_ms == null ? '-' : item.duration_ms + ' ms',
      account,
      item.error_code || '',
    ]));
  });
  if (!items.length) body.appendChild(row(['（没有符合条件的记录）', '', '', '', '', '', '']));

  $('#logs-page').textContent = '第 ' + logsState.page + ' 页 · 本页 ' + items.length + ' 条';
  $('#logs-prev').disabled = logsState.page <= 1;
  $('#logs-next').disabled = items.length < logsState.pageSize;
  $('#logs-hint').textContent = '每页 ' + logsState.pageSize + ' 条';
};

function reloadLogs() {
  renderers.logs().catch((failure) => toast(failure.message, true));
}

$('#refresh-logs').addEventListener('click', () => { logsState.page = 1; reloadLogs(); });
$('#log-status').addEventListener('change', () => { logsState.page = 1; reloadLogs(); });
$('#log-hours').addEventListener('change', () => { logsState.page = 1; reloadLogs(); });
$('#log-path').addEventListener('keydown', (event) => { if (event.key === 'Enter') { logsState.page = 1; reloadLogs(); } });
$('#log-reset').addEventListener('click', () => {
  $('#log-path').value = '';
  $('#log-status').value = 'all';
  $('#log-hours').value = '24';
  logsState.page = 1;
  reloadLogs();
});
$('#logs-prev').addEventListener('click', () => {
  if (logsState.page > 1) {
    logsState.page -= 1;
    reloadLogs();
  }
});
$('#logs-next').addEventListener('click', () => {
  logsState.page += 1;
  reloadLogs();
});

renderers.proxies = async () => {
  const groups = await loadGroups(true);
  const body = $('#groups tbody');
  body.textContent = '';
  groups.forEach((group) => {
    const url = document.createElement('input');
    url.value = group.url || '';
    url.placeholder = '(直连)';
    const save = document.createElement('button');
    save.textContent = '保存';
    save.addEventListener('click', async () => {
      try {
        const result = await api('/api/proxy-groups/' + group.id, {
          method: 'PUT',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ name: nameInput.value.trim() || group.name, url: url.value.trim() })
        });
        toast('已保存，同步了 ' + (result.synced_accounts || 0) + ' 个账号');
        await renderers.proxies();
      } catch (failure) { toast(failure.message, true); }
    });
    const remove = document.createElement('button');
    remove.textContent = '删除';
    remove.className = 'danger';
    remove.addEventListener('click', async () => {
      if (!confirm('删除分组「' + group.name + '」？组内账号会被摘出（保留当前代理）。')) return;
      try {
        const result = await api('/api/proxy-groups/' + group.id, { method: 'DELETE' });
        toast('已删除，摘出 ' + (result.detached_accounts || 0) + ' 个账号');
        await renderers.proxies();
      } catch (failure) { toast(failure.message, true); }
    });
    const nameInput = document.createElement('input');
    nameInput.value = group.name || '';
    nameInput.style.width = '100%';

    const urlCell = document.createElement('td');
    urlCell.appendChild(url);
    const actionCell = document.createElement('td');
    actionCell.appendChild(save);
    actionCell.appendChild(document.createTextNode(' '));
    actionCell.appendChild(remove);

    const countCell = document.createElement('td');
    countCell.textContent = group.members;
    countCell.title = '组内账号数；改组地址会让这些账号一次性换线';

    body.appendChild(row([nameInput, urlCell, countCell, actionCell]));
  });
  if (!groups.length) body.appendChild(row(['（还没有分组）', '', '', '']));
};

$('#group-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  const name = $('#group-name').value.trim();
  if (!name) return;
  try {
    await api('/api/proxy-groups', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ name: name, url: $('#group-url').value.trim() })
    });
    $('#group-name').value = '';
    $('#group-url').value = '';
    groupCache = [];
    await renderers.proxies();
  } catch (failure) { toast(failure.message, true); }
});

const imagesState = { page: 1, pageSize: 24 };

function imageFigure(item) {
  const figure = document.createElement('figure');
  const link = document.createElement('a');
  link.href = '/' + item.rel_path;
  link.target = '_blank';
  link.rel = 'noopener';
  const image = document.createElement('img');
  image.loading = 'lazy';
  image.src = '/' + item.rel_path;
  image.alt = item.prompt || item.rel_path;
  link.appendChild(image);
  figure.appendChild(link);

  const caption = document.createElement('figcaption');
  caption.textContent = item.width + '×' + item.height + ' · ' + Math.round(item.bytes / 1024) + ' KB';
  if (item.created_at) caption.title = item.created_at;
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
  figure.appendChild(caption);
  return figure;
}

renderers.images = async () => {
  const offset = (imagesState.page - 1) * imagesState.pageSize;
  const [stats, payload] = await Promise.all([
    api('/api/images/stats'),
    api('/api/images?limit=' + imagesState.pageSize + '&offset=' + offset),
  ]);
  const items = payload.data || [];
  $('#images-summary').textContent = '共 ' + stats.count + ' 张 / ' + (stats.bytes / 1048576).toFixed(1) + ' MB';

  const host = $('#images');
  host.textContent = '';
  items.forEach((item) => host.appendChild(imageFigure(item)));
  if (!items.length) {
    host.textContent = imagesState.page > 1 ? '（这一页没有图片）' : '（还没有生成的图片）';
  }

  $('#images-page').textContent = '第 ' + imagesState.page + ' 页 · 本页 ' + items.length + ' 张';
  $('#images-prev').disabled = imagesState.page <= 1;
  $('#images-next').disabled = items.length < imagesState.pageSize;

};

$('#images-prev').addEventListener('click', () => {
  if (imagesState.page > 1) {
    imagesState.page -= 1;
    renderers.images().catch((failure) => toast(failure.message, true));
  }
});

$('#images-next').addEventListener('click', () => {
  imagesState.page += 1;
  renderers.images().catch((failure) => toast(failure.message, true));
});

// 设置：按用途分组，标签与说明直接用后端返回的 description
const SETTING_GROUPS = [
  ['图片生成', (key) => key.startsWith('image_') && !/retention|settle/.test(key)],
  ['并发与重试', (key) => /concurrency|attempts/.test(key)],
  ['保留与清理', (key) => /retention|settle|ttl/.test(key)],
  ['账号刷新', (key) => key.startsWith('refresh_account')],
  ['日志与请求', (key) => /^log_|^console_/.test(key)],
  ['输出与内容策略', (key) => /rich_text|blocked_keywords/.test(key)],
];
const settingInputs = new Map();

function unitLabel(unit) {
  return { seconds: '秒', minutes: '分钟', hours: '小时' }[unit] || unit;
}

function settingGroupOf(key) {
  const found = SETTING_GROUPS.find(([, match]) => match(key));
  return found ? found[0] : '其他';
}

function settingRow(spec) {
  const tr = document.createElement('tr');

  const nameCell = document.createElement('td');
  const title = document.createElement('div');
  title.textContent = spec.description || spec.key;
  const keyLine = document.createElement('div');
  keyLine.className = 'muted';
  keyLine.style.fontSize = '12px';
  keyLine.style.fontFamily = 'ui-monospace, Consolas, monospace';
  keyLine.textContent = spec.key;
  nameCell.appendChild(title);
  nameCell.appendChild(keyLine);

  const valueCell = document.createElement('td');
  let input;
  if (spec.key === 'rich_text_policy') {
    input = document.createElement('select');
    ['off', 'guard', 'strip', 'guard+strip'].forEach((choice) => {
      const option = document.createElement('option');
      option.value = choice;
      option.textContent = choice;
      input.appendChild(option);
    });
  } else if (spec.max_len != null && spec.max_len > 120) {
    input = document.createElement('textarea');
    input.rows = 2;
  } else {
    input = document.createElement('input');
    if (spec.kind === 'integer' || spec.kind === 'float') input.type = 'number';
  }
  input.value = spec.value == null ? '' : spec.value;
  input.style.width = '100%';
  valueCell.appendChild(input);

  const infoCell = document.createElement('td');
  infoCell.className = 'muted';
  infoCell.style.fontSize = '12px';
  const parts = [];
  if (spec.min != null || spec.max != null) {
    parts.push('范围 ' + (spec.min == null ? '-∞' : spec.min) + ' .. ' + (spec.max == null ? '+∞' : spec.max)
      + (spec.unit ? ' ' + unitLabel(spec.unit) : ''));
  }
  if (spec.max_len != null) parts.push('最长 ' + spec.max_len + ' 字符');
  parts.push('默认 ' + (spec.default === '' || spec.default == null ? '空' : spec.default));
  parts.push(spec.restart_required ? '需重启生效' : '热生效');
  infoCell.textContent = parts.join(' · ');

  const actionCell = document.createElement('td');
  const reset = document.createElement('button');
  reset.textContent = '恢复默认';
  reset.className = 'ghost';
  reset.addEventListener('click', () => { input.value = spec.default == null ? '' : spec.default; });
  actionCell.appendChild(reset);

  settingInputs.set(spec.key, { spec, input });
  tr.appendChild(nameCell);
  tr.appendChild(valueCell);
  tr.appendChild(infoCell);
  tr.appendChild(actionCell);
  return tr;
}

renderers.settings = async () => {
  const payload = await api('/api/settings');
  const specs = [].concat(payload.numerics || [], payload.strings || []);
  settingInputs.clear();

  const host = $('#settings-groups');
  host.textContent = '';
  const buckets = new Map();
  specs.forEach((spec) => {
    const name = settingGroupOf(spec.key);
    if (!buckets.has(name)) buckets.set(name, []);
    buckets.get(name).push(spec);
  });

  SETTING_GROUPS.map(([name]) => name).concat(['其他']).forEach((name) => {
    const list = buckets.get(name);
    if (!list) return;
    const heading = document.createElement('h3');
    heading.textContent = name + '（' + list.length + '）';
    host.appendChild(heading);
    const table = document.createElement('table');
    const head = document.createElement('thead');
    const headRow = document.createElement('tr');
    [['设置', ''], ['值', 'width:180px'], ['说明与范围', ''], ['', 'width:96px']].forEach(([label, style]) => {
      const th = document.createElement('th');
      th.textContent = label;
      if (style) th.setAttribute('style', style);
      headRow.appendChild(th);
    });
    head.appendChild(headRow);
    table.appendChild(head);
    const body = document.createElement('tbody');
    list.forEach((spec) => body.appendChild(settingRow(spec)));
    table.appendChild(body);
    host.appendChild(table);
  });

  $('#settings-updated').textContent = payload.updated_at ? ' · 最近更新 ' + relativeTime(payload.updated_at) : '';
  $('#settings-hint').className = 'muted';
  $('#settings-hint').textContent = '共 ' + specs.length + ' 项';
};

function collectSettingChanges() {
  const values = {};
  const problems = [];
  const changed = [];
  settingInputs.forEach(({ spec, input }, key) => {
    const raw = input.value;
    const numeric = spec.kind === 'integer' || spec.kind === 'float';
    if (numeric) {
      const value = Number(raw);
      if (raw === '' || Number.isNaN(value)) {
        problems.push((spec.description || key) + '：需要数值');
        return;
      }
      if (spec.min != null && value < spec.min) {
        problems.push((spec.description || key) + '：不能小于 ' + spec.min);
        return;
      }
      if (spec.max != null && value > spec.max) {
        problems.push((spec.description || key) + '：不能大于 ' + spec.max);
        return;
      }
      if (value !== Number(spec.value)) { values[key] = value; changed.push(key); }
      return;
    }
    if (spec.max_len != null && String(raw).length > spec.max_len) {
      problems.push((spec.description || key) + '：超过 ' + spec.max_len + ' 字符');
      return;
    }
    if (raw !== String(spec.value)) { values[key] = raw; changed.push(key); }
  });
  return { values, problems, changed };
}

$('#settings-save').addEventListener('click', async () => {
  const hint = $('#settings-hint');
  const { values, problems } = collectSettingChanges();
  if (problems.length) {
    hint.className = 'error';
    hint.textContent = problems.slice(0, 3).join('；');
    return;
  }
  if (!Object.keys(values).length) {
    hint.className = 'muted';
    hint.textContent = '没有改动';
    return;
  }
  try {
    hint.className = 'muted';
    hint.textContent = '保存中…';
    await api('/api/settings', { method: 'PUT', headers: JSON_TYPE, body: JSON.stringify({ values }) });
    toast('已保存 ' + Object.keys(values).length + ' 项');
    await renderers.settings();
    $('#settings-hint').textContent = '已保存 ' + Object.keys(values).length + ' 项';
  } catch (failure) {
    hint.className = 'error';
    hint.textContent = failure.message;
  }
});

$('#settings-reload').addEventListener('click', () => {
  renderers.settings().catch((failure) => toast(failure.message, true));
});

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
    const created = cell(relativeTime(item.created_at));
    created.title = item.created_at || '';
    const lastUsed = cell(item.last_used_at ? relativeTime(item.last_used_at) : '从未');
    if (item.last_used_at) lastUsed.title = String(item.last_used_at);
    const state = cell(item.revoked_at ? '已吊销' : '有效');
    if (item.revoked_at) state.className = 'muted';
    body.appendChild(row([
      item.name,
      item.prefix,
      created,
      lastUsed,
      state,
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
    if (!hint.nextElementSibling || hint.nextElementSibling.id !== 'copy-key') {
      const copy = document.createElement('button');
      copy.id = 'copy-key';
      copy.className = 'ghost';
      copy.textContent = '复制';
      copy.addEventListener('click', async () => {
        try {
          await navigator.clipboard.writeText(created.plaintext);
          toast('已复制');
        } catch (_) { toast('复制失败，请手动选择', true); }
      });
      hint.insertAdjacentElement('afterend', copy);
    }
    await renderers.keys();
  } catch (failure) { toast(failure.message, true); }
});

renderers.backup = async () => {
  const payload = await api('/api/backups');
  const body = $('#backups tbody');
  body.textContent = '';
  (payload.data || []).forEach((item) => {
    const restore = document.createElement('button');
    restore.textContent = '恢复';
    restore.addEventListener('click', async () => {
      if (!confirm('合并恢复 ' + item.file + '？只补库中缺失的数据。')) return;
      try {
        const stats = await api('/api/restore', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ file: item.file })
        });
        toast('恢复完成：账号 +' + stats.accounts_inserted + '，文件 +' + stats.files_written);
        await renderers.backup();
      } catch (failure) { toast(failure.message, true); }
    });
    body.appendChild(row([
      item.file,
      Math.round(item.bytes / 1024) + ' KB',
      relativeTimeText(item.modified_at),
      restore
    ]));
  });
  if (!(payload.data || []).length) body.appendChild(row(['（还没有备份）', '', '', '']));
};

$('#make-backup').addEventListener('click', async () => {
  try {
    const made = await api('/api/backup', { method: 'POST' });
    $('#backup-hint').textContent = '已生成 ' + made.file + '（' + Math.round(made.bytes / 1024) + ' KB，账号 ' + made.accounts + '，图片 ' + made.images + '）';
    await renderers.backup();
  } catch (failure) { toast(failure.message, true); }
});


checkSession();
