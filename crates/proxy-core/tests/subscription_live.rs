//! 拿真实订阅文件验证解析：设 `SUBSCRIPTION_FILE` 指向本地订阅正文才会跑。

use gpt2apirs_proxy_core::{config, subscription};

#[test]
fn parses_real_subscription_file() {
    let Ok(path) = std::env::var("SUBSCRIPTION_FILE") else {
        eprintln!("跳过：未设置 SUBSCRIPTION_FILE");
        return;
    };
    let text = std::fs::read_to_string(&path).expect("读取订阅文件");
    let parsed = subscription::parse(&text).expect("解析订阅");
    println!(
        "可用 {} 个，跳过 {} 个",
        parsed.nodes.len(),
        parsed.skipped.len()
    );
    for node in parsed.nodes.iter().take(3) {
        println!("  {} [{}]", node.name, node.kind.as_str());
    }
    for skip in parsed.skipped.iter().take(3) {
        println!("  跳过 {} [{}]：{}", skip.name, skip.kind, skip.reason);
    }
    let layout = config::Layout {
        listen_port: 17901,
        api_port: 19091,
        secret: "probe".to_string(),
        group: "outbound".to_string(),
        selected: None,
    };
    let yaml = config::render(&parsed.nodes, &layout);
    println!("生成的配置 {} 字节", yaml.len());
    assert!(!parsed.nodes.is_empty(), "真实订阅应有可用节点");
}
