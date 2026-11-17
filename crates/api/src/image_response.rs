//! 生图响应的流式编码。
//!
//! base64 边编码边写出去，不在内存里拼整串：一张 2 MB 的图，整串 base64 是 2.7 MB，
//! n=4 就是 11 MB 的瞬时复制。这里每次只产出 12 KB（3 的倍数，分块编码不会串位）。

use axum::body::{Body, Bytes};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use futures_util::stream;
use serde_json::Value;

/// 单块 base64 的原始字节数。
///
/// 必须是 3 的倍数：base64 按 3 字节一组编码，只有最后一组才需要补 `=`。
const CHUNK: usize = 12_000;

/// 一张结果：`url` 与 `bytes` 二选一。
pub struct ImageItem {
    /// 图片 URL（url 模式）。
    pub url: Option<String>,
    /// 图片字节（b64 模式）。
    pub bytes: Option<Vec<u8>>,
    /// 原样回传的提示词。
    pub prompt: Option<String>,
}

/// OpenAI 兼容的生图响应，流式输出。
pub struct ImageResponse {
    /// 生成时间戳（秒）。
    pub created: i64,
    /// token 用量。
    pub usage: Value,
    /// 结果。
    pub items: Vec<ImageItem>,
    /// 上游没出图时的说明。
    pub message: Option<String>,
}

impl IntoResponse for ImageResponse {
    fn into_response(self) -> Response {
        let state = State {
            created: self.created,
            usage: self.usage,
            items: self.items,
            message: self.message,
            stage: Stage::Head,
        };
        let body = Body::from_stream(stream::unfold(state, |mut state| async move {
            state.next_chunk().map(|chunk| {
                (
                    Ok::<Bytes, std::io::Error>(chunk),
                    state,
                )
            })
        }));
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .body(body)
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    }
}

/// 输出到哪一步了。
enum Stage {
    /// 还没写开头。
    Head,
    /// 正在写第 `index` 张的头部。
    Item(usize),
    /// 正在写第 `index` 张的 base64，已写 `offset` 字节。
    Body { index: usize, offset: usize },
    /// 收尾。
    Tail,
    /// 写完了。
    Done,
}

struct State {
    created: i64,
    usage: Value,
    items: Vec<ImageItem>,
    message: Option<String>,
    stage: Stage,
}

impl State {
    fn next_chunk(&mut self) -> Option<Bytes> {
        loop {
            match self.stage {
                Stage::Head => {
                    self.stage = if self.items.is_empty() {
                        Stage::Tail
                    } else {
                        Stage::Item(0)
                    };
                    return Some(Bytes::from(format!("{{\"created\":{},\"data\":[", self.created)));
                }
                Stage::Item(index) => {
                    let item = &self.items[index];
                    let mut out = String::new();
                    if index > 0 {
                        out.push(',');
                    }
                    out.push('{');
                    let mut wrote = false;
                    if let Some(url) = &item.url {
                        out.push_str(&format!("\"url\":{}", json_string(url)));
                        wrote = true;
                    }
                    if let Some(prompt) = &item.prompt {
                        if wrote {
                            out.push(',');
                        }
                        out.push_str(&format!("\"revised_prompt\":{}", json_string(prompt)));
                        wrote = true;
                    }
                    match &item.bytes {
                        Some(bytes) if !bytes.is_empty() => {
                            if wrote {
                                out.push(',');
                            }
                            out.push_str("\"b64_json\":\"");
                            self.stage = Stage::Body { index, offset: 0 };
                        }
                        _ => {
                            out.push('}');
                            self.stage = next_stage(&self.items, index);
                        }
                    }
                    return Some(Bytes::from(out));
                }
                Stage::Body { index, offset } => {
                    let total = self.items[index]
                        .bytes
                        .as_ref()
                        .map(|bytes| bytes.len())
                        .unwrap_or(0);
                    if offset >= total {
                        self.stage = next_stage(&self.items, index);
                        return Some(Bytes::from_static(b"\"}"));
                    }
                    let end = (offset + CHUNK).min(total);
                    let slice = self.items[index]
                        .bytes
                        .as_ref()
                        .map(|bytes| &bytes[offset..end])
                        .unwrap_or(&[]);
                    let encoded = base64::engine::general_purpose::STANDARD.encode(slice);
                    self.stage = Stage::Body { index, offset: end };
                    return Some(Bytes::from(encoded));
                }
                Stage::Tail => {
                    self.stage = Stage::Done;
                    let mut out = String::from("],\"usage\":");
                    out.push_str(&self.usage.to_string());
                    if let Some(message) = &self.message {
                        out.push_str(&format!(",\"message\":{}", json_string(message)));
                    }
                    out.push('}');
                    return Some(Bytes::from(out));
                }
                Stage::Done => return None,
            }
        }
    }
}

fn next_stage(items: &[ImageItem], index: usize) -> Stage {
    if index + 1 < items.len() {
        Stage::Item(index + 1)
    } else {
        Stage::Tail
    }
}

/// 字符串按 JSON 规则转义（提示词可能带引号、换行）。
fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| String::from("\"\""))
}
