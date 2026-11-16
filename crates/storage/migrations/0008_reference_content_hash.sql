-- 0008_reference_content_hash.sql
-- 参考图改为内容寻址：同内容只存一份，rel_path 由 sha256 决定。

alter table image_references add column if not exists sha256 text;
create index if not exists image_references_rel_path_idx on image_references (rel_path);
create index if not exists image_references_sha256_idx on image_references (sha256);
