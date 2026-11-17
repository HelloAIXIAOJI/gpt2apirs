//! Turnstile 字节码求解（纯逻辑，无 HTTP）。
//!
//! 上游 prepare 返回 `turnstile.dx`（base64），配合同一次请求里**我们自己发出去的** `p`，
//! 可以解出一段程序：先 base64 解码，再按 `p` 逐字符 XOR，得到的 JSON 就是任务队列，
//! 每条任务写成 `[算子号, 参数...]`；解释执行产出的字符串再 base64，即 finalize 要的
//! `turnstile_token`。
//!
//! 求解失败一律返回 `None`（调用方降级成空 token），**不允许 panic**：
//! 这段程序由上游定义，结构随时可能变，不能让它的意外输入拖垮整条链路。

use std::collections::HashMap;
use std::time::Instant;

use base64::Engine as _;

/// 执行步数上限：防上游构造的病态队列把线程卡住。
const STEP_LIMIT: usize = 20_000;

/// 单次队列长度上限：真实程序最多几千条，超了按病态处理。
///
/// 没有这条，跑飞队列会先被步数上限接住，但每步都要从 `Vec` 头部移除，
/// 代价是 O(n²)，测试上表现为分钟级卡顿。
const MAX_QUEUE_LEN: usize = 8_000;

#[derive(Clone, Debug)]
enum Val {
    Null,
    Num(f64),
    Str(String),
    List(Vec<Val>),
    Obj(Obj),
    Func(u8),
    /// 指向环境本身（12 号算子会把整个环境塞进某个槽位）。
    EnvRef,
    /// 30 号算子造出来的子过程：捕获键 + 一段队列。
    Sub(Box<Sub>),
}

#[derive(Clone, Debug, Default)]
struct Obj {
    keys: Vec<String>,
    values: HashMap<String, Val>,
}

#[derive(Clone, Debug)]
struct Sub {
    capture_keys: Vec<u64>,
    queue: Vec<Val>,
}

/// 槽位：上游用浮点数当标识符，按位模式做键，避免 0.89/40.32 被截成整数。
type Env = HashMap<u64, Val>;

fn slot(number: f64) -> u64 {
    if number == 0.0 { 0 } else { number.to_bits() }
}

/// 诊断用：解开 `dx` 后明文的前若干字符。
///
/// 排错时必须能分清「key 不对」和「字节码结构不对」——只看 `None` 什么也判断不了。
#[doc(hidden)]
pub fn debug_decoded_prefix(dx: &str, p: &str, take: usize) -> Option<String> {
    let bytes = decode_base64(dx)?;
    let text = String::from_utf8(bytes).ok()?;
    let plain = xor_with_key(&text, p);
    Some(plain.chars().take(take).collect())
}

/// 诊断用：返回执行过的算子序列与结果。
///
/// 排错时要能看清「跑到哪一步停的」，只看 `None` 无从下手。
#[doc(hidden)]
pub fn debug_trace(dx: &str, p: &str) -> (Vec<(u8, String)>, Option<String>, usize, Vec<String>) {
    let Some((mut vm, program)) = prepare_vm(dx, p) else {
        return (Vec::new(), None, 0, Vec::new());
    };
    vm.trace = Some(Vec::new());
    vm.env.insert(slot(9.0), program);
    let _ = vm.run_loop();
    let trace = vm.trace.take().unwrap_or_default();
    let result = if vm.result.is_empty() { None } else { Some(vm.result.clone()) };
    (trace, result, vm.skipped, vm.unknown)
}

/// 求解 Turnstile token；输入非法或执行出错时返回 `None`。
pub fn solve_challenge(dx: &str, p: &str) -> Option<String> {
    let (mut vm, program) = prepare_vm(dx, p)?;
    vm.env.insert(slot(9.0), program);
    vm.run_loop()?;
    if vm.result.is_empty() {
        None
    } else {
        Some(vm.result)
    }
}

/// 解码 `dx`：上游用的是标准 base64，但留好 URL-safe 的余量。
fn decode_base64(raw: &str) -> Option<Vec<u8>> {
    let text = raw.trim();
    let standard = base64::engine::general_purpose::STANDARD.decode(text);
    let url_safe = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(text);
    let url_safe_pad = base64::engine::general_purpose::URL_SAFE.decode(text);
    standard.or(url_safe).or(url_safe_pad).ok()
}

fn prepare_vm(dx: &str, p: &str) -> Option<(Vm, Val)> {
    let bytes = decode_base64(dx)?;
    let text = String::from_utf8(bytes).ok()?;
    let plain = xor_with_key(&text, p);
    let program: serde_json::Value = serde_json::from_str(&plain).ok()?;
    let items = program.as_array()?;
    if items.is_empty() {
        return None;
    }
    Some((Vm::new(p), to_val(&program)?))
}

fn to_val(value: &serde_json::Value) -> Option<Val> {
    Some(match value {
        serde_json::Value::Null => Val::Null,
        serde_json::Value::Bool(flag) => Val::Num(if *flag { 1.0 } else { 0.0 }),
        serde_json::Value::Number(number) => Val::Num(number.as_f64()? as f64),
        serde_json::Value::String(text) => Val::Str(text.clone()),
        serde_json::Value::Array(items) => {
            Val::List(items.iter().filter_map(to_val).collect())
        }
        serde_json::Value::Object(map) => {
            let mut obj = Obj::default();
            for (key, item) in map {
                obj.keys.push(key.clone());
                obj.values.insert(key.clone(), to_val(item)?);
            }
            Val::Obj(obj)
        }
    })
}

fn xor_with_key(text: &str, key: &str) -> String {
    if key.is_empty() {
        return text.to_string();
    }
    let key: Vec<char> = key.chars().collect();
    text.chars()
        .enumerate()
        .map(|(index, ch)| {
            let k = key[index % key.len()];
            char::from_u32((ch as u32) ^ (k as u32)).unwrap_or(ch)
        })
        .collect()
}

/// 把值转成 JS 里那个值的字符串形态（含一批浏览器全局的固定写法）。
fn text_of(value: &Val) -> String {
    match value {
        Val::Null => "undefined".to_string(),
        Val::Num(number) => format_num(*number),
        Val::Str(text) => match text.as_str() {
            "window.Math" => "[object Math]".to_string(),
            "window.Reflect" => "[object Reflect]".to_string(),
            "window.performance" => "[object Performance]".to_string(),
            "window.localStorage" => "[object Storage]".to_string(),
            "window.Object" => "function Object() { [native code] }".to_string(),
            "window.Reflect.set" => "function set() { [native code] }".to_string(),
            "window.performance.now" => "function () { [native code] }".to_string(),
            "window.Object.create" => "function create() { [native code] }".to_string(),
            "window.Object.keys" => "function keys() { [native code] }".to_string(),
            "window.Math.random" => "function random() { [native code] }".to_string(),
            other => other.to_string(),
        },
        Val::List(items) if items.iter().all(|item| matches!(item, Val::Str(_))) => items
            .iter()
            .map(text_of)
            .collect::<Vec<_>>()
            .join(","),
        _ => String::new(),
    }
}

fn format_num(number: f64) -> String {
    if number.fract() == 0.0 && number.abs() < 1e15 {
        format!("{}", number as i64)
    } else {
        format!("{number}")
    }
}

struct Vm {
    env: Env,
    result: String,
    started: Instant,
    rng: u64,
    steps: usize,
    trace: Option<Vec<(u8, String)>>,
    skipped: usize,
    unknown: Vec<String>,
}

impl Vm {
    fn new(p: &str) -> Self {
        let mut env: Env = HashMap::new();
        for fid in 1u8..=34 {
            env.insert(slot(fid as f64), Val::Func(fid));
        }
        env.insert(slot(10.0), Val::Str("window".to_string()));
        env.insert(slot(16.0), Val::Str(p.to_string()));
        Self {
            env,
            result: String::new(),
            started: Instant::now(),
            rng: 0x9E37_79B9_7F4A_7C15,
            steps: 0,
            trace: None,
            skipped: 0,
            unknown: Vec::new(),
        }
    }

    fn random(&mut self) -> f64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng >> 11) as f64 / (1u64 << 53) as f64
    }

    fn get(&self, key: &Val) -> Val {
        match key {
            Val::Num(number) => self.env.get(&slot(*number)).cloned().unwrap_or(Val::Null),
            _ => Val::Null,
        }
    }

    /// 槽位是否存在（区别于「存在但为 Null」）。
    ///
    /// 取不存在的槽位时应当整条任务跳过；我们如果当成 Null 继续算，
    /// 会把「没算完的状态」写回环境，后续步骤就全歪了。
    fn present(&self, key: &Val) -> bool {
        match key {
            Val::Num(number) => self.env.contains_key(&slot(*number)),
            _ => false,
        }
    }

    fn set(&mut self, key: &Val, value: Val) {
        if let Val::Num(number) = key {
            self.env.insert(slot(*number), value);
        }
    }

    /// 主循环：**每步都重新读队列槽**（9 号槽）。
    ///
    /// 这一段是整个求解器的关键：程序会在执行过程中把解出来的新程序写回 9 号槽，
    /// 把队列拷成局部变量就会把后续程序丢掉（曾经就是这么卡住的）。
    fn run_loop(&mut self) -> Option<()> {
        loop {
            let Some(task) = self.take_next_task() else { return Some(()) };
            self.steps += 1;
            if self.steps > STEP_LIMIT {
                return None;
            }
            let Val::List(parts) = task else { continue };
            let Some(head) = parts.first().cloned() else { continue };
            // 任务头可能直接是算子号，也可能是被赋成算子的槽位：必须经环境解析
            let callee = self.get(&head);
            let raw = &parts[1..];
            if matches!(callee, Val::Null) {
                self.skipped += 1;
                continue;
            }
            match callee {
                // 算子收到的参数是「槽位号」，由算子自己再取值
                Val::Func(_) => {
                    self.call(&callee, raw);
                }
                // 子过程收到的是「值」
                _ => {
                    let values: Vec<Val> = raw.iter().map(|item| self.get(item)).collect();
                    self.call(&callee, &values);
                }
            }
        }
    }

    fn take_next_task(&mut self) -> Option<Val> {
        let key = slot(9.0);
        let Some(Val::List(items)) = self.env.get(&key).cloned() else { return None };
        if items.is_empty() || items.len() > MAX_QUEUE_LEN {
            return None;
        }
        let mut items = items;
        let task = items.remove(0);
        self.env.insert(key, Val::List(items));
        Some(task)
    }

    /// 在临时队列上跑一段（结束后恢复原队列）。
    fn run_program(&mut self, queue: &[Val]) -> Option<()> {
        let key = slot(9.0);
        let saved = self.env.insert(key, Val::List(queue.to_vec()));
        let outcome = self.run_loop();
        match saved {
            Some(value) => {
                self.env.insert(key, value);
            }
            None => {
                self.env.remove(&key);
            }
        }
        outcome
    }

    /// 该算子会去索引哪些实参位置（取不到就整条跳过）。
    fn indexed_args(fid: u8) -> &'static [usize] {
        match fid {
            1 | 5 => &[0, 1],
            18 | 19 | 23 => &[0],
            6 | 24 | 29 | 33 => &[1, 2],
            8 | 13 | 14 | 15 | 34 => &[1],
            20 | 21 => &[0, 1, 2, 3],
            27 => &[0, 1],
            _ => &[],
        }
    }

    fn apply(&mut self, fid: u8, args: &[Val]) {
        if self.trace.is_some() {
            let first = args.first().map(|item| self.get(item)).map(|value| format!("{value:?}")).unwrap_or_default();
            let second = args.get(1).map(|item| self.get(item)).map(|value| format!("{value:?}")).unwrap_or_default();
            let note = format!("{} | {}", truncate(&first, 40), truncate(&second, 40));
            if let Some(trace) = self.trace.as_mut() {
                trace.push((fid, note));
            }
        }
        let arg = |index: usize| args.get(index).cloned().unwrap_or(Val::Null);
        for index in Self::indexed_args(fid).iter().copied() {
            if !self.present(&arg(index)) {
                return;
            }
        }
        // 7/17 会索引「目标 + 全部实参」
        if matches!(fid, 7 | 17) {
            let start = if fid == 7 { 0 } else { 1 };
            if args[start..].iter().any(|item| !self.present(item)) {
                return;
            }
        }
        match fid {
            // 1 号：两个值按字符异或
            1 => {
                let (target, source) = (arg(0), arg(1));
                let left = text_of(&self.get(&target));
                let right = text_of(&self.get(&source));
                self.set(&target, Val::Str(xor_with_key(&left, &right)));
            }
            // 2: 直接赋「参数本身」（不是槽位内容），这点与 34 不同
            2 => {
                self.set(&arg(0), arg(1));
            }
            // 3 号：产出结果（把传入值 base64）
            //
            // 注意：这里编码的是**传进来的值本身**（对称于 func_7 传值），
            // 不是槽位号。曾经当成槽位查表，结果永远取不到值、解不出 token。
            3 => {
                let value = text_of(&arg(0));
                if !value.is_empty() && value != "undefined" {
                    self.result =
                        base64::engine::general_purpose::STANDARD.encode(value.as_bytes());
                }
            }
            // 5: 拼接 / 追加
            5 => {
                let (target, source) = (arg(0), arg(1));
                let current = self.get(&target);
                let incoming = self.get(&source);
                let merged = match (&current, &incoming) {
                    (Val::List(items), _) => {
                        let mut items = items.clone();
                        items.push(incoming.clone());
                        Val::List(items)
                    }
                    (Val::Str(_), _) | (Val::Num(_), _) => {
                        Val::Str(format!("{}{}", text_of(&current), text_of(&incoming)))
                    }
                    _ => Val::Str("NaN".to_string()),
                };
                self.set(&target, merged);
            }
            // 6 / 24: 取属性
            6 | 24 => {
                let value = self.member_of(&arg(1), &arg(2));
                self.set(&arg(0), value);
            }
            // 7: 调用（丢弃返回值）
            7 => {
                let target = self.get(&arg(0));
                let call_args: Vec<Val> = args[1..].iter().map(|item| self.get(item)).collect();
                self.call(&target, &call_args);
            }
            // 8: 复制
            8 => {
                let value = self.get(&arg(1));
                self.set(&arg(0), value);
            }
            // 11: 置空
            11 => self.set(&arg(0), Val::Null),
            // 12: 把整个进程表塞进槽位
            12 => self.set(&arg(0), Val::EnvRef),
            // 13: 调用（参数保持槽位语义）
            13 => {
                let target = self.get(&arg(1));
                let call_args: Vec<Val> = args[2..].to_vec();
                let value = self.call(&target, &call_args);
                self.set(&arg(0), value);
            }
            // 14: JSON 解析
            14 => {
                let source = text_of(&self.get(&arg(1)));
                let parsed = serde_json::from_str::<serde_json::Value>(&source)
                    .ok()
                    .and_then(|value| to_val(&value))
                    .unwrap_or(Val::Null);
                self.set(&arg(0), parsed);
            }
            // 15: JSON 序列化
            15 => {
                let value = self.get(&arg(1));
                self.set(&arg(0), Val::Str(val_to_json(&value)));
            }
            // 17: 调用并取返回值
            17 => {
                let target = self.get(&arg(1));
                let call_args: Vec<Val> = args[2..].iter().map(|item| self.get(item)).collect();
                let value = self.call(&target, &call_args);
                self.set(&arg(0), value);
            }
            // 18 / 19: base64 解码 / 编码
            18 => {
                let source = text_of(&self.get(&arg(0)));
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(source.trim())
                    .ok()
                    .and_then(|bytes| String::from_utf8(bytes).ok())
                    .unwrap_or_default();
                self.set(&arg(0), Val::Str(decoded));
            }
            19 => {
                let source = text_of(&self.get(&arg(0)));
                let encoded = base64::engine::general_purpose::STANDARD.encode(source.as_bytes());
                self.set(&arg(0), Val::Str(encoded));
            }
            // 20: 相等则调用
            20 => {
                if js_eq(&self.get(&arg(0)), &self.get(&arg(1))) {
                    let target = self.get(&arg(2));
                    let call_args: Vec<Val> = args[3..].to_vec();
                    self.call(&target, &call_args);
                }
            }
            // 21: 差值超过阈值则调用
            21 => {
                let left = as_num(&self.get(&arg(0)));
                let right = as_num(&self.get(&arg(1)));
                let limit = as_num(&self.get(&arg(2))).abs();
                if (left - right).abs() > limit {
                    let target = self.get(&arg(3));
                    let call_args: Vec<Val> = args[4..].to_vec();
                    self.call(&target, &call_args);
                }
            }
            // 22: 单独跑一段队列
            22 => {
                if let Val::List(items) = self.get(&arg(1)) {
                    self.run_program(&items);
                }
                self.set(&arg(0), Val::Str("None".to_string()));
            }
            // 23: 非空才调用
            23 => {
                if !matches!(self.get(&arg(0)), Val::Null) {
                    let target = self.get(&arg(1));
                    let call_args: Vec<Val> = args[2..].to_vec();
                    self.call(&target, &call_args);
                }
            }
            // 27: 从列表里删一项，或数值相减
            27 => {
                let current = self.get(&arg(0));
                let incoming = self.get(&arg(1));
                let next = match current {
                    Val::List(items) => {
                        let mut items = items.clone();
                        if let Some(index) = items.iter().position(|item| js_eq(item, &incoming)) {
                            items.remove(index);
                        }
                        Val::List(items)
                    }
                    other => Val::Num(as_num(&other) - as_num(&incoming)),
                };
                self.set(&arg(0), next);
            }
            // 29: 小于
            29 => {
                let flag = as_num(&self.get(&arg(1))) < as_num(&self.get(&arg(2)));
                self.set(&arg(0), Val::Num(if flag { 1.0 } else { 0.0 }));
            }
            // 30: 造子过程
            30 => {
                let (capture_keys, queue) = if let Val::List(keys) = self.get(&arg(3)) {
                    let keys = keys.iter().map(|item| slot(as_num(item))).collect();
                    (keys, self.get(&arg(2)))
                } else {
                    (Vec::new(), self.get(&arg(2)))
                };
                let queue = match queue {
                    Val::List(items) => items,
                    _ => Vec::new(),
                };
                self.set(&arg(0), Val::Sub(Box::new(Sub { capture_keys, queue })));
            }
            // 33: 乘法
            33 => {
                let product = as_num(&self.get(&arg(1))) * as_num(&self.get(&arg(2)));
                self.set(&arg(0), Val::Num(product));
            }
            // 34: 别名赋值
            34 => {
                let value = self.get(&arg(1));
                self.set(&arg(0), value);
            }
            // 25 / 26 / 28 及其它：无操作
            _ => {}
        }
    }

    fn member_of(&self, obj: &Val, key: &Val) -> Val {
        match obj {
            Val::Obj(map) => map.values.get(&text_of(key)).cloned().unwrap_or(Val::Null),
            Val::EnvRef => self.env.get(&slot(as_num(key))).cloned().unwrap_or(Val::Null),
            Val::List(items) => items
                .get(as_num(key) as usize)
                .cloned()
                .unwrap_or(Val::Null),
            Val::Str(text) => {
                let name = text_of(key);
                if name == "location" && text == "window.document" {
                    return Val::Str("https://chatgpt.com/".to_string());
                }
                if name.is_empty() || name == "undefined" {
                    Val::Null
                } else {
                    Val::Str(format!("{text}.{name}"))
                }
            }
            _ => Val::Null,
        }
    }

    fn call(&mut self, target: &Val, args: &[Val]) -> Val {
        match target {
            Val::Func(fid) => {
                self.apply(*fid, args);
                Val::Null
            }
            Val::Sub(sub) => {
                let mut saved: Vec<(u64, Val)> = Vec::new();
                for (index, key) in sub.capture_keys.iter().enumerate() {
                    if let Some(value) = args.get(index) {
                        saved.push((*key, self.env.insert(*key, value.clone()).unwrap_or(Val::Null)));
                    }
                }
                self.run_program(&sub.queue);
                for (key, value) in saved {
                    self.env.insert(key, value);
                }
                Val::Null
            }
            Val::Str(name) => self.call_known(name, args),
            _ => Val::Null,
        }
    }

    fn call_known(&mut self, name: &str, args: &[Val]) -> Val {
        match name {
            "window.performance.now" => {
                let elapsed = self.started.elapsed().as_secs_f64() * 1000.0;
                Val::Num(elapsed + self.random())
            }
            "window.Object.create" => Val::Obj(Obj::default()),
            "window.Math.random" => Val::Num(self.random()),
            "window.Object.keys" => match args.first() {
                Some(Val::Str(text)) if text == "window.localStorage" => Val::List(
                    [
                        "STATSIG_LOCAL_STORAGE_INTERNAL_STORE_V4",
                        "STATSIG_LOCAL_STORAGE_STABLE_ID",
                        "client-correlated-secret",
                        "oai/apps/capExpiresAt",
                        "oai-did",
                        "STATSIG_LOCAL_STORAGE_LOGGING_REQUEST",
                        "UiState.isNavigationCollapsed.1",
                    ]
                    .iter()
                    .map(|item| Val::Str((*item).to_string()))
                    .collect(),
                ),
                Some(Val::Obj(map)) => {
                    Val::List(map.keys.iter().map(|item| Val::Str(item.clone())).collect())
                }
                _ => Val::List(Vec::new()),
            },
            "window.Reflect.set" => {
                if args.len() >= 3 {
                    let key = text_of(&args[1]);
                    if let Val::Obj(map) = &args[0] {
                        let mut map = map.clone();
                        if !map.values.contains_key(&key) {
                            map.keys.push(key.clone());
                        }
                        map.values.insert(key, args[2].clone());
                        return Val::Num(1.0);
                    }
                }
                Val::Num(0.0)
            }
            other => {
                if self.trace.is_some() {
                    self.unknown.push(other.to_string());
                }
                Val::Null
            }
        }
    }
}

/// 把一次真实挑战（`dx` 与本次用的 `p`）落到文件，供离线复现。
///
/// 只在显式开启时调用（见 generate 里的环境变量开关）：这对数据只在
/// 排错和写 fixture 时有用，平时不该在磁盘上留上游挑战。
pub fn capture(dir: &std::path::Path, dx: &str, p: &str) -> std::io::Result<std::path::PathBuf> {
    std::fs::create_dir_all(dir)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0);
    let path = dir.join(format!("turnstile-{stamp}.json"));
    let payload = serde_json::json!({ "dx": dx, "p": p });
    std::fs::write(&path, serde_json::to_vec_pretty(&payload)?)?;
    Ok(path)
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(limit).collect::<String>())
    }
}

fn as_num(value: &Val) -> f64 {
    match value {
        Val::Num(number) => *number,
        Val::Str(text) => text.trim().parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

fn js_eq(left: &Val, right: &Val) -> bool {
    match (left, right) {
        (Val::Null, Val::Null) => true,
        (Val::Num(a), Val::Num(b)) => (a - b).abs() < f64::EPSILON,
        (Val::Str(a), Val::Str(b)) => a == b,
        (Val::List(a), Val::List(b)) => {
            a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| js_eq(x, y))
        }
        _ => false,
    }
}

fn val_to_json(value: &Val) -> String {
    match value {
        Val::Null => "null".to_string(),
        Val::Num(number) => format_num(*number),
        Val::Str(text) => serde_json::Value::String(text.clone()).to_string(),
        Val::List(items) => format!(
            "[{}]",
            items.iter().map(val_to_json).collect::<Vec<_>>().join(",")
        ),
        Val::Obj(map) => {
            let pairs: Vec<String> = map
                .keys
                .iter()
                .map(|key| {
                    let text = serde_json::Value::String(key.clone()).to_string();
                    let value = map.values.get(key).map(val_to_json).unwrap_or_else(|| "null".to_string());
                    format!("{text}:{value}")
                })
                .collect();
            format!("{{{}}}", pairs.join(","))
        }
        _ => "null".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrap(program: &str, key: &str) -> String {
        let coded = xor_with_key(program, key);
        base64::engine::general_purpose::STANDARD.encode(coded.as_bytes())
    }

    #[test]
    fn capture_writes_dx_and_p() {
        let dir = std::env::temp_dir().join(format!("gpt2apirs-t5-{}", std::process::id()));
        let path = capture(&dir, "DX-BLOB", "P-VALUE").expect("应能落盘");
        let text = std::fs::read_to_string(&path).expect("应能读回");
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("应是 JSON");
        assert_eq!(parsed["dx"], "DX-BLOB");
        assert_eq!(parsed["p"], "P-VALUE");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn garbage_inputs_return_none() {
        assert!(solve_challenge("", "").is_none());
        assert!(solve_challenge("不是 base64", "p").is_none());
        let not_json = base64::engine::general_purpose::STANDARD.encode(b"hello");
        assert!(solve_challenge(&not_json, "").is_none());
        let too_short = base64::engine::general_purpose::STANDARD.encode(b"[]");
        assert!(solve_challenge(&too_short, "").is_none());
    }

    /// 解释器跑通：产出算子编码「传入值本身」。
    #[test]
    fn synthetic_program_produces_expected_token() {
        let program = r#"[[3, 10]]"#;
        let dx = wrap(program, "key");
        let token = solve_challenge(&dx, "key").expect("应算得出 token");
        let expected = base64::engine::general_purpose::STANDARD.encode(b"10");
        assert_eq!(token, expected);
    }

    /// 没有产出算子时应返回 None（而不是随便给个 token）。
    #[test]
    fn program_without_result_op_yields_none() {
        let program = r#"[[19, 10]]"#;
        let dx = wrap(program, "key");
        assert!(solve_challenge(&dx, "key").is_none());
    }

    #[test]
    fn wrong_key_breaks_the_program() {
        let program = r#"[[19, 10], [3, 10]]"#;
        let dx = wrap(program, "right-key");
        assert!(
            solve_challenge(&dx, "wrong-key").is_none(),
            "key 不对时 JSON 解不开，必须返回 None 而不是乱算"
        );
    }

    /// 病态队列要被步数上限拦住，不能把线程卡死。
    #[test]
    fn runaway_queue_is_stopped() {
        let tasks = vec!["[2]"; STEP_LIMIT + 10].join(",");
        let program = format!("[{tasks}]");
        let dx = wrap(&program, "");
        assert!(solve_challenge(&dx, "").is_none(), "超步数上限应放弃");
    }

    /// 真实抓包的 dx（31KB）配错误 key：不能 panic，也不能声称算出了 token。
    #[test]
    fn real_challenge_with_wrong_key_is_safe() {
        let dx = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/turnstile_dx.txt"
        ));
        assert!(dx.len() > 10_000, "fixture 应是真实挑战");
        assert!(solve_challenge(dx, "definitely-not-the-key").is_none());
    }
}
