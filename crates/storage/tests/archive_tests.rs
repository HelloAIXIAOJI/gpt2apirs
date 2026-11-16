//! 归档写入与校验的单测：只碰临时目录，不连数据库。

use std::path::PathBuf;

use chrono::Utc;
use gpt2apirs_storage::archive::{self, ArchiveEntry};

fn temp_root(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gpt2apirs-archive-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("images/2026/11/16")).expect("建目录");
    dir
}

fn entry(rel_path: &str, bytes: i64) -> ArchiveEntry {
    ArchiveEntry {
        rel_path: rel_path.to_string(),
        mime: "image/png".to_string(),
        bytes,
        width: 4,
        height: 4,
        account_id: None,
        prompt: Some("小红点".to_string()),
        created_at: Utc::now(),
        references: Vec::new(),
    }
}

#[test]
fn writes_manifest_and_images_then_verifies() {
    let root = temp_root("ok");
    let rel_a = "images/2026/11/16/a.png";
    let rel_b = "images/2026/11/16/b.png";
    std::fs::write(root.join(rel_a), vec![1u8; 32]).expect("写图");
    std::fs::write(root.join(rel_b), vec![2u8; 64]).expect("写图");
    let entries = vec![entry(rel_a, 32), entry(rel_b, 64)];

    let stats = archive::write_archive(&root.join("archives"), "a.tar.gz", &entries, &root)
        .expect("写包");
    assert_eq!(stats.image_count, 2);
    assert_eq!(stats.image_bytes, 96);
    assert!(stats.bytes > 0);

    let path = root.join("archives/a.tar.gz");
    assert!(path.is_file(), "包应存在");
    assert!(!root.join("archives/a.tar.gz.part").exists(), "不应残留 .part");
    archive::verify_archive(&path, 2).expect("包应能通过校验");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn verify_rejects_missing_images() {
    let root = temp_root("bad");
    let rel = "images/2026/11/16/a.png";
    std::fs::write(root.join(rel), vec![1u8; 8]).expect("写图");
    let stats = archive::write_archive(
        &root.join("archives"),
        "b.tar.gz",
        &[entry(rel, 8)],
        &root,
    )
    .expect("写包");
    // 声称有 2 张但包里只有 1 张：校验必须失败
    let path = root.join("archives/b.tar.gz");
    assert!(archive::verify_archive(&path, 2).is_err());
    assert_eq!(stats.image_count, 1);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn missing_source_file_aborts_without_leaving_part() {
    let root = temp_root("missing");
    let rel = "images/2026/11/16/gone.png";
    let result = archive::write_archive(
        &root.join("archives"),
        "c.tar.gz",
        &[entry(rel, 8)],
        &root,
    );
    assert!(result.is_err(), "源文件缺失应报错");
    assert!(!root.join("archives/c.tar.gz").exists(), "不该产生正式包");
    assert!(!root.join("archives/c.tar.gz.part").exists(), "不该残留临时文件");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn archive_includes_references() {
    let root = temp_root("refs");
    let rel = "images/2026/11/16/a.png";
    let ref_rel = "references/2026/11/16/r.png";
    std::fs::create_dir_all(root.join("references/2026/11/16")).expect("建参考图目录");
    std::fs::write(root.join(rel), vec![1u8; 16]).expect("写图");
    std::fs::write(root.join(ref_rel), vec![9u8; 24]).expect("写参考图");
    let mut item = entry(rel, 16);
    item.references = vec![gpt2apirs_storage::archive::ArchiveReference {
        ordinal: 1,
        rel_path: ref_rel.to_string(),
        mime: "image/png".to_string(),
        bytes: 24,
    }];

    let stats = archive::write_archive(&root.join("archives"), "r.tar.gz", &[item], &root)
        .expect("写包");
    assert_eq!(stats.image_count, 1);
    archive::verify_archive(&root.join("archives/r.tar.gz"), 1).expect("校验通过");

    // 包里应有一条 references/ 开头的条目
    let file = std::fs::File::open(root.join("archives/r.tar.gz")).expect("开包");
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let names: Vec<String> = archive
        .entries()
        .expect("读包")
        .map(|item| item.expect("条目").path().expect("路径").to_string_lossy().to_string())
        .collect();
    assert!(
        names.iter().any(|name| name.starts_with("references/")),
        "包里应包含参考图：{names:?}"
    );
    assert!(names.iter().any(|name| name == "manifest.json"), "{names:?}");
    let _ = std::fs::remove_dir_all(&root);
}
