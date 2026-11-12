//! 数据库连通性探针。
//!
//! 复用 `gpt2apirs_core::BootstrapConfig` + `gpt2apirs_storage::pool`，
//! 不再自己实现 .env 查找与连接逻辑（避免两份实现漂移）。
//!
//! ```bash
//! cargo run -p gpt2apirs-storage --bin db-probe
//! ```
//!
//! 密码永不打印。

use anyhow::{Context, Result};
use gpt2apirs_core::{BootstrapConfig, env};
use gpt2apirs_storage::pool::{build_pool, current_database, health_check, pool_stats, server_version};

#[tokio::main]
async fn main() -> Result<()> {
    println!("可执行文件: {}", std::env::current_exe()?.display());

    match env::load_dotenv() {
        Ok(Some(path)) => println!("找到 .env: {}", path.display()),
        Ok(None) => println!(".env: 未找到（将依赖进程环境变量）"),
        Err(err) => println!(".env: 加载失败：{err}"),
    }

    let cfg = BootstrapConfig::from_lookup(&env::read_var).context("引导层配置加载失败")?;
    println!("DATABASE_URL: {}", cfg.redacted_database_url());
    println!("连接池: max={} acquire={:?}", cfg.pool_max_connections, cfg.pool_acquire_timeout);
    for warning in &cfg.warnings {
        println!("注意: {warning}");
    }

    let started = std::time::Instant::now();
    let pool = build_pool(&cfg).await?;
    println!("连接成功，耗时 {:?}", started.elapsed());

    health_check(&pool).await?;
    println!("健康检查: ok");
    println!("服务端版本: {}", server_version(&pool).await?);
    println!("当前数据库: {}", current_database(&pool).await?);

    let stats = pool_stats(&pool);
    println!("池状态: size={} idle={}", stats.size, stats.idle);

    let count: (i64,) = sqlx::query_as(
        "select count(*) from information_schema.tables where table_schema = 'public'",
    )
    .fetch_one(&pool)
    .await?;
    println!("public schema 表数量: {}", count.0);

    let tables: Vec<(String,)> = sqlx::query_as(
        "select table_name from information_schema.tables where table_schema='public' order by table_name limit 40",
    )
    .fetch_all(&pool)
    .await?;
    if tables.is_empty() {
        println!("（空库）");
    } else {
        println!("表清单:");
        for (name,) in tables {
            println!("  - {name}");
        }
    }

    pool.close().await;
    Ok(())
}
