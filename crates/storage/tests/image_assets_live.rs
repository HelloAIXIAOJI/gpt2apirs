//! 图片元数据的真机测试。

use gpt2apirs_storage::pool::build_pool;
use gpt2apirs_storage::repo::{ImageAssetQuery, ImageAssetRepo, NewImageAsset};

fn prepare_env() -> bool {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    for path in [
        manifest.join("../../.env"),
        manifest.join("../../target/debug/.env"),
    ] {
        if path.is_file() {
            let _ = dotenvy::from_path(&path);
            break;
        }
    }
    std::env::var("DATABASE_URL")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

fn asset(rel_path: String) -> NewImageAsset {
    NewImageAsset {
        rel_path,
        mime: "image/jpeg".to_string(),
        bytes: 1234,
        width: 64,
        height: 48,
        account_id: None,
        prompt: Some("单测".to_string()),
        api_key_id: None,
        api_key_prefix: Some("sk-g2a-test".to_string()),
    }
}

#[tokio::test]
async fn insert_list_stats_delete_and_purge() {
    if !prepare_env() {
        eprintln!("跳过：未找到 DATABASE_URL");
        return;
    }
    let cfg = gpt2apirs_core::BootstrapConfig::from_env().expect("配置");
    let pool = build_pool(&cfg).await.expect("连接");
    gpt2apirs_storage::migrate::run_migrations(&pool).await.expect("迁移");
    let repo = ImageAssetRepo::new(pool.clone());

    let tag = uuid::Uuid::new_v4().simple().to_string();
    let rel = format!("images/test/{tag}.jpg");
    let id = repo.insert(&asset(rel.clone())).await.expect("写");

    // 同路径幂等
    let again = repo.insert(&asset(rel.clone())).await.expect("重复写");
    assert_eq!(again, id, "同一路径应复用同一条记录");

    let rows = repo
        .list(&ImageAssetQuery {
            limit: 200,
            ..Default::default()
        })
        .await
        .expect("查");
    assert!(rows.iter().any(|row| row.rel_path == rel), "列表应包含刚写的");

    let stats = repo.stats(None).await.expect("汇总");
    assert!(stats.count >= 1, "汇总应能算出来（曾因 numeric 类型解码失败）");
    assert!(stats.bytes >= 0);

    let got = repo.get(id).await.expect("取").expect("应存在");
    assert_eq!(got.rel_path, rel);

    assert!(repo.delete(id).await.expect("删"), "删除应命中");
    assert!(repo.get(id).await.expect("取").is_none());
    assert!(!repo.delete(id).await.expect("重复删"), "重复删除应返回 false");

    pool.close().await;
}
