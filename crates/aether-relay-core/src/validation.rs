//! 通道配置校验（纯函数，无 I/O）

use crate::models::ChannelConfig;

/// 校验通道配置完整性
///
/// 返回 Err 时包含具体缺失字段名称
pub fn validate_channel_config(config: &ChannelConfig) -> Result<(), Vec<String>> {
    let mut missing_fields = Vec::new();

    if config.endpoint.trim().is_empty() {
        missing_fields.push("endpoint".to_string());
    }

    if config.keys.is_empty() {
        missing_fields.push("keys (at least one API key required)".to_string());
    } else {
        for (i, key) in config.keys.iter().enumerate() {
            if key.api_key.trim().is_empty() {
                missing_fields.push(format!("keys[{}].api_key", i));
            }
        }
    }

    if config.name.trim().is_empty() {
        missing_fields.push("name".to_string());
    }

    if missing_fields.is_empty() {
        Ok(())
    } else {
        Err(missing_fields)
    }
}
