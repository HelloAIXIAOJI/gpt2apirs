//! 订阅代理的运行管理：拉订阅、生成配置、起停代理核、切换出口、测速。
//!
//! 代理核是进程内的（见 `gpt2apirs-proxy-core`），这里只持有一个实例；
//! 出口地址写进 `AppState::egress`，由 `client_for` 统一接管上游流量。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpt2apirs_http::{Client, ClientOptions, Emulation};
use gpt2apirs_proxy_core::{config, control, subscription, Core};
use serde::Serialize;

/// 本地 mixed 监听端口（HTTP + SOCKS5）。
pub const LISTEN_PORT: u16 = 17899;
/// 本地 Clash 控制端口。
pub const API_PORT: u16 = 19090;

/// 一个节点的对外视图。
#[derive(Debug, Clone, Serialize)]
pub struct NodeView {
    /// 名称。
    pub name: String,
    /// 协议。
    pub kind: String,
    /// 服务器。
    pub server: String,
    /// 端口。
    pub port: u16,
}

/// 测试结果。
#[derive(Debug, Clone, Serialize)]
pub struct ProbeView {
    /// 名称。
    pub name: String,
    /// 延迟毫秒数；失败为 None。
    pub delay_ms: Option<u64>,
    /// 失败原因。
    pub error: Option<String>,
}

/// 运行状态。
#[derive(Debug, Clone, Serialize)]
pub struct StatusView {
    /// 代理核是否在跑。
    pub running: bool,
    /// 出口代理串。
    pub proxy_url: Option<String>,
    /// 当前选中的节点。
    pub selected: Option<String>,
    /// 节点数。
    pub node_count: usize,
    /// 被跳过的节点（协议不支持等）。
    pub skipped: Vec<String>,
    /// 最近一次错误。
    pub last_error: Option<String>,
}

#[derive(Default)]
struct Inner {
    core: Option<Core>,
    control: Option<Arc<control::Control>>,
    nodes: Vec<subscription::Node>,
    skipped: Vec<subscription::Skipped>,
    selected: Option<String>,
    last_error: Option<String>,
}

/// 订阅代理服务。
#[derive(Default)]
pub struct ProxyService {
    inner: Mutex<Inner>,
    data_dir: Mutex<PathBuf>,
}

impl ProxyService {
    /// 新建。
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            data_dir: Mutex::new(data_dir),
        }
    }

    /// 运行状态。
    pub fn status(&self) -> StatusView {
        let guard = self.lock();
        StatusView {
            running: guard.core.is_some(),
            proxy_url: guard.core.as_ref().map(|core| core.proxy_url()),
            selected: guard.selected.clone(),
            node_count: guard.nodes.len(),
            skipped: guard
                .skipped
                .iter()
                .map(|item| format!("{} [{}]：{}", item.name, item.kind, item.reason))
                .collect(),
            last_error: guard.last_error.clone(),
        }
    }

    /// 节点列表。
    pub fn nodes(&self) -> Vec<NodeView> {
        let guard = self.lock();
        guard
            .nodes
            .iter()
            .map(|node| NodeView {
                name: node.name.clone(),
                kind: node.kind.as_str().to_string(),
                server: node.server.clone(),
                port: node.port,
            })
            .collect()
    }

    /// 拉订阅并启动代理核；返回状态视图。
    pub async fn start(
        self: &Arc<Self>,
        subscription_url: &str,
        selected: Option<&str>,
    ) -> anyhow::Result<StatusView> {
        self.stop();
        let text = fetch_subscription(subscription_url).await?;
        let parsed = subscription::parse(&text).map_err(|error| anyhow::anyhow!(error))?;
        if parsed.nodes.is_empty() {
            anyhow::bail!("订阅里没有可用节点");
        }

        let dir = self.data_dir.lock().map(|guard| guard.clone()).unwrap_or_default();
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("shoes.yaml");
        let layout = config::Layout {
            listen_port: LISTEN_PORT,
            api_port: API_PORT,
            secret: secret()?,
            group: "outbound".to_string(),
            selected: selected.map(str::to_string),
        };
        std::fs::write(&path, config::render(&parsed.nodes, &layout))?;

        let listen = format!("127.0.0.1:{LISTEN_PORT}")
            .parse()
            .map_err(|error| anyhow::anyhow!("监听到地址非法：{error}"))?;
        let core = Core::start(path, listen)?;
        let control = Arc::new(control::Control::new(API_PORT, &secret()?)?);

        {
            let mut guard = self.lock();
            guard.core = Some(core);
            guard.control = Some(control);
            guard.nodes = parsed.nodes;
            guard.skipped = parsed.skipped;
            guard.selected = selected
                .filter(|name| !name.trim().is_empty())
                .map(str::to_string);
            guard.last_error = None;
        }
        Ok(self.status())
    }

    /// 停止代理核。
    pub fn stop(&self) {
        let mut guard = self.lock();
        if let Some(core) = guard.core.take() {
            let _ = core.stop();
        }
        guard.control = None;
    }

    /// 切换出口节点。
    pub async fn select(&self, name: &str) -> anyhow::Result<()> {
        let control = self
            .lock()
            .control
            .clone()
            .ok_or_else(|| anyhow::anyhow!("代理核未运行"))?;
        control.select("outbound", name).await?;
        self.lock().selected = Some(name.to_string());
        Ok(())
    }

    /// 批量测速。
    pub async fn probe(
        &self,
        names: Option<Vec<String>>,
        probe_url: &str,
        timeout_ms: u64,
        concurrency: usize,
    ) -> Vec<ProbeView> {
        let targets: Vec<String> = match names {
            Some(list) if !list.is_empty() => list,
            _ => self.lock().nodes.iter().map(|node| node.name.clone()).collect(),
        };
        let control = match self.lock().control.clone() {
            Some(control) => control,
            None => {
                return targets
                    .into_iter()
                    .map(|name| ProbeView {
                        name,
                        delay_ms: None,
                        error: Some("代理核未运行".to_string()),
                    })
                    .collect();
            }
        };

        let limit = concurrency.max(1);
        let semaphore = Arc::new(tokio::sync::Semaphore::new(limit));
        let probe_url = probe_url.to_string();
        let mut tasks = Vec::new();
        for name in targets {
            let control = control.clone();
            let semaphore = semaphore.clone();
            let probe_url = probe_url.clone();
            tasks.push(tokio::spawn(async move {
                let _permit = semaphore.acquire_owned().await.expect("信号量");
                let (delay_ms, error) = control.delay(&name, &probe_url, timeout_ms).await;
                ProbeView {
                    name,
                    delay_ms,
                    error,
                }
            }));
        }
        let mut out = Vec::with_capacity(tasks.len());
        for task in tasks {
            if let Ok(view) = task.await {
                out.push(view);
            }
        }
        out.sort_by(|a, b| match (a.delay_ms, b.delay_ms) {
            (Some(left), Some(right)) => left.cmp(&right),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.name.cmp(&b.name),
        });
        out
    }


    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// 控制接口口令：进程级随机值，写在环境里没必要，重启即换。
fn secret() -> anyhow::Result<String> {
    use std::sync::OnceLock;
    static SECRET: OnceLock<String> = OnceLock::new();
    Ok(SECRET.get_or_init(|| uuid::Uuid::new_v4().to_string()).clone())
}

/// 拉订阅：Clash UA 才会返回 YAML。
async fn fetch_subscription(url: &str) -> anyhow::Result<String> {
    let client = Client::new(ClientOptions {
        emulation: Some(Emulation::Chrome110),
        proxy: None,
        timeout_secs: Some(30),
    })?;
    let headers = vec![("user-agent", "clash-verge/v2.0.0".to_string())];
    let response = client
        .get_text(url, &headers, Some(Duration::from_secs(30)))
        .await
        .map_err(|error| anyhow::anyhow!("拉取订阅失败：{error}"))?;
    if response.status != 200 {
        anyhow::bail!("订阅返回 {}：{}", response.status, response.body);
    }
    Ok(response.body)
}
