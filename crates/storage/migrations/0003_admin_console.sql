-- 0003_admin_console.sql
-- 管理后台所需：类型修正 + 两张新表。设计见 docs/m6-admin-plan.md。

-- call_records.account_id 原来是 bigint，与 accounts.id(uuid) 类型不符，
-- 关联不上账号。该表此前没有任何写入方，直接重建这一列。
alter table call_records drop column if exists account_id;
alter table call_records add column account_id uuid references accounts(id) on delete set null;
create index if not exists call_records_account_id_idx on call_records (account_id);

-- 调用 /v1/* 的 API key。只存 sha256：明文仅在创建时返回一次。
create table api_keys (
    id           uuid        primary key,
    name         text        not null,
    key_sha256   char(64)    not null unique,
    prefix       text        not null,
    created_at   timestamptz not null default now(),
    last_used_at timestamptz,
    revoked_at   timestamptz
);

-- 图片元数据：后台需要按账号/时间/大小查询，光有磁盘目录做不到。
create table image_assets (
    id         uuid        primary key,
    rel_path   text        not null unique,
    mime       text        not null,
    bytes      bigint      not null,
    width      int         not null,
    height     int         not null,
    account_id uuid        references accounts(id) on delete set null,
    prompt     text,
    created_at timestamptz not null default now()
);
create index if not exists image_assets_created_at_idx on image_assets (created_at desc);
create index if not exists image_assets_account_id_idx on image_assets (account_id);
