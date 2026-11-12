//! 迁移与设置仓储对**真实 PostgreSQL** 的集成测试。
//!
//! 需要 `.env` 里配置 `DATABASE_URL`，缺失时自动跳过。
//!
//!  这组测试会**真的建表**并写入 `system_settings`，
//! 跑完会把设置恢复成 `{}`。

use std::collections::HashMap;

use gpt2apirs_core::BootstrapConfig;
use gpt2apirs_core::settings::RuntimeSettings;
use gpt2apirs_storage::migrate::{applied_migrations, run_migrations};
use gpt2apirs_storage::pool::{build_pool, current_database};
use gpt2apirs_storage::repo::PgSettingsRepo;
use serde_json::{Value, json};
use sqlx::PgPool;

/// 三个测试共用 `system_settings` 的**同一行**，
/// cargo test 默认并行跑会互相覆盖。用互斥把它们串行化。
static DB_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn db_guard() -> std::sync::MutexGuard<'static, ()> {
    DB_GUARD.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

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

async fn setup() -> Option<PgPool> {
    if !prepare_env() {
        eprintln!("跳过：未找到 DATABASE_URL");
        return None;
    }
    let cfg = BootstrapConfig::from_env().expect("配置应可加载");
    let pool = build_pool(&cfg).await.expect("应能连接 PostgreSQL");
    Some(pool)
}

#[tokio::test]
async fn migrations_are_idempotent() {
    let Some(pool) = setup().await else { return };

    // 第一次
    run_migrations(&pool).await.expect("首次迁移应成功");
    // 第二次（幂等）
    run_migrations(&pool).await.expect("重复迁移应成功");

    let applied = applied_migrations(&pool).await.expect("应能读迁移记录");
    assert!(!applied.is_empty(), "应有迁移记录");
    println!("已应用迁移: {applied:?}");
    assert!(
        applied.iter().any(|(version, _)| *version == 1),
        "应有版本 1"
    );

    // 三张表都应存在
    let tables: Vec<(String,)> = sqlx::query_as(
        "select table_name from information_schema.tables \
         where table_schema = 'public' and table_name in ('system_settings','app_meta','call_records') \
         order by table_name",
    )
    .fetch_all(&pool)
    .await
    .expect("查询表失败");

    let names: Vec<String> = tables.into_iter().map(|(n,)| n).collect();
    println!("数据库: {}", current_database(&pool).await.unwrap_or_default());
    println!("已建表: {names:?}");
    assert_eq!(names, vec!["app_meta", "call_records", "system_settings"]);

    pool.close().await;
}

#[tokio::test]
async fn settings_repo_roundtrip() {
    let _guard = db_guard();
    let Some(pool) = setup().await else { return };
    run_migrations(&pool).await.expect("迁移应成功");

    let repo = PgSettingsRepo::new(pool.clone());

    // ---- 从空开始 ----
    repo.replace(&HashMap::new()).await.expect("清空应成功");
    let empty = repo.load().await.expect("读取应成功");
    assert!(empty.data.is_empty(), "初始应为空: {:?}", empty.data);

    // ---- 整体替换 ----
    let mut values: HashMap<String, Value> = HashMap::new();
    values.insert("image_poll_timeout_secs".to_string(), json!(120));
    values.insert("image_poll_interval_secs".to_string(), json!(2.5));
    let at1 = repo.replace(&values).await.expect("替换应成功");

    let loaded = repo.load().await.expect("读取应成功");
    assert_eq!(loaded.data.len(), 2);
    assert_eq!(loaded.data.get("image_poll_timeout_secs"), Some(&json!(120)));
    assert_eq!(loaded.data.get("image_poll_interval_secs"), Some(&json!(2.5)));
    assert_eq!(loaded.updated_at, Some(at1));

    // ---- 合并（保留未提到的键） ----
    let mut patch: HashMap<String, Value> = HashMap::new();
    patch.insert("image_settle_secs".to_string(), json!(7.5));
    let after_merge = repo.merge(&patch).await.expect("合并应成功");
    assert_eq!(after_merge.data.len(), 3);
    assert_eq!(
        after_merge.data.get("image_poll_timeout_secs"),
        Some(&json!(120)),
        "合并不应丢失已有键"
    );
    assert_eq!(after_merge.data.get("image_settle_secs"), Some(&json!(7.5)));

    // ---- 删除某键 ----
    let after_remove = repo.remove("image_settle_secs").await.expect("删除应成功");
    assert_eq!(after_remove.data.len(), 2);
    assert!(!after_remove.data.contains_key("image_settle_secs"));

    // ---- updated_at 应变化 ----
    let before = repo.last_updated().await.expect("应能读时间");
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let mut patch2: HashMap<String, Value> = HashMap::new();
    patch2.insert("log_retention_hours".to_string(), json!(48));
    repo.merge(&patch2).await.expect("合并应成功");
    let after = repo.last_updated().await.expect("应能读时间");
    assert!(after > before, "updated_at 应递增");

    // ---- 恢复到空 ----
    repo.replace(&HashMap::new()).await.expect("恢复应成功");

    pool.close().await;
}

/// 端到端：数据库  RuntimeSettings  按契约钳制。
#[tokio::test]
async fn runtime_settings_clamps_database_values() {
    let _guard = db_guard();
    let Some(pool) = setup().await else { return };
    run_migrations(&pool).await.expect("迁移应成功");
    let repo = PgSettingsRepo::new(pool.clone());

    // 故意写入越界值
    let mut values: HashMap<String, Value> = HashMap::new();
    values.insert("image_account_concurrency".to_string(), json!(999)); // 上限 3
    values.insert("image_poll_interval_secs".to_string(), json!(0.01)); // 下限 0.5
    values.insert("image_poll_timeout_secs".to_string(), json!("90")); // 字符串数字
    repo.replace(&values).await.expect("写入应成功");

    // 载入快照
    let snap = repo.load().await.expect("读取应成功");
    let settings = RuntimeSettings::new(snap.data);

    assert_eq!(settings.get_i64("image_account_concurrency"), 3, "应钳到上限");
    assert_eq!(settings.get_f64("image_poll_interval_secs"), 0.5, "应钳到下限");
    assert_eq!(settings.get_i64("image_poll_timeout_secs"), 90, "应接受字符串数字");

    // 数据库里存的仍是原始值（钳制发生在读取层，不污染存储）
    let raw = repo.load().await.expect("读取应成功");
    assert_eq!(raw.data.get("image_account_concurrency"), Some(&json!(999)));

    repo.replace(&HashMap::new()).await.expect("恢复应成功");
    pool.close().await;
}

/// 并发写不应丢失数据（咨询锁 + 行锁）。
#[tokio::test]
async fn concurrent_merges_do_not_lose_keys() {
    let _guard = db_guard();
    let Some(pool) = setup().await else { return };
    run_migrations(&pool).await.expect("迁移应成功");
    let repo = PgSettingsRepo::new(pool.clone());
    repo.replace(&HashMap::new()).await.expect("清空应成功");

    let mut handles = Vec::new();
    for index in 0..8 {
        let repo = repo.clone();
        handles.push(tokio::spawn(async move {
            let mut patch: HashMap<String, Value> = HashMap::new();
            patch.insert(format!("key_{index}"), json!(index));
            repo.merge(&patch).await.expect("并发合并应成功");
        }));
    }
    for handle in handles {
        handle.await.expect("任务不应 panic");
    }

    let loaded = repo.load().await.expect("读取应成功");
    assert_eq!(
        loaded.data.len(),
        8,
        "8 个并发合并应全部保留，实际: {:?}",
        loaded.data.keys().collect::<Vec<_>>()
    );

    repo.replace(&HashMap::new()).await.expect("恢复应成功");
    pool.close().await;
}
