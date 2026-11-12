//! 基础设施：配置 / 错误 / 日志脱敏 / 图片 token 估算 / 通用工具。

#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod config;
pub mod deadline;
pub mod env;
pub mod error;
pub mod logging;
pub mod paths;
pub mod settings;
pub mod redact;
pub mod tokens;
pub mod util;

pub use config::BootstrapConfig;
pub use deadline::{Deadline, DeadlineExceeded};
pub use env::{
    EnvError, ensure_loaded, find_env_file, load_dotenv, read_required, read_var, redact_url,
};
pub use error::{
    AppError, anthropic_error_payload, default_error_code, default_error_type,
    error_message_from_detail, openai_error_payload,
};
pub use logging::{LogFormat, init_logging, parse_levels};
pub use paths::{PathError, data_dir, ensure_data_dir, exe_dir};
pub use settings::{FieldMeta, NUMERIC_SPECS, NumericKind, NumericSpec, RuntimeSettings};
pub use redact::{is_base64_like, mask_base64, mask_secret, redact_line};
pub use util::{new_uuid, now_unix_secs_f64, now_utc};
pub use tokens::{
    count_generated_image_tokens, count_image_input_tokens, count_image_output_tokens,
    image_size_from_bytes, parse_image_size,
};