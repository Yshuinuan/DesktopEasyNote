//! DeepSeek Chat Completions 客户端（reqwest 异步 + tauri 运行时桥接）。
//!
//! 契约（任务书 R6）：
//! - 从配置读 baseURL / model，请求 `POST {baseURL}/chat/completions`，messages 含 system + user；
//! - 超时 60 秒，超时必须可失败退出；
//! - 非 2xx 记状态码 + 响应体摘要（**不记 key**）；
//! - 解析第一 choice 的 `message.content`；结构异常按失败处理。
//!
//! 不新增 Cargo 依赖：reqwest 是异步客户端，用 `tauri::async_runtime::block_on`
//! 在后台线程里同步等待，避免引入 tokio 依赖。

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// 单次 HTTP 请求超时（R6：超时必须可失败退出，不无限挂起）。
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// 建立连接超时，避免网络黑洞时长时间占用。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// 一次 Chat Completions 调用的输入。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatRequest {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    pub system: String,
    pub user: String,
}

/// 调用失败分类；`summary()` 只输出可写日志的内容，绝不包含 key。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatError {
    /// 非 2xx 响应
    Http { status: u16, body: String },
    /// 网络/超时/连接错误
    Network(String),
    /// 响应结构异常（不是预期 JSON、没有 choices 等）
    Shape(String),
}

impl ChatError {
    /// 面向日志与汇报的错误摘要（截断到 160 字符，不含 key/请求头）。
    pub fn summary(&self) -> String {
        match self {
            ChatError::Http { status, body } => {
                let preview = preview_text(body, 160);
                if preview.is_empty() {
                    format!("HTTP {status}")
                } else {
                    format!("HTTP {status}: {preview}")
                }
            }
            ChatError::Network(message) => format!("网络错误: {}", preview_text(message, 160)),
            ChatError::Shape(message) => format!("响应异常: {}", preview_text(message, 160)),
        }
    }
}

/// 拼出 reqwest 错误的完整原因链（顶层信息太笼统，排错需要底层原因）。
fn describe_reqwest_error(error: &reqwest::Error) -> String {
    let mut message = error.to_string();
    let mut source: Option<&(dyn std::error::Error + 'static)> =
        std::error::Error::source(error);
    while let Some(cause) = source {
        message.push_str(" / ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

fn preview_text(text: &str, max_chars: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max_chars {
        collapsed
    } else {
        let mut truncated = collapsed.chars().take(max_chars).collect::<String>();
        truncated.push('…');
        truncated
    }
}

/// 可注入的 HTTP 客户端（测试用假实现，生产用 reqwest）。
pub trait ChatClient: Send + Sync {
    fn complete(&self, request: &ChatRequest) -> Result<String, ChatError>;
}

/// 生产实现：reqwest 异步 + tauri 全局运行时 `block_on`。
pub struct ReqwestChatClient {
    client: reqwest::Client,
}

impl ReqwestChatClient {
    pub fn new() -> Self {
        Self::with_settings(REQUEST_TIMEOUT, false)
    }

    /// `disable_proxy`：测试本地 mock HTTP 服务时禁用系统代理，避免 127.0.0.1 被代理劫持。
    pub fn with_settings(timeout: Duration, disable_proxy: bool) -> Self {
        let mut builder = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(CONNECT_TIMEOUT.min(timeout));
        if disable_proxy {
            builder = builder.no_proxy();
        }

        let client = builder.build().unwrap_or_else(|error| {
            crate::log_warn!("HTTP 客户端初始化失败，回退默认客户端: {}", error);
            reqwest::Client::new()
        });
        Self { client }
    }
}

impl Default for ReqwestChatClient {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatClient for ReqwestChatClient {
    fn complete(&self, request: &ChatRequest) -> Result<String, ChatError> {
        let url = format!("{}/chat/completions", request.base_url.trim_end_matches('/'));
        let body = ChatBody {
            model: request.model.as_str(),
            messages: vec![
                ChatMessage {
                    role: "system",
                    content: request.system.as_str(),
                },
                ChatMessage {
                    role: "user",
                    content: request.user.as_str(),
                },
            ],
            stream: false,
        };
        let payload = serde_json::to_vec(&body)
            .map_err(|error| ChatError::Shape(format!("请求序列化失败: {error}")))?;

        let client = self.client.clone();
        let api_key = request.api_key.clone();

        tauri::async_runtime::block_on(async move {
            let response = client
                .post(url)
                .header("Authorization", format!("Bearer {api_key}"))
                .header("Content-Type", "application/json")
                .body(payload)
                .send()
                .await
                .map_err(|error| ChatError::Network(describe_reqwest_error(&error)))?;

            let status = response.status().as_u16();
            let text = response
                .text()
                .await
                .map_err(|error| ChatError::Network(describe_reqwest_error(&error)))?;

            if !(200..300).contains(&status) {
                return Err(ChatError::Http {
                    status,
                    body: text,
                });
            }

            parse_response(&text)
        })
    }
}

#[derive(Serialize)]
struct ChatBody<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    stream: bool,
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

/// 解析 Chat Completions 响应，取第一 choice 的 `message.content`。
///
/// `content` 为 `null` 时返回空串，由上层按「正文为空」失败处理（R14）。
fn parse_response(raw: &str) -> Result<String, ChatError> {
    #[derive(Deserialize)]
    struct Response {
        choices: Vec<Choice>,
    }

    #[derive(Deserialize)]
    struct Choice {
        message: Message,
    }

    #[derive(Deserialize)]
    struct Message {
        content: Option<String>,
    }

    let parsed: Response = serde_json::from_str(raw)
        .map_err(|error| ChatError::Shape(format!("响应不是预期 JSON: {error}")))?;
    let choice = parsed
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| ChatError::Shape("响应缺少 choices".to_string()))?;

    Ok(choice.message.content.unwrap_or_default())
}
