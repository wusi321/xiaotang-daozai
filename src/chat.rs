//! AI 对话：OpenAI 兼容接口、SSE 流式与 Windows 凭据保管。
//!
//! 界面只负责发请求、消费事件；联网在 [`Service`] 的后台线程完成。
//! 密钥只从 Windows 凭据管理器读取，不写配置文件、不进日志。

use serde_json::{json, Value};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
use base64::Engine;

const SERVICE: &str = "Daozai.AIChat";
/// 单次回答最多保留的字符数，防止异常服务端无限刷屏。
const MAX_ANSWER: usize = 64 * 1024;
/// 随请求携带的历史上限（字符），超出时从最早的一轮开始丢。
const MAX_CONTEXT: usize = 24 * 1024;
/// 单次请求的最大时长，兜住没有整体超时的流式读取。
const MAX_SECONDS: u64 = 300;
/// 一条消息最多带几个附件；图片与文本都有大小上限，避免一次请求过大。
pub const MAX_ATTACHMENTS: usize = 5;
const MAX_IMAGES: usize = 4;
const MAX_IMAGE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_TEXT_BYTES: usize = 64 * 1024;
pub const DEFAULT_SYSTEM: &str = "你是岛仔，一只住在桌面小岛上的伙伴。回答简洁、直接，默认用中文。";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Preset {
    #[default]
    DeepSeek,
    Kimi,
    OpenAi,
    Custom,
}

impl Preset {
    pub const ALL: [Self; 4] = [Self::DeepSeek, Self::Kimi, Self::OpenAi, Self::Custom];
    pub fn label(self) -> &'static str {
        match self { Self::DeepSeek => "DeepSeek", Self::Kimi => "Kimi", Self::OpenAi => "OpenAI", Self::Custom => "自定义" }
    }
    pub fn key(self) -> &'static str {
        match self { Self::DeepSeek => "deepseek", Self::Kimi => "kimi", Self::OpenAi => "openai", Self::Custom => "custom" }
    }
    pub fn parse(value: &str) -> Self {
        Self::ALL.into_iter().find(|preset| preset.key() == value).unwrap_or_else(|| Self::ALL.into_iter().find(|preset| preset.label().eq_ignore_ascii_case(value)).unwrap_or_default())
    }
    pub fn base_url(self) -> &'static str {
        match self { Self::DeepSeek => "https://api.deepseek.com/v1", Self::Kimi => "https://api.moonshot.cn/v1", Self::OpenAi => "https://api.openai.com/v1", Self::Custom => "" }
    }
    pub fn model(self) -> &'static str {
        match self { Self::DeepSeek => "deepseek-chat", Self::Kimi => "moonshot-v1-8k", Self::OpenAi => "gpt-4o-mini", Self::Custom => "" }
    }
}

/// 连接配置：只保存地址、模型和凭据引用，密钥本体不在这里。
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub preset: Preset,
    pub base_url: String,
    pub model: String,
    pub system: String,
    pub temperature: f32,
    pub max_tokens: u32,
    /// 允许岛仔在发送时读取附件内容（图片按 Base64、文本内联）；
    /// 关闭时只把路径告诉模型。
    pub read_attachments: bool,
    /// Windows 凭据管理器里的槽位；`None` 表示还没保存过密钥。
    pub slot: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            preset: Preset::DeepSeek,
            base_url: Preset::DeepSeek.base_url().into(),
            model: Preset::DeepSeek.model().into(),
            system: DEFAULT_SYSTEM.into(),
            temperature: 0.7,
            max_tokens: 1024,
            read_attachments: false,
            slot: None,
        }
    }
}

impl Config {
    /// 真正的请求地址：允许用户直接填到 `/chat/completions`。
    pub fn endpoint(&self) -> String {
        let base = self.base_url.trim().trim_end_matches('/');
        if base.ends_with("/chat/completions") { base.to_string() } else { format!("{base}/chat/completions") }
    }

    pub fn validate(&self) -> Result<(), String> {
        let url = self.base_url.trim().to_ascii_lowercase();
        if url.is_empty() {
            return Err("请填写接口地址（Base URL）".into());
        }
        let loopback = url.starts_with("http://127.0.0.1") || url.starts_with("http://localhost") || url.starts_with("http://[::1]");
        if !url.starts_with("https://") && !loopback {
            return Err("接口地址需要以 https:// 开头；本机模型可用 http://127.0.0.1".into());
        }
        if self.model.trim().is_empty() {
            return Err("请填写模型名".into());
        }
        Ok(())
    }

    /// 切换服务商时填入默认地址与模型（自定义保留用户已填内容）。
    pub fn apply_preset(&mut self, preset: Preset) {
        self.preset = preset;
        if preset != Preset::Custom {
            self.base_url = preset.base_url().into();
            self.model = preset.model().into();
            self.slot = None;
        }
    }

    pub fn load() -> Self {
        let path = crate::model::config_dir().join("ai-chat.json");
        let Ok(raw) = std::fs::read(&path) else { return Self::default(); };
        serde_json::from_slice::<Value>(&raw).ok().map(|value| Self::from_json(&value)).unwrap_or_default()
    }

    fn from_json(value: &Value) -> Self {
        let mut config = Self::default();
        config.preset = Preset::parse(value["preset"].as_str().unwrap_or("deepseek"));
        if let Some(url) = value["base_url"].as_str().filter(|url| !url.trim().is_empty()) { config.base_url = url.into(); }
        if let Some(model) = value["model"].as_str().filter(|model| !model.trim().is_empty()) { config.model = model.into(); }
        if let Some(system) = value["system"].as_str() { config.system = system.into(); }
        if let Some(temperature) = value["temperature"].as_f64() { config.temperature = (temperature as f32).clamp(0.0, 2.0); }
        if let Some(max_tokens) = value["max_tokens"].as_u64() { config.max_tokens = (max_tokens as u32).clamp(64, 8192); }
        config.read_attachments = value["read_attachments"].as_bool().unwrap_or(false);
        config.slot = value["slot"].as_str().filter(|slot| !slot.is_empty()).map(str::to_owned);
        config
    }

    fn json(&self) -> Value {
        json!({
            "preset": self.preset.key(),
            "base_url": self.base_url,
            "model": self.model,
            "system": self.system,
            "temperature": self.temperature,
            "max_tokens": self.max_tokens,
            "read_attachments": self.read_attachments,
            "slot": self.slot,
        })
    }

    pub fn save(&self) {
        let dir = crate::model::config_dir();
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(raw) = serde_json::to_vec_pretty(&self.json()) {
            let _ = crate::model::atomic_write(&dir.join("ai-chat.json"), &raw);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role { System, User, Assistant }

impl Role {
    pub fn api(self) -> &'static str {
        match self { Self::System => "system", Self::User => "user", Self::Assistant => "assistant" }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Outcome { Streaming, Ok, Stopped, Failed(String) }

/// 附件只记录本机路径，不复制文件；是否读取内容由配置决定。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachmentKind { Image, Text, Other }

impl AttachmentKind {
    pub fn of(path: &Path) -> Self {
        let extension = path.extension().and_then(|value| value.to_str()).unwrap_or("").to_ascii_lowercase();
        match extension.as_str() {
            "png" | "jpg" | "jpeg" | "webp" => Self::Image,
            "txt" | "md" | "json" | "rs" | "py" | "js" | "ts" | "tsx" | "jsx" | "html" | "htm"
            | "css" | "csv" | "log" | "yaml" | "yml" | "toml" | "xml" | "ini" | "cfg"
            | "sh" | "ps1" | "bat" | "c" | "h" | "cpp" | "java" | "go" | "kt" | "swift" | "sql" => Self::Text,
            _ => Self::Other,
        }
    }
    pub fn label(self) -> &'static str {
        match self { Self::Image => "图片", Self::Text => "文本", Self::Other => "文件" }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Attachment {
    pub path: PathBuf,
    pub kind: AttachmentKind,
}

impl Attachment {
    pub fn of(path: PathBuf) -> Self { Self { kind: AttachmentKind::of(&path), path } }
    pub fn name(&self) -> String {
        self.path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_else(|| self.path.display().to_string())
    }
    pub fn size(&self) -> u64 { std::fs::metadata(&self.path).map(|meta| meta.len()).unwrap_or(0) }
}

/// 随消息一起发给模型的路径清单：即使不读取内容，模型也知道附件在哪里。
pub fn path_summary(attachments: &[Attachment]) -> String {
    if attachments.is_empty() { return String::new(); }
    let mut out = String::from("\n\n[附件]\n");
    for attachment in attachments {
        out.push_str(&format!("- {}（{}，{}）\n", attachment.path.display(), attachment.kind.label(), crate::model::human_size(attachment.size())));
    }
    out.pop();
    out
}

#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    pub role: Role,
    pub text: String,
    pub reasoning: String,
    pub outcome: Outcome,
    pub attachments: Vec<Attachment>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self { role: Role::User, text: text.into(), reasoning: String::new(), outcome: Outcome::Ok, attachments: Vec::new() }
    }
    pub fn user_with(text: impl Into<String>, attachments: Vec<Attachment>) -> Self {
        Self { attachments, ..Self::user(text) }
    }
    pub fn assistant() -> Self {
        Self { role: Role::Assistant, text: String::new(), reasoning: String::new(), outcome: Outcome::Streaming, attachments: Vec::new() }
    }
    pub fn is_streaming(&self) -> bool { self.outcome == Outcome::Streaming }
    /// 气泡里显示的内容；只有思考没有回答时退而显示思考。
    pub fn display(&self) -> &str {
        if !self.text.is_empty() { &self.text } else if !self.reasoning.is_empty() { &self.reasoning } else { "" }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Conversation {
    pub title: String,
    pub messages: Vec<Message>,
}

impl Conversation {
    pub fn title_of(text: &str) -> String {
        let text = text.trim().replace(['\n', '\r'], " ");
        let mut title: String = text.chars().take(16).collect();
        if text.chars().count() > 16 { title.push('…'); }
        if title.is_empty() { "新对话".into() } else { title }
    }
    pub fn is_empty(&self) -> bool { self.messages.is_empty() }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChatRequest {
    pub config: Config,
    pub messages: Vec<Message>,
    /// 本轮是否读取附件内容；关闭时只发送路径。
    pub read_attachments: bool,
}

/// 交给界面消费的流式事件。
pub enum Event {
    Delta { reasoning: bool, text: String },
    Done { ms: u64 },
    Stopped,
    Error(String),
    Test(Result<TestOk, String>),
}

pub struct TestOk { pub reply: String, pub slot: String }

// ---------------------------------------------------------------- 请求体

/// 组装上下文：系统提示 + 有效历史，超出预算时从最早一轮开始丢。
fn context(system: &str, messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::new();
    if !system.trim().is_empty() {
        out.push(json!({"role": "system", "content": system.trim()}));
    }
    let mut start = 0;
    while start < messages.len() {
        let total = system.chars().count() + messages[start..].iter()
            .filter(|message| !matches!(message.outcome, Outcome::Streaming | Outcome::Failed(_)))
            .map(|message| message.text.chars().count()).sum::<usize>();
        if total <= MAX_CONTEXT || start + 1 >= messages.len() { break; }
        start += 1;
    }
    // 从中间截断时不要把「回答」放在第一条，尽量保持 user/assistant 成对。
    if messages.get(start).is_some_and(|message| message.role == Role::Assistant) { start += 1; }
    for message in &messages[start..] {
        if matches!(message.outcome, Outcome::Streaming | Outcome::Failed(_)) || message.text.trim().is_empty() { continue; }
        out.push(json!({"role": message.role.api(), "content": message.text}));
    }
    out
}

fn body(config: &Config, messages: &[Message], stream: bool, max_tokens: u32, read_attachments: bool) -> Value {
    let mut rows = context(&config.system, messages);
    if read_attachments { augment_attachments(&mut rows, messages); }
    json!({
        "model": config.model.trim(),
        "messages": rows,
        "stream": stream,
        "temperature": config.temperature.clamp(0.0, 2.0),
        "max_tokens": max_tokens.clamp(1, 8192),
    })
}

fn image_data_url(path: &Path) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|_| "文件不存在或不可读".to_string())?;
    if meta.len() > MAX_IMAGE_BYTES { return Err(format!("图片超过 {} MB", MAX_IMAGE_BYTES / 1024 / 1024)); }
    let bytes = std::fs::read(path).map_err(|_| "无法读取文件".to_string())?;
    let mime = match path.extension().and_then(|value| value.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        _ => "image/png",
    };
    Ok(format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes)))
}

fn read_text(path: &Path) -> Result<(String, bool), String> {
    let meta = std::fs::metadata(path).map_err(|_| "文件不存在或不可读".to_string())?;
    if !meta.is_file() { return Err("不是文件".into()); }
    let bytes = std::fs::read(path).map_err(|_| "无法读取文件".to_string())?;
    let truncated = bytes.len() > MAX_TEXT_BYTES;
    Ok((String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_TEXT_BYTES)]).into_owned(), truncated))
}

/// 只给最后一条用户消息附加内容：历史图片/文本不重复读，省流量也省上下文。
fn augment_attachments(rows: &mut [Value], messages: &[Message]) {
    let Some(user) = messages.iter().rev().find(|message| message.role == Role::User && !message.attachments.is_empty()) else { return; };
    let Some(last) = rows.last_mut() else { return; };
    if last["role"].as_str() != Some("user") { return; }
    let mut text = last["content"].as_str().unwrap_or("").to_string();
    let mut images = Vec::new();
    for attachment in &user.attachments {
        match attachment.kind {
            AttachmentKind::Image => {
                if images.len() >= MAX_IMAGES { continue; }
                match image_data_url(&attachment.path) {
                    Ok(url) => images.push(json!({"type": "image_url", "image_url": {"url": url}})),
                    Err(error) => text.push_str(&format!("\n（无法读取图片 {}：{error}）", attachment.name())),
                }
            }
            AttachmentKind::Text => match read_text(&attachment.path) {
                Ok((content, truncated)) => {
                    text.push_str(&format!("\n\n--- {} ---\n{}{}", attachment.name(), content, if truncated { "\n（内容过长，已截断）" } else { "" }));
                }
                Err(error) => text.push_str(&format!("\n（无法读取 {}：{error}）", attachment.name())),
            },
            AttachmentKind::Other => text.push_str(&format!("\n（{} 不是可读取的文本或图片，只能提供路径）", attachment.name())),
        }
    }
    *last = if images.is_empty() {
        json!({"role": "user", "content": text})
    } else {
        let mut parts = vec![json!({"type": "text", "text": text})];
        parts.extend(images);
        json!({"role": "user", "content": parts})
    };
}

// ---------------------------------------------------------------- SSE

/// 按行切分字节流。必须以字节缓冲，汉字可能被切在两个 TCP 块之间。
#[derive(Default)]
pub struct Lines { buffer: Vec<u8> }

impl Lines {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        self.buffer.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(index) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let mut line: Vec<u8> = self.buffer.drain(..=index).collect();
            line.pop();
            if line.last() == Some(&b'\r') { line.pop(); }
            out.push(line);
        }
        out
    }
    pub fn rest(&self) -> Option<&[u8]> {
        if self.buffer.is_empty() { None } else { Some(&self.buffer) }
    }
}

#[derive(Debug, PartialEq)]
pub enum Parsed { Delta { reasoning: bool, text: String }, Done, Ignore }

/// 解析一行 SSE：`data: {...}`、`data: [DONE]`、空行与注释都安全忽略。
pub fn parse_line(line: &[u8]) -> Parsed {
    let Ok(text) = std::str::from_utf8(line) else { return Parsed::Ignore; };
    let text = text.trim();
    if text.is_empty() || text.starts_with(':') { return Parsed::Ignore; }
    let Some(payload) = text.strip_prefix("data:") else { return Parsed::Ignore; };
    let payload = payload.trim();
    if payload == "[DONE]" { return Parsed::Done; }
    let Ok(value) = serde_json::from_str::<Value>(payload) else { return Parsed::Ignore; };
    let Some(delta) = value.pointer("/choices/0/delta") else { return Parsed::Ignore; };
    let text_of = |key: &str| delta.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    let content = text_of("content");
    let reasoning = {
        let deep = text_of("reasoning_content");
        if deep.is_empty() { text_of("reasoning") } else { deep }
    };
    match (reasoning.is_empty(), content.is_empty()) {
        (false, _) => Parsed::Delta { reasoning: true, text: reasoning },
        (true, false) => Parsed::Delta { reasoning: false, text: content },
        (true, true) => Parsed::Ignore,
    }
}

/// 服务端忽略 `stream` 时，从整包 JSON 里取出完整回答。
fn full_reply(value: &Value) -> (String, String) {
    let message = value.pointer("/choices/0/message");
    let text = message.and_then(|message| message.get("content")).and_then(Value::as_str).unwrap_or("").to_string();
    let reasoning = message.and_then(|message| message.get("reasoning_content").or_else(|| message.get("reasoning"))).and_then(Value::as_str).unwrap_or("").to_string();
    (reasoning, text)
}

// ---------------------------------------------------------------- 错误

/// 服务端消息可能很长，截断并压掉换行；不回显任何鉴权信息。
fn server_message(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    let message = value.pointer("/error/message").and_then(Value::as_str)
        .or_else(|| value.get("message").and_then(Value::as_str))?;
    let message = message.replace(['\n', '\r'], " ");
    let clean: String = message.chars().filter(|c| *c != '\u{0}').take(120).collect();
    (!clean.trim().is_empty()).then_some(clean)
}

pub fn error_for(status: u16, retry: Option<u64>, body: &str) -> String {
    let detail = server_message(body).filter(|_| !matches!(status, 401 | 403));
    match status {
        401 | 403 => "密钥无效或没有权限（401）。请到设置里重新填写 API Key".into(),
        404 => "接口地址或模型名不正确（404）。请检查 Base URL 与模型名".into(),
        429 => format!("请求太频繁（429）{}", retry.map(|seconds|format!("，约 {seconds} 秒后再试")).unwrap_or_else(|| "，请稍后再试".into())),
        400 | 422 => detail.map(|message|format!("请求被拒绝（{status}）：{message}")).unwrap_or_else(|| format!("请求被拒绝（{status}），请检查模型名与参数")),
        status if status >= 500 => format!("AI 服务暂时不可用（{status}），请稍后再试"),
        status => detail.map(|message|format!("请求失败（{status}）：{message}")).unwrap_or_else(|| format!("请求失败（{status}）")),
    }
}

fn retry_after(response: &ureq::Response) -> Option<u64> {
    response.header("Retry-After").and_then(|value| value.trim().parse::<u64>().ok()).map(|seconds| seconds.clamp(1, 600))
}

fn transport_error(error: ureq::Error) -> String {
    match error {
        ureq::Error::Status(status, response) => {
            let retry = retry_after(&response);
            let mut raw = String::new();
            let _ = response.into_reader().take(8 * 1024).read_to_string(&mut raw);
            error_for(status, retry, &raw)
        }
        other => format!("无法连接 AI 服务，请检查网络或代理（{other}）"),
    }
}

// ---------------------------------------------------------------- 凭据

fn slot_for(config: &Config) -> String {
    let raw = format!("{}|{}", config.preset.key(), config.base_url.trim().to_ascii_lowercase());
    let digest = ring::digest::digest(&ring::digest::SHA256, raw.as_bytes());
    digest.as_ref()[..16].iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 配置对应的凭据槽位：地址或服务商变了，槽位就变，界面据此提示重新连接。
pub fn key_slot(config: &Config) -> String { slot_for(config) }

#[cfg(windows)]
fn entry(slot: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, slot).map_err(|_| "无法打开 Windows 凭据管理器".to_string())
}
#[cfg(windows)]
pub fn save_key(slot: &str, key: &str) -> Result<(), String> {
    entry(slot)?.set_password(key).map_err(|_| "无法安全保存 API Key".into())
}
#[cfg(not(windows))]
pub fn save_key(_slot: &str, _key: &str) -> Result<(), String> { Err("当前系统不支持安全保存 API Key".into()) }

#[cfg(windows)]
pub fn load_key(slot: &str) -> Result<String, String> {
    entry(slot)?.get_password().map_err(|error| match error {
        keyring::Error::NoEntry => "没有找到已保存的 Key，请到设置里重新填写".into(),
        _ => "暂时无法读取 Windows 凭据，请稍后重试".into(),
    })
}
#[cfg(not(windows))]
pub fn load_key(_slot: &str) -> Result<String, String> { Err("当前系统不支持读取 API Key".into()) }

#[cfg(windows)]
pub fn forget_key(slot: &str) {
    if let Ok(entry) = entry(slot) { let _ = entry.delete_credential(); }
}
#[cfg(not(windows))]
pub fn forget_key(_slot: &str) {}

// ---------------------------------------------------------------- 网络

fn agent() -> ureq::Agent {
    let mut builder = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(15))
        .redirects(0);
    let proxy = ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy", "HTTP_PROXY", "http_proxy"]
        .iter().find_map(|key| std::env::var(key).ok().filter(|value| !value.is_empty()))
        .or_else(crate::quota::system_proxy);
    if let Some(proxy) = proxy.and_then(|value| ureq::Proxy::new(value).ok()) { builder = builder.proxy(proxy); }
    builder.build()
}

fn check(config: &Config, key: &str) -> Result<(), String> {
    config.validate()?;
    if key.trim().is_empty() { return Err("请先填写 API Key".into()); }
    Ok(())
}

/// 流式请求。返回 `Ok(true)` 表示正常结束，`Ok(false)` 表示被用户停止。
fn stream(key: &str, request: &ChatRequest, cancel: &AtomicBool, tx: &mpsc::Sender<Event>, repaint: &dyn Fn()) -> Result<bool, String> {
    let config = &request.config;
    check(config, key)?;
    let payload = body(config, &request.messages, true, config.max_tokens, request.read_attachments);
    let has_user = payload["messages"].as_array().is_some_and(|rows| rows.iter().any(|row| row["role"] == "user"));
    if !has_user {
        return Err("这轮对话太长了，请开始新对话".into());
    }
    let payload = payload.to_string();
    let response = agent().post(&config.endpoint())
        .set("Authorization", &format!("Bearer {}", key.trim()))
        .set("Content-Type", "application/json")
        .set("Accept", "text/event-stream")
        .send_string(&payload)
        .map_err(transport_error)?;
    let content_type = response.header("Content-Type").unwrap_or("").to_ascii_lowercase();
    // 服务端忽略 stream 或返回普通 JSON：按整包处理。
    if !content_type.contains("text/event-stream") {
        let mut raw = String::new();
        response.into_reader().take(4 * 1024 * 1024).read_to_string(&mut raw).map_err(|_| "读取回答失败".to_string())?;
        let value: Value = serde_json::from_str(&raw).map_err(|_| "服务返回的不是有效 JSON".to_string())?;
        if let Some(message) = value.pointer("/error/message").and_then(Value::as_str) {
            return Err(format!("请求失败：{}", message.chars().take(120).collect::<String>()));
        }
        let (reasoning, text) = full_reply(&value);
        if !reasoning.is_empty() { let _ = tx.send(Event::Delta { reasoning: true, text: reasoning }); }
        if !text.is_empty() { let _ = tx.send(Event::Delta { reasoning: false, text }); }
        repaint();
        return Ok(true);
    }
    let mut reader = BufReader::new(response.into_reader());
    let mut lines = Lines::default();
    let mut chunk = [0u8; 8192];
    let started = Instant::now();
    let mut chars = 0usize;
    loop {
        if cancel.load(Ordering::SeqCst) {
            let _ = tx.send(Event::Stopped);
            repaint();
            return Ok(false);
        }
        let read = match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err("连接中断，回答可能不完整".into()),
        };
        for line in lines.push(&chunk[..read]) {
            match parse_line(&line) {
                Parsed::Delta { reasoning, text } => {
                    chars += text.chars().count();
                    if chars > MAX_ANSWER {
                        let _ = tx.send(Event::Stopped);
                        repaint();
                        return Ok(false);
                    }
                    let _ = tx.send(Event::Delta { reasoning, text });
                    repaint();
                }
                Parsed::Done => return Ok(true),
                Parsed::Ignore => {}
            }
        }
        if started.elapsed().as_secs() > MAX_SECONDS {
            let _ = tx.send(Event::Stopped);
            repaint();
            return Ok(false);
        }
    }
    if let Some(rest) = lines.rest() {
        if let Parsed::Delta { reasoning, text } = parse_line(rest) {
            let _ = tx.send(Event::Delta { reasoning, text });
        }
    }
    Ok(true)
}

/// 设置页的「测试连接」：发一条极短的非流式请求，成功才把密钥存入凭据。
fn test(config: &Config, key: &str) -> Result<TestOk, String> {
    check(config, key)?;
    let payload = body(config, &[Message::user("回复“好”")], false, 8, false).to_string();
    let response = agent().post(&config.endpoint())
        .set("Authorization", &format!("Bearer {}", key.trim()))
        .set("Content-Type", "application/json")
        .set("Accept", "application/json")
        .send_string(&payload)
        .map_err(transport_error)?;
    let mut raw = String::new();
    response.into_reader().take(1024 * 1024).read_to_string(&mut raw).map_err(|_| "读取测试结果失败".to_string())?;
    let value: Value = serde_json::from_str(&raw).map_err(|_| "服务返回的不是有效 JSON".to_string())?;
    let (_, text) = full_reply(&value);
    let slot = slot_for(config);
    save_key(&slot, key.trim())?;
    Ok(TestOk { reply: text.chars().take(24).collect(), slot })
}

// ---------------------------------------------------------------- 服务

enum Job {
    Send { key: String, request: ChatRequest },
    Test { config: Config, key: String },
}

pub struct Service {
    jobs: mpsc::Sender<Job>,
    events: mpsc::Receiver<Event>,
    cancel: Arc<AtomicBool>,
}

impl Service {
    pub fn start(ctx: eframe::egui::Context) -> Self {
        let (jobs, job_rx) = mpsc::channel::<Job>();
        let (tx, events) = mpsc::channel::<Event>();
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let _ = std::thread::Builder::new().name("daozai-chat".into()).spawn(move || {
            while let Ok(job) = job_rx.recv() {
                flag.store(false, Ordering::SeqCst);
                let repaint = || ctx.request_repaint();
                match job {
                    Job::Send { key, request } => {
                        let started = Instant::now();
                        match stream(&key, &request, &flag, &tx, &repaint) {
                            Ok(true) => { let _ = tx.send(Event::Done { ms: started.elapsed().as_millis() as u64 }); }
                            Ok(false) => {}
                            Err(error) => {
                                if !flag.load(Ordering::SeqCst) { let _ = tx.send(Event::Error(error)); }
                            }
                        }
                    }
                    Job::Test { config, key } => { let _ = tx.send(Event::Test(test(&config, &key))); }
                }
                repaint();
            }
        });
        Self { jobs, events, cancel }
    }

    pub fn send(&self, key: &str, request: ChatRequest) -> Result<(), String> {
        self.jobs.send(Job::Send { key: key.into(), request }).map_err(|_| "对话服务未启动".to_string())
    }
    pub fn test(&self, config: Config, key: String) -> Result<(), String> {
        self.jobs.send(Job::Test { config, key }).map_err(|_| "对话服务未启动".to_string())
    }
    pub fn cancel(&self) { self.cancel.store(true, Ordering::SeqCst); }
    pub fn try_recv(&self) -> Option<Event> { self.events.try_recv().ok() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(text: &str) -> String { format!("data: {{\"choices\":[{{\"delta\":{{\"content\":\"{text}\"}}}}]}}") }

    #[test]
    fn sse_lines_survive_split_utf8_crlf_and_done() {
        let mut lines = Lines::default();
        let payload = format!("{}\r\n\r\n{}\n\n", delta("你"), "data: [DONE]");
        let bytes = payload.as_bytes();
        // 故意把「你」的三个字节切到两个块里。
        let split = payload.find("你").unwrap() + 1;
        assert!(lines.push(&bytes[..split]).is_empty(), "半行不能提前交给解析器");
        let rest = lines.push(&bytes[split..]);
        assert_eq!(rest.len(), 4, "两块拼起来应得到 4 行（含两个空行）");
        assert_eq!(parse_line(&rest[0]), Parsed::Delta { reasoning: false, text: "你".into() });
        assert_eq!(parse_line(&rest[1]), Parsed::Ignore);
        assert_eq!(parse_line(&rest[2]), Parsed::Done);
        assert_eq!(parse_line(&rest[3]), Parsed::Ignore);
        assert!(lines.rest().is_none());
    }

    #[test]
    fn stream_deltas_read_content_and_reasoning_separately() {
        let reasoning = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"先想\"}}]}";
        let content = "data: {\"choices\":[{\"delta\":{\"content\":\"再答\"}}]}";
        let comment = ": keep-alive";
        let usage = "data: {\"choices\":[],\"usage\":{\"total_tokens\":3}}";
        assert_eq!(parse_line(reasoning.as_bytes()), Parsed::Delta { reasoning: true, text: "先想".into() });
        assert_eq!(parse_line(content.as_bytes()), Parsed::Delta { reasoning: false, text: "再答".into() });
        assert_eq!(parse_line(comment.as_bytes()), Parsed::Ignore);
        assert_eq!(parse_line(usage.as_bytes()), Parsed::Ignore);
        assert_eq!(parse_line(b"data: not-json"), Parsed::Ignore);
        // 兼容另一个字段名。
        let other = "data: {\"choices\":[{\"delta\":{\"reasoning\":\"想一想\"}}]}";
        assert_eq!(parse_line(other.as_bytes()), Parsed::Delta { reasoning: true, text: "想一想".into() });
    }

    #[test]
    fn request_keeps_system_and_drops_the_oldest_rounds() {
        let config = Config::default();
        let mut messages = Vec::new();
        for index in 0..40 {
            messages.push(Message::user(format!("问题 {index} {}", "很长的内容".repeat(400))));
            let mut reply = Message::assistant();
            reply.text = format!("回答 {index}");
            reply.outcome = Outcome::Ok;
            messages.push(reply);
        }
        let value = body(&config, &messages, true, 1024, false);
        let rows = value["messages"].as_array().unwrap();
        assert_eq!(rows[0]["role"], "system");
        assert!(rows.len() < 30, "超出预算时应丢弃最早的历史：{}", rows.len());
        assert_eq!(rows.last().unwrap()["content"], "回答 39");
        assert_eq!(value["stream"], true);
        assert_eq!(value["model"], Preset::DeepSeek.model());
    }

    #[test]
    fn streaming_placeholders_and_empty_messages_never_reach_the_api() {
        let config = Config::default();
        let mut messages = vec![Message::user("在吗")];
        messages.push(Message::assistant()); // Streaming 占位
        messages.push(Message::user("   "));
        let rows = body(&config, &messages, true, 64, false)["messages"].as_array().unwrap().clone();
        assert_eq!(rows.len(), 2, "只有 system 与真实用户消息");
        assert_eq!(rows[1]["content"], "在吗");
    }

    #[test]
    fn errors_are_distinct_and_never_echo_the_key() {
        let body = "{\"error\":{\"message\":\"Incorrect API key provided: sk-secret\"}}";
        let auth = error_for(401, None, body);
        assert!(auth.contains("401") && auth.contains("API Key"));
        assert!(!auth.contains("sk-secret"), "错误文案不能回显密钥");
        assert!(error_for(404, None, "").contains("模型"));
        assert!(error_for(429, Some(30), "").contains("30"));
        assert!(error_for(503, None, "").contains("暂时不可用"));
        assert!(error_for(400, None, "{\"error\":{\"message\":\"bad model\"}}").contains("bad model"));
        let long = format!("{{\"error\":{{\"message\":\"{}\"}}}}", "x".repeat(400));
        assert!(error_for(400, None, &long).chars().count() < 160, "服务端原文要截断");
    }

    #[test]
    fn urls_require_https_except_loopback_and_endpoints_are_normalized() {
        let mut config = Config::default();
        config.base_url = "http://api.example.com/v1".into();
        assert!(config.validate().is_err());
        config.base_url = "http://127.0.0.1:11434/v1".into();
        assert!(config.validate().is_ok());
        assert_eq!(config.endpoint(), "http://127.0.0.1:11434/v1/chat/completions");
        config.base_url = "https://api.deepseek.com/v1/".into();
        assert_eq!(config.endpoint(), "https://api.deepseek.com/v1/chat/completions");
        config.base_url = "https://proxy.example.com/chat/completions".into();
        assert_eq!(config.endpoint(), "https://proxy.example.com/chat/completions");
        config.model.clear();
        assert!(config.validate().is_err());
    }

    #[test]
    fn presets_fill_url_and_model_but_custom_keeps_edits() {
        let mut config = Config::default();
        config.apply_preset(Preset::Kimi);
        assert_eq!(config.base_url, "https://api.moonshot.cn/v1");
        assert_eq!(config.model, "moonshot-v1-8k");
        config.base_url = "https://my.gateway/v1".into();
        config.model = "my-model".into();
        config.apply_preset(Preset::Custom);
        assert_eq!(config.base_url, "https://my.gateway/v1", "自定义不应覆盖已填地址");
        assert_eq!(config.model, "my-model");
    }

    #[test]
    fn credentials_slot_is_stable_and_hides_the_key_shape() {
        let config = Config::default();
        let slot = slot_for(&config);
        assert_eq!(slot.len(), 32);
        assert!(slot.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(slot, slot_for(&config));
        let mut other = config.clone();
        other.base_url = "https://api.openai.com/v1".into();
        assert_ne!(slot, slot_for(&other), "换地址应换槽位");
    }

    #[test]
    fn conversation_titles_are_short_and_single_line() {
        assert_eq!(Conversation::title_of("  帮我把这段话改客气  "), "帮我把这段话改客气");
        assert_eq!(Conversation::title_of("第一行\n第二行"), "第一行 第二行");
        let long = "很长的一句话".repeat(8);
        let title = Conversation::title_of(&long);
        assert_eq!(title.chars().count(), 17, "16 个字符 + 省略号");
        assert!(title.ends_with('…'));
        assert_eq!(Conversation::title_of("   "), "新对话");
    }

    #[test]
    fn full_reply_reads_both_shapes_of_json() {
        let value: Value = serde_json::from_str("{\"choices\":[{\"message\":{\"content\":\"好\",\"reasoning_content\":\"想了想\"}}]}").unwrap();
        assert_eq!(full_reply(&value), ("想了想".into(), "好".into()));
        let value: Value = serde_json::from_str("{\"choices\":[{\"message\":{\"role\":\"assistant\"}}]}").unwrap();
        assert_eq!(full_reply(&value), (String::new(), String::new()));
    }

    fn fixture_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("daozai-chat-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn attachments_are_classified_and_summarized_by_path_only() {
        let dir = fixture_dir("attach");
        let text = dir.join("note.md");
        std::fs::write(&text, "hello 世界").unwrap();
        let image = dir.join("pixel.png");
        image::RgbaImage::from_pixel(1, 1, image::Rgba([255, 0, 0, 255])).save(&image).unwrap();
        let other = dir.join("a.bin");
        std::fs::write(&other, b"x").unwrap();
        assert_eq!(AttachmentKind::of(&text), AttachmentKind::Text);
        assert_eq!(AttachmentKind::of(&image), AttachmentKind::Image);
        assert_eq!(AttachmentKind::of(&other), AttachmentKind::Other);
        let attachments = vec![Attachment::of(text.clone()), Attachment::of(image.clone()), Attachment::of(other)];
        let summary = path_summary(&attachments);
        assert!(summary.contains("note.md") && summary.contains("文本"));
        assert!(summary.contains("pixel.png") && summary.contains("图片"));
        assert!(summary.contains("a.bin") && summary.contains("文件"));
        assert!(summary.contains(&text.display().to_string()), "必须带上完整路径");
        assert_eq!(path_summary(&[]), "");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reading_attachments_inlines_text_and_builds_image_parts() {
        let dir = fixture_dir("read");
        let text = dir.join("note.md");
        std::fs::write(&text, "hello 世界").unwrap();
        let image = dir.join("pixel.png");
        image::RgbaImage::from_pixel(1, 1, image::Rgba([0, 255, 0, 255])).save(&image).unwrap();
        let message = Message::user_with("看看", vec![Attachment::of(text.clone()), Attachment::of(image.clone())]);
        let config = Config::default();
        let value = body(&config, std::slice::from_ref(&message), false, 64, true);
        let rows = value["messages"].as_array().unwrap();
        let parts = rows.last().unwrap()["content"].as_array().expect("有图片时应为 content 数组");
        assert_eq!(parts[0]["type"], "text");
        assert!(parts[0]["text"].as_str().unwrap().contains("hello 世界"), "文本附件应内联");
        assert_eq!(parts[1]["type"], "image_url");
        assert!(parts[1]["image_url"]["url"].as_str().unwrap().starts_with("data:image/png;base64,"));
        // 没有读取权限时只发文字与路径，content 仍是字符串。
        let value = body(&config, &[message], false, 64, false);
        let content = &value["messages"].as_array().unwrap().last().unwrap()["content"];
        assert!(content.is_string());
        assert!(content.as_str().unwrap().contains("看看"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn oversized_or_missing_attachments_degrade_to_a_note_instead_of_failing() {
        let dir = fixture_dir("missing");
        let missing = dir.join("gone.png");
        let message = Message::user_with("看看", vec![Attachment::of(missing)]);
        let value = body(&Config::default(), &[message], false, 64, true);
        let content = &value["messages"].as_array().unwrap().last().unwrap()["content"];
        assert!(content.is_string(), "读不到图片时应退回纯文本");
        assert!(content.as_str().unwrap().contains("无法读取图片"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
