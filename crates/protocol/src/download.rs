//! 图片下载链路的纯逻辑部分（不含 HTTP）。

use serde_json::Value;

/// 解析附件（sediment）下载地址的 API 路径。
///
/// 注意：传入的 sediment_id 是 sediment:// 之后的部分，不含前缀。
pub fn attachment_download_path(conversation_id: &str, sediment_id: &str) -> String {
    format!("/backend-api/conversation/{conversation_id}/attachment/{sediment_id}/download")
}

/// 解析文件（file）下载地址的 API 路径。
pub fn file_download_path(file_id: &str) -> String {
    format!("/backend-api/files/{file_id}/download")
}

/// 从下载地址解析响应中取出 URL。
///
/// 取下载地址：优先 `download_url`，退回 `url`，都没有则为空。
pub fn parse_download_url(body: &Value) -> Option<String> {
    for key in ["download_url", "url"] {
        if let Some(value) = body.get(key).and_then(Value::as_str) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

/// 判断下载 URL 是否与上游同源。
///
/// 同源才允许携带 Authorization 等账号凭据；跨域一律不带。
pub fn is_same_origin(base_url: &str, url: &str) -> bool {
    // 无 scheme 的相对路径视为同源
    if !url.contains("://") {
        return true;
    }
    host_of(base_url) == host_of(url)
}

fn host_of(url: &str) -> String {
    let without_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    without_scheme
        .split('/')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// 需要跳过的伪文件 ID（如 `file_upload` 这类占位值）。
pub fn should_skip_file_id(file_id: &str) -> bool {
    file_id == "file_upload"
}

/// 把 file_ids / sediment_ids 整理成待解析任务。
///
/// 返回 (attachment 任务, file 任务)：attachment 需要 conversation_id，file 不需要。
pub fn plan_download_targets(
    conversation_id: &str,
    file_ids: &[String],
    sediment_ids: &[String],
) -> (Vec<(String, String)>, Vec<String>) {
    let mut attachments: Vec<(String, String)> = Vec::new();
    let mut files: Vec<String> = Vec::new();

    for file_id in file_ids {
        if should_skip_file_id(file_id) {
            continue;
        }
        if !files.iter().any(|existing| existing == file_id) {
            files.push(file_id.clone());
        }
    }

    if !conversation_id.is_empty() {
        for sediment_id in sediment_ids {
            if sediment_id.is_empty() {
                continue;
            }
            if !attachments.iter().any(|(_, existing)| existing == sediment_id) {
                attachments.push((conversation_id.to_string(), sediment_id.clone()));
            }
        }
    }

    (attachments, files)
}