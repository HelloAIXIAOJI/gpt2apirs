//! API key 仓储的真机测试。

use gpt2apirs_storage::pool::build_pool;
use gpt2apirs_storage::repo::ApiKeyRepo;

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

async fn repo() -> Option<(ApiKeyRepo, sqlx::PgPool)> {
    if !prepare_env() {
        eprintln!("跳过：未找到 DATABASE_URL");
        return None;
    }
    let cfg = gpt2apirs_core::BootstrapConfig::from_env().expect("配置");
    let pool = build_pool(&cfg).await.expect("连接");
    gpt2apirs_storage::migrate::run_migrations(&pool).await.expect("迁移");
    Some((ApiKeyRepo::new(pool.clone()), pool))
}

#[tokio::test]
async fn create_verify_revoke_round_trip() {
    let Some((repo, pool)) = repo().await else { return };

    let created = repo.create("单测用").await.expect("创建");
    assert!(created.plaintext.starts_with(gpt2apirs_core::api_key::KEY_PREFIX));
    assert_eq!(created.row.name, "单测用");
    assert!(created.row.revoked_at.is_none());

    // 明文能校验通过，并刷新 last_used_at
    let verified = repo.verify(&created.plaintext).await.expect("校验");
    let verified = verified.expect("应命中");
    assert_eq!(verified.id, created.row.id);
    assert!(verified.last_used_at.is_some(), "命中后应记录使用时间");

    // 库里不该出现明文
    let (leaked,): (i64,) = sqlx::query_as(
        r#"select count(*) from api_keys where key_sha256 = $1"#,
    )
    .bind(&created.plaintext)
    .fetch_one(&pool)
    .await
    .expect("查明文");
    assert_eq!(leaked, 0, "key_sha256 列不能等于明文");

    // 吊销后立刻失效
    assert!(repo.revoke(created.row.id).await.expect("吊销"), "应吊销成功");
    assert!(repo.verify(&created.plaintext).await.expect("校验").is_none());
    assert!(!repo.revoke(created.row.id).await.expect("重复吊销"), "重复吊销应返回 false");

    pool.close().await;
}

#[tokio::test]
async fn legacy_import_only_when_no_active_keys() {
    let Some((repo, pool)) = repo().await else { return };

    // 先保证至少有一把：直接建
    let created = repo.create("占位").await.expect("创建");
    assert!(
        !repo.import_legacy("legacy-key-123456").await.expect("导入"),
        "已有 key 时不该再导入 legacy"
    );

    repo.revoke(created.row.id).await.ok();
    pool.close().await;
}

#[tokio::test]
async fn wrong_shape_is_rejected_without_touching_db() {
    let Some((repo, pool)) = repo().await else { return };
    assert!(repo.verify("not-a-key").await.expect("校验").is_none());
    assert!(repo.verify("").await.expect("校验").is_none());
    pool.close().await;
}
