//! 连接池对**真实 PostgreSQL** 的集成测试。
//!
//! 需要 `.env`（与可执行文件同目录或仓库根）里有 `DATABASE_URL` 与 `ADMIN_KEY`。
//! 缺失时自动跳过，不会让 CI 挂掉。

use gpt2apirs_core::BootstrapConfig;
use gpt2apirs_storage::pool::{build_pool, current_database, health_check, pool_stats, server_version};

/// 尝试加载 .env；返回是否具备连接条件。
fn prepare_env() -> bool {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut candidates = vec![
        manifest.join("../../.env"),
        manifest.join("../../target/debug/.env"),
    ];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(".env"));
        }
    }
    for path in candidates {
        if path.is_file() {
            let _ = dotenvy::from_path(&path);
            break;
        }
    }
    std::env::var("DATABASE_URL").map(|v| !v.trim().is_empty()).unwrap_or(false)
}

fn config() -> BootstrapConfig {
    BootstrapConfig::from_env().expect("BootstrapConfig 应可加载")
}

#[tokio::test]
async fn connects_to_real_postgres() {
    if !prepare_env() {
        eprintln!("跳过：未找到 DATABASE_URL");
        return;
    }
    let cfg = config();
    let pool = build_pool(&cfg).await.expect("应能连接 PostgreSQL");

    health_check(&pool).await.expect("select 1 应成功");

    let version = server_version(&pool).await.expect("version() 应成功");
    assert!(
        version.contains("PostgreSQL"),
        "版本串应包含 PostgreSQL：{version}"
    );
    println!("服务端：{version}");

    let db = current_database(&pool).await.expect("current_database() 应成功");
    println!("当前数据库：{db}");
    assert!(!db.is_empty());

    // 连接池应至少有 1 条连接
    let stats = pool_stats(&pool);
    println!("池状态：size={} idle={}", stats.size, stats.idle);
    assert!(stats.size >= 1, "池里应至少有一条连接");

    pool.close().await;
}

#[tokio::test]
async fn pool_reuse_is_stable() {
    if !prepare_env() {
        eprintln!("跳过：未找到 DATABASE_URL");
        return;
    }
    let pool = build_pool(&config()).await.expect("应能连接");
    for _ in 0..20 {
        health_check(&pool).await.expect("重复查询应稳定");
    }
    pool.close().await;
}
