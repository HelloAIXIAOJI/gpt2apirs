-- 0002_accounts.sql
-- 账号池主表。设计见 docs/account-pool-design.md 3.2。
--
-- 约定：
--   * 状态存小写英文（normal/limited/abnormal/disabled），中文只在展示层
--   * access_token 明文存（生图要用），唯一索引走 sha256
--   * 时间一律 timestamptz

create table accounts (
    id                  uuid primary key,

    -- 凭证
    access_token        text        not null,
    access_token_sha    char(64)    not null,
    refresh_token       text,
    id_token            text,

    -- 身份
    email               text,
    upstream_account_id text,

    -- 分类
    plan                text        not null default 'unknown',
    source              text        not null default 'web',

    -- 状态
    status              text        not null default 'normal',
    quota               integer     not null default 0,
    quota_confirmed     boolean     not null default false,
    restore_at          timestamptz,

    -- 统计
    proxy               text        not null default '',
    success_count       bigint      not null default 0,
    fail_count          bigint      not null default 0,
    invalid_count       integer     not null default 0,

    -- 时间线
    last_used_at        timestamptz,
    last_invalid_at     timestamptz,
    last_error          text,
    last_error_at       timestamptz,
    last_checked_at     timestamptz,
    last_refresh_at     timestamptz,
    created_at          timestamptz not null default now(),
    updated_at          timestamptz not null default now(),

    constraint accounts_status_valid check (
        status in ('normal', 'limited', 'abnormal', 'disabled')
    ),
    constraint accounts_quota_non_negative check (quota >= 0)
);

-- token 唯一（走哈希，避免 2KB 明文进索引）
create unique index accounts_access_token_sha_key on accounts (access_token_sha);

-- 选号热路径：只关心 normal 且 quota > 0
create index accounts_selectable_idx on accounts (status, quota)
    where status = 'normal' and quota > 0;

-- 按套餐筛选
create index accounts_plan_idx on accounts (plan);

-- 找陈旧未校验的账号
create index accounts_checked_idx on accounts (last_checked_at nulls first);

-- updated_at 自动维护
create or replace function accounts_touch_updated_at() returns trigger as $$
begin
    new.updated_at = now();
    return new;
end;
$$ language plpgsql;

create trigger accounts_touch_updated_at_trigger
    before update on accounts
    for each row execute function accounts_touch_updated_at();
