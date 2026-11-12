-- 0004_proxy_groups.sql
-- 代理分组：一组共享一个代理串。账号加入分组时把组地址复制到 accounts.proxy，
-- 组地址变更时同步重写组内账号 —— 不做运行时 join，保持账号查询路径不变。

create table proxy_groups (
    id         uuid        primary key,
    name       text        not null unique,
    url        text        not null default '',
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now()
);

alter table accounts add column proxy_group_id uuid references proxy_groups(id) on delete set null;
create index if not exists accounts_proxy_group_idx on accounts (proxy_group_id);
