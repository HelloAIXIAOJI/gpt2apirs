//! Clash 控制接口客户端：列节点、切换全局出口、逐节点测速。
//!
//! 只连本地回环上的控制端口，口令由启动配置里的 secret 决定。

use serde::Deserialize;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
///
/// 只连本地回环的明文 HTTP，因此不借助带 TLS 指纹的客户端：
/// 那会把 `tls` 特性传染给所有引用本 crate 的构建。
pub struct Control {
    base: String,
    secret: String,
}

impl Control {
    /// 指向本地控制端口。
    pub fn new(api_port: u16, secret: &str) -> anyhow::Result<Self> {
        Ok(Self {
            base: format!("127.0.0.1:{api_port}"),
            secret: secret.to_string(),
        })
    }

    /// 发一个最小 HTTP/1.1 请求，返回 (状态码, 响应体)。
    async fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<String>,
    ) -> anyhow::Result<(u16, String)> {
        let mut stream = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::net::TcpStream::connect(&self.base),
        )
        .await
        .map_err(|_| anyhow::anyhow!("连接控制接口超时"))??;
        let body = body.unwrap_or_default();
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\n",
            self.base, self.secret
        ) + &if body.is_empty() {
            "Content-Length: 0\r\n\r\n".to_string()
        } else {
            format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
        };
        stream.write_all(request.as_bytes()).await?;
        let mut raw = Vec::new();
        tokio::time::timeout(Duration::from_secs(20), stream.read_to_end(&mut raw))
            .await
            .map_err(|_| anyhow::anyhow!("读取控制接口超时"))??;
        let text = String::from_utf8_lossy(&raw).to_string();
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_str(), ""));
        let status = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse::<u16>().ok())
            .ok_or_else(|| anyhow::anyhow!("控制接口响应行非法"))?;
        Ok((status, body.to_string()))
    }

    /// 列出全部记录。
    pub async fn proxies(&self) -> anyhow::Result<Vec<ProxyEntry>> {
        let response = self.call("GET", "/proxies", None).await?;
        if response.0 != 200 {
            anyhow::bail!("控制接口返回 {}：{}", response.0, response.1);
        }
        parse_proxies(&response.1)
    }

    /// 把组切到指定成员。
    pub async fn select(&self, group: &str, name: &str) -> anyhow::Result<()> {
        let body = serde_json::json!({ "name": name }).to_string();
        let path = format!("/proxies/{}", encode_component(group));
        let (status, text) = self.call("PUT", &path, Some(body)).await?;
        if !(200..300).contains(&status) {
            anyhow::bail!("切换出口返回 {status}：{text}");
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
        // 探测目标不要百分号编码：控制接口自己会做 form 解码，再编码一次会被解成
        // `https%3A%2F%2F...`，对方按 URL 解析就失败（实测返回 invalid delay test query）。
        let path = format!(
            "/proxies/{}/delay?url={}&timeout={}",
            encode_component(name),
            probe_url,
            timeout_ms
        );
        match self.call("GET", &path, None).await {
            Ok((200, body)) => {
                let value: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
                (value["delay"].as_u64(), None)
            }
            Ok((status, body)) => (None, Some(format!("{status} {body}"))),
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
