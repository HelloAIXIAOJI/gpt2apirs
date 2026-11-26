//! `POST /v1/audio/speech`（TTS）与 `POST /v1/audio/transcriptions`（STT）。
//!
//! 两条都是**模拟**上游网页端的能力：
//! - TTS：上游只能合成已存在的消息，所以要先让模型把文字原样复述一遍（两跳）
//! - STT：直接 multipart 上传，一轮就回
//!
//! TTS 的中间对话在 `library::speak` 里会被删掉，不留垃圾会话。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Multipart, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use gpt2apirs_core::AppError;
use gpt2apirs_protocol::speech;

use crate::error::ApiError;
use crate::state::AppState;

/// `POST /v1/audio/speech` 的请求体。
///
/// `speed` 只接受不实现：上游没有语速参数，假装支持会让调用方以为调了有用。
#[derive(Debug, Deserialize)]
pub struct SpeechRequest {
    /// 模型名（上游没有这个概念，接受但忽略）。
    #[serde(default)]
    pub model: Option<String>,
    /// 要念的文字。
    pub input: String,
    /// 音色。
    #[serde(default)]
    pub voice: Option<String>,
    /// 输出格式：mp3 / opus / aac / flac / wav。
    #[serde(default)]
    pub response_format: Option<String>,
    /// 语速（不支持）。
    #[serde(default)]
    pub speed: Option<f64>,
}

/// `POST /v1/audio/speech`
pub async fn speech(
    State(state): State<Arc<AppState>>,
    Json(request): Json<SpeechRequest>,
) -> Result<Response, ApiError> {
    let text = request.input.trim().to_string();
    if text.is_empty() {
        return Err(ApiError::openai(AppError::invalid_param(
            "input 不能为空",
            "input",
        )));
    }
    let voice = speech::resolve_voice(request.voice.as_deref().unwrap_or(""))
        .map_err(|message| ApiError::openai(AppError::invalid_param(message, "voice")))?;
    let format = speech::resolve_format(request.response_format.as_deref().unwrap_or(""));
    let (bytes, content_type) = crate::library::speak(&state, &text, &voice, &format).await?;
    Ok(([(header::CONTENT_TYPE, content_type)], bytes).into_response())
}

/// `POST /v1/audio/transcriptions`
///
/// 只认 `file` 字段；`model` / `language` / `prompt` 接受但忽略（上游不认）。
pub async fn transcriptions(
    State(state): State<Arc<AppState>>,
    mut multipart: Multipart,
) -> Result<Json<Value>, ApiError> {
    let mut filename = String::from("audio.mp3");
    let mut content_type = String::from("application/octet-stream");
    let mut data: Option<Vec<u8>> = None;

    while let Some(field) = multipart.next_field().await.map_err(|error| {
        ApiError::openai(AppError::invalid_param(
            format!("读取上传内容失败：{error}"),
            "file",
        ))
    })? {
        if field.name().unwrap_or("") != "file" {
            continue;
        }
        if let Some(name) = field.file_name() {
            filename = name.to_string();
        }
        if let Some(kind) = field.content_type() {
            content_type = kind.to_string();
        }
        data = Some(
            field
                .bytes()
                .await
                .map_err(|error| {
                    ApiError::openai(AppError::invalid_param(
                        format!("读取音频失败：{error}"),
                        "file",
                    ))
                })?
                .to_vec(),
        );
    }

    let Some(data) = data else {
        return Err(ApiError::openai(AppError::invalid_param(
            "缺少 file 字段（multipart 上传音频）",
            "file",
        )));
    };
    if data.is_empty() {
        return Err(ApiError::openai(AppError::invalid_param(
            "上传的音频是空的",
            "file",
        )));
    }

    let text = crate::library::transcribe(&state, &filename, &content_type, &data).await?;
    Ok(Json(json!({ "text": text })))
}
