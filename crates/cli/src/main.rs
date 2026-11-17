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
    // 账号池按「账号代理 → 全局出口 → 直连」取客户端，出口换节点后无需重建。
    let egress = Arc::new(gpt2apirs_api::state::Egress::default());
    let clients = Arc::new(gpt2apirs_api::state::ClientFactory::new(
        client.clone(),
        egress.clone(),
    ));
    let resolver: gpt2apirs_account::ClientResolver = {
        let clients = clients.clone();
        Arc::new(move |proxy: &str| clients.get(proxy))
    };
    let accounts = Arc::new(
        AccountPool::new(account_repo, pool_config.clone())
            .with_resolver(resolver, gpt2apirs_http::DEFAULT_USER_AGENT),
    );
    let loaded = accounts.reload().await.context("载入账号池快照失败")?;
    tracing::info!(accounts = loaded, concurrency = image_concurrency, "账号池已就绪");
    if loaded == 0 {
        tracing::warn!(
            "账号池为空：/v1/images/generations 会返回 503。\
             请用 POST /api/accounts 导入账号"
        );
    }

    let api_keys = Arc::new(gpt2apirs_storage::repo::ApiKeyStore::Postgres(
        gpt2apirs_storage::repo::ApiKeyRepo::new(pool.clone()),
    ));
    if api_keys.active_count().await.unwrap_or(0) == 0 {
        tracing::warn!("库里还没有 API key：/v1/* 会全部 401，请登录后台在「密钥」页创建一把");
    }
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

    // 状态与账号池共用同一套出口：账号页的测试/同步/刷新才会走代理。
    let state = Arc::new(AppState::with_egress(
        config.clone(),
        settings,
        pool.clone(),
        client,
        clients,
        egress,
        accounts.clone(),
        progress,
        oauth,
        api_keys,
    ));
    // 容量阈值自动归档（阈值 0 时后台什么也不做）。
    gpt2apirs_cli::auto_archive::spawn(state.clone());

    // 异步任务：上次进程留下的未完成任务标失败，最近历史灌回内存供页面查看。
    {
        let repo = gpt2apirs_storage::repo::ImageTaskRepo::new(state.db.clone());
        match repo.fail_unfinished("服务重启，任务中断").await {
            Ok(0) => {}
            Ok(count) => tracing::warn!(count, "启动时把中断的任务标为失败"),
            Err(error) => tracing::warn!(%error, "启动恢复任务状态失败"),
        }
        match repo.list_recent(200).await {
            Ok(rows) => {
                for row in &rows {
                    state.image_tasks.hydrate(gpt2apirs_api::image_tasks::task_from_row(row));
                }
                tracing::info!(count = rows.len(), "已载入历史任务");
            }
            Err(error) => tracing::warn!(%error, "载入历史任务失败"),
        }
    }

    // 订阅代理：设置里启用过就按保存的地址起一次；失败只记日志，不影响主服务。
    if let Err(error) = gpt2apirs_api::routes::proxy::start_from_settings(&state).await {
        tracing::warn!(%error, "订阅代理启动失败");
    }
    // 自动刷新：间隔来自运行设置，最小 5 分钟（服务商对高频拉取会 403）。
    let refresh_minutes = state.settings.get_i64("proxy_auto_refresh_minutes").max(0) as u64;
    if refresh_minutes >= 5 {
        let task_state = state.clone();
        tokio::spawn(async move {
            let mut ticker =
                tokio::time::interval(std::time::Duration::from_secs(refresh_minutes * 60));
            ticker.tick().await;
            loop {
                ticker.tick().await;
                if task_state.settings.get_i64("proxy_enabled") != 1 {
                    continue;
                }
                let url = task_state.settings.get_string("proxy_subscription_url");
                let selected = task_state.settings.get_string("proxy_selected_node");
                let selected = if selected.trim().is_empty() {
                    None
                } else {
                    Some(selected.as_str())
                };
                // 定时任务只刷新订阅；出口由用户手动启动或启动时按设置恢复。
                match task_state.proxy_service.refresh(&url, selected).await {
                    Ok(status) => tracing::info!(nodes = status.node_count, "订阅已自动刷新"),
                    Err(error) => tracing::warn!(%error, "订阅自动刷新失败"),
                }
            }
        });
    }

    let app = build_router(state.clone());

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
