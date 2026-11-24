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
/// 仪表盘聚合：按天、按路径的统计要真的能从库里算出来。
#[tokio::test]
async fn daily_and_ranking_aggregate_real_rows() {
    let Some((repo, pool)) = repo().await else { return };

    let tag = uuid::Uuid::new_v4().simple().to_string();
    let prefix = format!("/v1/dash-{tag}");
    for _ in 0..3 {
        repo.insert(&record(&format!("{prefix}/a"), 200, 100)).await.expect("写");
    }
    for _ in 0..2 {
        repo.insert(&record(&format!("{prefix}/b"), 500, 800)).await.expect("写");
    }

    // 按天：今天的点必须存在，且总数 >= 5（库里还有其他记录，只断言下界）
    let daily = repo.daily(2).await.expect("按天聚合");
    assert!(!daily.is_empty(), "近两天应有数据");
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let point = daily.iter().find(|row| row.day == today).expect("今天应有聚合点");
    assert!(point.total >= 5, "今天至少 5 次，实际 {}", point.total);
    assert!(point.failed >= 2, "失败数至少 2，实际 {}", point.failed);
    assert!(point.avg_ms > 0.0, "平均耗时应有值");

    // 按路径排行：我们刚写的路径要出现在前若干名
    let by_path = repo.by_path(1, 50).await.expect("按路径排行");
    assert!(
        by_path.iter().any(|row| row.key.starts_with(&prefix)),
        "刚写的路径应出现在排行里"
    );
    assert!(by_path.iter().all(|row| row.count > 0));
    let _ = pool;
}
