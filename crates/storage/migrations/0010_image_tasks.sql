-- 0010_image_tasks.sql
-- 异步生图任务落库：重启后历史仍可查，中断的 running 在启动时标失败。

create table if not exists image_tasks (
    id                uuid        primary key,
    status            text        not null,
    prompt            text        not null,
    model             text        not null,
    n                 int         not null,
    response_format   text        not null,
    output_format     text,
    frame_count       int,
    frame_duration_ms int,
    base_url          text        not null,
    progress          text,
    duration_ms       bigint,
    input_tokens      bigint,
    output_tokens     bigint,
    total_tokens      bigint,
    error             text,
    images            jsonb       not null default '[]'::jsonb,
    created_at        timestamptz not null default now(),
    updated_at        timestamptz not null default now(),
    finished_at       timestamptz
);

create index if not exists image_tasks_created_idx on image_tasks (created_at desc);
create index if not exists image_tasks_status_idx on image_tasks (status)
    where status in ('queued', 'running');
