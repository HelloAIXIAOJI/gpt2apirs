-- 0014_drop_bucket_egress.sql
-- 出口统一走代理页那一个全局设置。
--
-- 曾经把出口提到桶上（buckets.proxy_url / proxy_node），结果是：
-- 桶的列表页被塞满出口输入框，账号上还留着一个「出口」概念，两处说法不一致。
-- 现在彻底收回：桶只管分组，代理只有全局一份。

alter table buckets drop column if exists proxy_node;
alter table buckets drop column if exists proxy_url;
