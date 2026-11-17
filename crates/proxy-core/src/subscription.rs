//! 订阅解析：把 Clash 风格的 YAML 订阅转成节点列表。
//!
//! 只认我们能把参数映射到 shoes 客户端的那几种协议；映射不了的节点会被跳过，
//! 跳过原因随节点一起返回，管理面直接显示，避免“订阅里有 N 个节点但只出现 M 个”的困惑。

use yaml_rust2::{Yaml, YamlLoader};

/// 支持的节点协议。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    /// hysteria2。
    Hysteria2,
    /// VLESS（可带 TLS / REALITY）。
    Vless,
    /// Trojan。
    Trojan,
}

impl NodeKind {
    /// 协议名。
    pub fn as_str(self) -> &'static str {
        match self {
            NodeKind::Hysteria2 => "hysteria2",
            NodeKind::Vless => "vless",
            NodeKind::Trojan => "trojan",
        }
    }
}

/// 一个可用的节点。
#[derive(Debug, Clone)]
pub struct Node {
    /// 显示名。
    pub name: String,
    /// 协议。
    pub kind: NodeKind,
    /// 服务器地址。
    pub server: String,
    /// 端口。
    pub port: u16,
    /// 已经生成好的 shoes 客户端配置片段（YAML 缩进由调用方处理）。
    pub client_yaml: String,
}

/// 一个被跳过的节点。
#[derive(Debug, Clone)]
pub struct Skipped {
    /// 显示名。
    pub name: String,
    /// 协议。
    pub kind: String,
    /// 跳过原因。
    pub reason: String,
}

/// 解析结果。
#[derive(Debug, Default)]
pub struct Parsed {
    /// 可用节点。
    pub nodes: Vec<Node>,
    /// 被跳过的节点。
    pub skipped: Vec<Skipped>,
}

/// 解析订阅正文。
pub fn parse(text: &str) -> Result<Parsed, String> {
    let docs = YamlLoader::load_from_str(text).map_err(|error| format!("订阅不是合法 YAML：{error}"))?;
    let doc = docs.first().ok_or_else(|| "订阅为空".to_string())?;
    let list = match doc["proxies"].as_vec() {
        Some(list) => list,
        None => return Err("订阅里没有 proxies 列表".to_string()),
    };

    let mut parsed = Parsed::default();
    for item in list {
        let name = item["name"].as_str().unwrap_or("").trim().to_string();
        let kind = item["type"].as_str().unwrap_or("").trim().to_string();
        if name.is_empty() {
            continue;
        }
        if is_info_node(&name) {
            parsed.skipped.push(Skipped {
                name,
                kind,
                reason: "订阅的流量信息条目，不是代理节点".to_string(),
            });
            continue;
        }
        match build(item) {
            Ok(node) => parsed.nodes.push(node),
            Err(reason) => parsed.skipped.push(Skipped { name, kind, reason }),
        }
    }
    Ok(parsed)
}

/// 订阅里常混入「剩余流量 / 到期时间」这类伪节点，它们不是真代理。
fn is_info_node(name: &str) -> bool {
    const MARKERS: &[&str] = &["剩余流量", "距离下次重置", "套餐到期", "官网", "客服", "重置剩余"];
    MARKERS.iter().any(|marker| name.contains(marker))
}

fn build(item: &Yaml) -> Result<Node, String> {
    let name = item["name"].as_str().unwrap_or("").trim().to_string();
    let kind_text = item["type"].as_str().unwrap_or("").trim().to_ascii_lowercase();
    let server = item["server"].as_str().unwrap_or("").trim().to_string();
    let port = item["port"].as_i64().unwrap_or(0);
    if server.is_empty() || !(1..=65535).contains(&port) {
        return Err("缺少 server 或 port".to_string());
    }
    let port = port as u16;
    let sni = item["sni"].as_str().or_else(|| item["servername"].as_str()).unwrap_or("").trim().to_string();
    let skip_verify = item["skip-cert-verify"].as_bool().unwrap_or(false);

    let (kind, client_yaml) = match kind_text.as_str() {
        "hysteria2" | "hy2" => {
            let password = item["password"].as_str().unwrap_or("").trim().to_string();
            if password.is_empty() {
                return Err("缺少 password".to_string());
            }
            let mut yaml = String::new();
            if skip_verify || !sni.is_empty() {
                yaml.push_str("quic_settings:\n");
                if skip_verify {
                    yaml.push_str("  verify: false\n");
                }
                if !sni.is_empty() {
                    yaml.push_str(&format!("  sni_hostname: \"{sni}\"\n"));
                }
            }
            yaml.push_str("protocol:\n  type: hy2\n");
            yaml.push_str(&format!("  password: \"{password}\"\n"));
            (NodeKind::Hysteria2, yaml)
        }
        "vless" => {
            let uuid = item["uuid"].as_str().unwrap_or("").trim().to_string();
            if uuid.is_empty() {
                return Err("缺少 uuid".to_string());
            }
            let reality = &item["reality-opts"];
            let public_key = reality["public-key"].as_str().unwrap_or("").trim().to_string();
            let short_id = reality["short-id"].as_str().unwrap_or("").trim().to_string();
            let flow = item["flow"].as_str().unwrap_or("").trim().to_string();
            let vision = flow.contains("vision");
            let tls_on = item["tls"].as_bool().unwrap_or(false) || !public_key.is_empty();
            if tls_on && !sni.is_empty() {
                // REALITY 用 public_key；普通 TLS 只校验证书
            }
            let mut yaml = String::new();
            if !public_key.is_empty() {
                yaml.push_str("protocol:\n  type: reality\n");
                yaml.push_str(&format!("  public_key: \"{public_key}\"\n"));
                yaml.push_str(&format!("  short_id: \"{short_id}\"\n"));
                if !sni.is_empty() {
                    yaml.push_str(&format!("  sni_hostname: \"{sni}\"\n"));
                }
                if vision {
                    yaml.push_str("  vision: true\n");
                }
                yaml.push_str("  protocol:\n    type: vless\n");
                yaml.push_str(&format!("    user_id: \"{uuid}\"\n"));
            } else if tls_on {
                yaml.push_str("protocol:\n  type: tls\n");
                if skip_verify {
                    yaml.push_str("  verify: false\n");
                }
                if !sni.is_empty() {
                    yaml.push_str(&format!("  sni_hostname: \"{sni}\"\n"));
                }
                yaml.push_str("  protocol:\n    type: vless\n");
                yaml.push_str(&format!("    user_id: \"{uuid}\"\n"));
            } else {
                yaml.push_str("protocol:\n  type: vless\n");
                yaml.push_str(&format!("  user_id: \"{uuid}\"\n"));
            }
            (NodeKind::Vless, yaml)
        }
        "trojan" => {
            let password = item["password"].as_str().unwrap_or("").trim().to_string();
            if password.is_empty() {
                return Err("缺少 password".to_string());
            }
            let mut yaml = String::new();
            yaml.push_str("protocol:\n  type: tls\n");
            if skip_verify {
                yaml.push_str("  verify: false\n");
            }
            if !sni.is_empty() {
                yaml.push_str(&format!("  sni_hostname: \"{sni}\"\n"));
            }
            yaml.push_str("  protocol:\n    type: trojan\n");
            yaml.push_str(&format!("    password: \"{password}\"\n"));
            (NodeKind::Trojan, yaml)
        }
        other => return Err(format!("暂不支持的协议：{other}")),
    };

    Ok(Node {
        name,
        kind,
        server,
        port,
        client_yaml,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
proxies:
  - { name: 'hy2-a', type: hysteria2, server: a.example.com, port: 443, password: pw1, sni: a.example.com, skip-cert-verify: true }
  - { name: 'vless-r', type: vless, server: b.example.com, port: 443, uuid: 11111111-2222-3333-4444-555555555555, tls: true, servername: b.example.com, flow: xtls-rprx-vision, reality-opts: { public-key: PK, short-id: abcd } }
  - { name: 'ss-x', type: ss, server: c.example.com, port: 8388, cipher: aes-256-gcm, password: pw2 }
"#;

    #[test]
    fn parses_supported_and_skips_unknown() {
        let parsed = parse(SAMPLE).expect("应能解析");
        assert_eq!(parsed.nodes.len(), 2, "两个可映射的节点");
        assert_eq!(parsed.skipped.len(), 1, "ss 暂不支持");
        assert!(parsed.skipped[0].reason.contains("暂不支持"));
        assert_eq!(parsed.nodes[0].kind, NodeKind::Hysteria2);
        assert!(parsed.nodes[0].client_yaml.contains("verify: false"));
        assert_eq!(parsed.nodes[1].kind, NodeKind::Vless);
        assert!(parsed.nodes[1].client_yaml.contains("type: reality"));
        assert!(parsed.nodes[1].client_yaml.contains("vision: true"));
    }

    #[test]
    fn rejects_non_yaml() {
        assert!(parse("not a yaml: [").is_err());
    }
}
