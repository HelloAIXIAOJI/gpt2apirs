//! 生成 shoes 运行配置：一个 selector 组装全部节点，一个 mixed 监听绑到该组。
//!
//! 只生成我们自己的最小配置，不使用订阅自带的策略组与规则——那些是给桌面客户端
//! 分流用的，这里只需要“全部流量走选中的那个节点”。

use crate::subscription::Node;

/// 生成参数。
#[derive(Debug, Clone)]
pub struct Layout {
    /// mixed 监听端口（HTTP + SOCKS5 同端口）。
    pub listen_port: u16,
    /// Clash 控制接口端口。
    pub api_port: u16,
    /// 控制接口口令。
    pub secret: String,
    /// selector 组名。
    pub group: String,
    /// 默认选中的节点名。
    pub selected: Option<String>,
}

/// 生成 YAML。
pub fn render(nodes: &[Node], layout: &Layout) -> String {
    let mut out = String::new();
    out.push_str(&format!("- clash_api_address: \"127.0.0.1:{}\"\n", layout.api_port));
    out.push_str(&format!("  secret: \"{}\"\n\n", layout.secret));

    out.push_str(&format!("- client_group: {}\n", layout.group));
    out.push_str("  type: selector\n");
    let default = layout
        .selected
        .as_deref()
        .filter(|name| nodes.iter().any(|node| node.name == *name))
        .or_else(|| nodes.first().map(|node| node.name.as_str()))
        .unwrap_or("DIRECT");
    out.push_str(&format!("  default: \"{default}\"\n"));
    out.push_str("  client_chains:\n");
    for node in nodes {
        out.push_str("    - chain:\n");
        out.push_str(&format!("        - name: \"{}\"\n", node.name));
        out.push_str(&format!("          address: \"{}:{}\"\n", node.server, node.port));
        for line in node.client_yaml.lines() {
            out.push_str("          ");
            out.push_str(line);
            out.push('\n');
        }
    }

    out.push_str(&format!("\n- name: egress\n  address: \"127.0.0.1:{}\"\n", layout.listen_port));
    out.push_str("  protocol: { type: mixed }\n");
    out.push_str("  rules:\n    - mask: \"0.0.0.0/0\"\n      action: allow\n");
    out.push_str(&format!("      client_chain: {}\n", layout.group));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subscription::{Node, NodeKind};

    fn node(name: &str, yaml: &str) -> Node {
        Node {
            name: name.to_string(),
            kind: NodeKind::Vless,
            server: "example.com".to_string(),
            port: 443,
            client_yaml: yaml.to_string(),
        }
    }

    #[test]
    fn renders_groups_and_listener() {
        let nodes = vec![
            node("a", "protocol:\n  type: vless\n  user_id: \"x\"\n"),
            node("b", "protocol:\n  type: vless\n  user_id: \"y\"\n"),
        ];
        let layout = Layout {
            listen_port: 17899,
            api_port: 19090,
            secret: "s3cret".to_string(),
            group: "outbound".to_string(),
            selected: Some("b".to_string()),
        };
        let yaml = render(&nodes, &layout);
        assert!(yaml.contains("client_group: outbound"));
        assert!(yaml.contains("default: \"b\""));
        assert_eq!(yaml.matches("    - chain:").count(), 2);
        assert!(yaml.contains("address: \"127.0.0.1:17899\""));
        assert!(yaml.contains("client_chain: outbound"));
    }

    #[test]
    fn falls_back_to_first_node_when_selection_missing() {
        let nodes = vec![node("only", "protocol:\n  type: vless\n  user_id: \"x\"\n")];
        let layout = Layout {
            listen_port: 1,
            api_port: 2,
            secret: "s".to_string(),
            group: "g".to_string(),
            selected: Some("does-not-exist".to_string()),
        };
        assert!(render(&nodes, &layout).contains("default: \"only\""));
    }
}
