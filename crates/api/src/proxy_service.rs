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

/// 默认探测目标：空设置会让控制接口报 `invalid delay test query`，所以必须有兜底。
pub const DEFAULT_PROBE_URL: &str = "https://www.gstatic.com/generate_204";

/// 取探测目标：设置优先，空或没有 scheme 时回落到默认值。
pub fn probe_url_from(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        DEFAULT_PROBE_URL.to_string()
    }
}

/// 测速并发：节点多就多开几条，避免几十个节点串成几十秒。
pub fn probe_concurrency(node_count: usize) -> usize {
    (node_count / 4).clamp(6, 24)
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
    nodes: Vec<(String, subscription::Node)>,
    skipped: Vec<subscription::Skipped>,
    selected: Option<String>,
    last_error: Option<String>,
    last_fetch: Option<std::time::Instant>,
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
            .map(|(_, node)| NodeView {
                name: node.name.clone(),
                kind: node.kind.as_str().to_string(),
                server: node.server.clone(),
                port: node.port,
            })
            .collect()
    }

    /// 拉订阅、更新节点表；代理已在跑就用新节点重建，没跑就只缓存节点。
    ///
    /// 与「启动」分开：拉取失败不影响正在用的出口，也不阻止用已缓存节点启动。
    pub async fn refresh(
        self: &Arc<Self>,
        subscription_url: &str,
        selected: Option<&str>,
    ) -> anyhow::Result<StatusView> {
        let text = fetch_subscription(subscription_url).await?;
        let parsed = subscription::parse(&text).map_err(|error| anyhow::anyhow!(error))?;
        if parsed.nodes.is_empty() {
            anyhow::bail!("订阅里没有可用节点");
        }
        {
            let mut guard = self.lock();
            guard.nodes = parsed
                .nodes
                .into_iter()
                .enumerate()
                .map(|(index, node)| (format!("n{index}"), node))
                .collect();
            guard.skipped = parsed.skipped;
            guard.last_fetch = Some(std::time::Instant::now());
            guard.last_error = None;
        }
        if self.status().running {
            self.restart_with_cached(selected)?;
        }
        Ok(self.status())
    }

    /// 用已缓存的节点启动代理核；没有节点则报错，让调用方先去刷新订阅。
    pub fn start_cached(self: &Arc<Self>, selected: Option<&str>) -> anyhow::Result<StatusView> {
        if self.lock().nodes.is_empty() {
            anyhow::bail!("还没有节点：先刷新订阅，再用缓存启动");
        }
        self.restart_with_cached(selected)?;
        Ok(self.status())
    }

    /// 用当前节点表重建配置并重启代理核（不动节点表本身）。
    fn restart_with_cached(self: &Arc<Self>, selected: Option<&str>) -> anyhow::Result<()> {
        let nodes = {
            let guard = self.lock();
            guard
                .nodes
                .iter()
                .map(|(_, node)| node.clone())
                .collect::<Vec<_>>()
        };
        self.stop();

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
        std::fs::write(&path, config::render(&nodes, &layout))?;

        let listen = format!("127.0.0.1:{LISTEN_PORT}")
            .parse()
            .map_err(|error| anyhow::anyhow!("监听到地址非法：{error}"))?;
        let core = Core::start(path, listen)?;
        let control = Arc::new(control::Control::new(API_PORT, &secret()?)?);

        let mut guard = self.lock();
        guard.core = Some(core);
        guard.control = Some(control);
        guard.selected = selected
            .filter(|name| !name.trim().is_empty())
            .map(str::to_string);
        guard.last_error = None;
        Ok(())
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
        let (control, chain) = {
            let guard = self.lock();
            let control = guard
                .control
                .clone()
                .ok_or_else(|| anyhow::anyhow!("代理核未运行"))?;
            let chain = guard
                .nodes
                .iter()
                .find(|(_, node)| node.name == name)
                .map(|(chain, _)| chain.clone())
                .ok_or_else(|| anyhow::anyhow!("没有名为 {name} 的节点"))?;
            (control, chain)
        };
        control.select("outbound", &chain).await?;
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
        let (targets, display): (Vec<(String, String)>, ()) = {
            let guard = self.lock();
            let wanted: Vec<String> = match names {
                Some(list) if !list.is_empty() => list,
                _ => guard.nodes.iter().map(|(_, node)| node.name.clone()).collect(),
            };
            let mut targets = Vec::new();
            for name in wanted {
                if let Some((chain, _)) = guard.nodes.iter().find(|(_, node)| node.name == name) {
                    targets.push((chain.clone(), name));
                } else {
                    targets.push((name.clone(), name));
                }
            }
            (targets, ())
        };
        let _ = display;
        let control = match self.lock().control.clone() {
            Some(control) => control,
            None => {
                return targets
                    .iter()
                    .map(|(_, name)| ProbeView {
                        name: name.clone(),
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
        for (chain, shown) in targets {
            let control = control.clone();
            let semaphore = semaphore.clone();
            let probe_url = probe_url.clone();
            tasks.push(tokio::spawn(async move {
                let _permit = semaphore.acquire_owned().await.expect("信号量");
                let (delay_ms, error) = control.delay(&chain, &probe_url, timeout_ms).await;
                ProbeView {
                    name: shown,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_url_falls_back_when_empty_or_malformed() {
        assert_eq!(probe_url_from(""), DEFAULT_PROBE_URL);
        assert_eq!(probe_url_from("   "), DEFAULT_PROBE_URL);
        assert_eq!(probe_url_from("www.example.com"), DEFAULT_PROBE_URL);
        assert_eq!(
            probe_url_from("https://example.com/204"),
            "https://example.com/204"
        );
    }

    #[test]
    fn probe_concurrency_bounds() {
        assert_eq!(probe_concurrency(0), 6, "没有节点也至少 6");
        assert_eq!(probe_concurrency(8), 6);
        assert_eq!(probe_concurrency(44), 11);
        assert_eq!(probe_concurrency(400), 24, "上限 24");
    }

    #[test]
    fn start_cached_without_nodes_explains_what_to_do() {
        let dir = std::env::temp_dir().join(format!("gpt2apirs-proxy-{}", std::process::id()));
        let service = std::sync::Arc::new(ProxyService::new(dir));
        let error = service.start_cached(None).expect_err("没有节点应报错");
        let message = error.to_string();
        assert!(message.contains("先刷新订阅"), "{message}");
    }
}
