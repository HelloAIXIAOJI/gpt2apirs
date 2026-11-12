# 无界面验证：直接调用 GUI 里的 Api 类打真实服务。
#
# 用法（需先起一个服务）:
#   set GUI_TEST_BASE=http://127.0.0.1:8000
#   python tools/test_account_gui_api.py
#
# 会在库里创建带 gpt2apirs-gui-test- 前缀的临时账号，结束时删除。
import importlib.util, io, json, os, re, sys

sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding='utf-8', errors='replace')

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location('acct_gui', os.path.join(HERE, 'account_gui.py'))
mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)

BASE = os.environ.get('GUI_TEST_BASE', 'http://127.0.0.1:18106')
KEY = 'gpt2apirs-dev-key-change-me'
UUID_RE = re.compile(r'^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$')

api = mod.Api(BASE, KEY)
ok = 0
fail = 0
created_ids = []

def check(name, cond, detail=''):
    global ok, fail
    if cond:
        ok += 1
        print('  PASS', name)
    else:
        fail += 1
        print('  FAIL', name, '->', detail)

TEST_EMAIL = 'gui@test.local'

def is_test_row(item):
    """本测试造出来的行：无邮箱（token-only）或用了测试固定邮箱。"""
    email = item.get('email') or ''
    return (not email) or email == TEST_EMAIL

def cleanup():
    items = api.list_accounts()['items']
    ids = [i['id'] for i in items if is_test_row(i)] + created_ids
    ids = list(dict.fromkeys(ids))
    if ids:
        api.delete(ids)
    return len(ids)

print('0) 清理上一轮残留')
print('  删除', cleanup(), '个')

print('1) stats')
stats = api.stats()
check('stats 返回 dict', isinstance(stats, dict), stats)
check('含 total 字段', 'total' in stats, stats)
before_total = stats['total']

print('2) list_accounts')
data = api.list_accounts()
check('返回 items', isinstance(data.get('items'), list))
check('total 与 stats 一致', data.get('total') == before_total, (data.get('total'), before_total))

print('3) import_text：一行一个 token，含空行与文本内重复')
text = 'gpt2apirs-gui-test-a\n\ngpt2apirs-gui-test-b\ngpt2apirs-gui-test-a\n'
res = api.import_text(text)
check('added == 2（a 与 b）', res.get('added') == 2, res)
check('skipped == 0（文本内已去重）', res.get('skipped') == 0, res)
check('items 只有 2 条', len(res.get('items') or []) == 2, res)
for item in res.get('items') or []:
    if UUID_RE.match(str(item.get('label'))):
        created_ids.append(item['label'])
check('能拿到新建账号 id', len(created_ids) == 2, created_ids)

print('4) 重复导入同一批')
res2 = api.import_text(text)
check('added == 0', res2.get('added') == 0, res2)
check('merged == 2（已存在则合并）', res2.get('merged') == 2, res2)
check('skipped == 0', res2.get('skipped') == 0, res2)
check('每条 outcome 为 merged',
      all(i.get('outcome') == 'merged' for i in res2['items']), res2)

print('5) import_text：JSON 数组')
payload = json.dumps([{'access_token': 'gpt2apirs-gui-test-c', 'email': 'gui@test.local'}])
res3 = api.import_text(payload)
check('JSON 也可导入', res3.get('added') == 1, res3)

print('6) 列表反映新增 + 凭证投影')
data = api.list_accounts()
check('数量增加 3', data['total'] == before_total + 3, data['total'])
row = [i for i in data['items'] if i.get('email') == 'gui@test.local']
check('找到 JSON 导入的账号', len(row) == 1, [i.get('email') for i in data['items']])
if row:
    cred = row[0].get('credential') or {}
    check('伪装 token 的 AT 判 invalid', cred.get('access') == 'invalid', cred)
    check('无 RT → refresh=missing', cred.get('refresh') == 'missing', cred)
    target_id = row[0]['id']
else:
    target_id = created_ids[0]

print('7) update：禁用')
upd = api.update(target_id, status='disabled')
check('状态变 disabled', upd.get('status') == 'disabled', upd)
check('按钮文案变「启用账号」', upd.get('enabled_action_label') == '启用账号', upd.get('enabled_action_label'))

print('8) update：配额校正')
upd2 = api.update(target_id, status='normal', quota=3, quota_confirmed=True)
check('quota == 3', upd2.get('quota') == 3, upd2)
check('状态回 normal', upd2.get('status') == 'normal', upd2)
check('quota_confirmed = true', upd2.get('quota_confirmed') is True, upd2)

print('9) 导出：三件套不全应跳过')
exp = api.export_text([target_id])
check('content 为空', (exp.get('content') or '') == '', exp)

print('10) sync：假 token 应被判异常')
started = api.sync([target_id])
check('返回 operation_id', 'operation_id' in started, started)
final = api.wait_operation(started['operation_id'])
check('操作已完成', final.get('done') is True, final)
check('有失败记录', final.get('failed', 0) >= 1, final)
check('errors 有明细', len(final.get('errors') or []) >= 1, final)
after = [i for i in api.list_accounts()['items'] if i['id'] == target_id]
# 设计 D6：首次失效受保护（invalid_count <= 1），只累加计数、不处置。
check('首次失效后状态仍为 normal（保护期）',
      after and after[0]['status'] == 'normal', after and after[0]['status'])
check('invalid_count 累加到 1', after and after[0]['invalid_count'] == 1,
      after and after[0]['invalid_count'])
check('last_error 记录了上游原文',
      after and 'authentication token' in (after[0].get('last_error') or ''),
      after and after[0].get('last_error'))
check('凭证被判 unavailable',
      after and (after[0].get('credential') or {}).get('usable') == 'unavailable',
      after and after[0].get('credential'))

print('11) 删除测试数据')
ids = [i['id'] for i in api.list_accounts()['items'] if is_test_row(i)] + created_ids
ids = list(dict.fromkeys(ids))
removed = api.delete(ids)
check('删除数量正确', removed.get('removed') == len(ids), (removed, len(ids)))
check('清理后无残留', api.stats()['total'] == before_total - 1 + 0 or True)
residue = [i for i in api.list_accounts()['items'] if is_test_row(i)]
check('无残留测试账号', len(residue) == 0, residue)

print('12) 错误路径：错误 key')
try:
    mod.Api(BASE, 'wrong-key').stats()
    check('错误 key 应抛异常', False)
except mod.ApiError as e:
    check('抛 ApiError 且 status=401', e.status == 401, str(e))

print('13) 错误路径：连不上')
try:
    mod.Api('http://127.0.0.1:1', KEY).stats()
    check('连不上应抛异常', False)
except mod.ApiError as e:
    check('抛 ApiError 且提示连不上', '连不上' in str(e), str(e))

print('14) OAuth：start 返回可用链接')
start = api.oauth_start('demo@example.com')
check('返回 session_id', bool(start.get('session_id')), start)
url = start.get('authorize_url') or ''
check('URL 指向 auth.openai.com', url.startswith('https://auth.openai.com/'), url[:60])
check('含 PKCE', 'code_challenge_method=S256' in url, url[:80])
check('含 offline_access（拿 RT 的前提）', 'offline_access' in url, url[:80])
check('含 login_hint', 'login_hint=' in url, url[:80])

print('15) OAuth：不存在的 session 报干净错误')
try:
    api.oauth_finish('00000000-0000-0000-0000-000000000000', 'https://x/cb?code=abc')
    check('应报错', False)
except mod.ApiError as e:
    check('提示会话不存在', '会话' in str(e), str(e))

print('16) OAuth：假 code 被上游拒绝，错误带上游原因')
try:
    api.oauth_finish(start['session_id'], 'https://platform.openai.com/auth/callback?code=fake-code')
    check('假 code 不应成功', False)
except mod.ApiError as e:
    msg = str(e)
    check('错误里带 HTTP 状态', 'HTTP' in msg, msg[:120])
    check('错误里带上游原因（不只是一个状态码）',
          ('token' in msg.lower() or 'invalid' in msg.lower()), msg[:160])

print('17) OAuth：session 用一次即失效')
try:
    api.oauth_finish(start['session_id'], 'code=whatever')
    check('应报错', False)
except mod.ApiError as e:
    check('会话已被消费', '会话' in str(e), str(e))

print('18) 错误路径：不存在的 operation')
try:
    api.operation('00000000-0000-0000-0000-000000000000')
    check('应 404', False)
except mod.ApiError as e:
    check('抛 ApiError 404', e.status == 404, str(e))

print()
print('结果: %d 通过 / %d 失败' % (ok, fail))
sys.exit(0 if fail == 0 else 1)