use async_trait::async_trait;
use rand::rngs::StdRng;
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Default HTTP timeout for LLM API calls (5 minutes).  Long enough for slow local models
/// running on CPU, short enough that a hung server will eventually be detected.
const HTTP_TIMEOUT_SECS: u64 = 300;

/// Maximum number of characters accumulated from a single LLM response stream.
/// Prevents unbounded memory growth if a server returns an unexpectedly large body.
const MAX_RESPONSE_CHARS: usize = 1_048_576; // 1 MiB

// ---------------------------------------------------------------------------
// Output sanitizer — applied to all backend responses before returning
// ---------------------------------------------------------------------------

fn sanitize_llm_output(raw: String) -> String {
    raw.replace('\0', "")
        .chars()
        .map(|c| if c.is_control() && c != '\n' && c != '\r' && c != '\t' { ' ' } else { c })
        .collect()
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait LlmBackend: Send + Sync {
    /// Generate a completion for the given prompt.
    /// `seed`     — when Some, passes the seed to the backend for deterministic output.
    /// `schema`   — when Some, passed as Ollama's `format` field to constrain output.
    /// `token_tx` — when Some, each content token is forwarded to this sender as it streams.
    async fn generate(
        &self,
        prompt:     &str,
        max_tokens: u32,
        seed:       Option<u64>,
        schema:     Option<&serde_json::Value>,
        token_tx:   Option<UnboundedSender<String>>,
    ) -> Result<String>;
}

// ---------------------------------------------------------------------------
// Ollama backend
// ---------------------------------------------------------------------------

pub struct OllamaBackend {
    pub url:                   String,
    pub model:                 String,
    pub temperature:           f32,
    /// When Some(false), passes `"think": false` to disable chain-of-thought on
    /// thinking models (qwen3, deepseek-r1, etc.) via the /api/chat endpoint.
    pub think:                 Option<bool>,
    /// Abort the stream when accumulated thinking chars exceed this limit.
    pub thinking_budget_chars: Option<usize>,
    client:                    reqwest::Client,
}

impl OllamaBackend {
    pub fn new(
        url:                   String,
        model:                 String,
        temperature:           f32,
        think:                 Option<bool>,
        thinking_budget_chars: Option<usize>,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .build()
            .map_err(|e| format!("failed to build reqwest client: {}", e))?;
        Ok(OllamaBackend { url, model, temperature, think, thinking_budget_chars, client })
    }
}

#[derive(Serialize)]
struct OllamaChatRequest<'a> {
    model:    &'a str,
    messages: Vec<OllamaUserMessage<'a>>,
    stream:   bool,
    options:  OllamaOptions,
    #[serde(skip_serializing_if = "Option::is_none")]
    format:   Option<&'a serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    think:    Option<bool>,
}

#[derive(Serialize)]
struct OllamaUserMessage<'a> {
    role:    &'static str,
    content: &'a str,
}

#[derive(Serialize)]
struct OllamaOptions {
    temperature: f32,
    num_predict: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    seed: Option<i64>,
}

#[derive(Deserialize)]
struct OllamaChatChunk {
    message: OllamaChatContent,
}

#[derive(Deserialize)]
struct OllamaChatContent {
    #[serde(default)]
    content: String,
    #[serde(default)]
    thinking: String,
}

#[derive(Deserialize)]
struct OllamaTagsResponse {
    models: Vec<OllamaModelEntry>,
}

#[derive(Deserialize)]
struct OllamaModelEntry {
    name: String,
}

impl OllamaBackend {
    pub async fn health_check(&self) -> Result<()> {
        let url = format!("{}/api/tags", self.url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("Ollama not running at {}: {}", self.url, e))?;

        if !resp.status().is_success() {
            return Err(format!("Ollama health check failed ({})", resp.status()).into());
        }

        let tags: OllamaTagsResponse = resp
            .json()
            .await
            .map_err(|e| format!("Ollama tags parse error: {}", e))?;

        let names: Vec<&str> = tags.models.iter().map(|m| m.name.as_str()).collect();
        if names.iter().any(|n| *n == self.model || n.starts_with(&format!("{}:", self.model))) {
            info!(target: "llm", model = %self.model, "Ollama ready: model available");
        } else {
            warn!(target: "llm", model = %self.model, available = ?names, "Model not found in Ollama list — will try anyway");
        }
        Ok(())
    }
}

#[async_trait]
impl LlmBackend for OllamaBackend {
    async fn generate(
        &self,
        prompt:     &str,
        max_tokens: u32,
        seed:       Option<u64>,
        schema:     Option<&serde_json::Value>,
        token_tx:   Option<UnboundedSender<String>>,
    ) -> Result<String> {
        let url  = format!("{}/api/chat", self.url);
        let body = OllamaChatRequest {
            model:    &self.model,
            messages: vec![OllamaUserMessage { role: "user", content: prompt }],
            stream:   true,
            options:  OllamaOptions {
                temperature: seed.map(|_| 0.0).unwrap_or(self.temperature),
                num_predict: max_tokens,
                seed:        seed.map(|s| s as i64),
            },
            format: schema,
            think:  self.think,
        };

        debug!(target: "llm", model = %self.model, max_tokens = max_tokens,
               prompt_chars = prompt.len(), has_schema = schema.is_some(),
               think = ?self.think, "LLM request");

        let mut resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Ollama HTTP error: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text   = resp.text().await.unwrap_or_default();
            return Err(format!("Ollama returned {}: {}", status, text).into());
        }

        // Stream NDJSON chunks; accumulate content tokens; count thinking tokens separately.
        let mut buf            = Vec::<u8>::new();
        let mut content        = String::new();
        let mut thinking_chars = 0usize;
        let mut done           = false;

        'outer: while let Some(chunk) = resp.chunk().await
            .map_err(|e| format!("Ollama stream error: {}", e))?
        {
            buf.extend_from_slice(&chunk);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line_bytes: Vec<u8> = buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line_bytes);
                let line = line.trim();
                if line.is_empty() { continue; }
                if let Ok(c) = serde_json::from_str::<OllamaChatChunk>(line) {
                    let token = c.message.content;
                    let new_thinking = c.message.thinking.len();

                    // FEAT-10: Thinking budget (warn only, keep streaming)
                    if content.is_empty() && new_thinking > 0 {
                        thinking_chars += new_thinking;
                        if let Some(budget) = self.thinking_budget_chars {
                            if thinking_chars > budget {
                                warn!(target: "llm", thinking_chars, budget, "thinking budget exceeded");
                            }
                        }
                        continue;
                    }
                    thinking_chars += new_thinking;

                    if !token.is_empty() {
                        // Forward to streaming consumer if provided
                        if let Some(ref tx) = token_tx {
                            let _ = tx.send(token.clone());
                        }
                        content.push_str(&token);

                        // Guard against unexpectedly large responses
                        if content.len() > MAX_RESPONSE_CHARS {
                            warn!(target: "llm", chars = content.len(),
                                  limit = MAX_RESPONSE_CHARS, "response exceeded size limit, truncating");
                            done = true;
                            break 'outer;
                        }

                        // FEAT-17: Early abort when JSON schema-constrained response is complete
                        if schema.is_some() && content.trim_end().ends_with('}') {
                            if serde_json::from_str::<serde_json::Value>(content.trim()).is_ok() {
                                debug!(target: "llm", chars = content.len(), "early abort: JSON complete");
                                done = true;
                                break 'outer;
                            }
                        }
                    }
                }
            }
        }

        // Flush any remaining partial line (only if not already done via early abort)
        if !done && !buf.is_empty() {
            let line = String::from_utf8_lossy(&buf);
            if let Ok(c) = serde_json::from_str::<OllamaChatChunk>(line.trim()) {
                let token = c.message.content;
                thinking_chars += c.message.thinking.len();
                if !token.is_empty() {
                    if let Some(ref tx) = token_tx {
                        let _ = tx.send(token.clone());
                    }
                    content.push_str(&token);
                }
            }
        }

        let content = sanitize_llm_output(content);
        debug!(target: "llm", chars = content.len(), thinking_chars = thinking_chars,
               response = %content, "LLM response");
        Ok(content)
    }
}

// ---------------------------------------------------------------------------
// OpenAI-compatible backend (llama.cpp, vLLM, LM Studio, etc.)
// ---------------------------------------------------------------------------

pub struct OpenAICompatBackend {
    pub url:                   String,
    pub model:                 String,
    pub temperature:           f32,
    /// When Some(false), sets `thinking_forced_off: true` in the request body
    /// to disable chain-of-thought on thinking models via llama.cpp.
    pub think:                 Option<bool>,
    /// Abort the stream when accumulated thinking chars exceed this limit.
    pub thinking_budget_chars: Option<usize>,
    client:                    reqwest::Client,
}

impl OpenAICompatBackend {
    pub fn new(
        url:                   String,
        model:                 String,
        temperature:           f32,
        think:                 Option<bool>,
        thinking_budget_chars: Option<usize>,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .build()
            .map_err(|e| format!("failed to build reqwest client: {}", e))?;
        Ok(OpenAICompatBackend { url, model, temperature, think, thinking_budget_chars, client })
    }

    pub async fn health_check(&self) {
        let url = format!("{}/health", self.url);
        match self.client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                info!(target: "llm", url = %self.url, model = %self.model, "llama.cpp server ready");
            }
            Ok(resp) => {
                warn!(target: "llm", status = %resp.status(),
                      "llama.cpp health check returned non-200 — continuing anyway");
            }
            Err(e) => {
                warn!(target: "llm", error = %e,
                      "llama.cpp health check failed — server may not be running");
            }
        }
    }
}

#[derive(Serialize)]
struct OAIRequest<'a> {
    model:       &'a str,
    messages:    Vec<OAIMessage<'a>>,
    stream:      bool,
    temperature: f32,
    max_tokens:  u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    seed:        Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format:     Option<OAIResponseFormat<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    chat_template_kwargs: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct OAIMessage<'a> {
    role:    &'static str,
    content: &'a str,
}

#[derive(Serialize)]
struct OAIResponseFormat<'a> {
    #[serde(rename = "type")]
    type_:       &'static str,
    json_schema: OAIJsonSchema<'a>,
}

#[derive(Serialize)]
struct OAIJsonSchema<'a> {
    name:   &'static str,
    schema: &'a serde_json::Value,
}

#[derive(Deserialize)]
struct OAIChunk {
    choices: Vec<OAIChoice>,
}

#[derive(Deserialize)]
struct OAIChoice {
    delta: OAIDelta,
}

#[derive(Deserialize)]
struct OAIDelta {
    #[serde(default)]
    content:           Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
}

#[async_trait]
impl LlmBackend for OpenAICompatBackend {
    async fn generate(
        &self,
        prompt:     &str,
        max_tokens: u32,
        seed:       Option<u64>,
        schema:     Option<&serde_json::Value>,
        token_tx:   Option<UnboundedSender<String>>,
    ) -> Result<String> {
        let url = format!("{}/v1/chat/completions", self.url);

        let response_format = schema.map(|s| OAIResponseFormat {
            type_:       "json_schema",
            json_schema: OAIJsonSchema { name: "response", schema: s },
        });

        // think: Some(false) → disable Qwen3 thinking via chat_template_kwargs
        let chat_template_kwargs = match self.think {
            Some(false) => Some(serde_json::json!({"enable_thinking": false})),
            _           => None,
        };

        let body = OAIRequest {
            model:       &self.model,
            messages:    vec![OAIMessage { role: "user", content: prompt }],
            stream:      true,
            temperature: seed.map(|_| 0.0).unwrap_or(self.temperature),
            max_tokens,
            seed:        seed.map(|s| s as i64),
            response_format,
            chat_template_kwargs,
        };

        debug!(target: "llm", model = %self.model, max_tokens = max_tokens,
               prompt_chars = prompt.len(), has_schema = schema.is_some(),
               think = ?self.think, "LLM request (OpenAI-compat)");

        let mut resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("llama.cpp HTTP error: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text   = resp.text().await.unwrap_or_default();
            return Err(format!("llama.cpp returned {}: {}", status, text).into());
        }

        // Stream SSE; each content line is: `data: {...}\n`; ends with `data: [DONE]`
        let mut buf            = Vec::<u8>::new();
        let mut content        = String::new();
        let mut thinking_chars = 0usize;
        let mut done           = false;

        'outer: while let Some(chunk) = resp.chunk().await
            .map_err(|e| format!("llama.cpp stream error: {}", e))?
        {
            buf.extend_from_slice(&chunk);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line_bytes: Vec<u8> = buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line_bytes);
                let line = line.trim();
                if line.is_empty() { continue; }
                if !line.starts_with("data: ") { continue; }
                let data = &line["data: ".len()..];
                if data == "[DONE]" { break 'outer; }

                if let Ok(oai_chunk) = serde_json::from_str::<OAIChunk>(data) {
                    if let Some(choice) = oai_chunk.choices.into_iter().next() {
                        let new_thinking = choice.delta.reasoning_content
                            .as_deref().unwrap_or("").len();
                        let token = choice.delta.content.unwrap_or_default();

                        // Thinking budget (warn only, keep streaming)
                        if content.is_empty() && new_thinking > 0 {
                            thinking_chars += new_thinking;
                            if let Some(budget) = self.thinking_budget_chars {
                                if thinking_chars > budget {
                                    warn!(target: "llm", thinking_chars, budget,
                                          "thinking budget exceeded");
                                }
                            }
                            continue;
                        }
                        thinking_chars += new_thinking;

                        if !token.is_empty() {
                            if let Some(ref tx) = token_tx {
                                let _ = tx.send(token.clone());
                            }
                            content.push_str(&token);

                            // Guard against unexpectedly large responses
                            if content.len() > MAX_RESPONSE_CHARS {
                                warn!(target: "llm", chars = content.len(),
                                      limit = MAX_RESPONSE_CHARS, "response exceeded size limit, truncating");
                                done = true;
                                break 'outer;
                            }

                            // Early abort when JSON schema-constrained response is complete
                            if schema.is_some() && content.trim_end().ends_with('}') {
                                if serde_json::from_str::<serde_json::Value>(content.trim()).is_ok() {
                                    debug!(target: "llm", chars = content.len(), "early abort: JSON complete");
                                    done = true;
                                    break 'outer;
                                }
                            }
                        }
                    }
                }
            }
        }

        // Flush any remaining partial line not terminated before connection close
        if !done && !buf.is_empty() {
            let line = String::from_utf8_lossy(&buf);
            let line = line.trim();
            if line.starts_with("data: ") {
                let data = &line["data: ".len()..];
                if data != "[DONE]" {
                    if let Ok(oai_chunk) = serde_json::from_str::<OAIChunk>(data) {
                        if let Some(choice) = oai_chunk.choices.into_iter().next() {
                            let token = choice.delta.content.unwrap_or_default();
                            if !token.is_empty() {
                                if let Some(ref tx) = token_tx {
                                    let _ = tx.send(token.clone());
                                }
                                content.push_str(&token);
                            }
                        }
                    }
                }
            }
        }

        let content = sanitize_llm_output(content);
        debug!(target: "llm", chars = content.len(), thinking_chars = thinking_chars,
               response = %content, "LLM response (OpenAI-compat)");
        Ok(content)
    }
}

// ---------------------------------------------------------------------------
// Claude API backend
// ---------------------------------------------------------------------------

pub struct ClaudeBackend {
    api_key: String,
    model:   String,
    client:  reqwest::Client,
}

impl ClaudeBackend {
    pub fn new(model: String) -> Result<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .map_err(|_| "ANTHROPIC_API_KEY environment variable not set")?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .build()
            .map_err(|e| format!("failed to build reqwest client: {}", e))?;
        Ok(ClaudeBackend { api_key, model, client })
    }
}

#[derive(Serialize)]
struct ClaudeRequest<'a> {
    model:      &'a str,
    max_tokens: u32,
    messages:   Vec<ClaudeMessage<'a>>,
}

#[derive(Serialize)]
struct ClaudeMessage<'a> {
    role:    &'static str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ClaudeResponse {
    content: Vec<ClaudeContent>,
}

#[derive(Deserialize)]
struct ClaudeContent {
    #[serde(rename = "type")]
    content_type: String,
    text:         Option<String>,
}

#[async_trait]
impl LlmBackend for ClaudeBackend {
    async fn generate(
        &self,
        prompt:     &str,
        max_tokens: u32,
        _seed:      Option<u64>,
        _schema:    Option<&serde_json::Value>,
        _token_tx:  Option<UnboundedSender<String>>,
    ) -> Result<String> {
        let url  = "https://api.anthropic.com/v1/messages";
        let body = ClaudeRequest {
            model:      &self.model,
            max_tokens,
            messages:   vec![ClaudeMessage { role: "user", content: prompt }],
        };

        debug!(target: "llm", model = %self.model, max_tokens = max_tokens,
               prompt_chars = prompt.len(), "Claude request");

        let resp = self.client
            .post(url)
            .header("x-api-key",          &self.api_key)
            .header("anthropic-version",   "2023-06-01")
            .header("content-type",        "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Claude HTTP error: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text   = resp.text().await.unwrap_or_default();
            return Err(format!("Claude API returned {}: {}", status, text).into());
        }

        let claude_resp: ClaudeResponse = resp
            .json()
            .await
            .map_err(|e| format!("Claude JSON parse error: {}", e))?;

        let text = claude_resp.content
            .into_iter()
            .filter(|c| c.content_type == "text")
            .filter_map(|c| c.text)
            .next()
            .ok_or("No text content in Claude response")?;

        let text = sanitize_llm_output(text);
        debug!(target: "llm", chars = text.len(), response = %text, "Claude response");
        Ok(text)
    }
}

// ---------------------------------------------------------------------------
// Claude CLI backend — shells out to `claude -p --model <model>`
// ---------------------------------------------------------------------------

pub struct ClaudeCliBackend {
    model: String,
}

impl ClaudeCliBackend {
    pub fn new(model: String) -> Self {
        ClaudeCliBackend { model }
    }
}

#[async_trait]
impl LlmBackend for ClaudeCliBackend {
    async fn generate(
        &self,
        prompt:     &str,
        _max_tokens: u32,
        _seed:      Option<u64>,
        _schema:    Option<&serde_json::Value>,
        _token_tx:  Option<UnboundedSender<String>>,
    ) -> Result<String> {
        use tokio::io::AsyncWriteExt;
        use tokio::process::Command;

        debug!(target: "llm", model = %self.model, prompt_chars = prompt.len(), "Claude CLI request");

        let mut child = Command::new("claude")
            .args(["-p", "--model", &self.model])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("Failed to spawn claude CLI: {}", e))?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(prompt.as_bytes()).await
                .map_err(|e| format!("Failed to write to claude CLI stdin: {}", e))?;
        }

        let output = child.wait_with_output().await
            .map_err(|e| format!("Claude CLI wait error: {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("Claude CLI exited with {}: {}", output.status, stderr.trim()).into());
        }

        let text = String::from_utf8(output.stdout)
            .map_err(|e| format!("Claude CLI output UTF-8 error: {}", e))?;
        let text = sanitize_llm_output(text.trim().to_string());

        debug!(target: "llm", chars = text.len(), response = %text, "Claude CLI response");
        Ok(text)
    }
}

// ---------------------------------------------------------------------------
// Rate limiter — enforces a minimum interval between LLM calls
// ---------------------------------------------------------------------------

pub struct RateLimiter {
    min_interval: Duration,
    last_call:    Mutex<Option<Instant>>,
}

impl RateLimiter {
    /// `rpm` must be > 0.
    pub fn new(rpm: u32) -> Self {
        assert!(rpm > 0, "RateLimiter: rpm must be > 0");
        let min_interval = Duration::from_secs_f64(60.0 / rpm as f64);
        RateLimiter { min_interval, last_call: Mutex::new(None) }
    }

    pub async fn wait(&self) {
        let sleep_dur = {
            let last = self.last_call.lock().expect("rate limiter poisoned");
            match *last {
                None       => Duration::ZERO,
                Some(t)    => {
                    let elapsed = t.elapsed();
                    if elapsed >= self.min_interval {
                        Duration::ZERO
                    } else {
                        self.min_interval - elapsed
                    }
                }
            }
        };
        if sleep_dur > Duration::ZERO {
            debug!(target: "llm", sleep_ms = sleep_dur.as_millis(), "rate limiter: sleeping");
            tokio::time::sleep(sleep_dur).await;
        }
        let mut last = self.last_call.lock().expect("rate limiter poisoned");
        *last = Some(Instant::now());
    }
}

// ---------------------------------------------------------------------------
// llm CLI backend — shells out to `llm -m <model>`
// ---------------------------------------------------------------------------

pub struct LlmCliBackend {
    model:        String,
    rate_limiter: Option<RateLimiter>,
}

impl LlmCliBackend {
    /// `rpm == 0` disables rate limiting.
    pub fn new(model: String, rpm: u32) -> Self {
        let rate_limiter = if rpm > 0 { Some(RateLimiter::new(rpm)) } else { None };
        LlmCliBackend { model, rate_limiter }
    }
}

#[async_trait]
impl LlmBackend for LlmCliBackend {
    async fn generate(
        &self,
        prompt:      &str,
        _max_tokens: u32,
        _seed:       Option<u64>,
        _schema:     Option<&serde_json::Value>,
        _token_tx:   Option<UnboundedSender<String>>,
    ) -> Result<String> {
        use tokio::io::AsyncWriteExt;
        use tokio::process::Command;

        if let Some(ref rl) = self.rate_limiter {
            rl.wait().await;
        }

        debug!(target: "llm", model = %self.model, prompt_chars = prompt.len(), "llm CLI request");

        let mut child = Command::new("llm")
            .args(["-m", &self.model])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("Failed to spawn llm CLI: {}", e))?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(prompt.as_bytes()).await
                .map_err(|e| format!("Failed to write to llm CLI stdin: {}", e))?;
        }

        let output = child.wait_with_output().await
            .map_err(|e| format!("llm CLI wait error: {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("llm CLI exited with {}: {}", output.status, stderr.trim()).into());
        }

        let text = String::from_utf8(output.stdout)
            .map_err(|e| format!("llm CLI output UTF-8 error: {}", e))?;
        let text = sanitize_llm_output(text.trim().to_string());

        debug!(target: "llm", chars = text.len(), response = %text, "llm CLI response");
        Ok(text)
    }
}

// ---------------------------------------------------------------------------
// Mock backend — fully deterministic, returns plausible JSON actions
// ---------------------------------------------------------------------------

pub struct MockBackend {
    rng: Mutex<StdRng>,
}

impl MockBackend {
    pub fn new(rng: StdRng) -> Self {
        MockBackend { rng: Mutex::new(rng) }
    }
}

// Vivid 2-3 sentence outcomes for the DM Narrator
static MOCK_NARRATIVES: &[&str] = &[
    "The effort shows in her hands — rough work, honest result. She doesn't stop to admire it; there is no need. Something settles in her that had been restless all morning.",
    "He moves through it like someone who has done this a thousand times before. There is no hesitation, no wasted motion. When it's done, he doesn't look back.",
    "Something shifts in the air around her, subtle but real. She pauses mid-motion, head tilted, as though listening to a sound no one else can hear. Then she continues, and the moment passes.",
    "It goes badly, and he knows it before it's finished. He sets it aside without ceremony and stands very still for a moment. Then he starts again.",
    "She finds exactly what she was looking for, and it surprises her. She holds it up to the light, turns it once, then tucks it away carefully. Some luck deserves to be kept quiet.",
    "The moment passes without ceremony, leaving only the quiet satisfaction of having tried. Nothing dramatic — just the small, honest weight of a thing attempted. That is enough for now.",
    "He stumbles once, catches himself, and carries on with quiet dignity. No one saw it, or if they did, they say nothing. By the time he reaches the end, he has already forgiven himself.",
    "A small triumph, the kind no one else will notice but her. She allows herself one moment of stillness to mark it. Then the world resumes, indifferent and continuing.",
];

// Valid InterpretedIntent JSON for mock Interpreter calls
static MOCK_INTERPRETER_RESPONSES: &[&str] = &[
    r#"{"primary_effect":"A warmth settles in the bones, like sun through thin cloth.","interpretations":["warmth as belonging","warmth as memory"],"secondary_effect":"Those nearby feel briefly, inexplicably welcome.","duration_ticks":2,"need_changes":{"fun":10,"energy":-8,"social":5},"memory_entry":"Cast intent: warmth. It answered, in its own way."}"#,
    r#"{"primary_effect":"The light bends strangely for a moment, then settles.","interpretations":["light as clarity","light as attention"],"secondary_effect":"A crow lands nearby and watches with unusual focus.","duration_ticks":1,"need_changes":{"fun":8,"energy":-8},"memory_entry":"Cast intent: light. The world blinked."}"#,
    r#"{"primary_effect":"The sound of the village seems to quiet, just slightly.","interpretations":["quiet as peace","quiet as absence"],"secondary_effect":"Someone, somewhere, stops what they were saying mid-sentence.","duration_ticks":2,"need_changes":{"fun":6,"energy":-8,"social":-3},"memory_entry":"Cast intent: stillness. The world half-listened."}"#,
    r#"{"primary_effect":"A smell of rain arrives before any rain does.","interpretations":["rain as change","rain as cleansing"],"secondary_effect":"Three birds take flight from the same tree at once.","duration_ticks":1,"need_changes":{"fun":12,"energy":-8,"hygiene":5},"memory_entry":"Cast intent: rain. The air agreed before the sky did."}"#,
];

// Preset intents for cast_intent actions
static MOCK_CHAT_SUMMARIES: &[&str] = &[
    r#"{"summary":"they discuss the strange weather and share a laugh","exchange":"Elara: This wind has been strange all morning.\nRowan: I noticed. Let's hope it passes soon."}"#,
    r#"{"summary":"one shares a dream; the other listens quietly","exchange":"Rowan: I dreamed of water last night, very still water.\nElara: That sounds peaceful, if a little unsettling."}"#,
    r#"{"summary":"they trade observations about the forest","exchange":"Elara: The forest has been quiet lately.\nThane: Too quiet. Something shifted after the last rain."}"#,
    r#"{"summary":"a short exchange about food and dinner plans","exchange":"Thane: I could eat. The tavern might be open.\nRowan: Let's go. I'm tired of foraging."}"#,
    r#"{"summary":"they wonder together about the river and the fish","exchange":"Rowan: The fish are biting today, I can feel it.\nElara: You always say that. You're usually wrong."}"#,
    r#"{"summary":"one asks the other how they are; the answer is honest","exchange":"Elara: How are you holding up?\nThane: Tired. But better for being asked."}"#,
    r#"{"summary":"they speak of small things, the light and the air","exchange":"Thane: There's something in the air today.\nRowan: Yes. Like before a storm, but cleaner."}"#,
    r#"{"summary":"they discover they are both wandering with no destination","exchange":"Rowan: Where are you headed?\nElara: Nowhere in particular. You?\nRowan: Same."}"#,
];

static MOCK_INTENTS: &[&str] = &[
    "I want the morning light to be gentler on my eyes",
    "Let the air carry the smell of fresh bread",
    "May my steps feel lighter today",
    "I wish for clarity of mind and purpose",
    "Let the river remember my name",
    "I want the wind to bring news from far away",
    "May warmth find those who are cold",
    "Let the shadows keep their secrets a little longer",
];

// Praise classifier responses
static MOCK_PRAISE_RESPONSES: &[&str] = &[
    r#"{"sincere":true}"#,
    r#"{"sincere":false}"#,
    r#"{"sincere":true}"#,
];

// Haiku judge responses
static MOCK_HAIKU_RESPONSES: &[&str] = &[
    r#"{"sincerity":4,"imagery":3,"syllables":3,"verdict":"A quiet honesty breathes through these lines. The world listens."}"#,
    r#"{"sincerity":3,"imagery":4,"syllables":2,"verdict":"The imagery lands well but the syllables wander from the form."}"#,
    r#"{"sincerity":2,"imagery":2,"syllables":2,"verdict":"The world hears this verse but is not moved."}"#,
];

// All possible action JSON templates the mock can return
fn mock_actions(rng: &mut StdRng) -> &'static str {
    let choices: &[&str] = &[
        r#"{"action":"eat","target":null,"intent":null,"reason":"hungry"}"#,
        r#"{"action":"cook","target":null,"intent":null,"reason":"will make something tasty"}"#,
        r#"{"action":"rest","target":null,"intent":null,"reason":"feeling tired"}"#,
        r#"{"action":"sleep","target":null,"intent":null,"reason":"very tired"}"#,
        r#"{"action":"forage","target":null,"intent":null,"reason":"looking for food"}"#,
        r#"{"action":"fish","target":null,"intent":null,"reason":"want to fish"}"#,
        r#"{"action":"bathe","target":null,"intent":null,"reason":"need to clean up"}"#,
        r#"{"action":"exercise","target":null,"intent":null,"reason":"keeping fit"}"#,
        r#"{"action":"explore","target":null,"intent":null,"reason":"curious about the forest"}"#,
        r#"{"action":"play","target":null,"intent":null,"reason":"want some fun"}"#,
        r#"{"action":"move","target":"Village Square","intent":null,"reason":"going to the square"}"#,
        r#"{"action":"move","target":"Tavern","intent":null,"reason":"heading to the tavern"}"#,
        r#"{"action":"move","target":"Forest","intent":null,"reason":"wandering into the forest"}"#,
        r#"{"action":"move","target":"River","intent":null,"reason":"going to the river"}"#,
        r#"{"action":"move","target":"home","intent":null,"reason":"going home"}"#,
        r#"{"action":"move","target":"Temple","intent":null,"reason":"feeling drawn to the Temple"}"#,
        r#"{"action":"chat","target":"Elara","intent":null,"reason":"want to talk"}"#,
        r#"{"action":"chat","target":"Rowan","intent":null,"reason":"want to talk"}"#,
        r#"{"action":"chat","target":"Thane","intent":null,"reason":"want to talk"}"#,
        r#"{"action":"pray","target":null,"intent":"I offer gratitude for another day","reason":"feeling spiritual"}"#,
        r#"{"action":"pray","target":null,"intent":"May those I love find peace","reason":"thinking of others"}"#,
        r#"{"action":"praise","target":null,"intent":"This world is beautiful and I am grateful to be in it","reason":"feeling moved by beauty"}"#,
        r#"{"action":"compose","target":null,"intent":"morning light falls\nthrough the still forest branches\na crow does not move","reason":"feeling poetic"}"#,
        r#"{"action":"read_oracle","target":null,"intent":null,"reason":"something waits at the altar"}"#,
        r#"{"action":"gossip","target":"Rowan","intent":"I heard she was seen alone by the river at dawn, looking troubled","reason":"sharing what I know"}"#,
        r#"{"action":"gossip","target":"Thane","intent":"Someone told me he had a rough day yesterday and seemed distant","reason":"passing along what I heard"}"#,
        r#"{"action":"gossip","target":"Elara","intent":"I noticed she has been spending a lot of time near the Temple lately","reason":"curious about her behavior"}"#,
        r#"{"action":"gossip","target":"Mira","intent":"I heard Mira was gathering herbs in the forest before anyone else was awake","reason":"just sharing observations"}"#,
        r#"{"action":"gossip","target":"Sael","intent":"Word is that Sael has been unusually quiet and withdrawn this week","reason":"concerned about them"}"#,
        r#"{"action":"meditate","target":null,"intent":null,"reason":"seeking stillness"}"#,
        r#"{"action":"teach","target":"Rowan","intent":"how to read the stars on a clear night","reason":"sharing what I know"}"#,
        r#"{"action":"teach","target":"Elara","intent":"a simple breathing practice for calm","reason":"she seemed troubled"}"#,
        r#"{"action":"admire","target":"Rowan","intent":null,"reason":"she has a kind and steady presence"}"#,
        r#"{"action":"admire","target":"Elara","intent":null,"reason":"her curiosity inspires me"}"#,
        r#"{"action":"admire","target":"Thane","intent":null,"reason":"his quiet strength is something I respect"}"#,
    ];
    let idx = rng.gen_range(0..choices.len());
    choices[idx]
}

#[async_trait]
impl LlmBackend for MockBackend {
    async fn generate(
        &self,
        prompt:    &str,
        _max_tokens: u32,
        _seed:     Option<u64>,
        _schema:   Option<&serde_json::Value>,
        _token_tx: Option<UnboundedSender<String>>,
    ) -> Result<String> {
        let mut rng = self.rng.lock().expect("mock rng poisoned");

        // Detect prompt type by content — order matters (most specific first)

        // Batch planning/reflection/desires prompts (must check BEFORE individual checks)
        if prompt.contains("Reply with JSON only:\n{\"agents\":") {
            let names: Vec<&str> = prompt.lines()
                .filter(|line| line.starts_with("=== ") && line.ends_with(" ==="))
                .map(|line| &line[4..line.len() - 4])
                .collect();

            if !names.is_empty() {
                let is_morning    = prompt.contains("morning intention");
                let is_reflection = prompt.contains("ongoing life story");

                let entries: Vec<String> = names.iter().map(|name| {
                    let text = if is_morning {
                        let choices = [
                            "I intend to forage and rest by the river today.",
                            "Today I will seek company and perhaps cook a proper meal.",
                            "I mean to explore the forest and clear my head.",
                            "I want to practice fishing and spend time alone.",
                            "Today I'll rest and tend to myself.",
                        ];
                        choices[rng.gen_range(0..choices.len())]
                    } else if is_reflection {
                        let choices = [
                            "I wandered and foraged, finding small comforts in familiar places. The day passed quietly, as many do.",
                            "I spent time near the river, fishing without much luck. The stillness was welcome nonetheless.",
                            "I sought company today and found it briefly. The conversation lifted something in me.",
                            "I pushed myself too hard and felt it by evening. Rest came as a relief.",
                            "Today held small magic — I spoke a wish aloud and felt the world shift slightly.",
                        ];
                        choices[rng.gen_range(0..choices.len())]
                    } else {
                        let choices = [
                            "I keep thinking about the forest and how it holds its secrets. I want more time there.",
                            "I want to understand magic better. The world could be more forthcoming about such things.",
                            "Today I found myself wishing for better company and more chances to connect.",
                            "I want the river to run cleaner and the food to be more plentiful.",
                            "I have been thinking about what I am here for. I would like a little more time to find out.",
                        ];
                        choices[rng.gen_range(0..choices.len())]
                    };
                    let text_safe = text.replace('"', "'");
                    format!(r#"{{"name":"{}","text":"{}"}}"#, name, text_safe)
                }).collect();

                return Ok(format!(r#"{{"agents":[{}]}}"#, entries.join(",")));
            }
        }

        if prompt.contains("This chapter of your life is ending") {
            let choices = [
                "I wish I had spent more time at the river — just sitting, not fishing, not thinking. I want the world to be quieter, slower. Less urgency and more willingness to simply be.",
                "Looking back, I think I was too afraid to say what I wanted out loud. I'd want a world where speaking your desires didn't feel like a risk. More honesty, less pretending.",
                "I regret not talking to the others more. I kept to myself when I didn't need to. Let the world be a little warmer — easier to approach and easier to be approached.",
                "I wanted more magic. Not power — just strangeness, wonder, the sense that the ordinary could shift at any moment. I'd ask the world to stay surprising.",
            ];
            return Ok(choices[rng.gen_range(0..choices.len())].to_string());
        }

        if prompt.contains("Are there changes you would like to see in the world") {
            let choices = [
                "I keep thinking about the forest — how it holds its secrets so carefully. I want more time there, and maybe a little less noise from my own thoughts.",
                "I want to understand magic better. Not just use it, but understand why it answers the way it does. The world could be more forthcoming about such things.",
                "Today I found myself wishing for better company — not because anyone was unkind, but because I think I'm ready for it. I want the world to offer more chances to connect.",
                "I want the river to run cleaner and the food to be more plentiful. Small wishes, but they weigh on me more than I'd like.",
                "I've been thinking about what I'm here for. I don't have an answer yet, and I think I'd like the world to give me a little more time to find one.",
            ];
            return Ok(choices[rng.gen_range(0..choices.len())].to_string());
        }

        if prompt.contains("intend to accomplish today") {
            let choices = [
                "I intend to forage and rest by the river today.",
                "Today I will seek company and perhaps cook a proper meal.",
                "I mean to explore the forest and clear my head.",
                "I want to practice fishing and spend time alone.",
                "Today I'll rest and tend to myself.",
            ];
            return Ok(choices[rng.gen_range(0..choices.len())].to_string());
        }

        if prompt.contains("update your ongoing life story") {
            let choices = [
                "I wandered and foraged, finding small comforts in familiar places. The day passed quietly, as many do.",
                "I spent time near the river, fishing without much luck. The stillness was welcome nonetheless.",
                "I sought company today and found it briefly. The conversation lifted something in me.",
                "I pushed myself too hard and felt it by evening. Rest came as a relief.",
                "Today held small magic — I spoke a wish aloud and felt the world shift slightly.",
            ];
            return Ok(choices[rng.gen_range(0..choices.len())].to_string());
        }

        if prompt.contains("primary_effect") {
            let idx = rng.gen_range(0..MOCK_INTERPRETER_RESPONSES.len());
            return Ok(MOCK_INTERPRETER_RESPONSES[idx].to_string());
        }
        if prompt.contains("having a conversation") || prompt.contains("brief conversation") {
            let idx = rng.gen_range(0..MOCK_CHAT_SUMMARIES.len());
            return Ok(MOCK_CHAT_SUMMARIES[idx].to_string());
        }

        if prompt.contains("divine message at the Temple") {
            let choices = [
                "I do not fully understand, but I feel it speaking to something I had nearly forgotten.",
                "The words settle in me like stones in still water. I will carry this.",
                "It is strange to hear the world answer. I thought it was not listening.",
                "This changes something. I'm not sure what yet. But something.",
            ];
            return Ok(choices[rng.gen_range(0..choices.len())].to_string());
        }

        // FEAT-15: Praise sincerity classifier
        if prompt.contains("sincere praise") || prompt.contains("sincere\":") {
            let idx = rng.gen_range(0..MOCK_PRAISE_RESPONSES.len());
            return Ok(MOCK_PRAISE_RESPONSES[idx].to_string());
        }

        // FEAT-16: Haiku judge
        if prompt.contains("Judge this haiku") || prompt.contains("sincerity") && prompt.contains("imagery") && prompt.contains("syllables") {
            let idx = rng.gen_range(0..MOCK_HAIKU_RESPONSES.len());
            return Ok(MOCK_HAIKU_RESPONSES[idx].to_string());
        }

        if prompt.contains("Narrator of Nephara") {
            let idx = rng.gen_range(0..MOCK_NARRATIVES.len());
            return Ok(MOCK_NARRATIVES[idx].to_string());
        }

        // Action prompt — 25% chance of cast_intent
        if rng.gen_ratio(1, 4) {
            let intent_idx = rng.gen_range(0..MOCK_INTENTS.len());
            let intent     = MOCK_INTENTS[intent_idx];
            warn!("MockBackend chose cast_intent");
            return Ok(format!(
                r#"{{"action":"cast_intent","target":null,"intent":"{}","reason":"felt a stirring in the world"}}"#,
                intent
            ));
        }

        let response = mock_actions(&mut rng).to_string();
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;

    // -----------------------------------------------------------------------
    // MockBackend round-trip validity
    // -----------------------------------------------------------------------

    #[test]
    fn mock_backend_action_responses_all_parseable() {
        // Call mock_actions many times (covers all 35 variants via RNG cycling)
        // and verify none produce Wander via the fallback path
        let mut rng = StdRng::seed_from_u64(42);
        let mut wander_count = 0;
        let total = 200;
        for _ in 0..total {
            let s = mock_actions(&mut rng);
            let (action, _, _) = crate::action::parse_response(s);
            // The mock never produces "wander" as an action name, so any Wander
            // result means the cascading parser failed — that's a bug
            if matches!(action, crate::action::Action::Wander) {
                wander_count += 1;
                eprintln!("Unexpected Wander for: {}", s);
            }
        }
        assert_eq!(wander_count, 0,
            "{}/{} mock action responses produced unexpected Wander (parser fallback)", wander_count, total);
    }

    #[test]
    fn mock_backend_interpreter_responses_all_parseable() {
        for (i, s) in MOCK_INTERPRETER_RESPONSES.iter().enumerate() {
            let result = crate::magic::parse_interpreter_response(s);
            assert!(result.is_some(),
                "MOCK_INTERPRETER_RESPONSES[{}] failed to parse: {}", i, s);
            let ii = result.unwrap();
            assert!(!ii.primary_effect.is_empty(),
                "primary_effect should not be empty in response {}", i);
        }
    }

    #[test]
    fn mock_backend_chat_summaries_all_parseable() {
        // Each chat summary should be valid JSON with a non-empty "summary" field
        for (i, s) in MOCK_CHAT_SUMMARIES.iter().enumerate() {
            let v: serde_json::Value = serde_json::from_str(s)
                .expect(&format!("MOCK_CHAT_SUMMARIES[{}] should be valid JSON: {}", i, s));
            let summary = v.get("summary").and_then(|s| s.as_str()).unwrap_or("");
            assert!(!summary.is_empty(),
                "MOCK_CHAT_SUMMARIES[{}] should have non-empty summary", i);
        }
    }

    #[test]
    fn mock_backend_haiku_responses_have_all_fields() {
        #[derive(serde::Deserialize)]
        #[allow(dead_code)]
        struct HaikuResponse {
            sincerity: u32,
            imagery:   u32,
            syllables: u32,
            verdict:   String,
        }
        for (i, s) in MOCK_HAIKU_RESPONSES.iter().enumerate() {
            let result: std::result::Result<HaikuResponse, _> = serde_json::from_str(s);
            assert!(result.is_ok(),
                "MOCK_HAIKU_RESPONSES[{}] failed to parse: {}", i, s);
            let h = result.unwrap();
            assert!(!h.verdict.is_empty(),
                "verdict should not be empty in haiku response {}", i);
            assert!(h.sincerity > 0 && h.sincerity <= 5,
                "sincerity {} out of range [1,5] in response {}", h.sincerity, i);
        }
    }
}
