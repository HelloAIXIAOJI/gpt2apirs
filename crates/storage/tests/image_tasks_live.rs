//! 任务仓储的库往返（需要 DATABASE_URL，没配就跳过）。

use gpt2apirs_storage::repo::{ImageTaskRepo, NewImageTask, TaskImageRow};
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

fn database_url() -> Option<String> {
    match std::env::var("DATABASE_URL") {
        Ok(url) if !url.trim().is_empty() => Some(url),
        _ => None,
    }
}

#[tokio::test]
async fn insert_progress_finish_and_delete() {
    let Some(url) = database_url() else {
        eprintln!("跳过：未设置 DATABASE_URL");
        return;
    };
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("连库");
    let repo = ImageTaskRepo::new(pool.clone());
    let id = Uuid::new_v4();

    repo.insert(&NewImageTask {
        id,
        prompt: "仓储往返".to_string(),
        model: "image".to_string(),
        n: 1,
        response_format: "url".to_string(),
        output_format: Some("gif".to_string()),
        frame_count: Some(8),
        frame_duration_ms: Some(70),
        base_url: "http://localhost:8000".to_string(),
    })
    .await
    .expect("写库");

    assert!(repo.mark_running(id, "生成中").await.expect("置运行中"));
    // 再来一次应当失败：已经不是 queued
    assert!(!repo.mark_running(id, "生成中").await.expect("再置"));
    repo.set_progress(id, "落盘中").await.expect("改阶段");

    let images = vec![TaskImageRow {
        rel_path: "images/2026/11/17/x.png".to_string(),
        mime: "image/png".to_string(),
        width: 4,
        height: 3,
    }];
    repo.finish(id, true, &images, None, 1234, 0, 1650)
        .await
        .expect("结算");

    let rows = repo.list_recent(50).await.expect("列表");
    let row = rows.iter().find(|row| row.id == id).expect("应能查到");
    assert_eq!(row.status, "succeeded");
    assert_eq!(row.progress.as_deref(), Some("已完成"));
    assert_eq!(row.duration_ms, Some(1234));
    assert_eq!(row.total_tokens, Some(1650));
    let decoded: Vec<TaskImageRow> = serde_json::from_value(row.images.clone()).expect("解图");
    assert_eq!(decoded.len(), 1);

    assert!(repo.delete(id).await.expect("删"));
    assert!(repo
        .list_recent(50)
        .await
        .expect("再列表")
        .iter()
        .all(|row| row.id != id));
}

#[tokio::test]
async fn fail_unfinished_marks_orphans() {
    let Some(url) = database_url() else {
        eprintln!("跳过：未设置 DATABASE_URL");
        return;
    };
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("连库");
    let repo = ImageTaskRepo::new(pool.clone());
    let id = Uuid::new_v4();
    repo.insert(&NewImageTask {
        id,
        prompt: "中断任务".to_string(),
        model: "image".to_string(),
        n: 1,
        response_format: "url".to_string(),
        output_format: None,
        frame_count: None,
        frame_duration_ms: None,
        base_url: "http://localhost:8000".to_string(),
    })
    .await
    .expect("写库");
    repo.mark_running(id, "生成中").await.expect("置运行中");

    let affected = repo.fail_unfinished("测试：服务重启").await.expect("标失败");
    assert!(affected >= 1, "至少要把刚插的那条标掉");

    let rows = repo.list_recent(50).await.expect("列表");
    let row = rows.iter().find(|row| row.id == id).expect("应能查到");
    assert_eq!(row.status, "failed");
    assert_eq!(row.error.as_deref(), Some("测试：服务重启"));
    repo.delete(id).await.expect("清理");
}
