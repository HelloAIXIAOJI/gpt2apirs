-- 0009_image_asset_api_key.sql
-- 记录这张图是哪把 API key 生成的：key 删除后 id 置空，前缀保留用于追溯。

alter table image_assets
    add column if not exists api_key_id uuid references api_keys(id) on delete set null,
    add column if not exists api_key_prefix text;
create index if not exists image_assets_api_key_idx on image_assets (api_key_id);
