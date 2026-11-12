import importlib.util, io, os, sys
sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding='utf-8', errors='replace')
HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location('acct_gui', os.path.join(HERE, 'account_gui.py'))

# 不联网的构造冒烟测试：验证所有控件能建起来、表格填充逻辑正确。
mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)
import tkinter as tk
root = tk.Tk()
app = mod.AccountGui(root)
root.update_idletasks()
root.update()
print('GUI 构造成功')
print('  列数:', len(app.tree['columns']))
print('  工具栏按钮:', sum(1 for w in root.winfo_children()))
# 验证表格填充逻辑（用假数据，不联网）
fake = {'total': 2, 'items': [
  {'id': 'x1', 'email': 'a@b.c', 'status': 'normal', 'quota': 5, 'success_count': 1, 'fail_count': 2,
   'credential': {'access_label': '有效', 'refresh_label': '有效', 'usable_label': '可用'}},
  {'id': 'x2', 'email': None, 'status': 'limited', 'quota': 0, 'success_count': 0, 'fail_count': 0,
   'credential': {'access_label': '即将过期', 'refresh_label': '缺失', 'usable_label': '可用'}},
]}
app._fill_table(fake)
rows = app.tree.get_children()
print('  表格行数:', len(rows))
print('  第1行:', app.tree.item(rows[0], 'values'))
print('  第2行:', app.tree.item(rows[1], 'values'))
assert len(rows) == 2
assert app.tree.item(rows[1], 'values')[0] == '(无邮箱)'
app._set_busy(True); app._set_busy(False)
print('  busy 切换 OK')
root.destroy()
print('冒烟测试全部通过')