//! 代理核封装：内嵌 `vendor/shoes`，对外提供启动/停止与 Clash 控制接口。
//!
//! 只在生图链路使用：调用方拿到本地 mixed 监听地址后，把它作为代理串交给 HTTP 客户端。
//! shoes 在 Windows 上启动需要大栈，因此运行体固定跑在 16MB 栈的线程里。

pub mod config;
pub mod control;
pub mod subscription;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// 内嵌的代理核（`vendor/shoes`）。
pub use shoes;

/// 运行体线程的栈大小。Windows 默认 1MB 会让 shoes 栈溢出。
const RUNNER_STACK_BYTES: usize = 16 * 1024 * 1024;

/// 一个内嵌运行的代理核实例。
pub struct Core {
    handle: Option<JoinHandle<i32>>,
    listen: SocketAddr,
}

impl Core {
    /// 用给定配置文件启动，等本地监听就绪后返回。
    ///
    /// `listen` 是配置里 mixed 监听器的地址，仅用于就绪探测。
    pub fn start(config_path: PathBuf, listen: SocketAddr) -> std::io::Result<Self> {
        let path = config_path.clone();
        let handle = std::thread::Builder::new()
            .name("proxy-core".to_string())
            .stack_size(RUNNER_STACK_BYTES)
            .spawn(move || {
                shoes::cli::run_config(&path.to_string_lossy(), 2)
            })?;
        let core = Self {
            handle: Some(handle),
            listen,
        };
        core.wait_ready(Duration::from_secs(15))?;
        Ok(core)
    }

    /// 本地 mixed 监听地址，形如 `http://127.0.0.1:PORT`。
    pub fn proxy_url(&self) -> String {
        format!("http://{}", self.listen)
    }

    fn wait_ready(&self, timeout: Duration) -> std::io::Result<()> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if std::net::TcpStream::connect_timeout(&self.listen, Duration::from_millis(200)).is_ok() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err(std::io::Error::other(format!("代理核未在 {timeout:?} 内就绪")))
    }

    /// 请求停机并等待运行体退出。
    pub fn stop(mut self) -> i32 {
        shoes::process::request_shutdown();
        self.handle
            .take()
            .and_then(|handle| handle.join().ok())
            .unwrap_or(-1)
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        if self.handle.is_some() {
            shoes::process::request_shutdown();
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_and_stops_in_process() {
        let dir = std::env::temp_dir().join(format!("gpt2apirs-core-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("临时目录");
        let path = dir.join("shoes.yaml");
        std::fs::write(
            &path,
            "- address: \"127.0.0.1:17899\"\n  protocol: { type: mixed }\n",
        )
        .expect("写配置");
        let core = Core::start(path.clone(), "127.0.0.1:17899".parse().unwrap()).expect("应能启动");
        assert_eq!(core.proxy_url(), "http://127.0.0.1:17899");
        let code = core.stop();
        assert_eq!(code, 0, "正常停机应返回 0");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
