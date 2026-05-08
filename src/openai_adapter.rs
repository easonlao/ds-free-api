//! OpenAI 协议适配层 —— OpenAI JSON 与 ds_core 内部格式的双向转换
//!
//! 本模块负责将 OpenAI 兼容的 HTTP 请求转换为 ds_core 内部格式，
//! 并将 ds_core 的响应转换为 OpenAI 兼容的 JSON 格式。
//!
//! 对外暴露最小接口：OpenAIAdapter, OpenAIAdapterError

use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use futures::{Stream, StreamExt};

use crate::ds_core::{CoreError, DeepSeekCore};
use std::collections::HashMap;

mod models;
pub(crate) mod request;
pub(crate) mod response;
pub(crate) mod types;

pub use types::{ChatCompletionsRequest, ChatCompletionsResponse, ChatCompletionsResponseChunk};

/// 流式响应类型（SSE 字节流）
pub type StreamResponse = Pin<Box<dyn Stream<Item = Result<Bytes, OpenAIAdapterError>> + Send>>;

/// 流式响应结构体流
pub type ChunkStream =
    Pin<Box<dyn Stream<Item = Result<ChatCompletionsResponseChunk, OpenAIAdapterError>> + Send>>;

/// Chat Completions 统一输出
pub enum ChatOutput {
    Stream(ChunkStream),
    Json(ChatCompletionsResponse),
}

/// adapter 层通用结果包装：携带请求结果和账号标识
pub struct ChatResult<T> {
    pub data: T,
    pub account_id: String,
    pub prompt_tokens: u32,
}

/// OpenAI 适配器
pub struct OpenAIAdapter {
    ds_core: Arc<DeepSeekCore>,
    model_types: tokio::sync::RwLock<Vec<String>>,
    model_registry: tokio::sync::RwLock<HashMap<String, String>>,
    model_aliases: tokio::sync::RwLock<Vec<String>>,
    max_input_tokens: tokio::sync::RwLock<Vec<u32>>,
    max_output_tokens: tokio::sync::RwLock<Vec<u32>>,
    tag_config: tokio::sync::RwLock<Arc<response::TagConfig>>,
    /// 缓存的 tiktoken BPE 编码器（避免每次请求重建）
    bpe: Option<Arc<tiktoken_rs::CoreBPE>>,
}

impl OpenAIAdapter {
    /// 创建适配器实例
    pub async fn new(config: &crate::config::Config) -> Result<Self, OpenAIAdapterError> {
        let ds_core = Arc::new(DeepSeekCore::new(config).await?);
        let model_registry = config.deepseek.model_registry();
        // 预初始化 tiktoken BPE（避免每次请求重建词表）
        let bpe = tiktoken_rs::cl100k_base().ok().map(Arc::new);

        Ok(Self {
            ds_core,
            model_types: tokio::sync::RwLock::new(config.deepseek.model_types.clone()),
            model_registry: tokio::sync::RwLock::new(model_registry),
            model_aliases: tokio::sync::RwLock::new(config.deepseek.model_aliases.clone()),
            max_input_tokens: tokio::sync::RwLock::new(config.deepseek.max_input_tokens.clone()),
            max_output_tokens: tokio::sync::RwLock::new(config.deepseek.max_output_tokens.clone()),
            tag_config: tokio::sync::RwLock::new(Arc::new(response::TagConfig::from_config(
                &config.deepseek.tool_call,
            ))),
            bpe,
        })
    }

    /// POST /v1/chat/completions（统一入口）
    ///
    /// 内部校验参数、构建 ChatML prompt、按 stream 标记分流：
    /// - stream=true  → 返回 SSE 字节流
    /// - stream=false → 将 SSE 流聚合为单个 JSON 对象后返回
    pub async fn chat_completions(
        &self,
        mut req: ChatCompletionsRequest,
        request_id: &str,
    ) -> Result<ChatResult<ChatOutput>, OpenAIAdapterError> {
        log::debug!(target: "adapter", "req={} 适配器开始处理: model={}, stream={}", request_id, req.model, req.stream);
        use crate::openai_adapter::types::{
            FunctionCallOption, NamedFunction, NamedToolChoice, Tool, ToolChoice,
        };

        // 兼容旧版 functions / function_call → tools / tool_choice
        if req.tools.as_ref().map(|t| t.is_empty()).unwrap_or(true)
            && let Some(functions) = req.functions.clone()
            && !functions.is_empty()
        {
            req.tools = Some(
                functions
                    .into_iter()
                    .map(|f| Tool {
                        ty: "function".to_string(),
                        function: Some(f),
                        custom: None,
                    })
                    .collect(),
            );
        }
        if req.tool_choice.is_none()
            && let Some(fc) = req.function_call.clone()
        {
            req.tool_choice = Some(match fc {
                FunctionCallOption::Mode(mode) => ToolChoice::Mode(mode),
                FunctionCallOption::Named(named) => ToolChoice::Named(NamedToolChoice {
                    ty: "function".to_string(),
                    function: NamedFunction { name: named.name },
                }),
            });
        }

        let norm = request::normalize::apply(&req).map_err(OpenAIAdapterError::BadRequest)?;
        let tool_ctx = request::tools::extract(&req).map_err(OpenAIAdapterError::BadRequest)?;
        let prompt = request::prompt::build(&req, &tool_ctx);
        let registry = self.model_registry.read().await;
        let model_res = request::resolver::resolve(
            &registry,
            &req.model,
            req.reasoning_effort.as_deref(),
        )
        .map_err(OpenAIAdapterError::BadRequest)?;

        let prompt_tokens = self
            .bpe
            .as_ref()
            .map(|bpe| bpe.encode_with_special_tokens(&prompt).len() as u32)
            .unwrap_or(0);

        let file_result = request::files::extract(&req);
        // 保存原始 prompt、model_type、files 用于工具调用自修正重试
        let base_prompt = prompt.clone();
        let model_type = model_res.model_type.clone();
        let retry_files = file_result.files.clone();

        let chat_req = crate::ds_core::ChatRequest {
            prompt,
            thinking_enabled: model_res.thinking_enabled,
            search_enabled: file_result.has_http_urls,
            model_type: model_res.model_type,
            files: file_result.files,
        };

        let chat_resp = self.try_chat(chat_req, request_id).await?;
        let account_id = chat_resp.account_id;
        let chatcmpl_id = crate::openai_adapter::response::next_chatcmpl_id();

        if req.stream {
            let s = response::stream(
                chat_resp.stream,
                req.model,
                response::StreamCfg {
                    include_usage: norm.include_usage,
                    include_obfuscation: norm.include_obfuscation,
                    stop: norm.stop,
                    prompt_tokens,
                    tag_config: self.tag_config.read().await.clone(),
                    chatcmpl_id: chatcmpl_id.clone(),
                },
            );
            Ok(ChatResult {
                data: ChatOutput::Stream(s),
                account_id,
                prompt_tokens,
            })
        } else {
            // 非流式响应：带工具调用自修正重试
            let max_retries: usize = 3;
            let search_enabled = file_result.has_http_urls;
            let stop = norm.stop.clone();

            // 首次尝试
            let agg_result = response::aggregate(
                chat_resp.stream,
                req.model.clone(),
                response::StreamCfg {
                    include_usage: true,
                    include_obfuscation: false,
                    stop: stop.clone(),
                    prompt_tokens,
                    tag_config: self.tag_config.read().await.clone(),
                    chatcmpl_id: chatcmpl_id.clone(),
                },
            )
            .await;

            if let Err(OpenAIAdapterError::ToolCallRepairNeeded(_)) = &agg_result {
                log::warn!(target: "adapter", "req={} 工具调用标签泄漏, 启动自修正重试",
                    request_id);

                for retry_idx in 1..max_retries {
                    let retry_prompt = format!(
                        "{}<｜end▁of▁sentence｜>\n\
                        <｜User｜>上一轮的输出中包含了格式错误的工具调用。\
                        请严格按照规范重新输出工具调用，\
                        只输出 `<|tool_calls_begin|>[{{...}}]<|tool_calls_end|>` 格式的工具调用，\
                        不要输出任何解释文字。\n\
                        <|Assistant|><think>\n",
                        base_prompt
                    );

                    let retry_req = crate::ds_core::ChatRequest {
                        prompt: retry_prompt,
                        thinking_enabled: model_res.thinking_enabled,
                        search_enabled,
                        model_type: model_type.clone(),
                        files: retry_files.clone(),
                    };

                    match self.try_chat(retry_req, request_id).await {
                        Ok(retry_resp) => {
                            match response::aggregate(
                                retry_resp.stream,
                                req.model.clone(),
                                response::StreamCfg {
                                    include_usage: true,
                                    include_obfuscation: false,
                                    stop: stop.clone(),
                                    prompt_tokens: 0,
                                    tag_config: self.tag_config.read().await.clone(),
                                    chatcmpl_id: chatcmpl_id.clone(),
                                },
                            )
                            .await
                            {
                                Ok(json) => {
                                    log::info!(target: "adapter",
                                        "req={} 第{}次修正重试成功", request_id, retry_idx);
                                    return Ok(ChatResult {
                                        data: ChatOutput::Json(json),
                                        account_id: retry_resp.account_id,
                                        prompt_tokens,
                                    });
                                }
                                Err(OpenAIAdapterError::ToolCallRepairNeeded(_)) => {
                                    log::warn!(target: "adapter",
                                        "req={} 第{}次修正重试仍失败", request_id, retry_idx);
                                    continue;
                                }
                                Err(e) => return Err(e),
                            }
                        }
                        Err(e) => {
                            log::warn!(target: "adapter",
                                "req={} 修正重试 try_chat 失败: {}", request_id, e);
                            continue;
                        }
                    }
                }

                return Err(OpenAIAdapterError::Internal(
                    "工具调用自修正失败：多次重试后工具调用格式仍然无法解析".into(),
                ));
            }

            // 首次尝试成功，或返回非 ToolCallRepairNeeded 错误
            agg_result.map(|json| ChatResult {
                data: ChatOutput::Json(json),
                account_id,
                prompt_tokens,
            })
        }
    }

    /// 内部辅助：对 `Overloaded` 进行退避重试（v0_chat 内部已做换号重试，此处为号池级兜底）
    pub(crate) async fn try_chat(
        &self,
        req: crate::ds_core::ChatRequest,
        request_id: &str,
    ) -> Result<crate::ds_core::ChatResponse, CoreError> {
        const MAX_RETRIES: usize = 2;
        const BASE_DELAY_MS: u64 = 2000;

        for attempt in 0..MAX_RETRIES {
            match self.ds_core.v0_chat(req.clone(), request_id).await {
                Ok(resp) => {
                    if attempt > 0 {
                        log::info!(target: "adapter", "req={} 第 {} 次重试成功", request_id, attempt);
                    }
                    return Ok(resp);
                }
                Err(CoreError::Overloaded) if attempt + 1 < MAX_RETRIES => {
                    let delay = BASE_DELAY_MS * (1 << attempt);
                    log::warn!(target: "adapter", "req={} Overloaded, 第 {} 次重试等待 {}ms", request_id, attempt + 1, delay);
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
                Err(e) => return Err(e),
            }
        }
        log::warn!(target: "adapter", "req={} {} 次重试均失败，放弃", request_id, MAX_RETRIES);
        Err(CoreError::Overloaded)
    }

    /// GET /v1/models
    pub async fn list_models(&self) -> types::OpenAIModelList {
        let model_types = self.model_types.read().await;
        let max_input = self.max_input_tokens.read().await;
        let max_output = self.max_output_tokens.read().await;
        let aliases = self.model_aliases.read().await;
        models::list(&model_types, &max_input, &max_output, &aliases)
    }

    /// GET /v1/models/{model_id}
    pub async fn get_model(&self, model_id: &str) -> Option<types::OpenAIModel> {
        let model_types = self.model_types.read().await;
        let max_input = self.max_input_tokens.read().await;
        let max_output = self.max_output_tokens.read().await;
        let aliases = self.model_aliases.read().await;
        models::get(&model_types, &max_input, &max_output, &aliases, model_id)
    }

    /// 原始 DeepSeek SSE 流（不经 OpenAI 协议转换）
    ///
    /// 用于流分析：对比原始响应与 OpenAI 转换后的差异，定位转换 bug
    pub async fn raw_chat_completions_stream(
        &self,
        body: &[u8],
        request_id: &str,
    ) -> Result<ChatResult<StreamResponse>, OpenAIAdapterError> {
        let chat_req: ChatCompletionsRequest = serde_json::from_slice(body)
            .map_err(|e| OpenAIAdapterError::BadRequest(format!("bad request: {}", e)))?;
        let registry = self.model_registry.read().await;
        let model_res = request::resolver::resolve(
            &registry,
            &chat_req.model,
            chat_req.reasoning_effort.as_deref(),
        )
        .map_err(OpenAIAdapterError::BadRequest)?;
        let ds_req = crate::ds_core::ChatRequest {
            prompt: request::prompt::build(
                &chat_req,
                &request::tools::extract(&chat_req).map_err(OpenAIAdapterError::BadRequest)?,
            ),
            thinking_enabled: model_res.thinking_enabled,
            search_enabled: false,
            model_type: model_res.model_type,
            files: vec![],
        };
        let chat_resp = self.try_chat(ds_req, request_id).await?;
        let data = Box::pin(
            chat_resp
                .stream
                .map(|r| r.map_err(OpenAIAdapterError::from)),
        );
        Ok(ChatResult {
            data,
            account_id: chat_resp.account_id,
            prompt_tokens: 0,
        })
    }

    /// 获取 ds_core 账号池状态
    pub fn account_statuses(&self) -> Vec<crate::ds_core::AccountStatus> {
        self.ds_core.account_statuses()
    }

    /// 动态添加账号
    pub async fn add_account(
        &self,
        creds: &crate::config::Account,
    ) -> Result<String, crate::ds_core::PoolError> {
        self.ds_core.add_account(creds).await
    }

    /// 动态移除账号
    pub async fn remove_account(
        &self,
        email_or_mobile: &str,
    ) -> Result<String, crate::ds_core::PoolError> {
        self.ds_core.remove_account(email_or_mobile).await
    }

    /// 标记账号为 Error 状态
    pub fn mark_error(&self, email_or_mobile: &str) {
        self.ds_core.mark_error(email_or_mobile)
    }

    /// 手动重新登录指定账号
    pub async fn re_login_single(&self, email_or_mobile: &str) -> Result<(), String> {
        self.ds_core.re_login_single(email_or_mobile).await
    }
}

impl OpenAIAdapter {
    /// 批量同步账号：对比当前账号池与目标配置，增删差异账号
    pub(crate) async fn sync_accounts(&self, new_accounts: &[crate::config::Account]) {
        let old_statuses = self.account_statuses();
        let old_ids: Vec<String> = old_statuses
            .iter()
            .map(|a| {
                if !a.email.is_empty() {
                    a.email.clone()
                } else {
                    a.mobile.clone()
                }
            })
            .collect();

        let mut _added = 0usize;
        let mut _failed = 0usize;
        for acct in new_accounts {
            let id = if !acct.email.is_empty() {
                &acct.email
            } else {
                &acct.mobile
            };
            if !old_ids.contains(id) {
                match self.add_account(acct).await {
                    Ok(_) => _added += 1,
                    Err(e) => {
                        log::warn!(target: "adapter", "同步添加账号 {} 失败: {}", id, e);
                        _failed += 1;
                    }
                }
            }
        }

        let mut _removed = 0usize;
        let new_ids: Vec<&str> = new_accounts
            .iter()
            .map(|a| {
                if !a.email.is_empty() {
                    a.email.as_str()
                } else {
                    a.mobile.as_str()
                }
            })
            .collect();
        for old_id in &old_ids {
            if !new_ids.contains(&old_id.as_str()) && !old_id.is_empty() {
                match self.remove_account(old_id).await {
                    Ok(_) => _removed += 1,
                    Err(e) => {
                        log::warn!(target: "adapter", "同步移除账号 {} 失败: {}", old_id, e);
                    }
                }
            }
        }
    }

    /// 优雅关闭
    pub async fn shutdown(&self) {
        self.ds_core.shutdown().await;
    }

    pub async fn reload_config(&self, new_config: &crate::config::Config) -> Result<(), CoreError> {
        // Sync accounts
        self.sync_accounts(&new_config.accounts).await;
        // Rebuild model registry
        let registry = new_config.deepseek.model_registry();
        *self.model_registry.write().await = registry;
        *self.model_types.write().await = new_config.deepseek.model_types.clone();
        *self.model_aliases.write().await = new_config.deepseek.model_aliases.clone();
        *self.max_input_tokens.write().await = new_config.deepseek.max_input_tokens.clone();
        *self.max_output_tokens.write().await = new_config.deepseek.max_output_tokens.clone();
        *self.tag_config.write().await = Arc::new(response::TagConfig::from_config(
            &new_config.deepseek.tool_call,
        ));
        // Rebuild DsClient if needed (deepseek/proxy changes)
        self.ds_core.reload_config(new_config).await
    }

}

/// 适配器错误类型
#[derive(Debug, thiserror::Error)]
pub enum OpenAIAdapterError {
    /// 请求格式错误
    #[error("bad request: {0}")]
    BadRequest(String),

    /// 服务过载，无可用的 ds_core 账号
    #[error("service overloaded")]
    Overloaded,

    /// 上游提供商错误（网络、业务错误等）
    #[error("provider error: {0}")]
    ProviderError(String),

    /// 内部错误（序列化、流转换等）
    #[error("internal error: {0}")]
    Internal(String),

    /// tool_calls 标记解析失败，携带 `{TOOL_CALL_START}...{TOOL_CALL_END}` 内的原始文本
    #[error("tool_calls repair needed: {0}")]
    ToolCallRepairNeeded(String),
}

impl From<CoreError> for OpenAIAdapterError {
    fn from(e: CoreError) -> Self {
        match e {
            CoreError::Overloaded => Self::Overloaded,
            CoreError::ProofOfWorkFailed(err) => {
                Self::Internal(format!("proof of work failed: {}", err))
            }
            CoreError::ProviderError(msg) => Self::ProviderError(msg),
            CoreError::Stream(msg) => Self::Internal(msg),
        }
    }
}

impl From<serde_json::Error> for OpenAIAdapterError {
    fn from(e: serde_json::Error) -> Self {
        Self::Internal(format!("json serialization failed: {}", e))
    }
}

impl OpenAIAdapterError {
    /// 返回对应 HTTP 状态码
    pub fn status_code(&self) -> u16 {
        match self {
            Self::BadRequest(_) => 400,
            Self::Overloaded => 429,
            Self::ProviderError(_) => 502,
            Self::Internal(_) => 500,
            Self::ToolCallRepairNeeded(_) => 500,
        }
    }
}
