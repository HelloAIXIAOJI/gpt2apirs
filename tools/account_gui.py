#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""账号池 GUI 管理工具。

单文件，只依赖 Python 标准库（tkinter + urllib），不需要 pip install。

用法:

    python tools/account_gui.py

它只是 /api/accounts* 这组接口的一层薄壳 —— 所有判定都在服务端，
GUI 不做任何本地状态推断，避免出现「界面显示能用、实际不能用」。

为什么不用 requests：目标是「拷到任意一台装了 Python 的机器就能跑」。
requests 不是标准库，多一个安装步骤就多一个失败点；
这里只需要 GET/POST/DELETE + JSON，标准库够用。
"""

import json
import os
import queue
import threading
import time
import tkinter as tk
import urllib.error
import urllib.request
from tkinter import filedialog, messagebox, ttk

CONFIG_PATH = os.path.join(os.path.expanduser("~"), ".gpt2apirs", "gui_config.json")
DEFAULT_BASE = "http://127.0.0.1:8000"
DEFAULT_PROMPT = "a small red ceramic teapot on a wooden table"

# 轮询批量操作的间隔与总超时
POLL_INTERVAL_SECS = 1.0
POLL_TIMEOUT_SECS = 600.0


class ApiError(Exception):
    """接口调用失败。"""

    def __init__(self, message, status=None, code=None):
        super().__init__(message)
        self.status = status
        self.code = code


class Api:
    """账号池管理接口的极薄封装。"""

    def __init__(self, base_url, auth_key, timeout=60):
        self.base_url = base_url.rstrip("/")
        self.auth_key = auth_key
        self.timeout = timeout

    # ---- 底层 ----
    def _request(self, method, path, body=None):
        url = self.base_url + path
        data = None
        headers = {"Authorization": "Bearer " + self.auth_key}
        if body is not None:
            data = json.dumps(body, ensure_ascii=False).encode("utf-8")
            headers["Content-Type"] = "application/json"
        request = urllib.request.Request(url, data=data, headers=headers, method=method)
        try:
            with urllib.request.urlopen(request, timeout=self.timeout) as response:
                raw = response.read().decode("utf-8", errors="replace")
                return json.loads(raw) if raw else {}
        except urllib.error.HTTPError as error:
            raw = error.read().decode("utf-8", errors="replace")
            message, code = raw, None
            try:
                payload = json.loads(raw)
                err = payload.get("error") or payload.get("detail") or {}
                if isinstance(err, dict):
                    message = err.get("message") or message
                    code = err.get("code")
                elif isinstance(err, str):
                    message = err
            except (ValueError, AttributeError):
                pass
            raise ApiError("HTTP %s: %s" % (error.code, message), error.code, code)
        except urllib.error.URLError as error:
            raise ApiError("连不上 %s：%s" % (self.base_url, error.reason))
        except ValueError as error:
            raise ApiError("响应不是合法 JSON：%s" % error)

    # ---- 业务 ----
    def stats(self):
        return self._request("GET", "/api/accounts/stats")

    def list_accounts(self, page=1, page_size=200, query=""):
        return self._request(
            "GET", "/api/accounts?" + query + "page=%d&page_size=%d" % (page, page_size)
        )

    def import_text(self, text):
        """导入任意文本。

        服务端 parse_any 会自动识别「一行一个 token」与「JSON」两种形态，
        因此这里统一走 content 字段。
        """
        return self._request("POST", "/api/accounts", {"content": text})

    def update(self, account_id, **fields):
        payload = {"id": account_id}
        payload.update({k: v for k, v in fields.items() if v is not None})
        return self._request("POST", "/api/accounts/update", payload)

    def delete(self, ids):
        return self._request("DELETE", "/api/accounts", {"ids": list(ids)})

    def sync(self, ids):
        return self._request("POST", "/api/accounts/sync", {"ids": list(ids)})

    def refresh_tokens(self, ids):
        return self._request("POST", "/api/accounts/refresh-token", {"ids": list(ids)})

    def operation(self, operation_id):
        return self._request("GET", "/api/accounts/operations/" + operation_id)

    def test(self, account_id, prompt=DEFAULT_PROMPT, model="gpt-image-2"):
        return self._request(
            "POST", "/api/accounts/" + account_id + "/test",
            {"prompt": prompt, "model": model},
        )

    def export_text(self, ids):
        return self._request(
            "POST", "/api/accounts/export", {"ids": list(ids), "format": "txt"}
        )

    def oauth_start(self, email_hint=None):
        """发起 OAuth 登录，返回 {session_id, authorize_url}。

        不自动打开浏览器 —— 由界面把链接展示给用户自行复制。
        """
        payload = {}
        if email_hint:
            payload["email_hint"] = email_hint
        return self._request("POST", "/api/accounts/oauth/start", payload)

    def oauth_finish(self, session_id, callback):
        """提交回调 URL 或裸 code，换取 token 并入库。"""
        return self._request(
            "POST", "/api/accounts/oauth/finish",
            {"session_id": session_id, "callback": callback},
        )

    def wait_operation(self, operation_id, on_tick=None):
        """轮询批量操作直到结束。"""
        deadline = time.time() + POLL_TIMEOUT_SECS
        while time.time() < deadline:
            progress = self.operation(operation_id)
            if on_tick:
                on_tick(progress)
            if progress.get("done"):
                return progress
            time.sleep(POLL_INTERVAL_SECS)
        raise ApiError("操作 %s 超时未完成" % operation_id)


# ---------------------------------------------------------------------------
# GUI
# ---------------------------------------------------------------------------

COLUMNS = [
    ("email", "邮箱 / ID", 230),
    ("status", "状态", 70),
    ("quota", "配额", 55),
    ("access", "AT", 90),
    ("refresh", "RT", 70),
    ("usable", "可用性", 80),
    ("counts", "成功/失败", 85),
    ("id", "ID", 250),
]

STATUS_LABEL = {
    "normal": "正常",
    "limited": "限流",
    "abnormal": "异常",
    "disabled": "禁用",
}


class OAuthDialog:
    """OAuth 登录对话框。

    设计上刻意**不自动打开浏览器**：
    用户可能要在无痕窗口里登录另一个账号，自动弹窗反而添乱。
    """

    def __init__(self, parent, api, on_done):
        self.api = api
        self.on_done = on_done
        self.session_id = None
        self.queue = queue.Queue()

        self.win = tk.Toplevel(parent)
        self.win.title("OAuth 登录（获取 refresh_token）")
        self.win.geometry("760x520")
        self.win.transient(parent)
        self.win.grab_set()

        self._build()

    def _build(self):
        pad = 8

        ttk.Label(
            self.win,
            text="流程：① 获取授权链接 → ② 复制到浏览器登录 → ③ 把回调地址粘回这里 → ④ 完成授权",
            wraplength=720, justify="left",
        ).pack(fill="x", padx=pad, pady=(pad, 4))

        row = ttk.Frame(self.win)
        row.pack(fill="x", padx=pad, pady=4)
        ttk.Label(row, text="邮箱提示（可选，填了会预填登录页）").pack(side="left")
        self.entry_email = ttk.Entry(row, width=40)
        self.entry_email.pack(side="left", padx=6)
        self.btn_start = ttk.Button(row, text="① 获取授权链接", command=self.on_start)
        self.btn_start.pack(side="left", padx=6)

        ttk.Label(self.win, text="授权链接（复制到浏览器打开；不自动打开）").pack(
            anchor="w", padx=pad, pady=(pad, 2)
        )
        self.text_url = tk.Text(self.win, height=6, wrap="char")
        self.text_url.pack(fill="x", padx=pad)
        bar1 = ttk.Frame(self.win)
        bar1.pack(fill="x", padx=pad, pady=4)
        ttk.Button(bar1, text="复制链接", command=self.on_copy_url).pack(side="left")
        self.lbl_session = ttk.Label(bar1, text="session: （未开始）")
        self.lbl_session.pack(side="left", padx=10)

        ttk.Label(
            self.win,
            text="回调地址（登录后浏览器地址栏里以 /auth/callback?code= 开头那条，整条粘贴）",
            wraplength=720, justify="left",
        ).pack(anchor="w", padx=pad, pady=(pad, 2))
        self.entry_callback = ttk.Entry(self.win)
        self.entry_callback.pack(fill="x", padx=pad)

        bar2 = ttk.Frame(self.win)
        bar2.pack(fill="x", padx=pad, pady=6)
        self.btn_finish = ttk.Button(
            bar2, text="④ 完成授权", command=self.on_finish, state="disabled"
        )
        self.btn_finish.pack(side="left")
        ttk.Button(bar2, text="关闭", command=self.win.destroy).pack(side="left", padx=6)

        self.lbl_status = ttk.Label(self.win, text="等待获取授权链接…", wraplength=720, justify="left")
        self.lbl_status.pack(fill="x", padx=pad, pady=(4, pad))

    # ---- 异步执行工具 ----
    # Tkinter 不是线程安全的：worker 线程只往队列里放结果，
    # 一切 GUI 操作都回到主线程做（用 after 轮询）。
    def _run(self, label, worker, on_done):
        self.lbl_status.configure(text=label + "…")
        self.btn_start.configure(state="disabled")

        def runner():
            try:
                self.queue.put((
                    ("ok", worker(), None)
                ))
            except ApiError as error:
                self.queue.put(("error", None, str(error)))
            except Exception as error:  # noqa: BLE001
                self.queue.put(
                    ("error", None, "%s: %s" % (type(error).__name__, error))
                )

        def poller():
            try:
                kind, result, message = self.queue.get_nowait()
            except queue.Empty:
                self.win.after(50, poller)
                return
            self._done(on_done, result if kind == "ok" else None, message)

        threading.Thread(target=runner, daemon=True).start()
        self.win.after(50, poller)

    def _done(self, on_done, result, message):
        self.btn_start.configure(state="normal")
        if message is not None:
            self.lbl_status.configure(text="失败：" + message)
            messagebox.showerror("OAuth", message, parent=self.win)
            return
        on_done(result)

    # ---- 动作 ----
    def on_start(self):
        hint = self.entry_email.get().strip() or None
        self._run(
            "获取授权链接",
            lambda: self.api.oauth_start(hint),
            self._after_start,
        )

    def _after_start(self, result):
        self.session_id = result.get("session_id")
        url = result.get("authorize_url") or ""
        self.text_url.delete("1.0", "end")
        self.text_url.insert("1.0", url)
        self.lbl_session.configure(text="session: " + str(self.session_id))
        self.btn_finish.configure(state="normal")
        self.lbl_status.configure(
            text="已生成链接。复制到浏览器登录，然后把回调地址粘到下面。"
        )

    def on_copy_url(self):
        url = self.text_url.get("1.0", "end").strip()
        if not url:
            messagebox.showwarning("复制", "还没有链接，先点「① 获取授权链接」", parent=self.win)
            return
        self.win.clipboard_clear()
        self.win.clipboard_append(url)
        self.lbl_status.configure(text="已复制到剪贴板（%d 字符）" % len(url))

    def on_finish(self):
        callback = self.entry_callback.get().strip()
        if not callback:
            messagebox.showwarning("完成授权", "请先粘贴回调地址或 code", parent=self.win)
            return
        if not self.session_id:
            messagebox.showwarning("完成授权", "请先获取授权链接", parent=self.win)
            return

        def done(result):
            added = result.get("added", 0) or 0
            merged = result.get("merged", 0) or 0
            items = result.get("items") or []
            lines = [
                "新增 %d，合并 %d" % (added, merged),
                "",
            ]
            for item in items:
                lines.append("  [%s] %s %s" % (
                    item.get("outcome"), item.get("label"),
                    item.get("reason") or "",
                ))
            lines.extend(["", "建议接着点「同步配额」拉真实额度。"])
            messagebox.showinfo("OAuth 完成", "\n".join(lines), parent=self.win)
            self.win.destroy()
            self.on_done(result)

        self._run(
            "交换授权码",
            lambda: self.api.oauth_finish(self.session_id, callback),
            done,
        )

class AccountGui:
    def __init__(self, root):
        self.root = root
        self.api = None
        self.rows = {}          # tree item id -> account dict
        self.events = queue.Queue()

        root.title("账号池管理")
        root.geometry("1120x760")
        root.minsize(900, 600)

        self._build_connection_bar()
        self._build_import_panel()
        self._build_table()
        self._build_action_bar()
        self._build_status_bar()

        self._load_config()

    # ---- 构建界面 ----
    def _build_connection_bar(self):
        frame = ttk.LabelFrame(self.root, text="连接")
        frame.pack(fill="x", padx=8, pady=(8, 4))

        ttk.Label(frame, text="服务地址").grid(row=0, column=0, padx=4, pady=4, sticky="w")
        self.entry_base = ttk.Entry(frame, width=30)
        self.entry_base.insert(0, DEFAULT_BASE)
        self.entry_base.grid(row=0, column=1, padx=4, pady=4, sticky="w")

        ttk.Label(frame, text="AUTH_KEY").grid(row=0, column=2, padx=4, pady=4, sticky="w")
        self.entry_key = ttk.Entry(frame, width=40, show="*")
        self.entry_key.grid(row=0, column=3, padx=4, pady=4, sticky="w")

        ttk.Button(frame, text="保存并连接", command=self.on_connect).grid(
            row=0, column=4, padx=4, pady=4
        )
        self.var_show_key = tk.BooleanVar(value=False)
        ttk.Checkbutton(
            frame, text="显示", variable=self.var_show_key, command=self._toggle_key_visibility
        ).grid(row=0, column=5, padx=4, pady=4)

    def _build_import_panel(self):
        frame = ttk.LabelFrame(
            self.root, text="导入账号（粘贴 Access Token，一行一个；也支持整段 JSON）"
        )
        frame.pack(fill="x", padx=8, pady=4)

        self.text_import = tk.Text(frame, height=6, wrap="none")
        self.text_import.pack(fill="x", padx=6, pady=(6, 4))

        bar = ttk.Frame(frame)
        bar.pack(fill="x", padx=6, pady=(0, 6))
        ttk.Button(bar, text="从文件读取…", command=self.on_pick_file).pack(side="left", padx=(0, 6))
        ttk.Button(
            bar, text="清空",
            command=lambda: self.text_import.delete("1.0", "end"),
        ).pack(side="left", padx=(0, 6))
        self.btn_import = ttk.Button(bar, text="导入", command=self.on_import)
        self.btn_import.pack(side="left")

    def _build_table(self):
        frame = ttk.LabelFrame(self.root, text="账号列表")
        frame.pack(fill="both", expand=True, padx=8, pady=4)

        container = ttk.Frame(frame)
        container.pack(fill="both", expand=True, padx=6, pady=6)

        self.tree = ttk.Treeview(
            container, columns=[c[0] for c in COLUMNS],
            show="headings", selectmode="extended",
        )
        for key, title, width in COLUMNS:
            self.tree.heading(key, text=title)
            self.tree.column(key, width=width, anchor="w", stretch=(key in ("email", "id")))

        y_scroll = ttk.Scrollbar(container, orient="vertical", command=self.tree.yview)
        x_scroll = ttk.Scrollbar(container, orient="horizontal", command=self.tree.xview)
        self.tree.configure(yscrollcommand=y_scroll.set, xscrollcommand=x_scroll.set)

        self.tree.grid(row=0, column=0, sticky="nsew")
        y_scroll.grid(row=0, column=1, sticky="ns")
        x_scroll.grid(row=1, column=0, sticky="ew")
        container.rowconfigure(0, weight=1)
        container.columnconfigure(0, weight=1)

        self.tree.bind("<Double-1>", lambda _event: self.on_test())

    def _build_action_bar(self):
        frame = ttk.Frame(self.root)
        frame.pack(fill="x", padx=8, pady=4)

        self.btn_refresh_list = ttk.Button(frame, text="刷新列表", command=self.on_refresh_list)
        self.btn_refresh_list.pack(side="left", padx=2)
        ttk.Separator(frame, orient="vertical").pack(side="left", fill="y", padx=6)

        self.btn_sync = ttk.Button(
            frame, text="同步配额", command=self.on_sync
        )
        self.btn_sync.pack(side="left", padx=2)
        self.btn_refresh_at = ttk.Button(
            frame, text="刷新 AT", command=self.on_refresh_tokens
        )
        self.btn_refresh_at.pack(side="left", padx=2)
        ttk.Separator(frame, orient="vertical").pack(side="left", fill="y", padx=6)

        ttk.Button(frame, text="启用", command=lambda: self.on_set_status("normal")).pack(
            side="left", padx=2
        )
        ttk.Button(frame, text="禁用", command=lambda: self.on_set_status("disabled")).pack(
            side="left", padx=2
        )
        ttk.Button(frame, text="删除", command=self.on_delete).pack(side="left", padx=2)
        ttk.Separator(frame, orient="vertical").pack(side="left", fill="y", padx=6)

        self.btn_test = ttk.Button(
            frame, text="测试出图", command=self.on_test
        )
        self.btn_test.pack(side="left", padx=2)
        ttk.Button(frame, text="导出 AT", command=self.on_export).pack(side="left", padx=2)
        ttk.Button(frame, text="统计", command=self.on_stats).pack(side="left", padx=2)
        ttk.Separator(frame, orient="vertical").pack(side="left", fill="y", padx=6)
        ttk.Button(frame, text="OAuth 登录", command=self.on_oauth).pack(side="left", padx=2)

    def _build_status_bar(self):
        frame = ttk.Frame(self.root)
        frame.pack(fill="x", side="bottom")
        self.var_status = tk.StringVar(value="未连接")
        ttk.Label(frame, textvariable=self.var_status, anchor="w").pack(
            fill="x", padx=8, pady=4
        )

    def _toggle_key_visibility(self):
        self.entry_key.configure(show="" if self.var_show_key.get() else "*")

    # ---- 配置 ----
    def _load_config(self):
        try:
            with open(CONFIG_PATH, "r", encoding="utf-8") as handle:
                config = json.load(handle)
        except (OSError, ValueError):
            return
        base = config.get("base_url")
        if base:
            self.entry_base.delete(0, "end")
            self.entry_base.insert(0, base)
        key = config.get("auth_key")
        if key:
            self.entry_key.delete(0, "end")
            self.entry_key.insert(0, key)

    def _save_config(self):
        config = {
            "base_url": self.entry_base.get().strip(),
            "auth_key": self.entry_key.get().strip(),
        }
        os.makedirs(os.path.dirname(CONFIG_PATH), exist_ok=True)
        with open(CONFIG_PATH, "w", encoding="utf-8") as handle:
            json.dump(config, handle, ensure_ascii=False, indent=2)

    # ---- 线程工具 ----
    def _run_async(self, label, worker, on_done):
        """在后台线程跑 worker，完成后把结果交回主线程。

        每次调用都用**独立队列**：共享队列会让两个并发的 _run_async 抢到
        对方的结果（例如「OAuth 后同步」与「刷新列表」同时进行时）。
        """
        self._set_busy(True)
        self.var_status.set(label + "…")
        mine = queue.Queue()

        def runner():
            try:
                mine.put(("ok", worker(), None))
            except ApiError as error:
                mine.put(("error", None, str(error)))
            except Exception as error:  # 兜底，避免线程静默死掉
                mine.put(("error", None, "%s: %s" % (type(error).__name__, error)))

        def poller():
            try:
                kind, result, message = mine.get_nowait()
            except queue.Empty:
                self.root.after(100, poller)
                return
            self._set_busy(False)
            if kind == "error":
                self.var_status.set("失败：" + message)
                messagebox.showerror(label, message)
                return
            try:
                on_done(result)
            except Exception as error:  # noqa: BLE001
                messagebox.showerror(label, str(error))

        threading.Thread(target=runner, daemon=True).start()
        self.root.after(100, poller)

    def _set_busy(self, busy):
        state = "disabled" if busy else "normal"
        for button in (
            self.btn_import,
            self.btn_refresh_list,
            self.btn_sync,
            self.btn_refresh_at,
            self.btn_test,
        ):
            button.configure(state=state)
        self.root.configure(cursor="watch" if busy else "")

    def _require_api(self):
        if self.api is None:
            raise ApiError(
                "请先填写服务地址与 AUTH_KEY 并点「保存并连接」"
            )
        return self.api

    # ---- 动作 ----
    def on_connect(self):
        base = self.entry_base.get().strip()
        key = self.entry_key.get().strip()
        if not base or not key:
            messagebox.showwarning("连接", "服务地址与 AUTH_KEY 都不能为空")
            return
        api = Api(base, key)
        try:
            self._save_config()
        except OSError as error:
            messagebox.showwarning("保存配置", "配置写入失败：%s" % error)
        self._run_async(
            "连接并读取列表",
            lambda: api.list_accounts(),
            lambda data: self._after_connect(api, data),
        )

    def _after_connect(self, api, data):
        self.api = api
        self._fill_table(data)
        self.var_status.set(
            "已连接 %s，共 %d 个账号" % (api.base_url, data.get("total", 0))
        )

    def _fill_table(self, data):
        self.tree.delete(*self.tree.get_children())
        self.rows.clear()
        for item in data.get("items", []):
            credential = item.get("credential") or {}
            values = (
                item.get("email") or "(无邮箱)",
                STATUS_LABEL.get(item.get("status"), item.get("status") or ""),
                str(item.get("quota", 0)),
                credential.get("access_label") or "",
                credential.get("refresh_label") or "",
                credential.get("usable_label") or "",
                "%s/%s" % (item.get("success_count", 0), item.get("fail_count", 0)),
                item.get("id") or "",
            )
            tree_id = self.tree.insert("", "end", values=values)
            self.rows[tree_id] = item
        self.var_status.set("共 %d 个账号" % data.get("total", 0))

    def _selected_accounts(self):
        return [self.rows[i] for i in self.tree.selection() if i in self.rows]

    def _selected_ids(self):
        return [item["id"] for item in self._selected_accounts()]

    def on_refresh_list(self):
        try:
            api = self._require_api()
        except ApiError as error:
            messagebox.showerror("刷新列表", str(error))
            return
        self._run_async("读取列表", lambda: api.list_accounts(), self._fill_table)

    def on_pick_file(self):
        path = filedialog.askopenfilename(
            title="选择包含 token 或 JSON 的文件",
            filetypes=[("文本/JSON", "*.txt *.json"), ("全部文件", "*.*")],
        )
        if not path:
            return
        try:
            with open(path, "r", encoding="utf-8", errors="replace") as handle:
                content = handle.read()
        except OSError as error:
            messagebox.showerror("读取文件", str(error))
            return
        self.text_import.delete("1.0", "end")
        self.text_import.insert("1.0", content)
        self.var_status.set(
            "已读入 %s（%d 字符），点「导入」提交" % (os.path.basename(path), len(content))
        )

    def on_import(self):
        text = self.text_import.get("1.0", "end").strip()
        if not text:
            messagebox.showwarning("导入", "请先粘贴 Access Token 或打开文件")
            return
        try:
            api = self._require_api()
        except ApiError as error:
            messagebox.showerror("导入", str(error))
            return

        def done(result):
            added = result.get("added", 0)
            skipped = result.get("skipped", 0)
            lines = ["新增 %d，跳过 %d" % (added, skipped), ""]
            for item in (result.get("items") or [])[:40]:
                reason = item.get("reason")
                suffix = ("- " + reason) if reason else ""
                lines.append(
                    "  [%s] %s %s" % (item.get("outcome"), item.get("label"), suffix)
                )
            if added == 0 and skipped > 0:
                lines.extend(["", "全部是同 token 重复导入（access_token 已存在）。"])
            messagebox.showinfo("导入结果", "\n".join(lines))
            self.text_import.delete("1.0", "end")
            self.var_status.set(
                "导入完成：新增 %d，跳过 %d。新号配额未知，建议点「同步配额」"
                % (added, skipped)
            )
            self.on_refresh_list()

        self._run_async("导入账号", lambda: api.import_text(text), done)

    def on_sync(self):
        try:
            api = self._require_api()
        except ApiError as error:
            messagebox.showerror("同步", str(error))
            return
        ids = self._selected_ids()
        scope = ("选中 %d 个" % len(ids)) if ids else "全部需同步的账号"

        def worker():
            started = api.sync(ids)
            return api.wait_operation(started["operation_id"])

        def done(final):
            messagebox.showinfo(
                "同步完成",
                "范围：%s\n成功 %d，失败 %d%s"
                % (
                    scope,
                    final.get("succeeded", 0),
                    final.get("failed", 0),
                    self._format_errors(final),
                ),
            )
            self.on_refresh_list()

        self._run_async("同步配额（%s）" % scope, worker, done)

    def on_refresh_tokens(self):
        try:
            api = self._require_api()
        except ApiError as error:
            messagebox.showerror("刷新 AT", str(error))
            return
        ids = self._selected_ids()
        if not ids:
            messagebox.showwarning("刷新 AT", "请先在列表里选中账号（只刷新选中的）")
            return

        def worker():
            started = api.refresh_tokens(ids)
            return api.wait_operation(started["operation_id"])

        def done(final):
            messagebox.showinfo(
                "刷新 AT 完成",
                "成功 %d，失败 %d%s"
                % (
                    final.get("succeeded", 0),
                    final.get("failed", 0),
                    self._format_errors(final),
                ),
            )
            self.on_refresh_list()

        self._run_async("刷新 AT（%d 个）" % len(ids), worker, done)

    def on_set_status(self, status):
        try:
            api = self._require_api()
        except ApiError as error:
            messagebox.showerror("更新状态", str(error))
            return
        accounts = self._selected_accounts()
        if not accounts:
            messagebox.showwarning("更新状态", "请先选中账号")
            return
        label = STATUS_LABEL.get(status, status)
        if not messagebox.askyesno("确认", "把选中的 %d 个账号设为「%s」？" % (len(accounts), label)):
            return

        def worker():
            for account in accounts:
                api.update(account["id"], status=status)
            return len(accounts)

        def done(count):
            self.var_status.set("已把 %d 个账号设为「%s」" % (count, label))
            self.on_refresh_list()

        self._run_async("更新状态", worker, done)

    def on_delete(self):
        try:
            api = self._require_api()
        except ApiError as error:
            messagebox.showerror("删除", str(error))
            return
        ids = self._selected_ids()
        if not ids:
            messagebox.showwarning("删除", "请先选中账号")
            return
        if not messagebox.askyesno(
            "确认删除", "删除选中的 %d 个账号？此操作不可撤销。" % len(ids)
        ):
            return

        def done(result):
            self.var_status.set("已删除 %d 个" % result.get("removed", 0))
            self.on_refresh_list()

        self._run_async("删除账号", lambda: api.delete(ids), done)

    def on_test(self):
        try:
            api = self._require_api()
        except ApiError as error:
            messagebox.showerror("测试", str(error))
            return
        accounts = self._selected_accounts()
        if len(accounts) != 1:
            messagebox.showwarning(
                "测试", "请只选中一个账号（测试会真实生成一张图并消耗配额）"
            )
            return
        account = accounts[0]

        def done(result):
            if result.get("status") == "success":
                messagebox.showinfo(
                    "测试成功",
                    "耗时 %s ms\n图片 %s 字节\n尺寸 %s"
                    % (
                        result.get("elapsed_ms"),
                        result.get("bytes"),
                        result.get("size") or "-",
                    ),
                )
            else:
                messagebox.showwarning(
                    "测试失败",
                    "耗时 %s ms\n%s"
                    % (
                        result.get("elapsed_ms"),
                        result.get("message") or "未知原因",
                    ),
                )
            self.on_refresh_list()

        self._run_async(
            "测试出图（可能耗时 1~2 分钟）",
            lambda: api.test(account["id"]),
            done,
        )

    def on_export(self):
        try:
            api = self._require_api()
        except ApiError as error:
            messagebox.showerror("导出", str(error))
            return
        ids = self._selected_ids()
        if not ids:
            messagebox.showwarning("导出", "请先选中账号")
            return

        def done(result):
            content = result.get("content") or ""
            if not content.strip():
                messagebox.showwarning(
                    "导出结果",
                    "没有可导出的账号。\n"
                    "导出的前提是 access_token / refresh_token / id_token 三者齐全。",
                )
                return
            path = filedialog.asksaveasfilename(
                title="保存 access token",
                defaultextension=".txt",
                filetypes=[("文本", "*.txt")],
            )
            if not path:
                return
            with open(path, "w", encoding="utf-8") as handle:
                handle.write(content)
            self.var_status.set("已导出到 " + path)

        self._run_async("导出", lambda: api.export_text(ids), done)

    def on_stats(self):
        try:
            api = self._require_api()
        except ApiError as error:
            messagebox.showerror("统计", str(error))
            return

        def done(stats):
            messagebox.showinfo(
                "账号池统计",
                "总数 %s\n正常 %s　限流 %s\n异常 %s　禁用 %s\n\n"
                "正常账号总配额 %s\n累计成功 %s　累计失败 %s"
                % (
                    stats.get("total", 0),
                    stats.get("normal", 0),
                    stats.get("limited", 0),
                    stats.get("abnormal", 0),
                    stats.get("disabled", 0),
                    stats.get("total_quota", 0),
                    stats.get("total_success", 0),
                    stats.get("total_fail", 0),
                ),
            )

        self._run_async("读取统计", api.stats, done)

    def on_oauth(self):
        """打开 OAuth 对话框。链接只展示、不自动打开浏览器。"""
        try:
            api = self._require_api()
        except ApiError as error:
            messagebox.showerror("OAuth 登录", str(error))
            return
        OAuthDialog(self.root, api, on_done=self._after_oauth)

    def _after_oauth(self, result):
        """OAuth 成功后：等自动触发的配额同步完成，再刷新列表。

        同步与拉列表放在同一个后台任务里，避免两个 _run_async 并发，
        也保证「已同步配额」这条提示不会被刷新列表的完成消息覆盖。
        """
        operation_id = result.get("sync_operation_id")
        api = self.api
        label = ""
        for item in result.get("items") or []:
            label = item.get("label") or ""

        if not operation_id or api is None:
            self.on_refresh_list()
            self.var_status.set("OAuth 完成。请点「同步配额」拉取额度")
            return

        def worker():
            progress = api.wait_operation(operation_id)
            data = api.list_accounts()
            return progress, data

        def done(pair):
            progress, data = pair
            self._fill_table(data)
            succeeded = progress.get("succeeded", 0) or 0
            failed = progress.get("failed", 0) or 0
            if succeeded:
                self.var_status.set(
                    "OAuth 完成并已同步配额（%s）。看「配额」列是否 > 0" % (label or "-")
                )
            else:
                self.var_status.set(
                    "OAuth 完成，但配额同步失败 %d 个；可手动点「同步配额」重试" % failed
                )

        self._run_async("OAuth 后同步配额", worker, done)

    @staticmethod
    def _format_errors(progress):
        errors = progress.get("errors") or []
        if not errors:
            return ""
        lines = ["", "", "失败明细（最多 10 条）："]
        for item in errors[:10]:
            lines.append("  %s：%s" % (item.get("label"), item.get("message")))
        return "\n".join(lines)


def main():
    root = tk.Tk()
    try:
        ttk.Style().theme_use("vista")
    except tk.TclError:
        pass
    AccountGui(root)
    root.mainloop()


if __name__ == "__main__":
    main()
