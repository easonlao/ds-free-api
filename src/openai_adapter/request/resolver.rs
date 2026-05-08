//! 模型解析 —— 将 OpenAI model 字段映射为 ds_core 能力标志
//!
//! 通过外部注入的 registry 实现模型别名到 model_type 的动态映射。

use std::collections::HashMap;

/// 模型解析结果
pub(crate) struct ModelResolution {
    /// ds_core 使用的 model_type
    pub model_type: String,
    pub thinking_enabled: bool,
}

/// 根据 model_id 和扩展参数解析模型配置
///
/// thinking_enabled 在 reasoning_effort 非 "none" 时启用。
/// 若 reasoning_effort 未提供，默认按 "high" 处理（即 reasoning 默认开启）。
pub(crate) fn resolve(
    registry: &HashMap<String, String>,
    model_id: &str,
    reasoning_effort: Option<&str>,
) -> Result<ModelResolution, String> {
    let key = model_id.to_lowercase();
    let model_type = registry
        .get(&key)
        .cloned()
        .ok_or_else(|| format!("不支持的模型: {}", model_id))?;

    let reasoning_effort = reasoning_effort.unwrap_or("high");
    let thinking_enabled = reasoning_effort != "none";

    Ok(ModelResolution {
        model_type,
        thinking_enabled,
    })
}
