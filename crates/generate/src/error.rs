//! 生图失败的分类。

/// 失败类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// access_token 失效或被拒（401/403）。
    TokenInvalid,
    /// 上游拦截（bootstrap 非 200，但不像 token 问题）。
    Blocked,
    /// 内容策略拒绝。
    ContentPolicy,
    /// 配额耗尽。
    QuotaExhausted,
    /// 限流（429）。
    RateLimited,
    /// 要求 Turnstile 验证。
    TurnstileRequired,
    /// 要求 Arkose 验证。
    ArkoseRequired,
    /// 网络或传输层错误。
    Network,
    /// 协议不符合预期（字段缺失、响应不是 JSON 等）。
    Protocol,
    /// 上游跑完了但没给图。
    EmptyImages,
    /// 上游判定「请求本身有问题」（改请求才有用，换号没用）。
    InvalidRequest,
    /// 上游正常结束但没吐任何文本。
    EmptyReply,
}

impl FailureKind {
    /// 换账号是否可能有用。
    pub fn switch_account(self) -> bool {
        matches!(
            self,
            Self::TokenInvalid
                | Self::Blocked
                | Self::QuotaExhausted
                | Self::RateLimited
                | Self::Network
                | Self::EmptyImages
        )
    }

    /// 是否属于「换号或重试有意义」。（内容策略/验证码换号也没用）
    pub fn retryable(self) -> bool {
        self.switch_account() || matches!(self, Self::Protocol | Self::EmptyReply)
    }

    /// 对外错误码。
    pub fn code(self) -> &'static str {
        match self {
            Self::TokenInvalid => "account_token_invalid",
            Self::Blocked => "upstream_blocked",
            Self::ContentPolicy => "content_policy_violation",
            Self::QuotaExhausted => "account_quota_exhausted",
            Self::RateLimited => "rate_limited",
            Self::TurnstileRequired => "turnstile_required",
            Self::ArkoseRequired => "arkose_required",
            Self::Network => "upstream_network_error",
            Self::Protocol => "upstream_protocol_error",
            Self::EmptyImages => "upstream_no_image",
            Self::InvalidRequest => "upstream_invalid_request",
            Self::EmptyReply => "upstream_empty_reply",
        }
    }

    /// 对外的 HTTP 状态码。
    ///
    /// 账号问题不能回 401/403：那会让调用方以为**自己的 API key** 有问题；
    /// 请求问题必须回 400，否则客户端会当服务端故障去重试。
    pub fn status(self) -> u16 {
        match self {
            // 请求本身有问题：客户端拿到 400 + 原文才知道要不要改提示词
            Self::ContentPolicy | Self::InvalidRequest => 400,
            Self::QuotaExhausted | Self::RateLimited => 429,
            // 人机验证：账号需要人工处理，不是调用方的错，也不是服务端挂了
            Self::TurnstileRequired | Self::ArkoseRequired => 503,
            _ => 502,
        }
    }

    /// 是否应该把账号标记为「可能失效」。
    ///
    /// 内容策略和空图**不是账号的错**，标失效会误伤。
    pub fn blames_account(self) -> bool {
        matches!(
            self,
            Self::TokenInvalid
                | Self::Blocked
                | Self::RateLimited
                // 验证码卡住等于账号当前不可用：标异常，管理面才看得见
                | Self::TurnstileRequired
                | Self::ArkoseRequired
        )
    }
}

/// 带分类的生成错误。
#[derive(Debug, Clone)]
pub struct GenerateError {
    /// 分类。
    pub kind: FailureKind,
    /// 人类可读说明（含上游响应片段，便于排错）。
    pub message: String,
}

impl GenerateError {
    /// 新建。
    pub fn new(kind: FailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for GenerateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

impl std::error::Error for GenerateError {}

/// 从 HTTP 状态码 + 响应体判定失败类型。
///
/// 只在这一处做判定，避免各调用点各写一套。
pub fn classify_http(status: u16, body: &str) -> FailureKind {
    let lowered = body.to_ascii_lowercase();
    if lowered.contains("content_policy") || lowered.contains("content policy") {
        return FailureKind::ContentPolicy;
    }
    if lowered.contains("turnstile") {
        return FailureKind::TurnstileRequired;
    }
    if lowered.contains("arkose") {
        return FailureKind::ArkoseRequired;
    }
    // 正文里的配额信号优先于状态码：429 之外的限流也会带这句话
    if lowered.contains("quota")
        || lowered.contains("limit reached")
        || lowered.contains("reached your limit")
        || lowered.contains("reached the limit")
        || lowered.contains("usage limit") {
        return FailureKind::QuotaExhausted;
    }
    // token 失效的正文信号：上游不一定用 401，别误判成请求错误
    if [
        "token is expired",
        "token_expired",
        "invalid_token",
        "token has expired",
    ]
    .iter()
    .any(|needle| lowered.contains(needle))
    {
        return FailureKind::TokenInvalid;
    }
    // 请求级问题的信号：改请求才有用，换号纯属浪费
    if [
        "invalid_request",
        "unsupported",
        "model_not_found",
        "context_length_exceeded",
        "string_above_max_length",
    ]
    .iter()
    .any(|needle| lowered.contains(needle))
    {
        return FailureKind::InvalidRequest;
    }

    match status {
        401 | 403 => FailureKind::TokenInvalid,
        429 => FailureKind::RateLimited,
        402 => FailureKind::QuotaExhausted,
        400 | 422 => FailureKind::InvalidRequest,
        _ => FailureKind::Protocol,
    }
}

impl From<anyhow::Error> for GenerateError {
    fn from(error: anyhow::Error) -> Self {
        // 传输层错误（超时、DNS、连接失败）都归到 Network；换号有限次重试
        Self::new(FailureKind::Network, error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_level_signals_are_recognised() {
        // 请求本身有问题：换号没用，必须回 400
        for body in [
            r#"{"error":{"type":"invalid_request_error"}}"#,
            r#"{"detail":"model_not_found"}"#,
            r#"{"error":"unsupported model"}"#,
            r#"{"detail":"context_length_exceeded"}"#,
        ] {
            assert_eq!(classify_http(400, body), FailureKind::InvalidRequest, "{body}");
        }
        assert_eq!(classify_http(422, ""), FailureKind::InvalidRequest);
        assert_eq!(classify_http(400, ""), FailureKind::InvalidRequest);
    }

    #[test]
    fn expired_token_in_body_is_not_a_request_error() {
        // 真实遇到过：上游用 400 报「token 过期」，归成请求错误就不会换号了
        assert_eq!(
            classify_http(400, r#"{"error":{"message":"Provided authentication token is expired"}}"#),
            FailureKind::TokenInvalid
        );
        assert_eq!(classify_http(403, "token_expired"), FailureKind::TokenInvalid);
    }

    #[test]
    fn quota_in_body_beats_status() {
        // 429 之外也可能带「额度」字样，别当成请求错误
        assert_eq!(
            classify_http(429, r#"{"detail":"usage limit reached"}"#),
            FailureKind::QuotaExhausted
        );
        assert_eq!(
            classify_http(400, r#"{"detail":"you have reached your limit"}"#),
            FailureKind::QuotaExhausted
        );
    }

    #[test]
    fn client_status_follows_failure_scope() {
        assert_eq!(FailureKind::InvalidRequest.status(), 400);
        assert_eq!(FailureKind::ContentPolicy.status(), 400);
        assert_eq!(FailureKind::QuotaExhausted.status(), 429);
        assert_eq!(FailureKind::RateLimited.status(), 429);
        // 账号问题不能回 401/403：那是调用方自己的 key 错了的意思
        assert_eq!(FailureKind::TokenInvalid.status(), 502);
        assert_eq!(FailureKind::EmptyReply.status(), 502);
        // 需要人工验证：503，且要标异常让管理面看得见
        assert_eq!(FailureKind::TurnstileRequired.status(), 503);
        assert_eq!(FailureKind::ArkoseRequired.status(), 503);
        assert!(FailureKind::TurnstileRequired.blames_account());
        assert!(FailureKind::ArkoseRequired.blames_account());
        assert!(!FailureKind::TurnstileRequired.switch_account());
    }

    #[test]
    fn request_errors_do_not_switch_accounts() {
        assert!(!FailureKind::InvalidRequest.switch_account());
        assert!(!FailureKind::InvalidRequest.retryable());
        assert!(!FailureKind::InvalidRequest.blames_account());
        // 空回复不是账号的错，换号也没意义
        assert!(!FailureKind::EmptyReply.switch_account());
        assert!(FailureKind::EmptyReply.retryable());
        assert!(!FailureKind::EmptyReply.blames_account());
    }

    #[test]
    fn new_kinds_have_codes() {
        assert_eq!(FailureKind::InvalidRequest.code(), "upstream_invalid_request");
        assert_eq!(FailureKind::EmptyReply.code(), "upstream_empty_reply");
    }

    #[test]
    fn unauthorized_means_token_invalid() {
        assert_eq!(classify_http(401, ""), FailureKind::TokenInvalid);
        assert_eq!(classify_http(403, ""), FailureKind::TokenInvalid);
    }

    #[test]
    fn content_policy_wins_over_status() {
        // 内容策略即使状态码不是 4xx 也要认出来 —— 换号没用
        assert_eq!(
            classify_http(400, r#"{"error":"content_policy_violation"}"#),
            FailureKind::ContentPolicy
        );
        assert_eq!(
            classify_http(200, "Content Policy rejected"),
            FailureKind::ContentPolicy
        );
    }

    #[test]
    fn rate_limit_and_quota() {
        assert_eq!(classify_http(429, ""), FailureKind::RateLimited);
        assert_eq!(classify_http(402, ""), FailureKind::QuotaExhausted);
        assert_eq!(
            classify_http(400, r#"{"detail":"quota exceeded"}"#),
            FailureKind::QuotaExhausted
        );
    }

    #[test]
    fn verification_requirements_are_detected() {
        assert_eq!(
            classify_http(403, r#"{"turnstile_required":true}"#),
            FailureKind::TurnstileRequired
        );
        assert_eq!(classify_http(400, "arkose required"), FailureKind::ArkoseRequired);
    }

    #[test]
    fn unknown_falls_back_to_protocol() {
        assert_eq!(classify_http(500, "boom"), FailureKind::Protocol);
    }

    #[test]
    fn switch_account_policy() {
        // 可换号
        for kind in [
            FailureKind::TokenInvalid,
            FailureKind::QuotaExhausted,
            FailureKind::RateLimited,
            FailureKind::Blocked,
            FailureKind::Network,
            FailureKind::EmptyImages,
        ] {
            assert!(kind.switch_account(), "{kind:?} 应允许换号");
        }
        // 换号没用
        assert!(!FailureKind::ContentPolicy.switch_account());
        assert!(!FailureKind::TurnstileRequired.switch_account());
        assert!(!FailureKind::ArkoseRequired.switch_account());
        assert!(!FailureKind::Protocol.switch_account());
    }

    #[test]
    fn only_account_faults_blame_the_account() {
        assert!(FailureKind::TokenInvalid.blames_account());
        assert!(FailureKind::RateLimited.blames_account());
        // 内容策略和空图不是账号的错
        assert!(!FailureKind::ContentPolicy.blames_account());
        assert!(!FailureKind::EmptyImages.blames_account());
        assert!(!FailureKind::Network.blames_account());
    }

    #[test]
    fn codes_are_stable() {
        assert_eq!(FailureKind::TokenInvalid.code(), "account_token_invalid");
        assert_eq!(FailureKind::EmptyImages.code(), "upstream_no_image");
        assert_eq!(
            FailureKind::ContentPolicy.code(),
            "content_policy_violation"
        );
    }
}
