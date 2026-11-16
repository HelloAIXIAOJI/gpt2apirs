-- 0006_image_archives.sql
-- 归档登记：每打一个包记一行，删掉原图后还能知道「历史上归档了什么、去哪了」。

create table image_archives (
    id          uuid        primary key,
    file_name   text        not null unique,
    bytes       bigint      not null,
    image_count int         not null,
    image_bytes bigint      not null,
    created_at  timestamptz not null default now()
);
create index if not exists image_archives_created_at_idx on image_archives (created_at desc);
