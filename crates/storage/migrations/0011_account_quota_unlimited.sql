-- 0011_account_quota_unlimited.sql
-- 上游的 limits_progress 里没有 image_gen 条目 = 这个号没有图片限额（付费号常见）。
-- 以前这种情况会写 quota = 0 并被判成「限流」，账号从此被跳过 —— 把「0 张」和「不限」分开记。

alter table accounts add column if not exists quota_unlimited boolean not null default false;

-- 选号索引也要认「不限量」：否则这些账号在不限量时 quota = 0，永远进不了热路径索引。
drop index if exists accounts_selectable_idx;
create index if not exists accounts_selectable_idx on accounts (status, quota)
    where status = 'normal' and (quota > 0 or quota_unlimited);
