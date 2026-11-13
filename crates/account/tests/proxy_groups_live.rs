//! 代理分组的真实 PostgreSQL 集成测试。
//!
//! 需要 .env 里的 DATABASE_URL，缺则自动跳过。
//!
//! # 数据安全
//!
//! 不截断整表：分组名与账号 token 都带 gpt2apirs-test- 前缀，
//! 每个测试只删自己的行，可以在已有真实账号的库上安全运行。

use std::sync::{Mutex, MutexGuard};

use gpt2apirs_account::ProxyGroupRepo;
use gpt2apirs_account::model::{Plan, Source};
use gpt2apirs_account::repo::{AccountRepo, NewAccount};
use gpt2apirs_core::BootstrapConfig;
use gpt2apirs_storage::migrate::run_migrations;
use gpt2apirs_storage::pool::build_pool;
use sqlx::PgPool;
use uuid::Uuid;

const TEST_PREFIX: &str = "gpt2apirs-test-";

static DB_GUARD: Mutex<()> = Mutex::new(());

fn db_guard() -> MutexGuard<'static, ()> {
    DB_GUARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}

struct Fixture {
    pool: PgPool,
    accounts: AccountRepo,
    groups: ProxyGroupRepo,
}

async fn setup() -> Option<Fixture> {
    if !prepare_env() {
        eprintln!("跳过：未找到 DATABASE_URL");
        return None;
    }
    let cfg = BootstrapConfig::from_env().expect("配置应可加载");
    let pool = build_pool(&cfg).await.expect("应能连接 PostgreSQL");
    run_migrations(&pool).await.expect("迁移应成功");
    Some(Fixture {
        accounts: AccountRepo::new(pool.clone()),
        groups: ProxyGroupRepo::new(pool.clone()),
        pool,
    })
}

async fn clean(pool: &PgPool) {
    // 先删账号，再删分组：即便 FK 有 on delete set null，也不留半截状态。
    sqlx::query("delete from accounts where access_token like $1")
        .bind(format!("{TEST_PREFIX}%"))
        .execute(pool)
        .await
        .expect("清理测试账号失败");
    sqlx::query("delete from proxy_groups where name like $1")
        .bind(format!("{TEST_PREFIX}%"))
        .execute(pool)
        .await
        .expect("清理测试分组失败");
}

async fn make_account(repo: &AccountRepo, suffix: &str) -> Uuid {
    let new = NewAccount {
        access_token: format!("{TEST_PREFIX}{suffix}"),
        refresh_token: None,
        email: Some(format!("{suffix}@test.local")),
        plan: Plan::Plus,
        source: Source::Web,
        quota: 1,
        quota_confirmed: true,
        ..Default::default()
    };
    repo.upsert(&new).await.expect("建账号").account().id
}

async fn proxy_of(repo: &AccountRepo, id: Uuid) -> (String, Option<Uuid>) {
    let account = repo
        .get_by_id(id)
        .await
        .expect("查账号")
        .expect("账号应存在");
    (account.proxy, account.proxy_group_id)
}

#[tokio::test]
async fn repo_roundtrip_create_get_list_update_delete() {
    let Some(fx) = setup().await else { return };
    let _guard = db_guard();
    clean(&fx.pool).await;

    let name = format!("{TEST_PREFIX}roundtrip");
    let created = fx
        .groups
        .create(&name, " http://127.0.0.1:1080 ")
        .await
        .expect("建组");
    assert_eq!(created.name, name, "名称应原样保存");
    assert_eq!(created.url, "http://127.0.0.1:1080", "地址应去掉首尾空白");

    let fetched = fx.groups.get(created.id).await.expect("查组").expect("组应存在");
    assert_eq!(fetched.id, created.id);

    let listed = fx.groups.list().await.expect("列组");
    let view = listed
        .iter()
        .find(|view| view.group.id == created.id)
        .expect("列表应包含新组");
    assert_eq!(view.members, 0, "新组没有成员");

    let updated = fx
        .groups
        .update(created.id, &format!("{name}-renamed"), "socks5://127.0.0.1:1081")
        .await
        .expect("改组");
    assert_eq!(updated.name, format!("{name}-renamed"));
    assert_eq!(updated.url, "socks5://127.0.0.1:1081");
    assert!(
        updated.updated_at >= created.updated_at,
        "改组应刷新 updated_at"
    );

    assert!(fx.groups.delete(created.id).await.expect("删组"), "首次删除应命中");
    assert!(
        !fx.groups.delete(created.id).await.expect("再删"),
        "重复删除应返回 false"
    );
    assert!(fx.groups.get(created.id).await.expect("查空组").is_none());

    clean(&fx.pool).await;
}

#[tokio::test]
async fn duplicate_name_is_rejected_by_unique_constraint() {
    let Some(fx) = setup().await else { return };
    let _guard = db_guard();
    clean(&fx.pool).await;

    let name = format!("{TEST_PREFIX}dup");
    fx.groups.create(&name, "").await.expect("首建应成功");
    let second = fx.groups.create(&name, "http://127.0.0.1:1080").await;
    assert!(second.is_err(), "同名分组必须被拒绝，实际: {second:?}");

    // 改名撞车同样要拒
    let other = fx
        .groups
        .create(&format!("{TEST_PREFIX}dup-other"), "")
        .await
        .expect("次建");
    let clash = fx.groups.update(other.id, &name, "").await;
    assert!(clash.is_err(), "改成已存在的名称必须被拒绝");

    clean(&fx.pool).await;
}

#[tokio::test]
async fn assign_copies_group_url_and_sync_rewrites_members_only() {
    let Some(fx) = setup().await else { return };
    let _guard = db_guard();
    clean(&fx.pool).await;

    let group = fx
        .groups
        .create(&format!("{TEST_PREFIX}members"), "http://127.0.0.1:1080")
        .await
        .expect("建组");
    let a = make_account(&fx.accounts, "pg-member-a").await;
    let b = make_account(&fx.accounts, "pg-member-b").await;
    let outsider = make_account(&fx.accounts, "pg-outsider").await;

    fx.accounts
        .assign_proxy_group(a, group.id, &group.url)
        .await
        .expect("a 入组");
    fx.accounts
        .assign_proxy_group(b, group.id, &group.url)
        .await
        .expect("b 入组");
    assert_eq!(
        proxy_of(&fx.accounts, a).await,
        (group.url.clone(), Some(group.id)),
        "入组应把组地址复制进 proxy"
    );

    let listed = fx.groups.list().await.expect("列组");
    let view = listed
        .iter()
        .find(|view| view.group.id == group.id)
        .expect("列表应含该组");
    assert_eq!(view.members, 2, "成员数应为 2");

    let new_url = "http://127.0.0.1:2080";
    fx.groups
        .update(group.id, &group.name, new_url)
        .await
        .expect("改组地址");
    let synced = fx
        .accounts
        .sync_proxy_group(group.id, new_url)
        .await
        .expect("同步成员");
    assert_eq!(synced, 2, "同步应命中两个成员");
    assert_eq!(proxy_of(&fx.accounts, a).await.0, new_url);
    assert_eq!(proxy_of(&fx.accounts, b).await.0, new_url);
    assert_eq!(
        proxy_of(&fx.accounts, outsider).await,
        (String::new(), None),
        "非成员不能被改写"
    );

    clean(&fx.pool).await;
}

#[tokio::test]
async fn clear_keeps_effective_proxy_but_drops_membership() {
    let Some(fx) = setup().await else { return };
    let _guard = db_guard();
    clean(&fx.pool).await;

    let group = fx
        .groups
        .create(&format!("{TEST_PREFIX}clear"), "http://127.0.0.1:1080")
        .await
        .expect("建组");
    let id = make_account(&fx.accounts, "pg-clear").await;
    fx.accounts
        .assign_proxy_group(id, group.id, &group.url)
        .await
        .expect("入组");

    let cleared = fx.accounts.clear_proxy_group(id).await.expect("移出");
    assert!(cleared.proxy_group_id.is_none(), "移出后不应再挂在组上");
    assert_eq!(
        cleared.proxy, "http://127.0.0.1:1080",
        "移出分组必须保留当前生效的代理串，否则路由会突变"
    );

    clean(&fx.pool).await;
}

#[tokio::test]
async fn detach_then_delete_keeps_member_proxy_and_clears_link() {
    let Some(fx) = setup().await else { return };
    let _guard = db_guard();
    clean(&fx.pool).await;

    let group = fx
        .groups
        .create(&format!("{TEST_PREFIX}detach"), "http://127.0.0.1:1080")
        .await
        .expect("建组");
    let a = make_account(&fx.accounts, "pg-detach-a").await;
    let b = make_account(&fx.accounts, "pg-detach-b").await;
    for id in [a, b] {
        fx.accounts
            .assign_proxy_group(id, group.id, &group.url)
            .await
            .expect("入组");
    }

    let detached = fx.accounts.detach_proxy_group(group.id).await.expect("摘成员");
    assert_eq!(detached, 2, "应摘掉两个成员");
    assert!(fx.groups.delete(group.id).await.expect("删组"), "删组应命中");
    for id in [a, b] {
        let (proxy, group_id) = proxy_of(&fx.accounts, id).await;
        assert_eq!(proxy, group.url, "删组后成员保留原代理串");
        assert!(group_id.is_none(), "删组后成员不应再指向已删组");
    }

    clean(&fx.pool).await;
}

#[tokio::test]
async fn db_delete_cascades_group_link_even_without_detach() {
    let Some(fx) = setup().await else { return };
    let _guard = db_guard();
    clean(&fx.pool).await;

    // 数据库层的兜底：外键是 on delete set null，删组不会连坐账号。
    let group = fx
        .groups
        .create(&format!("{TEST_PREFIX}cascade"), "http://127.0.0.1:1080")
        .await
        .expect("建组");
    let id = make_account(&fx.accounts, "pg-cascade").await;
    fx.accounts
        .assign_proxy_group(id, group.id, &group.url)
        .await
        .expect("入组");

    fx.groups.delete(group.id).await.expect("不摘成员直接删组");
    let (proxy, group_id) = proxy_of(&fx.accounts, id).await;
    assert_eq!(proxy, group.url, "账号应保留代理串");
    assert!(group_id.is_none(), "外键应把 proxy_group_id 置空而不是删账号");

    clean(&fx.pool).await;
}
