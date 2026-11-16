//! 参考图落盘：不连数据库，只验证写盘与清单结构。

use std::path::PathBuf;

/// 最小的 PNG 头：`sniff_mime` 只认魔术字节，够用来验证落盘路径。
const PNG_HEADER: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

#[test]
fn persists_known_image_and_skips_unknown() {
    // data_dir 读 DATA_DIR；先指到临时目录，避免污染真实数据目录。
    let dir = std::env::temp_dir().join(format!("gpt2apirs-refs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建临时目录");
    // edition 2024 里改环境变量是 unsafe：本测试独占该变量，安全。
    unsafe {
        std::env::set_var("DATA_DIR", &dir);
    }

    let items = vec![
        (PNG_HEADER.to_vec(), "ref.png".to_string()),
        (b"not an image".to_vec(), "junk.bin".to_string()),
    ];
    let persisted = gpt2apirs_api::references::persist_all(&items);
    assert_eq!(persisted.len(), 1, "只应落盘可识别的图片");
    let reference = &persisted[0];
    assert!(reference.rel_path.starts_with("references/"), "{}", reference.rel_path);
    assert_eq!(reference.mime, "image/png");

    let absolute: PathBuf = dir.join(reference.rel_path.replace('/', std::path::MAIN_SEPARATOR_STR));
    assert!(absolute.is_file(), "文件应存在: {}", absolute.display());
    assert_eq!(std::fs::metadata(&absolute).expect("元数据").len(), PNG_HEADER.len() as u64);

    let _ = std::fs::remove_dir_all(&dir);
}
