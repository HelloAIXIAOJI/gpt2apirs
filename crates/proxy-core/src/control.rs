//! Clash 控制接口客户端：列节点、切换全局出口、逐节点测速。
//!
//! 只连本地回环上的控制端口，口令由启动配置里的 secret 决定。

use gpt2apirs_http::{Client, ClientOptions, Emulation};
use serde::Deserialize;
use std::time::Duration;

/// 控制接口的一条记录（可能是节点，也可能是策略组）。
#[derive(Debug, Clone, Deserialize)]
pub struct ProxyEntry {
    /// 名称。
    pub name: String,
    /// 类型，例如 `Vless` / `Hysteria2` / `Selector`。
    #[serde(rename = "type")]
    pub kind: String,
    /// 是否健康。
    #[serde(default)]
    pub alive: bool,
    /// 组的成员。
    #[serde(default)]
    pub all: Vec<String>,
    /// 组当前选中的成员。
    #[serde(default)]
    pub now: Option<String>,
}

impl ProxyEntry {
    /// 是否是策略组。
    pub fn is_group(&self) -> bool {
        !self.all.is_empty()
    }
}

#[derive(Debug, Deserialize)]
struct ProxiesBody {
    proxies: std::collections::BTreeMap<String, ProxyEntry>,
}

/// 控制接口客户端。
pub struct Control {
    base: String,
    secret: String,
    client: Client,
}

impl Control {
    /// 指向本地控制端口。
    pub fn new(api_port: u16, secret: &str) -> anyhow::Result<Self> {
        let client = Client::new(ClientOptions {
            emulation: Some(Emulation::Chrome110),
            proxy: None,
            timeout_secs: Some(10),
        })?;
        Ok(Self {
            base: format!("http://127.0.0.1:{api_port}"),
            secret: secret.to_string(),
            client,
        })
    }

    fn auth(&self) -> Vec<(&'static str, String)> {
        vec![("authorization", format!("Bearer {}", self.secret))]
    }

    /// 列出全部记录。
    pub async fn proxies(&self) -> anyhow::Result<Vec<ProxyEntry>> {
        let response = self
            .client
            .get_text(&format!("{}/proxies", self.base), &self.auth(), Some(Duration::from_secs(10)))
            .await
            .map_err(|error| anyhow::anyhow!("读取控制接口失败：{error}"))?;
        if response.status != 200 {
            anyhow::bail!("控制接口返回 {}：{}", response.status, response.body);
        }
        parse_proxies(&response.body)
    }

    /// 把组切到指定成员。
    pub async fn select(&self, group: &str, name: &str) -> anyhow::Result<()> {
        let body = serde_json::json!({ "name": name });
        let response = self
            .client
            .put_bytes(
                &format!("{}/proxies/{}", self.base, encode_component(group)),
                &self.auth(),
                serde_json::to_vec(&body)?,
                Some(Duration::from_secs(10)),
            )
            .await
            .map_err(|error| anyhow::anyhow!("切换出口失败：{error}"))?;
        if !(200..300).contains(&response.status) {
            anyhow::bail!("切换出口返回 {}：{}", response.status, response.body);
        }
        Ok(())
    }

    /// 测一个节点的延迟（毫秒）。失败返回 `None`，错误原因放第二项。
    pub async fn delay(
        &self,
        name: &str,
        probe_url: &str,
        timeout_ms: u64,
    ) -> (Option<u64>, Option<String>) {
        let url = format!(
            "{}/proxies/{}/delay?url={}&timeout={}",
            self.base,
            encode_component(name),
            encode_component(probe_url),
            timeout_ms
        );
        let response = self
            .client
            .get_text(&url, &self.auth(), Some(Duration::from_millis(timeout_ms + 5_000)))
            .await;
        match response {
            Ok(response) if response.status == 200 => {
                let value: serde_json::Value = serde_json::from_str(&response.body).unwrap_or_default();
                (value["delay"].as_u64(), None)
            }
            Ok(response) => (None, Some(format!("{} {}", response.status, response.body))),
            Err(error) => (None, Some(format!("{error}"))),
        }
    }
}

/// 解析 `/proxies` 响应。
pub fn parse_proxies(body: &str) -> anyhow::Result<Vec<ProxyEntry>> {
    let parsed: ProxiesBody = serde_json::from_str(body)?;
    Ok(parsed.proxies.into_values().collect())
}

/// 路径/查询参数的百分号编码。
///
/// 节点名里常有中文、emoji 和空格，拼 URL 前必须编码。
pub fn encode_component(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proxies_payload() {
        let body = r#"{"proxies":{"DIRECT":{"name":"DIRECT","type":"Direct","alive":true},
            "outbound":{"name":"outbound","type":"Selector","all":["a","b"],"now":"a","alive":true},
            "a":{"name":"a","type":"Vless","alive":true}}}"#;
        let list = parse_proxies(body).expect("应能解析");
        assert_eq!(list.len(), 3);
        let group = list.iter().find(|entry| entry.name == "outbound").expect("组");
        assert!(group.is_group());
        assert_eq!(group.now.as_deref(), Some("a"));
        assert_eq!(group.all, vec!["a", "b"]);
    }

    #[test]
    fn encodes_non_ascii_names() {
        assert_eq!(encode_component("a b"), "a%20b");
        assert_eq!(encode_component("日本"), "%E6%97%A5%E6%9C%AC");
        assert_eq!(encode_component("https://x/y?z=1"), "https%3A%2F%2Fx%2Fy%3Fz%3D1");
    }
}
