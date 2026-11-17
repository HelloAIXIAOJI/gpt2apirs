-- 0005_drop_proxy_groups.sql
-- 代理分组已废弃：批量设置账号代理由「全局出口 + 账号自身 proxy」取代。
-- 先删列再删表，避免外键先行报错。

alter table accounts drop column if exists proxy_group_id;
drop table if exists proxy_groups;
