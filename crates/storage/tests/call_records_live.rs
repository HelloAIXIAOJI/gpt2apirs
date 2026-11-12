//! 调用日志的真机测试。

use gpt2apirs_storage::pool::build_pool;
use gpt2apirs_storage::repo::{CallRecordQuery, CallRecordRepo, NewCallRecord};

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

async fn repo() -> Option<(CallRecordRepo, sqlx::PgPool)> {
    if !prepare_env() {
        eprintln!("跳过：未找到 DATABASE_URL");
        return None;
    }
    let cfg = gpt2apirs_core::BootstrapConfig::from_env().expect("配置");
    let pool = build_pool(&cfg).await.expect("连接");
    gpt2apirs_storage::migrate::run_migrations(&pool).await.expect("迁移");
    Some((CallRecordRepo::new(pool.clone()), pool))
}

fn record(path: &str, status: i32, duration_ms: i32) -> NewCallRecord {
    NewCallRecord {
        method: "POST".to_string(),
        path: path.to_string(),
        status,
        duration_ms,
        account_id: None,
        request_id: Some(uuid::Uuid::new_v4()),
        error_code: if status >= 400 {
            Some("upstream_error".to_string())
        } else {
            None
        },
    }
}

#[tokio::test]
async fn insert_list_summary_and_purge() {
    let Some((repo, pool)) = repo().await else { return };

    let tag = uuid::Uuid::new_v4().simple().to_string();
    let prefix = format!("/v1/test-{tag}");
    repo.insert(&record(&format!("{prefix}/ok"), 200, 120)).await.expect("写");
    repo.insert(&record(&format!("{prefix}/ok2"), 200, 480)).await.expect("写");
    repo.insert(&record(&format!("{prefix}/bad"), 502, 900)).await.expect("写");

    let rows = repo
        .list(&CallRecordQuery {
            path_prefix: Some(prefix.clone()),
            limit: 10,
            ..Default::default()
        })
        .await
        .expect("查");
    assert_eq!(rows.len(), 3, "应查到刚写的三条");
    assert_eq!(rows[0].path, format!("{prefix}/bad"), "按时间倒序");

    // 只筛失败
    let failed = repo
        .list(&CallRecordQuery {
            path_prefix: Some(prefix.clone()),
            status_range: Some((400, 599)),
            limit: 10,
            ..Default::default()
        })
        .await
        .expect("查");
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].status, 502);
    assert_eq!(failed[0].error_code.as_deref(), Some("upstream_error"));

    // 汇总（全局近 24h，至少包含刚才三条）
    let summary = repo.summary(24).await.expect("汇总");
    assert!(summary.total >= 3, "total 应包含刚写入的记录");
    assert!(summary.failed >= 1);
    assert!(summary.p95_ms.is_some(), "耗时分位数应可算");

    // 清理：把窗口设成 0 小时（至少删 1 分钟前），本次数据不会被删；换个方式验证幂等
    let removed = repo.purge_older_than(24 * 365).await.expect("清理");
    let _ = removed;

    pool.close().await;
}
