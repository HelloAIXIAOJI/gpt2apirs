//! API key 的生成与校验。
//!
//! 调用 `/v1/*` 用 API key，登录后台用 `ADMIN_KEY`，两者互不通。
//! 这里只处理 API key：明文只在创建时出现一次，库里只留 sha256。

use rand::RngCore;
use sha2::{Digest, Sha256};

/// 明文前缀，便于在日志/列表里辨认这是本项目的 key。
pub const KEY_PREFIX: &str = "sk-g2a-";

/// 随机部分的字节数（编码成 48 个 hex 字符）。
const RANDOM_BYTES: usize = 24;

/// 列表里展示的前缀长度（含 `sk-g2a-`）。
const DISPLAY_PREFIX_LEN: usize = 16;

/// 新生成的一把 key。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedKey {
    /// 明文，返回给用户一次后不再保存。
    pub plaintext: String,
    /// sha256 小写 hex，入库用。
    pub sha256: String,
    /// 展示用前缀。
    pub prefix: String,
}

/// 生成一把新 key。
pub fn generate() -> GeneratedKey {
    let mut bytes = [0u8; RANDOM_BYTES];
    rand::thread_rng().fill_bytes(&mut bytes);
    let plaintext = format!("{KEY_PREFIX}{}", hex::encode(bytes));
    let sha256 = hash(&plaintext);
    let prefix = plaintext.chars().take(DISPLAY_PREFIX_LEN).collect();
    GeneratedKey {
        plaintext,
        sha256,
        prefix,
    }
}

/// 计算 sha256 小写 hex。
pub fn hash(plaintext: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(plaintext.as_bytes());
    hex::encode(hasher.finalize())
}

/// 形态是否像本项目的 key。
///
/// 只做快速拒绝，真正的校验靠哈希比对。
pub fn looks_valid(plaintext: &str) -> bool {
    let trimmed = plaintext.trim();
    trimmed.starts_with(KEY_PREFIX)
        && trimmed.len() == KEY_PREFIX.len() + RANDOM_BYTES * 2
        && trimmed[KEY_PREFIX.len()..].chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_key_has_prefix_and_matches_hash() {
        let key = generate();
        assert!(key.plaintext.starts_with(KEY_PREFIX));
        assert_eq!(key.sha256, hash(&key.plaintext));
        assert_eq!(key.sha256.len(), 64);
        assert!(key.plaintext.starts_with(&key.prefix));
        assert!(looks_valid(&key.plaintext));
    }

    #[test]
    fn two_keys_differ() {
        assert_ne!(generate().plaintext, generate().plaintext);
    }

    #[test]
    fn rejects_wrong_shape() {
        assert!(!looks_valid(""));
        assert!(!looks_valid("sk-g2a-short"));
        assert!(!looks_valid("sk-other-0000000000000000000000000000000000000000000000"));
        let mut weird = generate().plaintext;
        weird.pop();
        assert!(!looks_valid(&weird), "长度不足应拒绝");
    }

    #[test]
    fn hash_is_stable() {
        assert_eq!(hash("abc"), hash("abc"));
        assert_ne!(hash("abc"), hash("abd"));
    }
}
