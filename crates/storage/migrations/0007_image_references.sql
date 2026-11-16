-- 0007_image_references.sql
-- 参考图清单：生成的图片用了哪几张参考图（含本体落盘路径），归档与审计都靠它。

create table image_references (
    id         uuid        primary key,
    image_id   uuid        not null references image_assets(id) on delete cascade,
    ordinal    int         not null,
    rel_path   text        not null,
    mime       text        not null,
    bytes      bigint      not null,
    created_at timestamptz not null default now()
);
create index if not exists image_references_image_idx on image_references (image_id, ordinal);
