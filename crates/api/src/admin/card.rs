//! 指标卡片组件。
//!
//! 一张卡片的 HTML 只在这里定义，页面模板用 `{{ card|safe }}` 直接放进网格；
//! 加新卡片就是构造一个 `StatCard`，不要再回到模板里手写 `<article>`。

use std::fmt;

/// 卡片语气，只影响数值颜色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// 正常。
    Normal,

    /// 异常（例如读不到数据）。
    Danger,
}

impl Tone {
    fn class(self) -> &'static str {
        match self {
            Tone::Normal => "",

            Tone::Danger => " stat--danger",
        }
    }
}

/// 一张指标卡片。
#[derive(Debug, Clone)]
pub struct StatCard {
    label: String,
    value: String,
    subs: Vec<String>,
    badges: Vec<String>,
    error: String,
    tone: Tone,
}

impl StatCard {
    /// 新建：标签 + 主数值。
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            subs: Vec::new(),
            badges: Vec::new(),
            error: String::new(),
            tone: Tone::Normal,
        }
    }

    /// 追加一行副信息。
    pub fn sub(mut self, text: impl Into<String>) -> Self {
        self.subs.push(text.into());
        self
    }

    /// 追加一个徽标。
    pub fn badge(mut self, text: impl Into<String>) -> Self {
        self.badges.push(text.into());
        self
    }

    /// 设置错误行；有错误时卡片语气变异常。
    pub fn error(mut self, text: impl Into<String>) -> Self {
        self.error = text.into();
        if !self.error.is_empty() {
            self.tone = Tone::Danger;
        }
        self
    }


    /// 读不到数据时的卡片：标签保留，数值和错误原因一起写清楚。
    pub fn failed(label: impl Into<String>, error: impl Into<String>) -> Self {
        Self::new(label, "-").error(error)
    }

}

impl fmt::Display for StatCard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<article class=\"stat{}\">", self.tone.class())?;
        write!(f, "<span class=\"stat-label\">{}</span>", escape(&self.label))?;
        write!(f, "<span class=\"stat-value\">{}</span>", escape(&self.value))?;
        for sub in &self.subs {
            write!(f, "<span class=\"stat-sub\">{}</span>", escape(sub))?;
        }
        if !self.badges.is_empty() {
            write!(f, "<span class=\"stat-badges\">")?;
            for badge in &self.badges {
                write!(f, "<span class=\"badge\">{}</span>", escape(badge))?;
            }
            write!(f, "</span>")?;
        }
        if !self.error.is_empty() {
            write!(f, "<span class=\"stat-error\">{}</span>", escape(&self.error))?;
        }
        write!(f, "</article>")
    }
}

/// 最小 HTML 转义：卡片内容全部来自我们自己的文本，仍然按规矩转义。
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_label_value_and_badges() {
        let html = StatCard::new("账号总数", "5")
            .badge("正常 3")
            .badge("异常 2")
            .to_string();
        assert!(html.contains("账号总数"), "{html}");
        assert!(html.contains(">5<"), "{html}");
        assert_eq!(html.matches("class=\"badge\"").count(), 2, "{html}");
        assert!(!html.contains("stat--danger"), "{html}");
    }

    #[test]
    fn error_sets_danger_tone_and_renders_reason() {
        let html = StatCard::failed("图片存储", "读取失败（超时）").to_string();
        assert!(html.contains("stat--danger"), "{html}");
        assert!(html.contains("读取失败（超时）"), "{html}");
        assert!(html.contains(">-<"), "{html}");
    }

    #[test]
    fn escapes_special_characters() {
        let html = StatCard::new("<script>", "a&b").to_string();
        assert!(html.contains("&lt;script&gt;"), "{html}");
        assert!(html.contains("a&amp;b"), "{html}");
        assert!(!html.contains("<script>"), "{html}");
    }

    #[test]
    fn subs_render_in_order() {
        let html = StatCard::new("出口", "运行中")
            .sub("节点 44 个")
            .sub("跳过 3 个")
            .to_string();
        let first = html.find("节点 44 个").expect("第一行");
        let second = html.find("跳过 3 个").expect("第二行");
        assert!(first < second, "副信息顺序不对：{html}");
    }
}
