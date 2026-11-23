//! 账号仓储对真实 PostgreSQL 的集成测试。
//!
//! 需要 .env 里配置 DATABASE_URL，缺失时自动跳过。
//!
//! # 数据安全
//!
//! 不截断整表。测试账号的 token 一律带 gpt2apirs-test- 前缀，
//! 每个测试只删自己的前缀行，结束时清掉。
//! 因而可以在已有真实账号的库上安全运行。

use std::sync::{Mutex, MutexGuard};

use gpt2apirs_account::model::{AccountStatus, Plan, Source};
use gpt2apirs_account::repo::{AccountFilter, AccountPatch, AccountRepo, NewAccount};
use gpt2apirs_core::BootstrapConfig;
use gpt2apirs_storage::migrate::run_migrations;
use gpt2apirs_storage::pool::build_pool;
use sqlx::PgPool;
use uuid::Uuid;

const TEST_PREFIX: &str = "gpt2apirs-test-";

static DB_GUARD: Mutex<()> = Mutex::new(());

fn db_guard() -> MutexGuard<'static, ()> {
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
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}

async fn setup() -> Option<(PgPool, AccountRepo)> {
    if !prepare_env() {
        eprintln!("跳过：未找到 DATABASE_URL");
        return None;
    }
    let cfg = BootstrapConfig::from_env().expect("配置应可加载");
    let pool = build_pool(&cfg).await.expect("应能连接 PostgreSQL");
    run_migrations(&pool).await.expect("迁移应成功");
    // 注意：这里不清理。清理必须发生在持有 DB_GUARD 之后，
    // 否则并行测试会互删对方刚插入的行。
    let repo = AccountRepo::new(pool.clone());
    Some((pool, repo))
}

async fn clean_test_rows(pool: &PgPool) {
    sqlx::query("delete from accounts where access_token like $1")
        .bind(format!("{TEST_PREFIX}%"))
        .execute(pool)
        .await
        .expect("清理测试行失败");
}

fn token(suffix: &str) -> String {
    format!("{TEST_PREFIX}{suffix}")
}

fn new_account(suffix: &str, quota: i32) -> NewAccount {
    NewAccount {
        access_token: token(suffix),
        refresh_token: Some(format!("rt-{suffix}")),
        email: Some(format!("{suffix}@test.local")),
        plan: Plan::Plus,
        source: Source::Web,
        quota,
        quota_confirmed: true,
        ..Default::default()
    }
}

/// 「不限量」必须能在库里存住，并且不被配额扣减/状态推导搞成限流。
#[tokio::test]
async fn quota_unlimited_survives_round_trip_and_usage() {
    let Some((pool, repo)) = setup().await else { return };
    let _guard = db_guard();
    clean_test_rows(&pool).await;

    let account = repo
        .upsert(&new_account("unlimited", 0))
        .await
        .expect("写入");
    let account = match account {
        gpt2apirs_account::repo::UpsertOutcome::Inserted(a)
        | gpt2apirs_account::repo::UpsertOutcome::Merged(a) => a,
    };
    assert!(!account.quota_unlimited, "默认不是不限量");

    let patched = repo
        .patch(
            account.id,
            &AccountPatch {
                quota_unlimited: Some(true),
                quota: Some(0),
                status: Some(AccountStatus::Normal),
                ..Default::default()
            },
        )
        .await
        .expect("改标记");
    assert!(patched.quota_unlimited, "标记要落库");
    assert_eq!(patched.quota_state(), gpt2apirs_account::QuotaState::Unlimited);
    assert!(patched.has_image_quota(), "不限量必须算有配额");

    // 用掉一次：配额不扣、状态不进 limited
    let used = repo.record_result(account.id, true).await.expect("记录使用");
    assert_eq!(used.quota, 0, "不限量账号不该被扣成负数或变号");
    assert_eq!(used.status, AccountStatus::Normal, "不限量账号不该判限流");

    clean_test_rows(&pool).await;
}

#[tokio::test]
async fn migration_creates_accounts_table_and_indexes() {
    let Some((pool, _repo)) = setup().await else { return };
    let _guard = db_guard();
    clean_test_rows(&pool).await;

    let exists: Option<(String,)> = sqlx::query_as(
        "select table_name from information_schema.tables \
         where table_schema = 'public' and table_name = 'accounts'",
    )
    .fetch_optional(&pool)
    .await
    .expect("查表失败");
    assert!(exists.is_some(), "accounts 表应存在");

    let indexes: Vec<(String,)> = sqlx::query_as(
        "select indexname from pg_indexes \
         where schemaname = 'public' and tablename = 'accounts' order by indexname",
    )
    .fetch_all(&pool)
    .await
    .expect("查索引失败");
    let names: Vec<String> = indexes.into_iter().map(|(name,)| name).collect();
    println!("accounts 索引: {names:?}");
    for expected in [
        "accounts_access_token_sha_key",
        "accounts_selectable_idx",
        "accounts_plan_idx",
        "accounts_checked_idx",
    ] {
        assert!(
            names.iter().any(|name| name == expected),
            "缺少索引 {expected}，实际: {names:?}"
        );
    }
}

#[tokio::test]
async fn upsert_rotated_token_merges_into_same_row_by_email() {
    let Some((pool, repo)) = setup().await else { return };
    let _guard = db_guard();
    clean_test_rows(&pool).await;

    // 第一次：token A + 邮箱
    let mut first = new_account("rotate-a", 1);
    first.email = Some("same-acct@test.local".to_string());
    first.refresh_token = None;
    let created = repo.upsert(&first).await.expect("首次").account().clone();
    assert!(created.refresh_token.is_none());

    // 第二次：**不同 token（模拟 OAuth 换回新 AT）**，同一邮箱，带 RT
    let mut second = new_account("rotate-b", 1);
    second.email = Some("same-acct@test.local".to_string());
    second.refresh_token = Some("rt-from-oauth".to_string());
    let merged = repo.upsert(&second).await.expect("再次");
    assert!(!merged.is_inserted(), "同一邮箱应识别为同一账号，不能新增");
    let merged = merged.account().clone();

    assert_eq!(merged.id, created.id, "必须落在同一行");
    assert_eq!(merged.access_token, token("rotate-b"), "token 应被轮换为新值");
    assert_eq!(merged.refresh_token.as_deref(), Some("rt-from-oauth"));
    assert_eq!(merged.email.as_deref(), Some("same-acct@test.local"));

    // 只有一行
    let count = repo
        .count(&AccountFilter {
            keyword: Some("same-acct@test.local".to_string()),
            ..Default::default()
        })
        .await
        .expect("计数");
    assert_eq!(count, 1, "同一邮箱不应产生第二行");

    // 旧 token 已失效（被轮换掉）
    assert!(
        repo.get_by_token(&token("rotate-a")).await.expect("查旧 token").is_none(),
        "旧 token 应已不在库中"
    );

    clean_test_rows(&pool).await;
}

#[tokio::test]
async fn upsert_rotated_token_merges_by_upstream_account_id() {
    let Some((pool, repo)) = setup().await else { return };
    let _guard = db_guard();
    clean_test_rows(&pool).await;

    // 第一次：有 upstream_account_id，但**没有邮箱**
    let mut first = new_account("accid-a", 1);
    first.email = None;
    first.upstream_account_id = Some("acc-xyz-999".to_string());
    let created = repo.upsert(&first).await.expect("首次").account().clone();

    // 第二次：不同 token，同一 upstream_account_id，仍无邮箱
    let mut second = new_account("accid-b", 1);
    second.email = None;
    second.upstream_account_id = Some("acc-xyz-999".to_string());
    second.refresh_token = Some("rt-by-accid".to_string());
    let merged = repo
        .upsert(&second)
        .await
        .expect("再次")
        .account()
        .clone();

    assert_eq!(merged.id, created.id, "同一 upstream_account_id 应合并");
    assert_eq!(merged.access_token, token("accid-b"), "token 应被轮换");
    assert_eq!(merged.refresh_token.as_deref(), Some("rt-by-accid"));

    clean_test_rows(&pool).await;
}

#[tokio::test]
async fn upsert_absorbs_duplicate_rows_and_sums_counters() {
    let Some((pool, repo)) = setup().await else { return };
    let _guard = db_guard();
    clean_test_rows(&pool).await;

    // 直接 SQL 造两行同邮箱的历史重复（模拟修复前 OAuth 插出的第二行）
    let email = "dup-acct@test.local";
    let seeds = [(2i64, 1i64, 7i32, 10i32), (3, 4, 11, 1)];
    for (idx, (success, fail, quota, days_ago)) in seeds.into_iter().enumerate() {
        let tok = format!("{TEST_PREFIX}dup-{idx}");
        let sha = gpt2apirs_account::Account::token_sha(&tok);
        sqlx::query(
"insert into accounts (id, access_token, access_token_sha, email, plan, source, \
                     status, quota, quota_confirmed, success_count, fail_count, created_at) \
 values ($1,$2,$3,$4,$5,$6,$7,$8,true,$9,$10, now() - make_interval(days => $11))",
        )
        .bind(Uuid::new_v4())
        .bind(&tok)
        .bind(&sha)
        .bind(email)
        .bind("free")
        .bind("web")
        .bind("normal")
        .bind(quota)
        .bind(success)
        .bind(fail)
        .bind(days_ago)
        .execute(&pool)
        .await
        .expect("插入历史重复行");
    }

    let before = repo
        .count(&AccountFilter {
            keyword: Some(email.to_string()),
            ..Default::default()
        })
        .await
        .expect("计数");
    assert_eq!(before, 2, "前置条件：应有两行重复");

    // 第三个 token（同邮箱）upsert —— 应吸收两行并保留最早那行
    let mut incoming = new_account("dup-new", 0);
    incoming.email = Some(email.to_string());
    incoming.quota = 0;
    incoming.quota_confirmed = false;
    incoming.refresh_token = Some("rt-absorbed".to_string());
    let outcome = repo.upsert(&incoming).await.expect("upsert");
    assert!(!outcome.is_inserted(), "应识别为同一账号");
    let merged = outcome.account().clone();

    let after = repo
        .count(&AccountFilter {
            keyword: Some(email.to_string()),
            ..Default::default()
        })
        .await
        .expect("计数");
    assert_eq!(after, 1, "应合并为一行");
    assert_eq!(merged.success_count, 5, "2 + 3 = 5");
    assert_eq!(merged.fail_count, 5, "1 + 4 = 5");
    assert_eq!(
        merged.quota, 7,
        "配额不相加，也不被新导入的 0 覆盖 —— 保留最早行的真实配额"
    );
    assert_eq!(merged.refresh_token.as_deref(), Some("rt-absorbed"));
    assert_eq!(merged.access_token, token("dup-new"), "token 轮换为新值");

    clean_test_rows(&pool).await;
}
#[tokio::test]
async fn upsert_different_accounts_stay_separate() {
    let Some((pool, repo)) = setup().await else { return };
    let _guard = db_guard();
    clean_test_rows(&pool).await;

    // 不同 token + 不同邮箱  必须是两行，不能误合并
    let mut a = new_account("sep-a", 1);
    a.email = Some("sep-a@test.local".to_string());
    let acc_a = repo.upsert(&a).await.expect("A").account().clone();

    let mut b = new_account("sep-b", 1);
    b.email = Some("sep-b@test.local".to_string());
    let outcome_b = repo.upsert(&b).await.expect("B");
    assert!(outcome_b.is_inserted(), "不同账号应是 Inserted");
    assert_ne!(outcome_b.account().id, acc_a.id);

    let count = repo
        .count(&AccountFilter {
            keyword: Some("sep-".to_string()),
            ..Default::default()
        })
        .await
        .expect("计数");
    assert_eq!(count, 2, "两个不同账号应各自一行");

    clean_test_rows(&pool).await;
}
#[tokio::test]
async fn upsert_same_token_is_merged_not_duplicated() {
    let Some((pool, repo)) = setup().await else { return };
    let _guard = db_guard();
    clean_test_rows(&pool).await;

    let first = repo
        .upsert(&new_account("dedupe", 3))
        .await
        .expect("首次写入应成功");
    assert!(first.is_inserted(), "首次应是 Inserted");
    let account = first.account().clone();
    assert_eq!(account.quota, 3);
    assert_eq!(account.status, AccountStatus::Normal);
    assert_eq!(account.plan, Plan::Plus);
    assert_eq!(account.email.as_deref(), Some("dedupe@test.local"));

    let second = repo
        .upsert(&new_account("dedupe", 3))
        .await
        .expect("重复写入不应报错");
    assert!(!second.is_inserted(), "第二次应是 Merged 而不是新增");
    assert_eq!(second.account().id, account.id, "合并必须落在同一行");

    let count = repo
        .count(&AccountFilter {
            keyword: Some("dedupe@test.local".to_string()),
            ..Default::default()
        })
        .await
        .expect("计数");
    assert_eq!(count, 1, "同一 token 只应有一行");

    clean_test_rows(&pool).await;
}

#[tokio::test]
async fn upsert_merges_missing_refresh_token() {
    let Some((pool, repo)) = setup().await else { return };
    let _guard = db_guard();
    clean_test_rows(&pool).await;

    // 先只导 access_token（refresh_token 为空）
    let mut bare = new_account("merge-rt", 1);
    bare.refresh_token = None;
    bare.id_token = None;
    let created = repo.upsert(&bare).await.expect("首次").account().clone();
    assert!(created.refresh_token.is_none(), "初始应没有 refresh token");

    // 再导同一 token，这次带上 refresh_token 与 id_token
    let mut full = new_account("merge-rt", 1);
    full.refresh_token = Some("rt-补进来的".to_string());
    full.id_token = Some("id-补进来的".to_string());
    let merged = repo.upsert(&full).await.expect("再次");
    assert!(!merged.is_inserted(), "应是合并而非新增");
    let merged = merged.account().clone();

    assert_eq!(merged.id, created.id, "同一行");
    assert_eq!(merged.refresh_token.as_deref(), Some("rt-补进来的"));
    assert_eq!(merged.id_token.as_deref(), Some("id-补进来的"));
    assert_eq!(merged.quota, created.quota, "合并不应改动 quota");

    clean_test_rows(&pool).await;
}

#[tokio::test]
async fn upsert_does_not_wipe_existing_fields() {
    let Some((pool, repo)) = setup().await else { return };
    let _guard = db_guard();
    clean_test_rows(&pool).await;

    let full = new_account("no-wipe", 5);
    let created = repo.upsert(&full).await.expect("首次").account().clone();
    assert!(created.refresh_token.is_some());

    // 再导一个只有 access_token 的（refresh/id/email 全 None）
    let bare = NewAccount {
        access_token: full.access_token.clone(),
        ..Default::default()
    };
    let merged = repo.upsert(&bare).await.expect("再次").account().clone();

    assert_eq!(
        merged.refresh_token, created.refresh_token,
        "空值不应把已有 refresh_token 覆盖成 NULL"
    );
    assert_eq!(merged.id_token, created.id_token, "空值不应覆盖 id_token");
    assert_eq!(merged.email, created.email, "空值不应覆盖 email");
    assert_eq!(merged.plan, created.plan, "plan=unknown 不应覆盖已知套餐");
    assert_eq!(merged.proxy, created.proxy, "空 proxy 不应覆盖已有 proxy");

    clean_test_rows(&pool).await;
}

#[tokio::test]
async fn upsert_many_inserts_unique_tokens_once() {
    let Some((pool, repo)) = setup().await else { return };
    let _guard = db_guard();
    clean_test_rows(&pool).await;

    let batch = vec![
        new_account("batch-a", 1),
        new_account("batch-b", 2),
        new_account("batch-a", 9),
    ];
    let report = repo.upsert_many(&batch).await.expect("批量写入应成功");
    assert_eq!(report.added(), 2, "应只新增 2 个唯一 token");
    assert_eq!(report.merged_count(), 1, "批内重复应合并");

    let mut emails: Vec<&str> = report
        .inserted
        .iter()
        .filter_map(|account| account.email.as_deref())
        .collect();
    emails.sort_unstable();
    assert_eq!(emails, vec!["batch-a@test.local", "batch-b@test.local"]);

    clean_test_rows(&pool).await;
}

#[tokio::test]
async fn upsert_many_skips_blank_tokens() {
    let Some((pool, repo)) = setup().await else { return };
    let _guard = db_guard();
    clean_test_rows(&pool).await;

    let mut blank = new_account("blank", 1);
    blank.access_token = "   ".to_string();

    let report = repo
        .upsert_many(&[blank, new_account("blank-ok", 1)])
        .await
        .expect("批量写入");
    assert_eq!(report.added(), 1, "只应有 1 个有效账号");
    assert_eq!(report.skipped_blank, 1, "空白 token 单独计数");

    clean_test_rows(&pool).await;
}

#[tokio::test]
async fn get_by_id_and_by_token_agree() {
    let Some((pool, repo)) = setup().await else { return };
    let _guard = db_guard();
    clean_test_rows(&pool).await;

    let created = repo
        .upsert(&new_account("get", 5))
        .await
        .expect("写入")
        .account()
        .clone();

    let by_id = repo.get_by_id(created.id).await.expect("按 id 查").expect("应存在");
    let by_token = repo
        .get_by_token(&token("get"))
        .await
        .expect("按 token 查")
        .expect("应存在");

    assert_eq!(by_id.id, by_token.id);
    assert_eq!(by_id.access_token, by_token.access_token);
    assert_eq!(by_id.created_at, by_token.created_at);

    assert!(
        repo.get_by_id(Uuid::new_v4()).await.expect("查不存在 id").is_none(),
        "不存在的 id 应返回 None"
    );
    assert!(
        repo.get_by_token("no-such-token").await.expect("查不存在 token").is_none(),
        "不存在的 token 应返回 None"
    );

    clean_test_rows(&pool).await;
}

#[tokio::test]
async fn list_filters_by_status_plan_and_keyword() {
    let Some((pool, repo)) = setup().await else { return };
    let _guard = db_guard();
    clean_test_rows(&pool).await;

    let mut free_one = new_account("filter-free", 2);
    free_one.plan = Plan::Free;
    let plus_one = new_account("filter-plus", 4);

    let created_plus = repo.upsert(&plus_one).await.expect("写入").account().clone();
    repo.upsert(&free_one).await.expect("写入");

    // 关键词只对 email / 上游 account_id / id / 完整 token 生效（见 D10）
    let keyword = "filter-".to_string();
    let by_keyword = repo
        .list(&AccountFilter {
            keyword: Some(keyword.clone()),
            ..Default::default()
        })
        .await
        .expect("按关键词查");
    assert_eq!(by_keyword.len(), 2, "两个账号都应命中关键词");

    let by_plan = repo
        .list(&AccountFilter {
            keyword: Some(keyword.clone()),
            plan: Some(Plan::Free),
            ..Default::default()
        })
        .await
        .expect("按套餐查");
    assert_eq!(by_plan.len(), 1);
    assert_eq!(by_plan[0].plan, Plan::Free);

    repo.patch(
        created_plus.id,
        &AccountPatch {
            status: Some(AccountStatus::Limited),
            ..Default::default()
        },
    )
    .await
    .expect("改状态");

    let limited = repo
        .list(&AccountFilter {
            keyword: Some(keyword.clone()),
            status: Some(AccountStatus::Limited),
            ..Default::default()
        })
        .await
        .expect("按状态查");
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0].id, created_plus.id);

    let count = repo
        .count(&AccountFilter {
            keyword: Some(keyword),
            ..Default::default()
        })
        .await
        .expect("计数");
    assert_eq!(count, 2);

    clean_test_rows(&pool).await;
}

#[tokio::test]
async fn keyword_matches_full_token_exactly() {
    let Some((pool, repo)) = setup().await else { return };
    let _guard = db_guard();
    clean_test_rows(&pool).await;

    // 真实 token 长度 > 1000；这里用 40+ 字符以越过 MIN_TOKEN_LOOKUP_LEN(32)
    let long_token = format!("{TEST_PREFIX}toksearch-{}", "a".repeat(40));
    let mut input = new_account("toksearch", 1);
    input.access_token = long_token.clone();
    repo.upsert(&input).await.expect("写入");

    let hit = repo
        .list(&AccountFilter {
            keyword: Some(long_token.clone()),
            ..Default::default()
        })
        .await
        .expect("按完整 token 查");
    assert_eq!(hit.len(), 1, "完整 token 应精确命中 1 个");
    assert_eq!(hit[0].access_token, long_token);

    // 片段  哈希不可逆，搜不到（D10 的已知代价）
    let miss = repo
        .list(&AccountFilter {
            keyword: Some("aaaaaaaaaa".to_string()),
            ..Default::default()
        })
        .await
        .expect("按片段查");
    assert!(miss.is_empty(), "token 片段搜不到是 D10 的已知限制");

    clean_test_rows(&pool).await;
}

#[tokio::test]
async fn patch_updates_only_given_fields() {
    let Some((pool, repo)) = setup().await else { return };
    let _guard = db_guard();
    clean_test_rows(&pool).await;

    let created = repo
        .upsert(&new_account("patch", 7))
        .await
        .expect("写入")
        .account()
        .clone();

    let updated = repo
        .patch(
            created.id,
            &AccountPatch {
                proxy: Some("http://127.0.0.1:7890".to_string()),
                plan: Some(Plan::Pro),
                ..Default::default()
            },
        )
        .await
        .expect("更新");

    assert_eq!(updated.proxy, "http://127.0.0.1:7890");
    assert_eq!(updated.plan, Plan::Pro);
    assert_eq!(updated.quota, 7, "未提供字段应保持原值");
    assert_eq!(updated.email, created.email, "未提供字段应保持原值");

    clean_test_rows(&pool).await;
}

#[tokio::test]
async fn record_text_result_does_not_touch_quota() {
    let Some((pool, repo)) = setup().await else { return };
    let _guard = db_guard();
    clean_test_rows(&pool).await;

    let mut account = new_account("text-quota", 5);
    account.quota_confirmed = true;
    let created = repo.upsert(&account).await.expect("upsert").account().clone();

    // 文本成功：计数 +1，配额必须原封不动
    let after = repo.record_text_result(created.id, true).await.expect("记录");
    assert_eq!(after.success_count, 1, "成功计数应 +1");
    assert_eq!(after.quota, 5, "图片配额不能被文本消耗");
    assert_eq!(after.status, gpt2apirs_account::AccountStatus::Normal, "状态不应变化");

    // 文本失败：失败计数 +1，配额同样不动
    let after = repo.record_text_result(created.id, false).await.expect("记录");
    assert_eq!(after.fail_count, 1, "失败计数应 +1");
    assert_eq!(after.quota, 5, "失败也不该动配额");
    assert!(after.last_used_at.is_some(), "应更新 last_used_at");

    // 对照：图片路径会扣配额
    let after = repo.record_result(created.id, true).await.expect("记录");
    assert_eq!(after.quota, 4, "图片路径应当扣配额");
    assert_eq!(after.success_count, 2, "成功计数继续累加");

    clean_test_rows(&pool).await;
}
#[tokio::test]
async fn record_result_decrements_quota_and_flips_to_limited() {
    let Some((pool, repo)) = setup().await else { return };
    let _guard = db_guard();
    clean_test_rows(&pool).await;

    let created = repo
        .upsert(&new_account("result-ok", 1))
        .await
        .expect("写入")
        .account()
        .clone();
    assert_eq!(created.status, AccountStatus::Normal);

    let after = repo.record_result(created.id, true).await.expect("记录成功");
    assert_eq!(after.quota, 0, "配额应减到 0");
    assert_eq!(after.status, AccountStatus::Limited, "配额为 0 应转限流");
    assert_eq!(after.success_count, 1);
    assert_eq!(after.fail_count, 0);

    repo.patch(
        created.id,
        &AccountPatch {
            quota: Some(2),
            ..Default::default()
        },
    )
    .await
    .expect("恢复配额");
    let recovered = repo.record_result(created.id, true).await.expect("再成功");
    assert_eq!(recovered.quota, 1);
    assert_eq!(recovered.status, AccountStatus::Normal, "配额回来应转正常");

    clean_test_rows(&pool).await;
}

#[tokio::test]
async fn record_result_failure_keeps_status() {
    let Some((pool, repo)) = setup().await else { return };
    let _guard = db_guard();
    clean_test_rows(&pool).await;

    let created = repo
        .upsert(&new_account("result-fail", 5))
        .await
        .expect("写入")
        .account()
        .clone();

    let after = repo.record_result(created.id, false).await.expect("记录失败");
    assert_eq!(after.status, AccountStatus::Normal, "失败不应改状态");
    assert_eq!(after.quota, 5, "失败不应扣配额");
    assert_eq!(after.fail_count, 1);
    assert_eq!(after.success_count, 0);

    clean_test_rows(&pool).await;
}

#[tokio::test]
async fn replace_tokens_keeps_id_and_updates_sha() {
    let Some((pool, repo)) = setup().await else { return };
    let _guard = db_guard();
    clean_test_rows(&pool).await;

    let created = repo
        .upsert(&new_account("rotate-old", 3))
        .await
        .expect("写入")
        .account()
        .clone();

    let new_token = token("rotate-new");
    let rotated = repo
        .replace_tokens(created.id, &new_token, Some("rt-rotate-new"), None)
        .await
        .expect("轮换");

    assert_eq!(rotated.id, created.id, "id 必须不变（D2）");
    assert_eq!(rotated.access_token, new_token);
    assert!(rotated.last_refresh_at.is_some(), "应记录刷新时间");

    assert!(
        repo.get_by_token(&token("rotate-old")).await.expect("查旧 token").is_none(),
        "旧 token 应查不到"
    );
    let found = repo
        .get_by_token(&new_token)
        .await
        .expect("查新 token")
        .expect("新 token 应可查");
    assert_eq!(found.id, created.id);

    clean_test_rows(&pool).await;
}

#[tokio::test]
async fn record_invalid_accumulates_and_clear_invalid_resets() {
    let Some((pool, repo)) = setup().await else { return };
    let _guard = db_guard();
    clean_test_rows(&pool).await;

    let created = repo
        .upsert(&new_account("invalid", 2))
        .await
        .expect("写入")
        .account()
        .clone();

    repo.record_invalid(created.id, "401 invalid token")
        .await
        .expect("记录失效");
    let after = repo
        .record_invalid(created.id, "401 invalid token")
        .await
        .expect("再记录");
    assert_eq!(after.invalid_count, 2);
    assert!(after.last_invalid_at.is_some());
    assert_eq!(after.last_error.as_deref(), Some("401 invalid token"));
    assert_eq!(after.status, AccountStatus::Normal, "record_invalid 不改状态");

    repo.patch(
        created.id,
        &AccountPatch {
            status: Some(AccountStatus::Abnormal),
            ..Default::default()
        },
    )
    .await
    .expect("标异常");
    let cleared = repo.clear_invalid(created.id).await.expect("清除");
    assert_eq!(cleared.invalid_count, 0);
    assert!(cleared.last_error.is_none());
    assert_eq!(cleared.status, AccountStatus::Normal, "异常应转回正常");

    clean_test_rows(&pool).await;
}

#[tokio::test]
async fn delete_many_removes_only_given_ids() {
    let Some((pool, repo)) = setup().await else { return };
    let _guard = db_guard();
    clean_test_rows(&pool).await;

    let keep = repo
        .upsert(&new_account("del-keep", 1))
        .await
        .expect("写入")
        .account()
        .clone();
    let drop_me = repo
        .upsert(&new_account("del-drop", 1))
        .await
        .expect("写入")
        .account()
        .clone();

    let removed = repo.delete_many(&[drop_me.id]).await.expect("删除");
    assert_eq!(removed, 1);
    assert!(
        repo.get_by_id(drop_me.id).await.expect("查已删").is_none(),
        "应已删除"
    );
    assert!(
        repo.get_by_id(keep.id).await.expect("查保留").is_some(),
        "不应误删"
    );
    assert_eq!(repo.delete_many(&[]).await.expect("空删除"), 0, "空列表应为 0");

    clean_test_rows(&pool).await;
}

#[tokio::test]
async fn stats_reflect_inserted_rows() {
    let Some((pool, repo)) = setup().await else { return };
    let _guard = db_guard();
    clean_test_rows(&pool).await;

    let before = repo.stats().await.expect("统计");

    let normal = repo
        .upsert(&new_account("stats-normal", 4))
        .await
        .expect("写入")
        .account()
        .clone();
    repo.upsert(&new_account("stats-normal-2", 6))
        .await
        .expect("写入");

    let after = repo.stats().await.expect("统计");
    assert_eq!(after.total, before.total + 2, "总数应 +2");
    assert_eq!(after.normal, before.normal + 2, "正常数应 +2");
    assert_eq!(after.total_quota, before.total_quota + 10, "总配额应 +10");

    repo.record_result(normal.id, true).await.expect("记录");
    let final_stats = repo.stats().await.expect("统计");
    assert_eq!(final_stats.total_quota, after.total_quota - 1);
    assert_eq!(final_stats.total_success, after.total_success + 1);

    clean_test_rows(&pool).await;
}

#[tokio::test]
async fn mark_checked_sets_timestamp() {
    let Some((pool, repo)) = setup().await else { return };
    let _guard = db_guard();
    clean_test_rows(&pool).await;

    let created = repo
        .upsert(&new_account("checked", 1))
        .await
        .expect("写入")
        .account()
        .clone();
    assert!(created.last_checked_at.is_none(), "新导入账号不应有校验时间");

    let checked = repo.mark_checked(created.id).await.expect("标记校验");
    assert!(checked.last_checked_at.is_some(), "应有校验时间");

    clean_test_rows(&pool).await;
}

#[tokio::test]
async fn patch_missing_id_returns_not_found() {
    let Some((pool, repo)) = setup().await else { return };
    let _guard = db_guard();
    clean_test_rows(&pool).await;

    let missing = Uuid::new_v4();
    let error = repo
        .patch(missing, &AccountPatch::default())
        .await
        .expect_err("不存在的 id 应报错");
    match error {
        gpt2apirs_account::AccountError::NotFound(id) => assert_eq!(id, missing),
        other => panic!("期望 NotFound，实际 {other:?}"),
    }
}

