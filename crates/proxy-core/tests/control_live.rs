//! 用真实节点验证控制接口：设 `SUBSCRIPTION_FILE` 指向本地订阅正文才跑。
//!
//! 自己生成配置（固定端口与口令）→ 启动代理核 → 列节点 → 测速 → 切换出口 → 停机。

use std::time::Duration;

use gpt2apirs_proxy_core::config::{self, Layout};
use gpt2apirs_proxy_core::{control::Control, subscription, Core};

const LISTEN_PORT: u16 = 17897;
const API_PORT: u16 = 19097;
const SECRET: &str = "control-live-secret";

#[test]
fn probes_and_switches_first_node() {
    let Ok(path) = std::env::var("SUBSCRIPTION_FILE") else {
        eprintln!("跳过：未设置 SUBSCRIPTION_FILE");
        return;
    };
    let text = std::fs::read_to_string(&path).expect("读取订阅");
    let parsed = subscription::parse(&text).expect("解析订阅");
    assert!(!parsed.nodes.is_empty(), "订阅里应有可用节点");

    let dir = std::env::temp_dir().join(format!("gpt2apirs-control-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("临时目录");
    let config_path = dir.join("shoes.yaml");
    let layout = Layout {
        listen_port: LISTEN_PORT,
        api_port: API_PORT,
        secret: SECRET.to_string(),
        group: "outbound".to_string(),
        selected: None,
    };
    std::fs::write(&config_path, config::render(&parsed.nodes, &layout)).expect("写配置");

    let core = Core::start(
        config_path,
        format!("127.0.0.1:{LISTEN_PORT}").parse().expect("地址"),
    )
    .expect("启动代理核");
    let control = Control::new(API_PORT, SECRET).expect("控制客户端");

    let runtime = tokio::runtime::Runtime::new().expect("tokio");
    runtime.block_on(async {
        let proxies = control.proxies().await.expect("列节点");
        assert!(proxies.iter().any(|entry| entry.name == "n0"), "应有 n0 链路");
        let (delay, error) = control
            .delay("n0", "https://www.gstatic.com/generate_204", 8000)
            .await;
        println!("n0 delay={delay:?} error={error:?}");
        assert!(delay.is_some() || error.is_some(), "测速必须给出结果或错误");
        control.select("outbound", "n0").await.expect("切换出口");
        let after = control.proxies().await.expect("再列一次");
        let group = after.iter().find(|entry| entry.name == "outbound").expect("组");
        assert_eq!(group.now.as_deref(), Some("n0"), "组应选中 n0");
    });

    let code = core.stop();
    assert_eq!(code, 0, "正常停机");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = Duration::from_secs(0);
}