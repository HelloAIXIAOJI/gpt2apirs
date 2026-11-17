//! 批量打包下载：把在线图片流式打成 tar.gz。
//!
//! 边读边写边发，不把所有图片读进内存；包是 tar.gz（PNG 压不动，压缩只多花 CPU）。

use std::io::Write;
use std::path::PathBuf;

use axum::body::{Body, Bytes};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::stream;
use tokio::sync::mpsc;

/// 发送缓冲块数：写满就阻塞打包线程，避免内存无限涨。
const BUFFERED_CHUNKS: usize = 8;

/// 把文件写进 channel 的 `Write` 适配器。
struct ChannelWriter {
    tx: mpsc::Sender<Vec<u8>>,
}

impl Write for ChannelWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.tx
            .blocking_send(buf.to_vec())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "接收端已关闭"))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// 流式打一个 tar.gz：`items` 是 (包内路径, 磁盘路径)。
pub fn tar_gz_stream(file_name: String, items: Vec<(String, PathBuf)>) -> Response {
    let (tx, rx) = mpsc::channel::<Vec<u8>>(BUFFERED_CHUNKS);
    let total = items.len();
    tokio::task::spawn_blocking(move || {
        let writer = ChannelWriter { tx };
        let encoder = flate2::write::GzEncoder::new(writer, flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        let mut written = 0usize;
        for (entry_name, path) in items {
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let mut file_header = tar::Header::new_gnu();
            file_header.set_size(bytes.len() as u64);
            file_header.set_mode(0o644);
            file_header.set_cksum();
            if builder
                .append_data(&mut file_header, PathBuf::from(entry_name), bytes.as_slice())
                .is_err()
            {
                break;
            }
            written += 1;
        }
        match builder.into_inner() {
            Ok(encoder) => {
                let _ = encoder.finish();
            }
            Err(error) => tracing::warn!(%error, "打包写入失败"),
        }
        tracing::info!(files = written, total, "打包下载完成");
    });

    let body = Body::from_stream(stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|chunk| {
            (Ok::<Bytes, std::io::Error>(Bytes::from(chunk)), rx)
        })
    }));
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/gzip")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{file_name}\""),
        )
        .body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}
