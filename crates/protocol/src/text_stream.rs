//! 从上游 SSE 里还原助手文本。
//!
//! 上行把「完整消息」和「增量补丁」混在一起下发，必须只取用户可见的那条。
//! 五条过滤规则，缺一条就泄漏或重复：
//!
//! `role` 必须是 assistant（否则把用户的话当回复）；
//! `is_visually_hidden_from_conversation` 不能为 true（否则漏 system 消息）；
//! `recipient` 必须空或 `all`（否则漏发给工具的指令）；`channel` 必须空或 `final`（否则**漏思维链**），
//! 生图链路例外地再接受 `commentary`（上游把拒绝理由放在那儿，见 [`TextAccumulator::with_commentary`]）；
//! 正文必须不属于**回放的历史**（否则上一轮的回复会被当成这一轮的开头重发出去）。

use serde_json::Value;

use crate::sse::iter_sse_payloads;

/// 内部注解的起始符（U+E200）。
const ANNOTATION_OPEN: char = '\u{E200}';
/// 内部注解的结束符（U+E201）。
const ANNOTATION_CLOSE: char = '\u{E201}';
/// 注解内部的分段符（U+E202）。
const ANNOTATION_SEP: char = '\u{E202}';

/// 一条增量：正文或思考内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delta {
    /// 正文（`delta.content`）。
    Text(String),
    /// 思考内容（`delta.reasoning_content`）。
    Reasoning(String),
}

/// 一条助手消息应该落到哪条通道。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MessageChannel {
    /// 正文（用户看到的那句话）。
    #[default]
    Answer,
    /// 思考内容：上游的 `analysis`，以及不当作正文的 `commentary`。
    Reasoning,
}

/// 判断某个 assistant 消息是否**面向用户**（只看 `final` 频道）。
pub fn is_visible_assistant_message(message: &Value) -> bool {
    classify_message_channel(message, false) == Some(MessageChannel::Answer)
}

/// 分类一条助手消息该进「正文」还是「思考内容」，不值得收的返回 `None`。
///
/// 规则：
/// - 角色必须是 assistant，`recipient` 必须空或 `all`；
/// - 隐藏消息只有当频道是 `final` 时才收（那些是真的答案，只是界面不展示）；
/// - `final` / 空频道 → 正文；
/// - `commentary` → 生图链路当正文（拒绝理由），文本链路当思考内容；
/// - `analysis` → 思考内容；
/// - 其它频道（工具、未知）→ 不收。
pub fn classify_message_channel(
    message: &Value,
    allow_commentary: bool,
) -> Option<MessageChannel> {
    let Some(author) = message.get("author") else {
        return None;
    };
    let role = author
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if role != "assistant" {
        return None;
    }
    let recipient = message
        .get("recipient")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if !recipient.is_empty() && recipient != "all" {
        return None;
    }
    let channel = message
        .get("channel")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let hidden = message
        .get("metadata")
        .and_then(|meta| meta.get("is_visually_hidden_from_conversation"))
        .and_then(Value::as_bool)
        == Some(true);
    match channel.as_str() {
        // 空频道：隐藏消息不能当正文（那多半是内部指令）
        "" if hidden => None,
        "" | "final" => Some(MessageChannel::Answer),
        "commentary" if allow_commentary => Some(MessageChannel::Answer),
        // 思考内容本来就常常是隐藏的（界面不展示），照收不误
        "commentary" | "analysis" => Some(MessageChannel::Reasoning),
        _ => None,
    }
}

/// 取一条消息的正文。
///
/// 优先 `content.parts`（片段拼接）；`content_type: "code"` 时正文在 `content.text`。
pub fn assistant_message_text(message: &Value) -> String {
    if let Some(parts) = message
        .get("content")
        .and_then(|content| content.get("parts"))
        .and_then(Value::as_array)
    {
        let joined: String = parts
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("");
        if !joined.is_empty() {
            return joined;
        }
    }
    message
        .get("content")
        .and_then(|content| content.get("text"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// 上游的历史回放。
///
/// 上游在开始本轮生成前，会把历史里的助手正文重发一遍，但**不代表每条都发**：
/// 实测同一串历史里两条助手消息，只回了后一条。所以这里按「集合」匹配，
/// 不按顺序 —— 任何一条还没吃掉的历史正文出现，都算回放。
///
/// 这些帧是**上一轮的话**，不是本轮输出，必须整条吃掉，否则流式客户端会
/// 看到上一轮的回复被原样重复（非流式看不见：最终值被后续帧覆盖）。
#[derive(Debug, Clone, Default)]
pub struct HistoryEcho {
    /// 还没吃掉的历史正文。
    pending: Vec<String>,
    /// 已经吃掉的。累积形态下它们还留在文本前端，要能重新越过。
    consumed: Vec<String>,
}

impl HistoryEcho {
    /// 用历史助手正文构造（空正文不参与匹配）。
    pub fn new(texts: impl IntoIterator<Item = String>) -> Self {
        Self {
            pending: texts
                .into_iter()
                .filter(|text| !text.trim().is_empty())
                .collect(),
            consumed: Vec::new(),
        }
    }

    /// 这段正文是不是还没吃掉的历史。
    fn knows(&self, text: &str) -> bool {
        self.pending.iter().any(|known| known == text)
    }

    /// 精确吃掉某条历史（结构判定已经确认它是回放时记账用）。
    fn consume_exact(&mut self, text: &str) {
        if let Some(index) = self.pending.iter().position(|known| known == text) {
            let known = self.pending.remove(index);
            self.consumed.push(known);
        }
    }

    /// 吃掉候选前端的**最长**一条待吃历史；吃完返回剩余部分。
    fn eat_longest(&mut self, candidate: &str) -> Option<String> {
        let mut best: Option<(usize, usize)> = None;
        for (index, known) in self.pending.iter().enumerate() {
            if !candidate.starts_with(known.as_str()) {
                continue;
            }
            let better = match best {
                Some((_, len)) => known.len() > len,
                None => true,
            };
            if better {
                best = Some((index, known.len()));
            }
        }
        let (index, len) = best?;
        let known = self.pending.remove(index);
        self.consumed.push(known);
        Some(candidate[len..].to_string())
    }

    /// 候选是**整段新消息**（完整消息对象，或带 replace 的补丁）时使用。
    ///
    /// 吃过的就不再吃：吃完之后同样的文本就是模型真的又回答了一遍。
    fn strip_fresh(&mut self, candidate: &str) -> String {
        let mut rest = candidate.to_string();
        while let Some(tail) = self.eat_longest(&rest) {
            rest = tail;
        }
        rest
    }

    /// 只越过**已经吃掉**的历史：累积文本里它们还留在前端。
    fn strip_consumed_only(&mut self, candidate: &str) -> String {
        let mut rest = candidate.to_string();
        loop {
            let before = rest.clone();
            for known in &self.consumed {
                if let Some(tail) = rest.strip_prefix(known.as_str()) {
                    rest = tail.to_string();
                    break;
                }
            }
            if rest == before {
                break;
            }
        }
        rest
    }

    /// 候选是**累积增长**的（append 补丁）时使用。
    ///
    /// 累积文本里已经吃掉的历史仍留在前端，要反复越过再往下吃。
    fn strip_growing(&mut self, candidate: &str) -> String {
        let mut rest = self.strip_consumed_only(candidate);
        while let Some(tail) = self.eat_longest(&rest) {
            rest = tail;
        }
        // 半截回放（被切成几帧）：先压住不吐，等下一帧补全再判。
        // 累积文本只增不减，压住不会丢内容。
        if !rest.is_empty() && self.pending.iter().any(|known| known.starts_with(&rest)) {
            return String::new();
        }
        rest
    }
}


/// 从多轮消息里取出会被上游回放的助手正文（按顺序）。
///
/// 只取 assistant：user / system 的正文本来就会被角色过滤挡掉。
pub fn assistant_history(messages: &[(String, String)]) -> Vec<String> {
    messages
        .iter()
        .filter(|(role, _)| role.trim().eq_ignore_ascii_case("assistant"))
        .map(|(_, text)| text.clone())
        .filter(|text| !text.trim().is_empty())
        .collect()
}

fn is_internal_annotation_part(part: &str) -> bool {
    let value = part.trim();
    if value.is_empty() {
        return true;
    }
    let lower = value.to_ascii_lowercase();
    lower.starts_with("turn") || lower.starts_with("source") || lower.starts_with("sources")
}

fn readable_annotation_part(parts: &[&str]) -> String {
    for part in parts {
        let value = part.trim();
        if !value.is_empty() && !is_internal_annotation_part(value) {
            return value.to_string();
        }
    }
    String::new()
}

fn annotation_text(payload: &str) -> String {
    let parts: Vec<&str> = payload.split(ANNOTATION_SEP).collect();
    let kind = parts.first().map(|p| p.trim().to_ascii_lowercase()).unwrap_or_default();
    let data: Vec<&str> = if parts.len() > 1 { parts[1..].to_vec() } else { Vec::new() };
    if kind == "url" {
        let label = data.first().map(|p| p.trim()).unwrap_or("");
        let url = data.get(1).map(|p| p.trim()).unwrap_or("");
        if !label.is_empty() && (url.starts_with("http://") || url.starts_with("https://")) {
            return format!("{label} ({url})");
        }
        return if !label.is_empty() { label.to_string() } else { url.to_string() };
    }
    // cite 与其它类型都只取第一个可读片段
    readable_annotation_part(&data)
}

/// 清掉内部注解标记（U+E200…U+E201）。
///
/// 上游 web 端会用私用区字符下发引用标记，API 客户端渲染不了。
/// 这里保留可读标签（如链接文字），丢掉内部引用编号。
pub fn sanitize_output_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(ANNOTATION_OPEN) {
        out.push_str(&rest[..start]);
        let after_open = &rest[start + ANNOTATION_OPEN.len_utf8()..];
        let Some(end) = after_open.find(ANNOTATION_CLOSE) else {
            // 未闭合：一直删到结尾
            return out;
        };
        let payload = &after_open[..end];
        let mut tail = &after_open[end + ANNOTATION_CLOSE.len_utf8()..];
        let replacement = annotation_text(payload);
        // 紧跟标点时，前面的空白只在有替换内容时才保留
        let before_punctuation = tail
            .chars()
            .next()
            .is_some_and(|c| matches!(c, '.' | ',' | ';' | ':' | '!' | '?'));
        if before_punctuation {
            let trimmed_len = out.trim_end().len();
            out.truncate(trimmed_len);
            if !replacement.is_empty() {
                out.push_str(&replacement);
                // 保留原本的空白（空白与替换内容一起处理）
            }
        } else {
            out.push_str(&replacement);
        }
        rest = tail;
        let _ = &mut tail;
    }
    out.push_str(rest);
    out
}

/// 应用一个补丁操作。返回（新文本, 是否整段覆盖）。
fn apply_patch_op(operation: &Value, current: &str) -> (String, bool) {
    let op = operation.get("o").and_then(Value::as_str).unwrap_or("");
    let value = operation
        .get("v")
        .map(|value| match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default();
    match op {
        "append" => (format!("{current}{value}"), false),
        // replace 之后候选不再累积：历史回放要按「整段」判定，不能重复吃
        "replace" => (value, true),
        _ => (current.to_string(), false),
    }
}

/// 递归应用文本补丁。返回（新文本, 是否整段覆盖）。
pub fn apply_text_patch(event: &Value, current: &str) -> (String, bool) {
    if event.get("p").and_then(Value::as_str) == Some("/message/content/parts/0") {
        return apply_patch_op(event, current);
    }
    let operations = event.get("v");
    // 裸字符串增量：{"v": "一段文本"} 且没有 p / o
    if let Some(Value::String(text)) = operations {
        let has_path = event.get("p").is_some();
        let has_op = event.get("o").is_some();
        if !current.is_empty() && !has_path && !has_op {
            return (format!("{current}{text}"), false);
        }
        return (current.to_string(), false);
    }
    let Some(Value::Array(items)) = operations else {
        return (current.to_string(), false);
    };
    let mut text = current.to_string();
    let mut replaced = false;
    for item in items {
        if item.is_object() {
            let (next, did_replace) = apply_text_patch(item, &text);
            text = next;
            replaced |= did_replace;
        }
    }
    (text, replaced)
}

/// 从一个事件里取「当前完整文本」。
///
/// 返回（候选文本, 是否整段覆盖）。先看有没有完整消息（直接取，整段覆盖），
/// 没有再按补丁累积 —— 补丁里是否 replace 由 apply_text_patch 一并报上来。
pub fn assistant_raw_text(event: &Value, current: &str) -> (String, bool) {
    assistant_raw_text_with(event, current, false)
}

/// 同 [`assistant_raw_text`]，可放宽到接受 `commentary` 频道。
fn assistant_raw_text_with(event: &Value, current: &str, allow_commentary: bool) -> (String, bool) {
    for candidate in [Some(event), event.get("v")].into_iter().flatten() {
        if !candidate.is_object() {
            continue;
        }
        if let Some(message) = candidate.get("message") {
            if message.is_object()
                && classify_message_channel(message, allow_commentary) == Some(MessageChannel::Answer)
            {
                let text = assistant_message_text(message);
                if !text.is_empty() {
                    return (text, true);
                }
            }
        }
    }
    apply_text_patch(event, current)
}

/// 事件里可见助手消息的 id（没有就是 None）。
fn assistant_message_id(event: &Value, allow_commentary: bool) -> Option<String> {
    for candidate in [Some(event), event.get("v")].into_iter().flatten() {
        let Some(message) = candidate.get("message") else {
            continue;
        };
        if classify_message_channel(message, allow_commentary) != Some(MessageChannel::Answer) {
            continue;
        }
        let id = message
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if !id.is_empty() {
            return Some(id.to_string());
        }
    }
    None
}

/// 一条可见助手消息的结构信息。
///
/// 判定「这帧是不是本轮的」靠它，而不是靠正文比对 ——
/// 回放帧是终态（`finished_successfully` / `end_turn`），本轮帧先非终态后终态。
struct AssistantMeta {
    /// 该落到哪条通道（正文 / 思考）。
    channel: MessageChannel,
    /// 消息 id。
    id: String,
    /// 上游给的状态串（`in_progress` / `finished_successfully` …），可能为空。
    status: String,
    /// `end_turn` 字段（只有 true 有意义）。
    end_turn: Option<bool>,
    /// 正文。
    text: String,
}

impl AssistantMeta {
    /// 有没有可用的结构信息；两者都缺就只能靠正文猜。
    fn is_structural(&self) -> bool {
        !self.status.is_empty() || self.end_turn.is_some()
    }

    /// 是否终态（回放帧、或整条流唯一的那条回复）。
    fn is_terminal(&self) -> bool {
        self.end_turn == Some(true) || self.status.starts_with("finished")
    }
}

/// 取事件里可见助手消息的结构信息。
fn assistant_meta(event: &Value, allow_commentary: bool) -> Option<AssistantMeta> {
    for candidate in [Some(event), event.get("v")].into_iter().flatten() {
        let Some(message) = candidate.get("message") else {
            continue;
        };
        if !message.is_object() {
            continue;
        }
        let Some(channel) = classify_message_channel(message, allow_commentary) else {
            continue;
        };
        return Some(AssistantMeta {
            channel,
            id: message
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string(),
            status: message
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string(),
            end_turn: message.get("end_turn").and_then(Value::as_bool),
            text: assistant_message_text(message),
        });
    }
    None
}

/// 文本累积器：喂 SSE 事件，吐出增量。
#[derive(Debug, Default, Clone)]
pub struct TextAccumulator {
    /// 未清洗的完整文本。
    pub raw: String,
    /// 已清洗的完整文本。
    pub text: String,
    /// 思考内容（`analysis` / 文本链路的 `commentary`）。
    ///
    /// 单独放，绝不并进 [`Self::text`]：客户端要正文时不该看到思维链。
    pub reasoning: String,
    /// 会话 ID（一旦出现就记住）。
    pub conversation_id: String,
    /// 是否见过 DONE。
    pub done: bool,
    /// 上游历史回放的判定器（**兜底**：帧里没有结构信息时才用）。
    echo: HistoryEcho,
    /// 上一条可见助手消息的 id。
    ///
    /// 结构判定用它锁定「本轮那条消息」；文本兜底用它区分「同一帧重复」和「新的一条消息」。
    current_message_id: Option<String>,
    /// 本轮是否已经开始（锁定了本轮消息）。
    started: bool,
    /// 是否接受 `commentary` 频道的助手消息。
    ///
    /// 只有生图链路需要：上游把「拒绝/解释」放在 commentary 上，
    /// 而文本链路要它保持不可见（否则过程话会被当成回答）。
    allow_commentary: bool,
    /// 未清洗的思考内容。
    reasoning_raw: String,
    /// 当前消息的频道（补丁帧跟着它走）。
    current_channel: MessageChannel,
    /// 最后一条被跳过的**终态**助手正文。
    ///
    /// 上游偶尔整条流只给一条终态回复（内容策略拒绝、极短回答），
    /// 那时不会出现非终态帧；整条流没正文时拿它兜底，别把回复丢成空。
    last_terminal_text: String,
}

impl TextAccumulator {
    /// 新建（没有历史可回放）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 带上「已经发给上游的历史助手正文」，用来吃掉上游回放的历史。
    pub fn with_history(texts: Vec<String>) -> Self {
        Self {
            echo: HistoryEcho::new(texts),
            ..Self::default()
        }
    }

    /// 放宽可见性：`commentary` 频道的助手消息也算数。
    ///
    /// 给生图链路用 —— 上游把拒绝理由放在 commentary 上。
    pub fn with_commentary(mut self) -> Self {
        self.allow_commentary = true;
        self
    }

    /// 喂一条 SSE payload（`data:` 后面的原文）。
    ///
    /// 返回**新增**的文本片段；没有新增返回 `None`。
    pub fn push_payload(&mut self, payload: &str) -> Option<Delta> {
        if payload.trim() == "[DONE]" {
            self.done = true;
            return None;
        }
        if self.conversation_id.is_empty() {
            if let Some(found) = find_conversation_id(payload) {
                self.conversation_id = found;
            }
        }
        let Ok(event) = serde_json::from_str::<Value>(payload) else {
            return None;
        };
        // 两条不同 id 的消息可能正文一模一样（模型把上一轮原样又说一遍），
        // 只看文本会把它们当成同一帧吃掉。
        let frame_id = assistant_message_id(&event, self.allow_commentary);
        let new_message = match (&frame_id, &self.current_message_id) {
            (Some(current), Some(previous)) => current != previous,
            _ => false,
        };

        // 有结构信息就按结构判，没有（补丁帧 / 老形态）才回头比正文。
        let (candidate, replaced, channel) = match assistant_meta(&event, self.allow_commentary) {
            Some(meta) if meta.is_structural() => {
                if !self.started {
                    if meta.is_terminal() && meta.channel == MessageChannel::Reasoning {
                        // 终态的思考帧（拒绝/解释）不是回放：历史里的助手正文走 Answer。
                        // 先收下来，别丢；也不据此认定本轮已开始。
                        return self.ingest_reasoning(meta.text);
                    }
                    if meta.is_terminal() {
                        // 本轮开始之前的终态帧 = 历史回放。
                        // 只有「整条流就这么一条终态回复」才留着兜底，不能让回放顶替答案。
                        if meta.channel == MessageChannel::Answer {
                            if self.echo.knows(&meta.text) {
                                self.echo.consume_exact(&meta.text);
                            } else if !meta.text.trim().is_empty() {
                                self.last_terminal_text = meta.text.clone();
                            }
                        }
                        return None;
                    }
                    // 第一条非终态消息 = 本轮开始，锁定它的 id
                    self.started = true;
                    if !meta.id.is_empty() {
                        self.current_message_id = Some(meta.id.clone());
                    }
                } else if let Some(current) = &self.current_message_id
                    && !meta.id.is_empty()
                    && *current != meta.id
                {
                    // 本轮已锁定另一条消息：并发分支或迟到的回放，整帧丢弃
                    return None;
                }
                self.current_channel = meta.channel;
                let text = if meta.text.is_empty() {
                    self.current_raw().to_string()
                } else {
                    meta.text
                };
                (text, true, meta.channel)
            }
            _ => {
                // 补丁帧：跟着当前消息的频道走
                let current = self.current_raw().to_string();
                let (candidate, replaced) =
                    assistant_raw_text_with(&event, &current, self.allow_commentary);
                (candidate, replaced, self.current_channel)
            }
        };

        if channel == MessageChannel::Reasoning {
            return self.ingest_reasoning(candidate);
        }

        if candidate == self.raw && !new_message {
            return None;
        }
        // 上游的历史回放不是本轮输出：整段吃掉，不公告给客户端。
        // 回放只会出现在本轮开始之前；一旦本轮有了内容，新消息帧一律不再剥离，
        // 免得把「答案恰好以历史文本开头」当成回声切掉。
        let rest = match (replaced, self.started) {
            (true, false) => self.echo.strip_fresh(&candidate),
            (true, true) => candidate.clone(),
            // 累积形态：本轮开始前连「疑似半截」一起压住；开始后只越过已吃掉的历史，
            // 免得把已经发出去的内容又压回去
            (false, false) => self.echo.strip_growing(&candidate),
            (false, true) => self.echo.strip_consumed_only(&candidate),
        };
        let next_text = sanitize_output_text(&rest);
        let delta = if next_text.starts_with(&self.text) {
            next_text[self.text.len()..].to_string()
        } else {
            next_text.clone()
        };
        self.raw = candidate;
        self.text = next_text;
        if !self.started && !self.text.is_empty() {
            self.started = true;
        }
        if frame_id.is_some() {
            self.current_message_id = frame_id;
        }
        if delta.is_empty() {
            None
        } else {
            Some(Delta::Text(delta))
        }
    }

    /// 当前频道对应的「未清洗文本」。
    fn current_raw(&self) -> &str {
        if self.current_channel == MessageChannel::Reasoning {
            &self.reasoning_raw
        } else {
            &self.raw
        }
    }

    /// 吃一段思考内容：单独累积，不碰正文。
    fn ingest_reasoning(&mut self, candidate: String) -> Option<Delta> {
        if candidate == self.reasoning_raw {
            return None;
        }
        let next = sanitize_output_text(&candidate);
        let delta = if next.starts_with(&self.reasoning) {
            next[self.reasoning.len()..].to_string()
        } else {
            next.clone()
        };
        self.reasoning_raw = candidate;
        self.reasoning = next;
        if delta.is_empty() {
            None
        } else {
            Some(Delta::Reasoning(delta))
        }
    }

    /// 收尾：整条流没见到本轮正文时，用最后一条终态回复兜底。
    ///
    /// 上游偶尔整条流只给一条终态消息（拒绝、极短回答）。结构判定会把它当回放跳过，
    /// 这时没正文，只能认它 —— 但前提是它不是历史（历史在跳过时已经记成回放了）。
    pub fn flush(&mut self) -> Option<Delta> {
        if !self.text.is_empty() || self.last_terminal_text.is_empty() {
            return None;
        }
        let text = sanitize_output_text(&self.last_terminal_text);
        if text.is_empty() {
            return None;
        }
        self.text = text.clone();
        self.raw = text.clone();
        Some(Delta::Text(text))
    }

    /// 一次性吃掉整段 SSE 文本。
    pub fn push_raw_sse(&mut self, raw: &str) -> Vec<Delta> {
        let mut deltas = Vec::new();
        for payload in iter_sse_payloads(raw) {
            if let Some(delta) = self.push_payload(&payload) {
                deltas.push(delta);
            }
        }
        deltas
    }
}

/// 从任意 payload 里抠 `conversation_id`。
fn find_conversation_id(payload: &str) -> Option<String> {
    let key = "\"conversation_id\"";
    let index = payload.find(key)?;
    let rest = &payload[index + key.len()..];
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    let value = &rest[..end];
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// 便捷函数：整段 SSE →（正文, 思考内容, conversation_id, 增量序列）。
pub fn collect_text(
    raw_sse: &str,
    history: &[String],
) -> (String, String, String, Vec<Delta>) {
    let mut accumulator = TextAccumulator::with_history(history.to_vec());
    let mut deltas = accumulator.push_raw_sse(raw_sse);
    if let Some(tail) = accumulator.flush() {
        deltas.push(tail);
    }
    (
        accumulator.text,
        accumulator.reasoning,
        accumulator.conversation_id,
        deltas,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 只拼正文增量（思考内容单独走）。
    fn join_text(deltas: &[Delta]) -> String {
        deltas
            .iter()
            .filter_map(|delta| match delta {
                Delta::Text(text) => Some(text.as_str()),
                Delta::Reasoning(_) => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    fn assistant_message(text: &str) -> Value {
        json!({
            "v": { "message": {
                "author": { "role": "assistant" },
                "content": { "content_type": "text", "parts": [text] },
                "recipient": "all",
                "channel": "final",
            }}
        })
    }

    #[test]
    fn visible_message_rules() {
        assert!(is_visible_assistant_message(&json!({
            "author": {"role": "assistant"},
            "content": {"parts": ["x"]},
        })));

        // 用户消息
        assert!(!is_visible_assistant_message(&json!({
            "author": {"role": "user"},
        })));
        // 视觉隐藏
        assert!(!is_visible_assistant_message(&json!({
            "author": {"role": "assistant"},
            "metadata": {"is_visually_hidden_from_conversation": true},
        })));
        // 发给工具的
        assert!(!is_visible_assistant_message(&json!({
            "author": {"role": "assistant"},
            "recipient": "web",
        })));
        // 非 final 通道（思维链）
        assert!(!is_visible_assistant_message(&json!({
            "author": {"role": "assistant"},
            "channel": "analysis",
        })));
        // 空 recipient/channel 是允许的
        assert!(is_visible_assistant_message(&json!({
            "author": {"role": "assistant"},
            "recipient": "",
            "channel": "",
        })));
    }

    #[test]
    fn message_text_prefers_parts_then_text_field() {
        let message = json!({"content": {"parts": ["a", "b"]}});
        assert_eq!(assistant_message_text(&message), "ab");
        let message = json!({"content": {"content_type": "code", "text": "code here"}});
        assert_eq!(assistant_message_text(&message), "code here");
        assert_eq!(assistant_message_text(&json!({"content": {}})), "");
    }

    #[test]
    fn history_replay_is_dropped() {
        // 上游在开始本轮生成前，会把发上去的助手正文按顺序重发一遍
        let mut accumulator = TextAccumulator::with_history(vec!["上一轮答复".to_string()]);
        assert_eq!(
            accumulator.push_payload(&assistant_message("上一轮答复").to_string()),
            None
        );
        assert_eq!(accumulator.text, "");
        assert_eq!(
            accumulator.push_payload(&assistant_message("本轮答复").to_string()),
            Some(Delta::Text("本轮答复".to_string()))
        );
        assert_eq!(accumulator.text, "本轮答复");
    }

    #[test]
    fn every_history_turn_is_dropped_in_order() {
        let mut accumulator =
            TextAccumulator::with_history(vec!["第一答".to_string(), "第二答".to_string()]);
        for replay in ["第一答", "第二答"] {
            assert_eq!(
                accumulator.push_payload(&assistant_message(replay).to_string()),
                None
            );
        }
        assert_eq!(accumulator.text, "");
        assert_eq!(
            accumulator.push_payload(&assistant_message("第三答").to_string()),
            Some(Delta::Text("第三答".to_string()))
        );
    }

    #[test]
    fn echo_of_only_the_last_history_turn_is_dropped() {
        // 线上真实情况：历史里两条助手消息，上游只回了后一条。
        // 按顺序匹配会在这里错位，把后一条当成正文吐出去。
        let mut accumulator =
            TextAccumulator::with_history(vec!["第一轮的回答".to_string(), "第二轮的答案".to_string()]);
        assert_eq!(
            accumulator.push_payload(&assistant_message("第二轮的答案").to_string()),
            None
        );
        assert_eq!(accumulator.text, "");
        assert_eq!(
            accumulator.push_payload(&assistant_message("这轮的新答案").to_string()),
            Some(Delta::Text("这轮的新答案".to_string()))
        );
        assert_eq!(accumulator.text, "这轮的新答案");
    }

    #[test]
    fn echo_out_of_order_is_dropped() {
        // 回放顺序也不可靠：两条都回了但顺序相反，同样不能漏
        let mut accumulator =
            TextAccumulator::with_history(vec!["先的回答".to_string(), "后的答案".to_string()]);
        for replay in ["后的答案", "先的回答"] {
            assert_eq!(
                accumulator.push_payload(&assistant_message(replay).to_string()),
                None,
                "回放 {replay} 不该漏出去"
            );
        }
        assert_eq!(accumulator.text, "");
    }

    #[test]
    fn identical_answer_after_replay_is_kept() {
        // 回放吃掉之后，同样文本的另一条消息就是模型真的又回答了一遍
        let mut accumulator = TextAccumulator::with_history(vec!["同样的回答".to_string()]);
        let message = |id: &str, text: &str| {
            json!({"v": {"message": {
                "id": id,
                "author": {"role": "assistant"},
                "content": {"content_type": "text", "parts": [text]},
                "recipient": "all",
                "channel": "final",
            }}})
            .to_string()
        };
        assert_eq!(accumulator.push_payload(&message("m-历史", "同样的回答")), None);
        assert_eq!(
            accumulator.push_payload(&message("m-本轮", "同样的回答")),
            Some(Delta::Text("同样的回答".to_string()))
        );
        assert_eq!(accumulator.text, "同样的回答");
    }

    #[test]
    fn growing_patch_replay_is_dropped() {
        // 累积形态：回放用 append 堆在当前消息上，吃完历史之后才是本轮
        let mut accumulator = TextAccumulator::with_history(vec!["上一轮".to_string()]);
        let append = |text: &str| {
            json!({"o": "patch", "v": [
                {"p": "/message/content/parts/0", "o": "append", "v": text}
            ]})
            .to_string()
        };
        assert_eq!(accumulator.push_payload(&append("上一轮")), None);
        assert_eq!(
            accumulator.push_payload(&append("本轮")),
            Some(Delta::Text("本轮".to_string()))
        );
        assert_eq!(accumulator.text, "本轮");
    }

    #[test]
    fn half_a_history_frame_is_held_back() {
        // 回放被切成两帧时，前半截不能先漏给客户端
        let mut accumulator = TextAccumulator::with_history(vec!["上一轮答复".to_string()]);
        let append = |text: &str| {
            json!({"o": "patch", "v": [
                {"p": "/message/content/parts/0", "o": "append", "v": text}
            ]})
            .to_string()
        };
        assert_eq!(accumulator.push_payload(&append("上一轮")), None);
        assert_eq!(accumulator.push_payload(&append("答复")), None);
        assert_eq!(accumulator.text, "");
        assert_eq!(
            accumulator.push_payload(&append("本轮答复")),
            Some(Delta::Text("本轮答复".to_string()))
        );
    }

    #[test]
    fn stripping_stops_once_the_turn_has_content() {
        // 回放只出现在本轮开始之前；开始之后，答案里出现历史文本也不能再切
        let mut accumulator =
            TextAccumulator::with_history(vec!["第一答".to_string(), "第二答".to_string()]);
        let message = |id: &str, text: &str| {
            json!({"v": {"message": {
                "id": id,
                "author": {"role": "assistant"},
                "content": {"content_type": "text", "parts": [text]},
                "recipient": "all",
                "channel": "final",
            }}})
            .to_string()
        };
        assert_eq!(accumulator.push_payload(&message("m-1", "第一答")), None);
        assert_eq!(
            accumulator.push_payload(&message("m-2", "本轮开头")),
            Some(Delta::Text("本轮开头".to_string()))
        );
        assert_eq!(
            accumulator.push_payload(&message("m-3", "第二答的后续")),
            Some(Delta::Text("第二答的后续".to_string()))
        );
    }

    /// 真实抓包（2026-11-18 采集）：三帧历史 + 新一轮回复，全是完整消息帧。
    ///
    /// 只删了与解析无关的帧（会话 token / 广告 / STE 元数据），结构与正文原样保留。
    fn real_multiturn_sse() -> &'static str {
        include_str!("../tests/fixtures/text_multiturn_sse.txt")
    }

    fn real_multiturn_history() -> Vec<String> {
        assistant_history(&[
            ("user".to_string(), "你好，你叫什么名字？".to_string()),
            (
                "assistant".to_string(),
                "MARKER_PREV_REPLY_ALPHA 我叫小暖，很高兴认识你。".to_string(),
            ),
            ("user".to_string(), "再说一句，只回一句话。".to_string()),
        ])
    }

    #[test]
    fn real_multiturn_stream_drops_replayed_history() {
        let (text, _reasoning, conversation_id, deltas) =
            collect_text(real_multiturn_sse(), &real_multiturn_history());
        assert_eq!(text, "我叫小暖，很高兴再次和你聊天！");
        assert!(!text.contains("MARKER_PREV_REPLY_ALPHA"), "回放不能进正文：{text}");
        assert_eq!(join_text(&deltas), text, "增量拼接必须等于正文");
        assert_eq!(conversation_id, "6afd870e-2e6c-83ee-9de2-4c112a853d44");
    }

    #[test]
    fn real_multiturn_stream_drops_replay_even_without_history() {
        // 回放帧是终态（finished_successfully），结构判定直接跳过 ——
        // 不依赖把历史传进来比对正文。
        let (text, _reasoning, _, deltas) = collect_text(real_multiturn_sse(), &[]);
        assert_eq!(text, "我叫小暖，很高兴再次和你聊天！");
        assert!(
            !join_text(&deltas).contains("MARKER_PREV_REPLY_ALPHA"),
            "流式增量里不该出现回放：{deltas:?}"
        );
    }

    /// 带结构信息的助手消息帧。
    fn assistant_message_with_status(id: &str, status: &str, text: &str) -> String {
        json!({"v": {"message": {
            "id": id,
            "author": {"role": "assistant"},
            "content": {"content_type": "text", "parts": [text]},
            "recipient": "all",
            "channel": "final",
            "status": status,
        }}})
        .to_string()
    }

    #[test]
    fn terminal_replay_is_dropped_by_structure_alone() {
        // 不传任何历史：只靠 status 就能认出回放
        let mut accumulator = TextAccumulator::new();
        assert_eq!(
            accumulator.push_payload(&assistant_message_with_status(
                "m-old",
                "finished_successfully",
                "上一轮的回答"
            )),
            None
        );
        assert_eq!(accumulator.text, "");
        assert_eq!(
            accumulator.push_payload(&assistant_message_with_status(
                "m-new",
                "in_progress",
                "这轮的回答"
            )),
            Some(Delta::Text("这轮的回答".to_string()))
        );
        assert_eq!(accumulator.text, "这轮的回答");
    }

    #[test]
    fn locked_message_id_rejects_late_or_concurrent_messages() {
        let mut accumulator = TextAccumulator::new();
        assert_eq!(
            accumulator.push_payload(&assistant_message_with_status("m-new", "in_progress", "本轮")),
            Some(Delta::Text("本轮".to_string()))
        );
        // 锁定之后再来的别的消息（并发分支、迟到回放）整帧丢掉
        assert_eq!(
            accumulator.push_payload(&assistant_message_with_status("m-other", "in_progress", "别处")),
            None
        );
        assert_eq!(accumulator.text, "本轮");
    }

    #[test]
    fn terminal_only_stream_is_used_as_the_answer() {
        // 上游整条流只给一条终态回复（拒绝、极短回答）：不能丢成空
        let mut accumulator = TextAccumulator::new();
        assert_eq!(
            accumulator.push_payload(&assistant_message_with_status(
                "m-only",
                "finished_successfully",
                "抱歉，这个我不能做。"
            )),
            None
        );
        assert_eq!(accumulator.flush(), Some(Delta::Text("抱歉，这个我不能做。".to_string())));
        assert_eq!(accumulator.text, "抱歉，这个我不能做。");
    }

    #[test]
    fn terminal_replay_is_not_resurrected_as_the_answer() {
        // 终态帧如果是历史回放，兜底也不能把它当答案
        let mut accumulator = TextAccumulator::with_history(vec!["上一轮的回答".to_string()]);
        accumulator.push_payload(&assistant_message_with_status(
            "m-old",
            "finished_successfully",
            "上一轮的回答",
        ));
        assert_eq!(accumulator.flush(), None, "回放不能顶替答案");
        assert_eq!(accumulator.text, "");
    }

    /// 带频道的助手消息。
    fn assistant_message_in_channel(id: &str, channel: &str, status: &str, text: &str) -> String {
        json!({"v": {"message": {
            "id": id,
            "author": {"role": "assistant"},
            "content": {"content_type": "text", "parts": [text]},
            "recipient": "all",
            "channel": channel,
            "status": status,
        }}})
        .to_string()
    }

    #[test]
    fn analysis_channel_becomes_reasoning_not_answer() {
        let mut accumulator = TextAccumulator::new();
        // 思考段：analysis 频道
        let delta = accumulator.push_payload(&assistant_message_in_channel(
            "m1",
            "analysis",
            "in_progress",
            "先想一想",
        ));
        assert_eq!(delta, Some(Delta::Reasoning("先想一想".to_string())));
        assert_eq!(accumulator.text, "", "思考内容不能进正文");
        assert_eq!(accumulator.reasoning, "先想一想");

        // 正文段：同一条消息切到 final
        let delta = accumulator.push_payload(&assistant_message_in_channel(
            "m1",
            "final",
            "in_progress",
            "答案是 42",
        ));
        assert_eq!(delta, Some(Delta::Text("答案是 42".to_string())));
        assert_eq!(accumulator.text, "答案是 42");
        assert_eq!(accumulator.reasoning, "先想一想");
    }

    #[test]
    fn commentary_refusal_is_captured_only_when_allowed() {
        // 线上实测：色情内容拒绝挂在 commentary 上
        let payload = json!({"v": {"message": {
            "id": "m-refusal",
            "author": {"role": "assistant"},
            "content": {"content_type": "text", "parts": ["非常抱歉，该提示可能违反了关于裸露、色情或情色内容的防护限制。"]},
            "recipient": "all",
            "channel": "commentary",
            "status": "finished_successfully",
            "end_turn": true,
        }}})
        .to_string();

        // 文本链路：commentary 不算正文，但会作为思考内容留下
        let mut text_side = TextAccumulator::new();
        text_side.push_payload(&payload);
        assert!(text_side.flush().is_none());
        assert_eq!(text_side.text, "");
        assert!(
            text_side.reasoning.contains("防护限制"),
            "应落到思考内容：{}",
            text_side.reasoning
        );

        // 生图链路：要拿到这段拒绝理由
        let mut image_side = TextAccumulator::new().with_commentary();
        image_side.push_payload(&payload);
        assert_eq!(
            image_side.flush(),
            Some(Delta::Text("非常抱歉，该提示可能违反了关于裸露、色情或情色内容的防护限制。".to_string()))
        );
    }

    #[test]
    fn assistant_history_keeps_only_assistant_text() {
        let messages = vec![
            ("system".to_string(), "内部指令".to_string()),
            ("user".to_string(), "问题".to_string()),
            ("assistant".to_string(), "答复".to_string()),
            ("user".to_string(), "追问".to_string()),
        ];
        assert_eq!(assistant_history(&messages), vec!["答复".to_string()]);
    }

    #[test]
    fn append_patch_accumulates() {
        let mut accumulator = TextAccumulator::new();
        let event = json!({"o": "patch", "v": [
            {"p": "/message/content/parts/0", "o": "append", "v": "你好"}
        ]});
        assert_eq!(accumulator.push_payload(&event.to_string()), Some(Delta::Text("你好".to_string())));
        let event = json!({"o": "patch", "v": [
            {"p": "/message/content/parts/0", "o": "append", "v": "，世界"}
        ]});
        assert_eq!(accumulator.push_payload(&event.to_string()), Some(Delta::Text("，世界".to_string())));
        assert_eq!(accumulator.text, "你好，世界");
    }

    #[test]
    fn replace_patch_overwrites() {
        let mut accumulator = TextAccumulator::new();
        accumulator.push_payload(
            &json!({"o": "patch", "v": [{"p": "/message/content/parts/0", "o": "append", "v": "旧"}]})
                .to_string(),
        );
        accumulator.push_payload(
            &json!({"o": "patch", "v": [{"p": "/message/content/parts/0", "o": "replace", "v": "新"}]})
                .to_string(),
        );
        assert_eq!(accumulator.text, "新");
    }

    #[test]
    fn bare_string_delta_appends_after_seed() {
        let mut accumulator = TextAccumulator::new();
        // 裸字符串增量只在「已有基础文本」时追加：
        // 首条通常是完整消息，之后的裸串才是增量。
        accumulator.push_payload(&assistant_message("第一段").to_string());
        // 真实形态是 {"v": "增量"}，不是顶层 JSON 字符串
        let delta = accumulator.push_payload(&json!({"v": "第二段"}).to_string());
        assert_eq!(delta, Some(Delta::Text("第二段".to_string())));
        assert_eq!(accumulator.text, "第一段第二段");
    }

    #[test]
    fn bare_string_before_any_text_is_ignored() {
        // 没有基础文本时，裸字符串无法判断是「增量」还是「新消息」，保守忽略
        let mut accumulator = TextAccumulator::new();
        assert_eq!(
            accumulator.push_payload(&json!({"v": "孤立增量"}).to_string()),
            None
        );
        assert_eq!(accumulator.text, "");
    }

    #[test]
    fn full_message_event_is_taken_directly() {
        let mut accumulator = TextAccumulator::new();
        let delta = accumulator.push_payload(&assistant_message("完整回复").to_string());
        assert_eq!(delta, Some(Delta::Text("完整回复".to_string())));
        assert_eq!(accumulator.text, "完整回复");
    }

    #[test]
    fn fresh_message_with_history_tail_is_trimmed() {
        // 整段候选 = 历史 + 本轮（同一帧里贴在一起）时只留本轮
        let mut accumulator = TextAccumulator::with_history(vec!["历史".to_string()]);
        let delta = accumulator.push_payload(&assistant_message("历史新内容").to_string());
        assert_eq!(delta, Some(Delta::Text("新内容".to_string())));
    }

    #[test]
    fn hidden_message_does_not_leak() {
        let mut accumulator = TextAccumulator::new();
        let event = json!({"v": {"message": {
            "author": {"role": "assistant"},
            "content": {"parts": ["系统内部说明"]},
            "metadata": {"is_visually_hidden_from_conversation": true},
        }}});
        assert_eq!(accumulator.push_payload(&event.to_string()), None);
        assert_eq!(accumulator.text, "");
    }

    #[test]
    fn reasoning_channel_does_not_leak() {
        let mut accumulator = TextAccumulator::new();
        let event = json!({"v": {"message": {
            "author": {"role": "assistant"},
            "content": {"parts": ["思维链内容"]},
            "channel": "analysis",
        }}});
        assert_eq!(accumulator.push_payload(&event.to_string()), None);
        assert_eq!(accumulator.text, "");
    }

    #[test]
    fn conversation_id_is_captured() {
        let mut accumulator = TextAccumulator::new();
        accumulator.push_payload(
            &json!({"type": "resume_conversation_token", "conversation_id": "conv-123"}).to_string(),
        );
        assert_eq!(accumulator.conversation_id, "conv-123");
    }

    #[test]
    fn done_payload_marks_finished() {
        let mut accumulator = TextAccumulator::new();
        assert_eq!(accumulator.push_payload("[DONE]"), None);
        assert!(accumulator.done);
    }

    #[test]
    fn whole_sse_stream_is_reduced_to_text() {
        let sse = concat!(
            "data: \"v1\"\n\n",
            "data: {\"conversation_id\":\"c-1\"}\n\n",
            "data: {\"o\":\"patch\",\"v\":[{\"p\":\"/message/content/parts/0\",\"o\":\"append\",\"v\":\"Hel\"}]}\n\n",
            "data: {\"o\":\"patch\",\"v\":[{\"p\":\"/message/content/parts/0\",\"o\":\"append\",\"v\":\"lo\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let (text, _reasoning, conversation_id, deltas) = collect_text(sse, &[]);
        assert_eq!(text, "Hello");
        assert_eq!(conversation_id, "c-1");
        assert_eq!(deltas, vec![Delta::Text("Hel".to_string()), Delta::Text("lo".to_string())]);
    }

    #[test]
    fn annotation_markers_are_cleaned() {
        // \u{E200}url\u{E202}标签\u{E202}https://x.y\u{E201}
        let raw = format!(
            "见这里{ANNOTATION_OPEN}url{ANNOTATION_SEP}来源{ANNOTATION_SEP}https://example.com{ANNOTATION_CLOSE}。"
        );
        let cleaned = sanitize_output_text(&raw);
        assert!(cleaned.contains("来源 (https://example.com)"), "{cleaned}");
        assert!(!cleaned.contains(ANNOTATION_OPEN), "标记应被移除: {cleaned}");
    }

    #[test]
    fn internal_annotation_is_dropped() {
        // 内部引用编号（turn0search1）应被丢弃
        let raw = format!(
            "结论{ANNOTATION_OPEN}cite{ANNOTATION_SEP}turn0search1{ANNOTATION_CLOSE}成立"
        );
        let cleaned = sanitize_output_text(&raw);
        assert_eq!(cleaned, "结论成立", "{cleaned}");
    }

    #[test]
    fn unclosed_annotation_truncates_tail() {
        let raw = format!("正文{ANNOTATION_OPEN}cite{ANNOTATION_SEP}turn0");
        assert_eq!(sanitize_output_text(&raw), "正文");
    }

    #[test]
    fn plain_text_is_untouched() {
        let text = "完全没有标记的一句话。";
        assert_eq!(sanitize_output_text(text), text);
    }
}