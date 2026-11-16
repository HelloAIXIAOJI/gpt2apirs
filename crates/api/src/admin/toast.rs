//! toast 片段：所有页面共用一份。
//!
//! 用 out-of-band swap 插到 `#toasts` 里，不走 HTTP 头（头只能放 ASCII，中文会乱码）。

/// 生成一段 toast 的 OOB 片段。
pub fn oob(message: &str, level: &str) -> String {
    let class = if level == "error" {
        "toast toast--error"
    } else {
        "toast"
    };
    format!(
        "<div id=\"toasts\" hx-swap-oob=\"beforeend\"><div class=\"{class}\">{}</div></div>",
        escape(message)
    )
}

/// 只回一条 toast 的响应（状态码 200）。
pub fn oob_response(message: &str, level: &str) -> axum::response::Response {
    use axum::response::IntoResponse;
    (axum::http::StatusCode::OK, axum::response::Html(oob(message, level))).into_response()
}

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
    fn fragment_is_oob_and_escaped() {
        let html = oob("刷新<失败>", "error");
        assert!(html.contains("hx-swap-oob=\"beforeend\""), "{html}");
        assert!(html.contains("toast--error"), "{html}");
        assert!(html.contains("刷新&lt;失败&gt;"), "{html}");
        assert!(!html.contains("<失败>"), "{html}");
    }
}
