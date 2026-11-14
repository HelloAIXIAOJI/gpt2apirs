//! gpt2apirs 服务入口。
//!
//! 为什么日志在配置之后：日志级别来自配置；且配置阶段产生的告警要在日志就绪后补发。
//! 为什么迁移在起服务之前：宁可启动失败，也不要带着半旧表结构对外服务。

use std::sync::Arc;

use anyhow::{Context, Result};
use gpt2apirs_account::{
    AccountPool, AccountRepo, OAuthSessions, PoolConfig, ProgressStore, WatcherConfig, spawn_watcher,
};
use gpt2apirs_api::{AppState, build_router};
use gpt2apirs_core::{BootstrapConfig, LogFormat, RuntimeSettings, env, logging, paths};
use gpt2apirs_http::{Client, ClientOptions};
use gpt2apirs_storage::migrate::run_migrations;
use gpt2apirs_storage::pool::{build_pool, health_check};
use gpt2apirs_storage::repo::PgSettingsRepo;

fn main() -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let check_only = args.iter().any(|arg| arg == "--check");

    // ---- 1. .env ----
    match env::load_dotenv() {
        Ok(Some(path)) => eprintln!("[bootstrap] .env: {}", path.display()),
        Ok(None) => eprintln!("[bootstrap] .env: 未找到，使用进程环境变量"),
        Err(err) => eprintln!("[bootstrap] .env 加载失败：{err}"),
    }

    // ---- 2. 引导层配置 ----
    let config = Arc::new(
        BootstrapConfig::from_env().context("引导层配置加载失败")?,
    );

    // ---- 3. 日志（带脱敏）----
    let format = if cfg!(debug_assertions) {
        LogFormat::Pretty
    } else {
        LogFormat::Json
    };
    logging::init_logging(&config.rust_log, &config.log_levels, format)
        .context("日志初始化失败")?;

    tracing::info!(
        version = gpt2apirs_api::APP_VERSION,
        bind = %config.bind_address(),
        database = %config.redacted_database_url(),
        pool_max = config.pool_max_connections,
        "gpt2apirs 启动中"
    );
    for warning in &config.warnings {
        tracing::warn!("{warning}");
    }
    if let Some(upstream) = gpt2apirs_protocol::endpoints::base_url_override() {
        tracing::warn!(
            upstream,
            env = gpt2apirs_protocol::endpoints::BASE_URL_ENV,
            "上游站点被环境变量覆盖，所有出站请求都不会打到默认主机"
        );
    }

    // 数据目录（顺带创建，失败早报）
    match paths::data_dir() {
        Ok(dir) => tracing::info!(data_dir = %dir.display(), "数据目录"),
        Err(err) => tracing::warn!("数据目录解析失败：{err}"),
    }

    // ---- 4. 连接池 ----
    let pool = build_pool(&config).await.context("建立数据库连接池失败")?;
    health_check(&pool).await.context("数据库健康检查失败")?;
    tracing::info!("数据库已连接");

    // ---- 5. 迁移 ----
    run_migrations(&pool).await.context("数据库迁移失败")?;
    tracing::info!("数据库迁移已完成");

    // 登录口令与调用 key 分离：库里的 API key 才是调用凭据。
    // 老部署只有 AUTH_KEY，这里一次性迁成名为 legacy 的 key，避免升级即断流。
    let api_keys = gpt2apirs_storage::repo::ApiKeyStore::Postgres(
        gpt2apirs_storage::repo::ApiKeyRepo::new(pool.clone()),
    );
    match api_keys.import_legacy(&config.auth_key).await {
        Ok(true) => tracing::warn!(
            "已把旧的 AUTH_KEY 迁移为名为 legacy 的 API key；请到后台创建新 key 并吊销它"
        ),
        Ok(false) => tracing::info!("API key 已存在，跳过旧口令迁移"),
        Err(error) => tracing::warn!("迁移旧 AUTH_KEY 失败：{error}"),
    }

    // ---- 6. 运行层设置 ----
    let repo = PgSettingsRepo::new(pool.clone());
    let snapshot = repo.load().await.context("读取运行层设置失败")?;
    let setting_count = snapshot.data.len();
    let settings = Arc::new(RuntimeSettings::new(snapshot.data));
    tracing::info!(
        keys = setting_count,
        updated_at = ?snapshot.updated_at,
        "运行层设置已载入"
    );

    if check_only {
        tracing::info!("--check 模式：自检通过，退出");
        pool.close().await;
        return Ok(());
    }

    // ---- 7. 上游客户端 ----
    let client = Arc::new(
        Client::new(ClientOptions {
            emulation: Some(gpt2apirs_http::Emulation::Chrome110),
            proxy: None,
            timeout_secs: Some(180),
        })
        .context("构建上游 HTTP 客户端失败")?,
    );

    // ---- 8. 账号池 ----
    let account_repo = AccountRepo::new(pool.clone());
    let image_concurrency = settings.get_i64("image_account_concurrency").clamp(1, 3) as u32;
    let pool_config = PoolConfig {
        image_concurrency,
        auto_remove_invalid: false,
    };
    let accounts = Arc::new(
        AccountPool::new(account_repo, pool_config.clone())
            .with_http((*client).clone(), gpt2apirs_http::DEFAULT_USER_AGENT),
    );
    let loaded = accounts.reload().await.context("载入账号池快照失败")?;
    tracing::info!(accounts = loaded, concurrency = image_concurrency, "账号池已就绪");
    if loaded == 0 {
        tracing::warn!(
            "账号池为空：/v1/images/generations 会返回 503。\
             请用 POST /api/accounts 导入账号（GPT2APIRS_ACCESS_TOKEN 已停用）"
        );
    }

    let api_keys = Arc::new(api_keys);
    let progress = Arc::new(ProgressStore::new());
    let oauth = Arc::new(OAuthSessions::new());

    // ---- 9. 后台维护 ----
    let watcher_interval_minutes = settings.get_i64("refresh_account_interval_minute").max(1) as u64;
    let watcher = spawn_watcher(
        accounts.clone(),
        progress.clone(),
        WatcherConfig {
            interval_secs: watcher_interval_minutes * 60,
            ..Default::default()
        },
    );
    tracing::info!(interval_minutes = watcher_interval_minutes, "账号池维护任务已启动");

    // 保留期清理：让 log_retention_hours / image_retention_hours 真正生效
    let data_dir = paths::data_dir()?;
    gpt2apirs_cli::retention::spawn(settings.clone(), pool.clone(), data_dir);

    let state = Arc::new(AppState::new(
        config.clone(),
        settings,
        pool.clone(),
        client,
        accounts.clone(),
        progress,
        oauth,
        api_keys,
    ));
    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind(config.bind_address())
        .await
        .with_context(|| format!("绑定 {} 失败", config.bind_address()))?;
    let local = listener.local_addr().context("读取监听地址失败")?;
    tracing::info!("监听 http://{local}");

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("HTTP 服务异常退出")?;

    watcher.abort();
    tracing::info!("账号池维护任务已停止");

    tracing::info!("正在关闭数据库连接池");
    pool.close().await;
    tracing::info!("已退出");
    Ok(())
}

/// 等待 Ctrl-C 或 SIGTERM。
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %err, "监听 Ctrl-C 失败");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(err) => {
                tracing::error!(error = %err, "监听 SIGTERM 失败");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("收到 Ctrl-C，开始优雅关闭"),
        _ = terminate => tracing::info!("收到 SIGTERM，开始优雅关闭"),
    }
}
