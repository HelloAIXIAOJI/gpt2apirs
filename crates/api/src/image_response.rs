//! 带 base64 的 JSON 响应的流式编码。
//!
//! base64 边编码边写出去，不在内存里拼整串：一张 2 MB 的图，整串是 2.7 MB；
//! 多图任务里这个复制会随张数线性叠加。这里每块只产出 12 KB。
//!
//! 用法：给 `StreamedJson` 一个前缀（`{"x":1,"data":[`）、若干条目、一个后缀（`]}`）。
//! 条目可以是 URL、内存字节，或者一个待读取的文件（后者一次只读一张）。

use std::path::PathBuf;

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

/// 一个结果条目的来源。
pub enum ItemSource {
    /// 直接给 URL（url 模式）。
    Url(String),
    /// 内存里的字节（同步生图：字节本来就在手上）。
    Bytes(Vec<u8>),
    /// 磁盘上的文件（异步任务：结果早落盘了，按需读）。
    File(PathBuf),
}

/// 一个结果条目。
pub struct StreamItem {
    /// 内容来源。
    pub source: ItemSource,
    /// 原样回传的提示词。
    pub prompt: Option<String>,
}

/// 前缀 + 条目 + 后缀，整体流式输出。
pub struct StreamedJson {
    /// 条目前的 JSON 前缀，必须以 `[` 结尾。
    pub prefix: String,
    /// 条目。
    pub items: Vec<StreamItem>,
    /// 条目之后的 JSON 后缀。
    pub suffix: String,
}

impl IntoResponse for StreamedJson {
    fn into_response(self) -> Response {
        let state = State {
            prefix: self.prefix,
            items: self.items,
            suffix: self.suffix,
            stage: Stage::Head,
            loaded: None,
        };
        let body = Body::from_stream(stream::unfold(state, |mut state| async move {
            state.next_chunk().map(|chunk| {
                (Ok::<Bytes, std::io::Error>(chunk), state)
            })
        }));
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .body(body)
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    }
}

/// OpenAI 兼容的生图响应（同步接口用）。
pub struct ImageResponse {
    /// 生成时间戳（秒）。
    pub created: i64,
    /// token 用量。
    pub usage: Value,
    /// 结果。
    pub items: Vec<StreamItem>,
    /// 上游没出图时的说明。
    pub message: Option<String>,
}

impl IntoResponse for ImageResponse {
    fn into_response(self) -> Response {
        let mut head = serde_json::Map::new();
        head.insert("created".to_string(), Value::from(self.created));
        let prefix = format!(
            "{},\"data\":[",
            strip_tail(&Value::Object(head))
        );
        let mut tail = serde_json::Map::new();
        tail.insert("usage".to_string(), self.usage);
        if let Some(message) = self.message {
            tail.insert("message".to_string(), Value::from(message));
        }
        let suffix = format!("],{}", strip_head(&Value::Object(tail)));
        StreamedJson {
            prefix,
            items: self.items,
            suffix,
        }
        .into_response()
    }
}

/// 去掉序列化结果首尾的大括号（用于拼接前后缀）。
fn strip_tail(value: &Value) -> String {
    let raw = value.to_string();
    raw.trim_end_matches('}').to_string()
}

/// 去掉开头的大括号（结尾的留给外层对象）。
fn strip_head(value: &Value) -> String {
    value.to_string().trim_start_matches('{').to_string()
}

/// 输出到哪一步了。
enum Stage {
    /// 还没写前缀。
    Head,
    /// 正在写第 `index` 条的头部。
    Item(usize),
    /// 正在写第 `index` 条的 base64，已写 `offset` 字节。
    Body { index: usize, offset: usize },
    /// 后缀。
    Tail,
    /// 写完了。
    Done,
}

struct State {
    prefix: String,
    items: Vec<StreamItem>,
    suffix: String,
    stage: Stage,
    /// 当前条目按需读进来的字节（一次只留一张）。
    loaded: Option<Vec<u8>>,
}

impl State {
    fn next_chunk(&mut self) -> Option<Bytes> {
        match self.stage {
            Stage::Head => {
                self.stage = if self.items.is_empty() {
                    Stage::Tail
                } else {
                    Stage::Item(0)
                };
                Some(Bytes::from(self.prefix.clone()))
            }
            Stage::Item(index) => {
                let item = &self.items[index];
                let mut out = String::new();
                if index > 0 {
                    out.push(',');
                }
                out.push('{');
                let mut wrote = false;
                if let ItemSource::Url(url) = &item.source {
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
                // 文件源按需读一张；读完只留在 `loaded` 里，下一张就换掉。
                let has_bytes = match &item.source {
                    ItemSource::Url(_) => false,
                    ItemSource::Bytes(bytes) => !bytes.is_empty(),
                    ItemSource::File(path) => match std::fs::read(path) {
                        Ok(bytes) => {
                            self.loaded = Some(bytes);
                            true
                        }
                        Err(error) => {
                            tracing::warn!(%error, path = %path.display(), "读取结果图片失败");
                            false
                        }
                    },
                };
                if has_bytes {
                    if wrote {
                        out.push(',');
                    }
                    out.push_str("\"b64_json\":\"");
                    self.stage = Stage::Body { index, offset: 0 };
                } else {
                    out.push('}');
                    self.stage = next_stage(&self.items, index);
                }
                Some(Bytes::from(out))
            }
            Stage::Body { index, offset } => {
                let total = self.current_len(index);
                if offset >= total {
                    self.loaded = None;
                    self.stage = next_stage(&self.items, index);
                    return Some(Bytes::from_static(b"\"}"));
                }
                let end = (offset + CHUNK).min(total);
                let encoded = {
                    let bytes = self.current_bytes(index);
                    base64::engine::general_purpose::STANDARD.encode(&bytes[offset..end])
                };
                self.stage = Stage::Body { index, offset: end };
                Some(Bytes::from(encoded))
            }
            Stage::Tail => {
                self.stage = Stage::Done;
                Some(Bytes::from(self.suffix.clone()))
            }
            Stage::Done => None,
        }
    }

    fn current_len(&self, index: usize) -> usize {
        match &self.items[index].source {
            ItemSource::Bytes(bytes) => bytes.len(),
            _ => self.loaded.as_ref().map(|bytes| bytes.len()).unwrap_or(0),
        }
    }

    fn current_bytes(&self, index: usize) -> &[u8] {
        match &self.items[index].source {
            ItemSource::Bytes(bytes) => bytes,
            _ => self.loaded.as_deref().unwrap_or(&[]),
        }
    }
}

fn next_stage(items: &[StreamItem], index: usize) -> Stage {
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
