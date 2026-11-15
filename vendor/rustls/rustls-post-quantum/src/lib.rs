//! The functionality of this crate became part of the core rustls
//! crate from the 0.23.22 release. When using that version of the crate,
//! use the `prefer-post-quantum` Cargo feature to control whether to prefer
//! using post-quantum algorithms instead of using this crate.

// 第三方代码：不参与本仓库的零告警要求。
#![allow(warnings)]

pub use rustls::crypto::aws_lc_rs::default_provider as provider;
pub use rustls::crypto::aws_lc_rs::kx_group::{MLKEM768, MLKEM1024, X25519MLKEM768};
