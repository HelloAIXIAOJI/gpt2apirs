//! Cloudflare clearance 探测。
//!
//! 订阅节点的 IP 是否被 Cloudflare 拦，决定了账号能不能用；
//! 「延迟低」不等于「能过 CF」。这里拿当前出口请求一个受 CF 保护的地址，
//! 按状态码与响应特征判断：通 / 被挑战 / 其它错误。

use std::sync::Arc;

use gpt2apirs_core::Deadline;

use crate::state::AppState;

/// 默认探测目标：ChatGPT 网页端，正常应当被 CF 放行（带 TLS 指纹）。
pub const DEFAULT_CLEARANCE_URL: &str = "https://chatgpt.com/";

/// 探测结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Clearance {
    /// 未被拦截。
    Passed,
    /// 被 Cloudflare 挑战/拦截。
    Challenged,
    /// 其它状态码。
    Unexpected(u16),
    /// 请求本身失败。
    Failed(String),
}

impl Clearance {
    /// 是否算通过。
    pub fn is_ok(&self) -> bool {
        matches!(self, Clearance::Passed)
    }

    /// 对外文案。
    pub fn as_text(&self) -> String {
        match self {
            Clearance::Passed => "通过（未被 Cloudflare 拦截）".to_string(),
            Clearance::Challenged => "被 Cloudflare 挑战/拦截".to_string(),
            Clearance::Unexpected(status) => format!("意外状态码 {status}"),
            Clearance::Failed(reason) => format!("请求失败：{reason}"),
        }
    }
}

/// 根据响应判定结论（纯函数，便于单测覆盖各种 CF 特征）。
///
/// 客户端只返回状态码与正文，拿不到 `cf-mitigated` 头，所以判定基于这两项。
pub fn classify(status: u16, body: &str) -> Clearance {
    if (200..300).contains(&status) {
        // 200 也可能是挑战页（部分边缘会这样回），再看一眼特征。
        if looks_like_challenge(body) {
            return Clearance::Challenged;
        }
        return Clearance::Passed;
    }

    if status == 403 && looks_like_challenge(body) {
        return Clearance::Challenged;
    }
    Clearance::Unexpected(status)
}

/// 响应体里有没有 Cloudflare 挑战页的痕迹。
fn looks_like_challenge(body: &str) -> bool {
    let head: String = body.chars().take(8192).collect::<String>().to_ascii_lowercase();
    head.contains("just a moment")
        || head.contains("cf-chl")
        || head.contains("cf_chl")
        || head.contains("cloudflare") && head.contains("challenge")
}

/// 跑一次探测：用当前全局出口请求目标地址。
pub async fn probe(state: &Arc<AppState>, url: &str, timeout_ms: u64) -> Clearance {
    let client = match state.client_for("") {
        Ok(client) => client,
        Err(error) => return Clearance::Failed(format!("{error}")),
    };
    let cap = std::time::Duration::from_millis(timeout_ms.max(1000));
    let deadline = Deadline::in_secs((timeout_ms.max(1000) as f64) / 1000.0);
    let Ok(budget) = deadline.budget("clearance", cap) else {
        return Clearance::Failed("预算不足".to_string());
    };
    let headers = vec![("Accept", "text/html,application/xhtml+xml".to_string())];
    match client.get_bytes(url, &headers, Some(budget)).await {
        Ok(response) => {
            let body = String::from_utf8_lossy(&response.body).to_string();
            classify(response.status, &body)
        }
        Err(error) => Clearance::Failed(format!("{error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_on_clean_200() {
        assert_eq!(classify(200, "<html>ok</html>"), Clearance::Passed);
    }


    #[test]
    fn detects_challenge_page_on_403() {
        assert_eq!(
            classify(403, "<title>Just a moment...</title>"),
            Clearance::Challenged
        );
        assert_eq!(
            classify(200, "<div id=\"cf-chl-widget\"></div>"),
            Clearance::Challenged
        );
    }

    #[test]
    fn other_status_is_unexpected() {
        assert_eq!(classify(500, "oops"), Clearance::Unexpected(500));
        assert_eq!(classify(429, "slow down"), Clearance::Unexpected(429));
    }
}
