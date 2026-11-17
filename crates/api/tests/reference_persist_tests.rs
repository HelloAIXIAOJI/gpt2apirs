//! 参考图落盘：内容寻址、去重、不可识别跳过。不连数据库。
//!
//! 两个断言写在同一个用例里：它们都要改进程级 `DATA_DIR`，并行跑会互相覆盖。

use std::path::PathBuf;

/// 最小 PNG 头：`sniff_mime` 只认魔术字节，够验证落盘行为。
const PNG_HEADER: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

fn count_files(root: &std::path::Path) -> usize {
    let mut count = 0usize;
    let mut stack = vec![root.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                count += 1;
            }
        }
    }
    count
}

#[test]
fn content_addressed_and_deduplicated() {
    let dir = std::env::temp_dir().join(format!("gpt2apirs-refs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建临时目录");
    unsafe {
        std::env::set_var("DATA_DIR", &dir);
    }

    // 1) 可识别图片落盘、不可识别跳过
    let items = vec![
        (PNG_HEADER.to_vec(), "ref.png".to_string()),
        (b"not an image".to_vec(), "junk.bin".to_string()),
    ];
    let persisted = gpt2apirs_api::references::persist_all(&items);
    assert_eq!(persisted.len(), 1, "只应落盘可识别的图片");
    let reference = &persisted[0];
    assert_eq!(reference.mime, "image/png");
    assert_eq!(reference.sha256.len(), 64, "sha256 应为 64 位十六进制");
    assert_eq!(
        reference.rel_path,
        format!(
            "references/{}/{}/{}.png",
            &reference.sha256[0..2],
            &reference.sha256[2..4],
            reference.sha256
        ),
        "路径应由内容哈希决定"
    );
    let absolute: PathBuf = dir.join(reference.rel_path.replace('/', std::path::MAIN_SEPARATOR_STR));
    assert!(absolute.is_file(), "文件应存在: {}", absolute.display());

    // 2) 同样内容再落一次：路径相同，磁盘上仍只有一个文件
    let again = gpt2apirs_api::references::persist_all(&[(PNG_HEADER.to_vec(), "copy.png".to_string())]);
    assert_eq!(again[0].rel_path, reference.rel_path, "同内容应得到同一路径");
    assert_eq!(count_files(&dir), 1, "同样内容只应落一个文件");

    let _ = std::fs::remove_dir_all(&dir);
}
