//! 管理后台相关表的真机测试：迁移后表与列都在。

use gpt2apirs_storage::pool::build_pool;

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

#[tokio::test]
async fn migrations_apply_and_tables_exist() {
    if !prepare_env() {
        eprintln!("跳过：未找到 DATABASE_URL");
        return;
    }
    let cfg = gpt2apirs_core::BootstrapConfig::from_env().expect("配置");
    let pool = build_pool(&cfg).await.expect("连接");
    gpt2apirs_storage::migrate::run_migrations(&pool).await.expect("迁移应成功");
    let applied = gpt2apirs_storage::migrate::applied_migrations(&pool)
        .await
        .expect("查迁移");
    println!("已应用迁移: {applied:?}");

    let tables: Vec<(String,)> = sqlx::query_as(
        r#"select table_name from information_schema.tables
           where table_schema = 'public'
             and table_name in ('api_keys','image_assets','call_records')"#,
    )
    .fetch_all(&pool)
    .await
    .expect("查表");
    let names: Vec<&str> = tables.iter().map(|(name,)| name.as_str()).collect();
    for expected in ["api_keys", "image_assets", "call_records"] {
        assert!(names.contains(&expected), "缺少表 {expected}，实际: {names:?}");
    }

    let account_id_type: (String,) = sqlx::query_as(
        r#"select data_type from information_schema.columns
           where table_name = 'call_records' and column_name = 'account_id'"#,
    )
    .fetch_one(&pool)
    .await
    .expect("查列");
    assert_eq!(account_id_type.0, "uuid", "call_records.account_id 应为 uuid");

    pool.close().await;
}
