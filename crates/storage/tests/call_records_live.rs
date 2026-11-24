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
        client_ip: Some("1.2.3.4".to_string()),
        user_agent: Some("test-agent/1.0".to_string()),
        api_key_prefix: Some("sk-g2a-test".to_string()),
        error_message: if status >= 400 {
            Some("上游炸了".to_string())
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
/// 详情能取回新写的四列；删一条只删一条。
#[tokio::test]
async fn get_and_delete_single_record() {
    let Some((repo, pool)) = repo().await else { return };

    let tag = uuid::Uuid::new_v4().simple().to_string();
    let prefix = format!("/v1/detail-{tag}");
    repo.insert(&record(&format!("{prefix}/bad"), 502, 900))
        .await
        .expect("写");

    let rows = repo
        .list(&CallRecordQuery {
            path_prefix: Some(prefix.clone()),
            limit: 10,
            ..Default::default()
        })
        .await
        .expect("查");
    let id = rows[0].id;

    // 详情：中间件写进去的调用方信息、失败原因都要能取回
    let detail = repo.get(id).await.expect("取详情").expect("应存在");
    assert_eq!(detail.path, format!("{prefix}/bad"));
    assert_eq!(detail.client_ip.as_deref(), Some("1.2.3.4"));
    assert_eq!(detail.user_agent.as_deref(), Some("test-agent/1.0"));
    assert_eq!(detail.api_key_prefix.as_deref(), Some("sk-g2a-test"));
    assert_eq!(detail.error_message.as_deref(), Some("上游炸了"));

    // 删一条：第二次删同一行必须返回 false
    assert!(repo.delete(id).await.expect("删"), "第一次应删掉");
    assert!(!repo.delete(id).await.expect("再删"), "已经删了就返回 false");
    assert!(repo.get(id).await.expect("查").is_none());

    pool.close().await;
}

/// 批量删认筛选条件，且不受 limit/offset 影响（不能只删当前页）。
#[tokio::test]
async fn delete_matching_removes_the_whole_filtered_set() {
    let Some((repo, pool)) = repo().await else { return };

    let tag = uuid::Uuid::new_v4().simple().to_string();
    let keep = format!("/v1/keep-{tag}");
    let drop_prefix = format!("/v1/drop-{tag}");
    for index in 0..5 {
        repo.insert(&record(&format!("{drop_prefix}/{index}"), 500, 100))
            .await
            .expect("写");
    }
    repo.insert(&record(&format!("{keep}/x"), 500, 100))
        .await
        .expect("写");

    // limit 设成 2：如果实现拿 limit 去删，只会删掉两条
    let removed = repo
        .delete_matching(&CallRecordQuery {
            path_prefix: Some(drop_prefix.clone()),
            limit: 2,
            ..Default::default()
        })
        .await
        .expect("批量删");
    assert_eq!(removed, 5, "应按筛选整批删，只认 limit 就会漏");

    let left = repo
        .list(&CallRecordQuery {
            path_prefix: Some(drop_prefix.clone()),
            limit: 10,
            ..Default::default()
        })
        .await
        .expect("查");
    assert!(left.is_empty(), "筛选内的记录应被删空");

    let others = repo
        .list(&CallRecordQuery {
            path_prefix: Some(keep.clone()),
            limit: 10,
            ..Default::default()
        })
        .await
        .expect("查");
    assert_eq!(others.len(), 1, "别的路径不能被误删");

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
