-- 0012_log_detail.sql
-- 日志详情要能回答「谁调过来的、失败原因是什么」。
-- 以前只有 method/path/status/耗时：点开详情也说不出是谁打过来的。
-- 明文 key 不落库，只留前缀（sk-g2a-xxxx 这种），能对上人就行。

alter table call_records add column if not exists client_ip       text;
alter table call_records add column if not exists user_agent      text;
alter table call_records add column if not exists api_key_prefix  text;
alter table call_records add column if not exists error_message   text;

-- 按 IP 排查（刷接口、找某个调用方）比按路径更常用。
create index if not exists call_records_client_ip_idx on call_records (client_ip) where client_ip is not null;
