-- 0001_init.sql
-- M1 只建 3 张表。其余（accounts / image_assets / call_records 扩展…）随 M3/M4 加。
-- 约定：时间一律 timestamptz；JSON 一律 jsonb；表名复数 snake_case。

-- ---------------------------------------------------------------
-- 运行层设置：单行表，整个设置存一个 JSONB
-- 单行约束用 check 保证（id 恒为 1）
-- ---------------------------------------------------------------
create table system_settings (
    id          smallint    primary key default 1,
    data        jsonb       not null default '{}'::jsonb,
    updated_at  timestamptz not null default now(),
    constraint system_settings_single_row check (id = 1)
);

insert into system_settings (id, data) values (1, '{}'::jsonb)
on conflict (id) do nothing;

-- ---------------------------------------------------------------
-- 应用元信息（版本、部署标记等）
-- ---------------------------------------------------------------
create table app_meta (
    key         text primary key,
    value       text not null,
    updated_at  timestamptz not null default now()
);

-- ---------------------------------------------------------------
-- 调用日志（M1 只建表，M6 才写入）
-- ---------------------------------------------------------------
create table call_records (
    id           bigserial   primary key,
    created_at   timestamptz not null default now(),
    method       text        not null,
    path         text        not null,
    status       int         not null,
    duration_ms  int         not null,
    account_id   bigint,
    request_id   uuid,
    error_code   text,
    detail       jsonb
);

create index call_records_created_at_idx on call_records (created_at desc);
create index call_records_status_idx      on call_records (status);
create index call_records_request_id_idx  on call_records (request_id) where request_id is not null;
