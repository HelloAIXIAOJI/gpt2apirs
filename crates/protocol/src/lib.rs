//! 上游协议逆向：PoW / Sentinel / SSE / 24 端点
//!
//!
//! 当前进度：
//! - PoW 求解（V-2）
//! - PoW 配置构造 + 资源解析

#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod conversation;
pub mod conversations;
pub mod download;
pub mod endpoints;
pub mod memories;
pub mod pow;
pub mod pow_config;
pub mod search;
pub mod sentinel;
pub mod sse;
pub mod sse_framer;
pub mod text_stream;
pub mod turnstile;
pub mod util;

pub use pow::{
    DEFAULT_LIMIT, LEGACY_REQUIREMENTS_PREFIX, PROOF_TOKEN_PREFIX, PowAnswer,
    build_legacy_requirements_token, build_proof_token, pow_generate, python_float_repr,
    python_json_compact,
};
pub use download::{
    attachment_download_path, file_download_path, is_same_origin, parse_download_url,
    plan_download_targets, should_skip_file_id,
};
pub use conversation::{
    DEFAULT_MODEL, build_conversation_payload, build_conversation_prepare_payload,
    parse_conduit_token,
};
pub use endpoints::{
    BASE_URL, BASE_URL_ENV, CONVERSATION, CONVERSATION_PREPARE, SENTINEL_FINALIZE,
    SENTINEL_PREPARE, base_url, base_url_override, url,
};
pub use sentinel::{
    PowChallenge, SentinelFinalize, SentinelPrepare, build_finalize_payload,
    build_prepare_payload, parse_finalize, parse_prepare,
};
pub use sse::{
    ConversationEvent, ConversationEventKind, ConversationState, extract_conversation_ids,
    iter_sse_payloads, parse_conversation_stream, update_conversation_state,
};
pub use pow_config::{
    CORES, DEFAULT_POW_SCRIPT, DOCUMENT_KEYS, NAVIGATOR_KEYS, PowConfigContext, SCREEN_RESOLUTIONS,
    WINDOW_KEYS, build_pow_config, build_pow_config_with, legacy_parse_time, parse_pow_resources,
};