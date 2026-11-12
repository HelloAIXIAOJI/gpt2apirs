//! 富文本标签处理（ADR-011：约束 + 窄剥离）。
//!
//! 上游有时把整段 UI 组件树当正文返回。两个手段：
//! - **guard**：system 里要求只输出纯文本，治根因
//! - **strip**：只剥「自闭合 + 大写开头」的组件，兜底
//!
//! strip 故意不剥小写 JSX（`<box>` 之类）—— 剥了只剩残句，那类交给 guard。
//! 剥完仍有标签时打 WARN，让 guard 失效可见。

/// 富文本处理策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RichTextPolicy {
    /// 什么都不做。
    Off,
    /// 只加提示词约束。
    Guard,
    /// 只剥自闭合组件。
    Strip,
    /// 两者都做（默认）。
    GuardStrip,
}

impl RichTextPolicy {
    /// 从设置值解析。无法识别时退回默认值，并打一条 WARN。
    pub fn from_setting(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "guard+strip" | "guard_strip" | "both" => Self::GuardStrip,
            "off" | "none" | "0" | "false" => Self::Off,
            "guard" => Self::Guard,
            "strip" => Self::Strip,
            other => {
                tracing::warn!(
                    value = other,
                    "无法识别的 rich_text_policy，按默认 guard+strip 处理"
                );
                Self::GuardStrip
            }
        }
    }

    /// 是否注入提示词约束。
    pub fn guard(self) -> bool {
        matches!(self, Self::Guard | Self::GuardStrip)
    }

    /// 是否剥离自闭合组件。
    pub fn strip(self) -> bool {
        matches!(self, Self::Strip | Self::GuardStrip)
    }
}

/// 注入给模型的约束。
///
/// 措辞刻意「具体点名」而不是笼统说「不要 HTML」：
/// 笼统的约束模型容易理解成「可以用 Markdown」，而这些标签恰恰是它自创的组件。
pub const GUARD_MESSAGE: &str = "用纯文本 / Markdown 回答。\
不要输出任何 UI 组件标签、JSX 或上游自定义富文本标记\
（例如 LearningViz、box、grid、badge、AsyncImage、radio-group 等），\
需要表达结构、对比或图表思路时用 Markdown 的表格、列表和文字描述。";

/// 把约束注入到消息列表最前面（已有相同的就不再重复加）。
pub fn apply_guard(messages: &mut Vec<(String, String)>) {
    if messages
        .iter()
        .any(|(role, text)| role == "system" && text.contains(GUARD_MESSAGE))
    {
        return;
    }
    messages.insert(0, ("system".to_string(), GUARD_MESSAGE.to_string()));
}

/// 一次剥离的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripReport {
    /// 剥离后的文本。
    pub text: String,
    /// 剥掉了几个组件。
    pub removed: usize,
}

/// 剥掉自闭合的富文本组件（`<Tag … />`，Tag 以**大写**开头）。
pub fn strip_rich_text(text: &str) -> StripReport {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut removed = 0usize;
    let mut index = 0usize;

    while index < bytes.len() {
        if bytes[index] == b'<' {
            // 命中自闭合组件：整段丢弃，计入剥离数
            if let Some(end) = match_self_closing_widget(text, index) {
                removed += 1;
                index = end;
                continue;
            }
        }
        // 普通字符按 UTF-8 长度推进，避免切在多字节字符中间
        let ch_len = utf8_len(bytes[index]);
        let next = (index + ch_len).min(text.len());
        out.push_str(&text[index..next]);
        index = next;
    }

    StripReport {
        text: out,
        removed,
    }
}

fn utf8_len(byte: u8) -> usize {
    match byte {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

/// 如果 `start` 处的 `<` 是一个「自闭合 + 大写开头」的组件，返回它的结束位置（`/>` 之后）。
fn match_self_closing_widget(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut index = start + 1;
    // 必须以大写字母开头
    let first = *bytes.get(index)?;
    if !first.is_ascii_uppercase() {
        return None;
    }
    index += 1;
    while let Some(byte) = bytes.get(index) {
        if byte.is_ascii_alphanumeric() || *byte == b'_' || *byte == b':' || *byte == b'.' {
            index += 1;
        } else {
            break;
        }
    }
    // 名字后面必须紧跟空白或直接 `/>`
    match bytes.get(index) {
        Some(byte) if byte.is_ascii_whitespace() || *byte == b'/' || *byte == b'>' => {}
        _ => return None,
    }

    // 扫到 `/>`；跳过引号内的内容（属性值里可能有 `>`）
    let mut quote: Option<u8> = None;
    while let Some(byte) = bytes.get(index) {
        match quote {
            Some(active) => {
                if *byte == active {
                    quote = None;
                }
            }
            None => match byte {
                b'"' | b'\'' => quote = Some(*byte),
                b'/' if bytes.get(index + 1) == Some(&b'>') => return Some(index + 2),
                b'<' => return None, // 遇到新标签，说明不是自闭合
                _ => {}
            },
        }
        index += 1;
    }
    None
}

/// 统计文本里还剩多少个标签（任何形式）。
///
/// 用于**观测 guard 是否失效**：窄剥离只处理自闭合组件，
/// 如果剥完还剩标签，说明模型又发了 JSX 树 —— 那必须被看见，而不是静默放过。
pub fn count_residual_tags(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut count = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'<' {
            // "<Tag" 或 "</Tag"
            let mut probe = index + 1;
            if bytes.get(probe) == Some(&b'/') {
                probe += 1;
            }
            if bytes.get(probe).is_some_and(u8::is_ascii_alphabetic) {
                count += 1;
            }
        }
        index += 1;
    }
    count
}

/// 流式场景下的增量剥离器。
///
/// 单个组件可能跨 chunk 到达，所以**看到可能的标签开头就先压住不发**，
/// 直到能确定「是组件（丢掉）」或「不是组件（原样放出）」。
#[derive(Debug, Default)]
pub struct StreamStripper {
    pending: String,
    removed: usize,
}

impl StreamStripper {
    /// 新建。
    pub fn new() -> Self {
        Self::default()
    }

    /// 已剥离的组件数。
    pub fn removed(&self) -> usize {
        self.removed
    }

    /// 喂一段增量，返回可以立刻发出的文本。
    pub fn push(&mut self, chunk: &str) -> String {
        self.pending.push_str(chunk);
        let (ready, rest) = split_ready(&self.pending);
        self.removed += count_widgets(&ready);
        self.pending = rest;
        strip_rich_text(&ready).text
    }

    /// 流结束时冲洗剩余内容。
    pub fn finish(&mut self) -> String {
        let rest = std::mem::take(&mut self.pending);
        self.removed += count_widgets(&rest);
        strip_rich_text(&rest).text
    }
}

fn count_widgets(text: &str) -> usize {
    strip_rich_text(text).removed
}

/// 把缓冲区切成「可以安全处理的前段」+「可能是不完整组件的尾部」。
///
/// 只压住**仍可能变成组件**的尾巴：`<` + 大写字母开头 + 名字字符。
/// 否则正文里的字面 `<`（例如 `3 < 5`）会把后面所有内容一直卡到流结束。
fn split_ready(buffer: &str) -> (String, String) {
    /// 单个组件标签的长度上限；超过就认为不是组件，直接放行。
    const MAX_HOLD: usize = 512;

    if let Some(last_lt) = buffer.rfind('<') {
        let tail = &buffer[last_lt..];
        if !tail.contains('>') && tail.len() <= MAX_HOLD && looks_like_widget_prefix(tail) {
            return (buffer[..last_lt].to_string(), tail.to_string());
        }
    }
    (buffer.to_string(), String::new())
}

/// `<` 后面是否可能是组件名（大写字母开头 + 名字字符，或还没写出来）。
fn looks_like_widget_prefix(tail: &str) -> bool {
    let rest = &tail[1..];
    match rest.chars().next() {
        None => true, // 只有 '<'，可能马上跟上大写字母
        Some(first) if !first.is_ascii_uppercase() => false,
        Some(_) => rest.chars().all(|c| {
            c.is_ascii_alphanumeric() || c == '_' || c == ':' || c == '.' || c.is_ascii_whitespace() || c == '/'
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_parsing() {
        assert_eq!(RichTextPolicy::from_setting(""), RichTextPolicy::GuardStrip);
        assert_eq!(
            RichTextPolicy::from_setting("guard+strip"),
            RichTextPolicy::GuardStrip
        );
        assert_eq!(RichTextPolicy::from_setting("off"), RichTextPolicy::Off);
        assert_eq!(RichTextPolicy::from_setting("guard"), RichTextPolicy::Guard);
        assert_eq!(RichTextPolicy::from_setting("strip"), RichTextPolicy::Strip);
        // 无法识别  默认
        assert_eq!(
            RichTextPolicy::from_setting("乱写"),
            RichTextPolicy::GuardStrip
        );
    }

    #[test]
    fn policy_flags() {
        assert!(RichTextPolicy::GuardStrip.guard());
        assert!(RichTextPolicy::GuardStrip.strip());
        assert!(RichTextPolicy::Guard.guard());
        assert!(!RichTextPolicy::Guard.strip());
        assert!(!RichTextPolicy::Off.guard());
        assert!(!RichTextPolicy::Off.strip());
    }

    #[test]
    fn guard_is_injected_once() {
        let mut messages = vec![("user".to_string(), "你好".to_string())];
        apply_guard(&mut messages);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].0, "system");
        assert!(messages[0].1.contains("纯文本"));
        // 再加一次不应重复
        apply_guard(&mut messages);
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn strips_learning_viz() {
        let text = "TCP 三次握手是这样的。\n\n<LearningViz type_id=\"TCP\" locale_override=\"zh-CN\"/>";
        let report = strip_rich_text(text);
        assert_eq!(report.removed, 1);
        assert!(!report.text.contains("LearningViz"));
        assert!(report.text.contains("TCP 三次握手是这样的。"));
    }

    #[test]
    fn strips_widget_with_braces_in_attributes() {
        let text = r#"前文<LearningViz type_id="X" initial_values={{"a":3,"b":4}} locale_override="zh-CN"/>后文"#;
        let report = strip_rich_text(text);
        assert_eq!(report.removed, 1);
        assert_eq!(report.text, "前文后文");
    }

    #[test]
    fn keeps_lowercase_jsx() {
        // 小写 JSX 树不剥（剥了只剩残句），交给 guard 治理
        let text = "<box border>内容</box>";
        let report = strip_rich_text(text);
        assert_eq!(report.removed, 0);
        assert_eq!(report.text, text);
    }

    #[test]
    fn keeps_user_html() {
        let text = "把 <div class=\"x\"> 和 <Span> 都保留";
        let report = strip_rich_text(text);
        assert_eq!(report.removed, 0, "小写 div 不剥");
        // <Span> 不是自闭合，也不剥
        assert_eq!(report.text, text);
    }

    #[test]
    fn plain_text_untouched() {
        let text = "完全没有标签的一句话。";
        let report = strip_rich_text(text);
        assert_eq!(report.removed, 0);
        assert_eq!(report.text, text);
    }

    #[test]
    fn unclosed_widget_is_kept() {
        // 没有 /> 收尾，无法确认是组件  原样保留
        let text = "<LearningViz type_id=\"X\"";
        let report = strip_rich_text(text);
        assert_eq!(report.removed, 0);
        assert_eq!(report.text, text);
    }

    #[test]
    fn residual_tag_counting() {
        assert_eq!(count_residual_tags("纯文本"), 0);
        assert_eq!(count_residual_tags("<box>内容</box>"), 2, "开闭标签各算一个");
        assert_eq!(count_residual_tags("<LearningViz a=\"b\"/>"), 1);
        assert_eq!(count_residual_tags("3 < 5 成立"), 0, "字面小于号不算");
        assert_eq!(count_residual_tags("< text>"), 0, "尖括号后是空格不算");
    }

    #[test]
    fn stream_stripper_handles_split_widget() {
        let mut stripper = StreamStripper::new();
        // 组件被切成三段
        let a = stripper.push("前文<Learning");
        assert_eq!(a, "前文", "可能的标签开头应被压住");
        let b = stripper.push("Viz type_id=\"X\"/>");
        assert_eq!(b, "", "组件应被丢弃");
        let c = stripper.push("后文");
        assert_eq!(c, "后文");
        assert_eq!(stripper.removed(), 1);
    }

    #[test]
    fn stream_stripper_releases_non_widget() {
        let mut stripper = StreamStripper::new();
        let a = stripper.push("算一下 3 < 5");
        // '<' 后面是空格，不是组件开头，但会先被压住
        let b = stripper.push(" 成立");
        assert_eq!(format!("{a}{b}"), "算一下 3 < 5 成立");
        assert_eq!(stripper.removed(), 0);
    }

    #[test]
    fn stream_stripper_finish_flushes() {
        let mut stripper = StreamStripper::new();
        let out = stripper.push("结尾的 <");
        assert_eq!(out, "结尾的 ");
        assert_eq!(stripper.finish(), "<");
    }
}
