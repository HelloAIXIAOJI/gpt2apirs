//! SSE 的**增量**分帧。
//!
//! [`crate::sse::iter_sse_payloads`] 要求先拿到**完整**响应体再切帧，
//! 这对「一次性读完」的场景够用，但要边收边吐就必须支持**半截输入**。

/// 增量 SSE 分帧器。
#[derive(Debug, Default)]
pub struct SseFramer {
    buffer: String,
}

impl SseFramer {
    /// 新建。
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂一段原始字节（已按 UTF-8 宽松解码），返回**已完整**的 payload 列表。
    pub fn push(&mut self, chunk: &str) -> Vec<String> {
        self.buffer.push_str(chunk);
        let mut payloads = Vec::new();

        loop {
            // 找到事件边界：优先 \n\n，兼容 \r\n\r\n
            let boundary = find_boundary(&self.buffer);
            let Some((index, width)) = boundary else {
                break;
            };
            let block = self.buffer[..index].to_string();
            self.buffer.drain(..index + width);
            if let Some(payload) = parse_block(&block) {
                payloads.push(payload);
            }
        }
        payloads
    }

    /// 流结束时把缓冲区里剩下的内容当作最后一条事件处理（如果有）。
    pub fn finish(&mut self) -> Option<String> {
        let rest = std::mem::take(&mut self.buffer);
        let trimmed = rest.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            return None;
        }
        parse_block(trimmed)
    }

    /// 当前缓冲区字节数（诊断用）。
    pub fn buffered_len(&self) -> usize {
        self.buffer.len()
    }
}

fn find_boundary(buffer: &str) -> Option<(usize, usize)> {
    let lf = buffer.find("\n\n");
    let crlf = buffer.find("\r\n\r\n");
    match (lf, crlf) {
        (Some(a), Some(b)) => {
            if b < a {
                Some((b, 4))
            } else {
                Some((a, 2))
            }
        }
        (Some(a), None) => Some((a, 2)),
        (None, Some(b)) => Some((b, 4)),
        (None, None) => None,
    }
}

fn parse_block(block: &str) -> Option<String> {
    let mut data: Vec<&str> = Vec::new();
    for line in block.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with(':') {
            continue; // 注释
        }
        if let Some(rest) = line.strip_prefix("data:") {
            // 规范允许 "data:" 后跟一个可选空格
            data.push(rest.strip_prefix(' ').unwrap_or(rest));
        }
        // event:/id:/retry: 我们不使用
    }
    if data.is_empty() {
        return None;
    }
    let payload = data.join("\n");
    let trimmed = payload.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_complete_event() {
        let mut framer = SseFramer::new();
        assert_eq!(
            framer.push("data: {\"a\":1}\n\n"),
            vec!["{\"a\":1}".to_string()]
        );
        assert_eq!(framer.buffered_len(), 0);
    }

    #[test]
    fn partial_event_is_held_back() {
        let mut framer = SseFramer::new();
        // 半截：JSON 还没结束，也没有空行
        assert!(framer.push("data: {\"a\":").is_empty());
        assert!(framer.buffered_len() > 0, "未完成的应该留在缓冲区");
        // 补齐后才放行，且拼出来是完整 JSON
        assert_eq!(
            framer.push("1}\n\n"),
            vec!["{\"a\":1}".to_string()]
        );
    }

    #[test]
    fn multiple_events_in_one_chunk() {
        let mut framer = SseFramer::new();
        let payloads = framer.push("data: one\n\ndata: two\n\n");
        assert_eq!(payloads, vec!["one".to_string(), "two".to_string()]);
    }

    #[test]
    fn crlf_boundaries_work() {
        let mut framer = SseFramer::new();
        assert_eq!(
            framer.push("data: x\r\n\r\n"),
            vec!["x".to_string()],
            "应兼容 CRLF 分帧"
        );
    }

    #[test]
    fn comments_and_event_lines_are_ignored() {
        let mut framer = SseFramer::new();
        let payloads = framer.push(": keep-alive\nevent: delta\ndata: payload\n\n");
        assert_eq!(payloads, vec!["payload".to_string()]);
    }

    #[test]
    fn multi_line_data_is_joined() {
        let mut framer = SseFramer::new();
        assert_eq!(
            framer.push("data: line1\ndata: line2\n\n"),
            vec!["line1\nline2".to_string()]
        );
    }

    #[test]
    fn done_sentinel_passes_through() {
        let mut framer = SseFramer::new();
        assert_eq!(framer.push("data: [DONE]\n\n"), vec!["[DONE]".to_string()]);
    }

    #[test]
    fn finish_flushes_remaining() {
        let mut framer = SseFramer::new();
        assert!(framer.push("data: tail").is_empty());
        assert_eq!(framer.finish(), Some("tail".to_string()));
        assert_eq!(framer.buffered_len(), 0);
    }

    #[test]
    fn finish_on_empty_buffer_returns_none() {
        let mut framer = SseFramer::new();
        assert!(framer.push("data: x\n\n").len() == 1);
        assert_eq!(framer.finish(), None, "完整事件之后不应再有内容");
    }

    #[test]
    fn byte_by_byte_feeding_works() {
        // 模拟最坏情况：一次只来 1 个字符
        let raw = "data: {\"v\":\"hi\"}\n\ndata: {\"v\":\"!\"}\n\n";
        let mut framer = SseFramer::new();
        let mut collected = Vec::new();
        for ch in raw.chars() {
            collected.extend(framer.push(&ch.to_string()));
        }
        assert_eq!(
            collected,
            vec!["{\"v\":\"hi\"}".to_string(), "{\"v\":\"!\"}".to_string()]
        );
    }
}
