//! 错误类型体系。
//!
//! 错误码与错误类型的映射取值来自实测，不要随意改。

use serde_json::Value;

/// 应用统一错误。
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// 请求参数非法（400）。
    #[error("{message}")]
    InvalidRequest {
        /// 人类可读消息。
        message: String,
        /// 出错的参数名。
        param: Option<String>,
        /// 覆盖默认错误码。
        code: Option<String>,
    },

    /// 未认证（401）。
    #[error("{message}")]
    Unauthorized {
        /// 人类可读消息。
        message: String,
    },

    /// 无权限（403）。
    #[error("{message}")]
    Forbidden {
        /// 人类可读消息。
        message: String,
    },

    /// 资源不存在（404）。
    #[error("{message}")]
    NotFound {
        /// 人类可读消息。
        message: String,
    },

    /// 冲突（409）。
    #[error("{message}")]
    Conflict {
        /// 人类可读消息。
        message: String,
    },

    /// 限流（429）。
    #[error("{message}")]
    RateLimited {
        /// 人类可读消息。
        message: String,
        /// 建议重试等待秒数。
        retry_after: Option<u64>,
    },

    /// 上游返回错误。
    #[error("upstream error {status}: {message}")]
    Upstream {
        /// 人类可读消息。
        message: String,
        /// 上游错误码（若有）。
        code: String,
        /// 是否可重试。
        retryable: bool,
        /// 上游 HTTP 状态码。
        status: u16,
    },

    /// 超时。
    #[error("timeout at {stage}")]
    Timeout {
        /// 超时发生的阶段，例如 `"sse"` / `"poll"` / `"download"`。
        stage: &'static str,
    },

    /// 暂时不可用（503）。典型场景：账号池没有可用账号。
    #[error("{message}")]
    ServiceUnavailable {
        /// 人类可读消息。
        message: String,
        /// 稳定错误码，例如 `no_available_account`。
        code: String,
    },

    /// 内部错误（500）。
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl AppError {
    /// HTTP 状态码。
    pub fn status(&self) -> u16 {
        match self {
            AppError::InvalidRequest { .. } => 400,
            AppError::Unauthorized { .. } => 401,
            AppError::Forbidden { .. } => 403,
            AppError::NotFound { .. } => 404,
            AppError::Conflict { .. } => 409,
            AppError::RateLimited { .. } => 429,
            AppError::Upstream { status, .. } => *status,
            AppError::Timeout { .. } => 504,
            AppError::ServiceUnavailable { .. } => 503,
            AppError::Internal(_) => 500,
        }
    }

    /// OpenAI 风格的 `error.type`。
    ///
    pub fn error_type(&self) -> &'static str {
        match self {
            AppError::RateLimited { .. } => "rate_limit_error",
            AppError::Timeout { .. } => "server_error",
            AppError::ServiceUnavailable { .. } => "server_error",
            AppError::Internal(_) => "server_error",
            other => default_error_type(other.status()),
        }
    }

    /// OpenAI 风格的 `error.code`。
    ///
    /// 默认错误码，允许具体错误覆盖。
    pub fn error_code(&self) -> String {
        if let AppError::InvalidRequest {
            code: Some(code), ..
        } = self
        {
            return code.clone();
        }
        if let AppError::Upstream { code, .. } = self {
            if !code.is_empty() {
                return code.clone();
            }
        }
        if let AppError::ServiceUnavailable { code, .. } = self {
            return code.clone();
        }
        default_error_code(self.status()).to_string()
    }

    /// OpenAI 风格 `error.param`。
    pub fn param(&self) -> Option<&str> {
        match self {
            AppError::InvalidRequest { param, .. } => param.as_deref(),
            _ => None,
        }
    }

    /// 人类可读消息。
    pub fn message(&self) -> String {
        match self {
            AppError::InvalidRequest { message, .. }
            | AppError::Unauthorized { message }
            | AppError::Forbidden { message }
            | AppError::NotFound { message }
            | AppError::Conflict { message }
            | AppError::RateLimited { message, .. }
            | AppError::Upstream { message, .. }
            | AppError::ServiceUnavailable { message, .. } => message.clone(),
            AppError::Timeout { stage } => format!("timeout at {stage}"),
            AppError::Internal(err) => err.to_string(),
        }
    }

    /// 是否可重试（决定上层是否换号/重发）。
    pub fn retryable(&self) -> bool {
        match self {
            AppError::RateLimited { .. } => true,
            AppError::Timeout { .. } => true,
            AppError::ServiceUnavailable { .. } => true,
            AppError::Upstream { retryable, .. } => *retryable,
            AppError::Internal(_) => false,
            _ => false,
        }
    }

    /// 快捷构造：参数错误。
    pub fn invalid(message: impl Into<String>) -> Self {
        AppError::InvalidRequest {
            message: message.into(),
            param: None,
            code: None,
        }
    }

    /// 快捷构造：带参数名的参数错误。
    pub fn invalid_param(message: impl Into<String>, param: impl Into<String>) -> Self {
        AppError::InvalidRequest {
            message: message.into(),
            param: Some(param.into()),
            code: None,
        }
    }

    /// 快捷构造：未认证。
    pub fn unauthorized(message: impl Into<String>) -> Self {
        AppError::Unauthorized {
            message: message.into(),
        }
    }
}

/// 按状态码给默认错误类型。
pub fn default_error_type(status: u16) -> &'static str {
    match status {
        401 => "authentication_error",
        403 => "permission_error",
        429 => "rate_limit_error",
        s if (400..500).contains(&s) => "invalid_request_error",
        _ => "server_error",
    }
}

/// 按状态码给默认错误码（具体错误可覆盖）。
pub fn default_error_code(status: u16) -> &'static str {
    match status {
        401 => "invalid_api_key",
        403 => "permission_denied",
        429 => "rate_limit_exceeded",
        s if (400..500).contains(&s) => "bad_request",
        _ => "upstream_error",
    }
}

/// 从任意 `detail` 里提取人类可读消息。
///
///
/// 关键行为：把 FastAPI / pydantic 的校验错误列表拼成人话。
///
/// ```text
/// [{"loc": ["body", "size"], "msg": "field required"}]
///     "size: field required"
/// ```
///
/// `loc` 里的 `"body"` 会被去掉；多条用 `"; "` 连接。
pub fn error_message_from_detail(detail: &Value) -> String {
    match detail {
        Value::Array(items) => {
            let mut messages: Vec<String> = Vec::new();
            for item in items {
                let Some(obj) = item.as_object() else {
                    continue;
                };
                let location = obj
                    .get("loc")
                    .and_then(Value::as_array)
                    .map(|parts| {
                        parts
                            .iter()
                            .filter(|p| p.as_str() != Some("body"))
                            .map(value_to_plain_string)
                            .filter(|s| !s.is_empty())
                            .collect::<Vec<_>>()
                            .join(".")
                    })
                    .unwrap_or_default();
                let message = obj
                    .get("msg")
                    .map(value_to_plain_string)
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if !location.is_empty() && !message.is_empty() {
                    messages.push(format!("{location}: {message}"));
                } else if !message.is_empty() {
                    messages.push(message);
                }
            }
            messages.join("; ")
        }
        Value::Object(obj) => {
            let from_error = message_from_value(obj.get("error"));
            if !from_error.is_empty() {
                return from_error;
            }
            message_from_value(Some(detail))
        }
        other => value_to_plain_string(other).trim().to_string(),
    }
}

/// 递归地从 `message` / `error.message` 取文本。
fn message_from_value(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    match value {
        Value::String(text) => text.clone(),
        Value::Object(obj) => {
            if let Some(Value::String(message)) = obj.get("message") {
                if !message.is_empty() {
                    return message.clone();
                }
            }
            message_from_value(obj.get("error"))
        }
        _ => String::new(),
    }
}

/// 把 JSON 值转成 Python `str()` 风格的文本。
fn value_to_plain_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Bool(b) => {
            if *b {
                "True".to_string()
            } else {
                "False".to_string()
            }
        }
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// OpenAI 错误载荷。
///
pub fn openai_error_payload(error: &AppError) -> Value {
    serde_json::json!({
        "error": {
            "message": if error.message().is_empty() { "request failed".to_string() } else { error.message() },
            "type": error.error_type(),
            "param": error.param(),
            "code": error.error_code(),
        }
    })
}

/// Anthropic 错误载荷。
///
/// Anthropic 错误体：`type` 在 5xx 时统一为 `api_error`。
pub fn anthropic_error_payload(error: &AppError) -> Value {
    let status = error.status();
    let error_type = if status >= 500 {
        "api_error"
    } else {
        default_error_type(status)
    };
    let message = error.message();
    serde_json::json!({
        "type": "error",
        "error": {
            "type": error_type,
            "message": if message.is_empty() { "request failed".to_string() } else { message },
        }
    })
}
