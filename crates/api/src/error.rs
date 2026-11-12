//! 错误  HTTP 响应。
//!
//!
//! # 按路径选格式
//!
//! 三种格式按路径分派：`/v1/messages` → Anthropic；其余 `/v1*` → OpenAI；
//! 管理面 → `{"detail": ...}`。客户端只认自己那一种，混用会导致二次解析失败。
//!
//! axum 的 `IntoResponse` 拿不到请求路径，因此用 [`ErrorFormat`] 在**构造时**显式指定。

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use gpt2apirs_core::error::{anthropic_error_payload, openai_error_payload};
use gpt2apirs_core::AppError;

/// 错误响应的格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorFormat {
    /// OpenAI 兼容格式（`/v1/*`）。
    OpenAi,
    /// Anthropic 兼容格式（`/v1/messages`）。
    Anthropic,
    /// 管理面：`{"detail": ...}`。
    Admin,
}

/// 带格式信息的 API 错误。
#[derive(Debug)]
pub struct ApiError {
    /// 底层错误。
    pub error: AppError,
    /// 要求的输出格式。
    pub format: ErrorFormat,
}

impl ApiError {
    /// 以 OpenAI 格式包装。
    pub fn openai(error: impl Into<AppError>) -> Self {
        Self {
            error: error.into(),
            format: ErrorFormat::OpenAi,
        }
    }

    /// 以 Anthropic 格式包装。
    pub fn anthropic(error: impl Into<AppError>) -> Self {
        Self {
            error: error.into(),
            format: ErrorFormat::Anthropic,
        }
    }

    /// 以管理面格式包装。
    pub fn admin(error: impl Into<AppError>) -> Self {
        Self {
            error: error.into(),
            format: ErrorFormat::Admin,
        }
    }

    /// 按格式渲染响应体。
    pub fn payload(&self) -> serde_json::Value {
        match self.format {
            ErrorFormat::OpenAi => openai_error_payload(&self.error),
            ErrorFormat::Anthropic => anthropic_error_payload(&self.error),
            ErrorFormat::Admin => {
                let mut detail = serde_json::Map::new();
                detail.insert(
                    "message".to_string(),
                    serde_json::Value::String(self.error.message()),
                );
                detail.insert(
                    "code".to_string(),
                    serde_json::Value::String(self.error.error_code()),
                );
                detail.insert(
                    "type".to_string(),
                    serde_json::Value::String(self.error.error_type().to_string()),
                );
                // 参数错误要带上是哪个参数，否则调用方只能从 message 里抠字符串
                if let Some(param) = self.error.param() {
                    detail.insert(
                        "param".to_string(),
                        serde_json::Value::String(param.to_string()),
                    );
                }
                serde_json::json!({ "detail": serde_json::Value::Object(detail) })
            }
        }
    }
}

impl From<AppError> for ApiError {
    fn from(error: AppError) -> Self {
        Self::openai(error)
    }
}

/// 挂在响应上的错误码，供调用日志中间件读取。
#[derive(Debug, Clone)]
pub struct ErrorCodeTag(pub String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.error.status())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let payload = self.payload();

        // 内部错误记 error 级日志（对外只说 "internal error"）
        if status.is_server_error() {
            tracing::error!(
                error = %self.error.message(),
                code = %self.error.error_code(),
                "request failed with server error"
            );
        }

        let code = self.error.error_code().to_string();
        let mut response = (status, Json(payload)).into_response();
        // 中间件据此把错误码写进调用日志
        response.extensions_mut().insert(ErrorCodeTag(code));
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_payload_shape() {
        let err = ApiError::openai(AppError::Unauthorized {
            message: "bad key".to_string(),
        });
        let body = err.payload();
        assert_eq!(body["error"]["message"], "bad key");
        assert_eq!(body["error"]["type"], "authentication_error");
        assert_eq!(body["error"]["code"], "invalid_api_key");
    }

    #[test]
    fn anthropic_payload_shape() {
        let err = ApiError::anthropic(AppError::Internal(anyhow::anyhow!("boom")));
        let body = err.payload();
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "api_error", "5xx 应为 api_error");
        assert_eq!(body["error"]["message"], "boom");
    }

    #[test]
    fn admin_payload_shape() {
        let err = ApiError::admin(AppError::Forbidden {
            message: "需要管理员权限".to_string(),
        });
        let body = err.payload();
        assert_eq!(body["detail"]["message"], "需要管理员权限");
        assert_eq!(body["detail"]["code"], "permission_denied");
        assert_eq!(body["detail"]["type"], "permission_error");
    }

    #[test]
    fn status_codes_map_correctly() {
        assert_eq!(AppError::invalid("x").status(), 400);
        assert_eq!(AppError::unauthorized("x").status(), 401);
        assert_eq!(
            AppError::Forbidden {
                message: "x".into()
            }
            .status(),
            403
        );
        assert_eq!(
            AppError::NotFound {
                message: "x".into()
            }
            .status(),
            404
        );
        assert_eq!(
            AppError::RateLimited {
                message: "x".into(),
                retry_after: Some(5)
            }
            .status(),
            429
        );
        assert_eq!(
            AppError::Timeout { stage: "sse" }.status(),
            504
        );
        assert_eq!(AppError::Internal(anyhow::anyhow!("x")).status(), 500);
    }
}
