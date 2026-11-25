-- 0013_buckets.sql
-- 账号桶：把「一批账号」做成一等公民。
--
-- 为什么需要：以前代理挂在每个账号上，100 个号要填 100 次；
-- 现在代理挂在桶上，账号只记「我在哪个桶」。
-- key 也能绑桶：一个 key 只用某个桶的号（空 = 全局，不限桶）。

create table buckets (
    id         uuid        primary key,
    name       text        not null,
    -- 该桶用的出口节点名；空 = 跟随全局。节点名来自订阅，由代理页选择。
    proxy_node text        not null default '',
    -- 该桶用的自定义出口地址（http://host:port）；空 = 不用自定义出口。
    -- 与 proxy_node 二选一：设了地址就不看节点。
    proxy_url  text        not null default '',
    is_default boolean     not null default false,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now()
);

-- 桶名唯一（不区分大小写）：两个「默认」会让人分不清往哪导。
create unique index buckets_name_idx on buckets (lower(name));
-- 只允许一个默认桶。
create unique index buckets_one_default_idx on buckets ((is_default)) where is_default;

-- 默认桶用固定 ID：迁移、脚本、文档都能直接引用，不用先查一遍。
insert into buckets (id, name, is_default)
values ('00000000-0000-4000-8000-000000000001', '默认', true)
on conflict (id) do nothing;

alter table accounts add column if not exists bucket_id uuid references buckets(id) on delete restrict;
update accounts set bucket_id = '00000000-0000-4000-8000-000000000001' where bucket_id is null;
alter table accounts alter column bucket_id set not null;

-- 账号级的自定义出口提升为桶级：先搬再删，别让用户已经配好的出站悄悄没了。
-- 多个账号填了不同地址时只能保留一个（桶只有一个出口）；其余请在代理页重设。
update buckets set proxy_url = coalesce(
    (select proxy from accounts where proxy <> '' order by created_at limit 1), ''
) where is_default;

alter table accounts drop column if exists proxy;
create index if not exists accounts_bucket_idx on accounts (bucket_id);

-- key 绑定桶：空 = 全局（任何桶的账号都能用）。
alter table api_keys add column if not exists bucket_id uuid references buckets(id) on delete set null;
create index if not exists api_keys_bucket_idx on api_keys (bucket_id);
