//! Direct ChatGPT-plan inference with a fixed VM-only tool surface.
//!
//! The model returns data. This module never evaluates code, invokes a process,
//! dispatches a tool, or accepts a model-supplied URL. Only a fully completed,
//! validated response can reach the Rust guest-tool dispatcher.
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    io::Read,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

const RESPONSES_URL: &str = "https://api.openai.com/v1/responses";
const MODELS_URL: &str = "https://api.openai.com/v1/models";
const MAX_MODELS_BYTES: usize = 2 * 1024 * 1024;
const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;
const MAX_STREAM_BYTES: usize = 32 * 1024 * 1024;
const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;
const MAX_PUBLIC_MESSAGE_BYTES: usize = 64 * 1024;
const MAX_ERROR_BYTES: usize = 64 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_ATTEMPTS: usize = 3;

/// Replayable output is held only in the current agent turn. Reasoning is kept
/// as opaque encrypted state, never exposed to the public observer journal.
pub struct ProviderResponse {
    pub output: Vec<Value>,
    pub usage: Value,
}

/// Fetch the account-specific ChatGPT catalog, preserving its display order.
/// This is a read-only OAuth request, never an inference or billing fallback.
pub fn models() -> Result<Vec<(String, String)>, String> {
    let token = crate::direct_auth::access_token()?;
    let client = reqwest::blocking::Client::builder()
        .https_only(true)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| "Could not initialize the ChatGPT model catalog connection.")?;
    let response = client
        .get(MODELS_URL)
        .bearer_auth(token)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .map_err(|_| "Could not fetch the ChatGPT model catalog; no retry was made.")?;
    let status = response.status().as_u16();
    let success = response.status().is_success();
    let limit = if success {
        MAX_MODELS_BYTES
    } else {
        MAX_ERROR_BYTES
    };
    let mut bytes = Vec::new();
    response
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "Could not read the ChatGPT model catalog response.")?;
    if bytes.len() > limit {
        return Err("ChatGPT model catalog exceeded the transport limit.".into());
    }
    let body: Value = serde_json::from_slice(&bytes)
        .map_err(|_| "ChatGPT returned a malformed model catalog response.")?;
    if !success {
        return Err(provider_error(&body["error"], Some(status)));
    }
    parse_models(&body)
}

fn parse_models(body: &Value) -> Result<Vec<(String, String)>, String> {
    let catalog = body["models"]
        .as_array()
        .filter(|items| items.len() <= 1024)
        .ok_or("ChatGPT returned an invalid account model catalog.")?;
    let mut names = BTreeSet::new();
    let mut result = Vec::new();
    for model in catalog.iter().filter(|model| model["visibility"] == "list") {
        let slug = model["slug"]
            .as_str()
            .filter(|s| {
                !s.is_empty()
                    && s.len() <= 120
                    && !s.chars().any(char::is_whitespace)
                    && !s.chars().any(char::is_control)
            })
            .ok_or("ChatGPT returned an invalid model identifier.")?;
        let display = model["display_name"]
            .as_str()
            .filter(|s| !s.trim().is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
            .ok_or("ChatGPT returned an invalid model display name.")?;
        if !names.insert(slug) {
            return Err("ChatGPT returned a duplicate model identifier.".into());
        }
        result.push((slug.to_owned(), display.to_owned()));
    }
    Ok(result)
}

/// No hosted tools, MCP, code execution, web access, or deferred discovery.
pub fn vm_tools() -> Value {
    let tools = crate::mcp::tools()
        .as_array()
        .expect("fixed machine catalog")
        .iter()
        .map(|tool| {
            json!({
                "type":"function", "name":tool["name"],
                "description":tool["description"],
                "parameters":tool["inputSchema"], "strict":true
            })
        })
        .collect::<Vec<_>>();
    json!([{"type":"namespace","name":"picos",
        "description":"The experiment's bounded virtual machine. All executable software runs in guest RAM and consumes guest cycles.",
        "tools":tools}])
}

fn request_body(model: &str, instructions: &str, input: &[Value]) -> Result<Value, String> {
    if model.trim().is_empty() || model.len() > 120 || model.contains(['\r', '\n', '\0']) {
        return Err("A valid, explicitly selected ChatGPT model is required.".into());
    }
    let body = json!({
        "model":model, "instructions":instructions, "input":input,
        "store":false, "stream":true, "parallel_tool_calls":false,
        "tools":vm_tools(),
        "include":["reasoning.encrypted_content"]
    });
    if body.to_string().len() > MAX_REQUEST_BYTES {
        return Err("Strict model context reached its 8 MiB request limit.".into());
    }
    Ok(body)
}

/// Use only picos's separately authorized ChatGPT OAuth grant. There is no API
/// key, Codex-token, alternate provider, or endpoint fallback.
pub fn respond(
    model: &str,
    instructions: &str,
    input: &[Value],
    cancel: &AtomicBool,
    mut on_event: impl FnMut(&str, &str),
) -> Result<ProviderResponse, String> {
    check_cancel(cancel)?;
    let body = request_body(model, instructions, input)?;
    let token = crate::direct_auth::access_token()?;
    check_cancel(cancel)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "Could not initialize the direct ChatGPT connection.".to_owned())?;
    runtime.block_on(async {
        let client = reqwest::Client::builder()
            .https_only(true)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(15))
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|_| "Could not initialize the direct ChatGPT connection.".to_owned())?;
        // One shared deadline includes every attempt and backoff. A retry never
        // replays dispatched tools: only this pending inference is repeated.
        tokio::time::timeout(REQUEST_TIMEOUT, request_with_retries(
            &client, RESPONSES_URL, &token, &body, cancel, &mut on_event,
            Duration::from_secs(1),
        )).await.map_err(|_| "ChatGPT inference exceeded its 300-second request deadline; no pending tools were executed. Continue explicitly when ready.".to_owned())?
    })
}

#[derive(Debug)]
struct AttemptFailure {
    message: String,
    retryable: bool,
    retry_after: Option<Duration>,
}
impl From<String> for AttemptFailure {
    fn from(message: String) -> Self {
        Self {
            message,
            retryable: false,
            retry_after: None,
        }
    }
}
fn transport_failure(error: reqwest::Error, phase: &str, elapsed: Duration) -> AttemptFailure {
    // Never print reqwest's Display/source chain: it can contain a URL or
    // provider data. Classify using its structured flags instead.
    let category = if error.is_timeout() {
        "timed out"
    } else if error.is_connect() {
        "connection failed"
    } else if error.is_body() {
        "body transfer failed"
    } else if error.is_request() {
        "request transport failed"
    } else {
        "transport failed"
    };
    AttemptFailure {
        message: format!(
            "ChatGPT {phase} {category} after {} ms",
            elapsed.as_millis()
        ),
        retryable: error.is_timeout()
            || error.is_connect()
            || error.is_body()
            || error.is_request(),
        retry_after: None,
    }
}

// Production callers always supply RESPONSES_URL. The argument permits local
// HTTP fixtures; it is never derived from model output, CLI options or env vars.
#[allow(clippy::too_many_arguments)]
async fn request_with_retries(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    body: &Value,
    cancel: &AtomicBool,
    on_event: &mut impl FnMut(&str, &str),
    backoff: Duration,
) -> Result<ProviderResponse, String> {
    for attempt in 1..=MAX_ATTEMPTS {
        check_cancel(cancel)?;
        let result = request_once(client, url, token, body, cancel, &mut |message| {
            on_event("agent.message", message);
        })
        .await;
        match result {
            Ok(response) => return Ok(response),
            Err(failure) if failure.retryable && attempt < MAX_ATTEMPTS => {
                let delay = failure
                    .retry_after
                    .unwrap_or(backoff * (1 << (attempt - 1)));
                on_event(
                    "agent.warning",
                    &format!(
                        "{}. Retrying pending inference ({}/{MAX_ATTEMPTS}) in {} ms; no pending VM tools were executed.",
                        failure.message,
                        attempt + 1,
                        delay.as_millis()
                    ),
                );
                cancellable(tokio::time::sleep(delay), cancel).await?;
            }
            Err(failure) if failure.retryable => {
                return Err(format!(
                    "{}; stopped after {attempt} attempts. No pending VM tools were executed. Continue explicitly when ready.",
                    failure.message
                ));
            }
            Err(failure) => return Err(failure.message),
        }
    }
    unreachable!("bounded attempts always return")
}

async fn request_once(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    body: &Value,
    cancel: &AtomicBool,
    on_message: &mut impl FnMut(&str),
) -> Result<ProviderResponse, AttemptFailure> {
    let began = Instant::now();
    let send = client
        .post(url)
        .bearer_auth(token)
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .json(body)
        .send();
    let mut response = cancellable(send, cancel).await?.map_err(|error| {
        transport_failure(error, "request before response headers", began.elapsed())
    })?;
    if !response.status().is_success() {
        // Only explicit temporary service failures may retry. Auth, quota,
        // capability and unknown errors remain authoritative failures.
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .cloned();
        let mut bytes = Vec::new();
        while let Some(chunk) = cancellable(response.chunk(), cancel)
            .await?
            .map_err(|_| format!("Could not read the ChatGPT error response (HTTP {status})."))?
        {
            if bytes.len().saturating_add(chunk.len()) > MAX_ERROR_BYTES {
                return Err(format!("ChatGPT rejected the request (HTTP {status}).").into());
            }
            bytes.extend_from_slice(&chunk);
        }
        let error = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
        return Err(http_failure(status, &error["error"], retry_after.as_ref()));
    }
    validate_stream_headers(response.headers())?;
    let mut parser = StreamParser::default();
    while let Some(chunk) = cancellable(response.chunk(), cancel)
        .await?
        .map_err(|error| transport_failure(error, "response stream", began.elapsed()))?
    {
        check_cancel(cancel)?;
        if let Some(result) = parser.feed(&chunk, on_message)? {
            check_cancel(cancel)?;
            return Ok(result);
        }
    }
    Err(AttemptFailure {
        message: "ChatGPT stream ended before response.completed".into(),
        retryable: true,
        retry_after: None,
    })
}

fn http_failure(
    status: u16,
    error: &Value,
    retry_after: Option<&reqwest::header::HeaderValue>,
) -> AttemptFailure {
    let mut failure = AttemptFailure::from(provider_error(error, Some(status)));
    if !matches!(status, 408 | 500 | 502 | 503 | 504)
        || !matches!(
            error["code"].as_str().unwrap_or(""),
            "" | "server_error"
                | "internal_server_error"
                | "service_unavailable"
                | "subscription_sharing_usage_unavailable"
                | "subscription_sharing_user_unavailable"
        )
    {
        return failure;
    }
    // Respect short numeric Retry-After values. Long or date-valued delays
    // require explicit recovery rather than retrying sooner than requested.
    let delay = match retry_after {
        Some(value) => match value
            .to_str()
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
        {
            Some(seconds) if seconds <= 60 => Duration::from_secs(seconds),
            _ => return failure,
        },
        None => Duration::from_secs(5),
    };
    failure.retryable = true;
    failure.retry_after = Some(delay);
    failure
}

fn validate_stream_headers(headers: &reqwest::header::HeaderMap) -> Result<(), String> {
    // Some direct-subscription responses omit Content-Type despite containing
    // valid SSE. A missing header is not evidence of a different wire format:
    // the bounded parser below must still validate every event and receive a
    // successful response.completed before returning any tool calls.
    let Some(value) = headers.get(reqwest::header::CONTENT_TYPE) else {
        return Ok(());
    };
    if value
        .to_str()
        .ok()
        .and_then(|v| v.split(';').next())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("text/event-stream"))
    {
        Ok(())
    } else {
        Err("ChatGPT returned an unexpected content type; no tools were executed.".into())
    }
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::SeqCst) {
        Err("Direct ChatGPT turn stopped; no pending tools were executed.".into())
    } else {
        Ok(())
    }
}

// Dropping the pending HTTP future and response cancels network work; no
// detached request threads can outlive a stopped turn and return tool calls.
async fn cancellable<F: Future>(future: F, cancel: &AtomicBool) -> Result<F::Output, String> {
    let mut future = Box::pin(future);
    loop {
        check_cancel(cancel)?;
        if let Ok(value) = tokio::time::timeout(Duration::from_millis(100), future.as_mut()).await {
            check_cancel(cancel)?;
            return Ok(value);
        }
    }
}

fn provider_error(error: &Value, status: Option<u16>) -> String {
    // Never include provider messages, response bodies, headers, or credentials
    // in errors: provider text is untrusted and may echo submitted content.
    let recovery = match error["code"].as_str().unwrap_or("") {
        "subscription_sharing_user_not_eligible" => {
            "ChatGPT plan usage is not enabled for this account or workspace."
        }
        "subscription_sharing_usage_limit_exceeded" => {
            "ChatGPT plan usage limit reached. Check ChatGPT Settings > Usage before continuing."
        }
        "subscription_sharing_usage_unavailable" | "subscription_sharing_user_unavailable" => {
            "ChatGPT plan usage is temporarily unavailable. Credentials were preserved; try again later."
        }
        "subscription_sharing_unsupported_capability" => {
            "This account or model does not support a requested capability. Strict mode will not relax its tool boundary."
        }
        "subscription_sharing_route_not_supported" => {
            "The account does not support the direct ChatGPT inference route."
        }
        "subscription_sharing_invalid_user" | "invalid_api_key" => {
            "ChatGPT authorization could not be validated. Run picos login --direct to renew consent."
        }
        "chatpass_v2_scope_not_authorized" | "chatpass_v2_invalid_authorization_context" => {
            "The ChatGPT grant does not authorize direct inference. Run picos login --direct."
        }
        "model_not_found" => {
            "The requested model is not available to the selected ChatGPT account. The model was not substituted."
        }
        "rate_limit_exceeded" => "ChatGPT rate limit reached. No automatic retry was made.",
        _ => {
            "ChatGPT inference failed. No pending tools were executed and no billing fallback was used."
        }
    };
    match status {
        Some(status) => format!("{recovery} (HTTP {status})"),
        None => recovery.into(),
    }
}

#[derive(Default)]
struct StreamParser {
    pending: Vec<u8>,
    data: Vec<u8>,
    total: usize,
    public_ids: BTreeSet<String>,
    completed_items: BTreeMap<usize, Value>,
    completed_bytes: usize,
}

impl StreamParser {
    fn feed(
        &mut self,
        chunk: &[u8],
        on_message: &mut impl FnMut(&str),
    ) -> Result<Option<ProviderResponse>, String> {
        self.total = self.total.saturating_add(chunk.len());
        if self.total > MAX_STREAM_BYTES {
            return Err("ChatGPT stream exceeded the strict transport limit.".into());
        }
        self.pending.extend_from_slice(chunk);
        let mut consumed = 0;
        while let Some(relative_end) = self.pending[consumed..]
            .iter()
            .position(|byte| *byte == b'\n')
        {
            let end = consumed + relative_end;
            let mut line = self.pending[consumed..end].to_vec();
            consumed = end + 1;
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.is_empty() {
                if self.data.is_empty() {
                    continue;
                }
                let data = std::mem::take(&mut self.data);
                if data == b"[DONE]" {
                    return Err("ChatGPT stream ended without a completed response.".into());
                }
                let event: Value = serde_json::from_slice(&data)
                    .map_err(|_| "ChatGPT returned a malformed stream event.".to_owned())?;
                if let Some(result) = self.event(&event, on_message)? {
                    return Ok(Some(result));
                }
            } else if let Some(data) = line.strip_prefix(b"data:") {
                let data = data.strip_prefix(b" ").unwrap_or(data);
                if !self.data.is_empty() {
                    self.data.push(b'\n');
                }
                if self.data.len().saturating_add(data.len()) > MAX_EVENT_BYTES {
                    return Err("ChatGPT event exceeded the strict transport limit.".into());
                }
                self.data.extend_from_slice(data);
            } else if !line.starts_with(b":")
                && !line.starts_with(b"event:")
                && !line.starts_with(b"id:")
                && !line.starts_with(b"retry:")
            {
                return Err("ChatGPT returned a malformed event stream.".into());
            }
        }
        self.pending.drain(..consumed);
        if self.pending.len() > MAX_EVENT_BYTES {
            return Err("ChatGPT event exceeded the strict transport limit.".into());
        }
        Ok(None)
    }

    fn public_message(
        &mut self,
        item: &Value,
        callback: &mut impl FnMut(&str),
    ) -> Result<(), String> {
        let message = sanitize_message(item)?;
        let id = identifier(&message, "id")?;
        if !self.public_ids.insert(id.to_owned()) {
            return Ok(());
        }
        let mut text = String::new();
        for part in message["content"].as_array().unwrap() {
            if let Some(part) = part["text"].as_str().or_else(|| part["refusal"].as_str()) {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(part);
            }
        }
        if !text.is_empty() {
            callback(&text);
        }
        Ok(())
    }

    fn event(
        &mut self,
        event: &Value,
        callback: &mut impl FnMut(&str),
    ) -> Result<Option<ProviderResponse>, String> {
        match event["type"].as_str().unwrap_or("") {
            "response.failed" => Err(provider_error(&event["response"]["error"], None)),
            "error" => Err(provider_error(event, None)),
            "response.incomplete" => {
                Err("ChatGPT response was incomplete; no pending tools were executed.".into())
            }
            "response.output_item.added" => {
                validate_item_type(&event["item"])?;
                if event["item"]["type"] == "function_call" {
                    validate_tool_name(&event["item"])?;
                }
                Ok(None)
            }
            "response.output_item.done" => {
                let index = event["output_index"]
                    .as_u64()
                    .filter(|index| *index < 128)
                    .ok_or("ChatGPT returned an invalid output item index.")?
                    as usize;
                let item = sanitize_output_item(&event["item"])?;
                let id = identifier(&item, "id")?;
                if self.completed_items.contains_key(&index)
                    || self
                        .completed_items
                        .values()
                        .any(|previous| previous["id"] == id)
                {
                    return Err("ChatGPT returned a duplicate output item index or ID.".into());
                }
                let size = item.to_string().len();
                if self.completed_bytes.saturating_add(size) > MAX_STREAM_BYTES {
                    return Err(
                        "ChatGPT completed output exceeded the strict transport limit.".into(),
                    );
                }
                self.completed_bytes += size;
                if item["type"] == "message" {
                    self.public_message(&item, callback)?;
                }
                self.completed_items.insert(index, item);
                // Completed items are buffered only. A later terminal failure
                // must never allow these pending tools to execute.
                Ok(None)
            }
            "response.completed" => {
                let response = &event["response"];
                if response["status"] != "completed" || !response["error"].is_null() {
                    return Err("ChatGPT completion did not confirm successful inference.".into());
                }
                let output = response["output"]
                    .as_array()
                    .ok_or("ChatGPT completion omitted its output.")?;
                if output.len() > 128 {
                    return Err("ChatGPT returned too many output items.".into());
                }
                let sanitized = if output.is_empty() && !self.completed_items.is_empty() {
                    // Some subscription streams deliver completed output only
                    // in indexed item events and use an empty terminal output.
                    for (expected, actual) in self.completed_items.keys().enumerate() {
                        if expected != *actual {
                            return Err("ChatGPT completed output has a missing item index.".into());
                        }
                    }
                    self.completed_items.values().cloned().collect::<Vec<_>>()
                } else {
                    let terminal = output
                        .iter()
                        .map(sanitize_output_item)
                        .collect::<Result<Vec<_>, _>>()?;
                    for (index, item) in &self.completed_items {
                        if terminal.get(*index) != Some(item) {
                            return Err(
                                "ChatGPT terminal output conflicts with completed stream items."
                                    .into(),
                            );
                        }
                    }
                    terminal
                };
                let mut ids = BTreeSet::new();
                let mut calls = BTreeSet::new();
                for item in &sanitized {
                    if !ids.insert(identifier(item, "id")?) {
                        return Err("ChatGPT returned duplicate output item IDs.".into());
                    }
                    if item["type"] == "function_call"
                        && (!calls.insert(identifier(item, "call_id")?) || calls.len() > 1)
                    {
                        return Err("ChatGPT returned multiple or duplicate calls despite sequential execution being required.".into());
                    }
                }
                for item in &sanitized {
                    if item["type"] == "message" {
                        self.public_message(item, callback)?;
                    }
                }
                Ok(Some(ProviderResponse {
                    output: sanitized,
                    usage: safe_usage(&response["usage"]),
                }))
            }
            // These are transport/progress only. No output or executable action
            // is accepted from deltas, including reasoning text/summary deltas.
            "response.metadata"
            | "response.created"
            | "response.queued"
            | "response.in_progress"
            | "response.content_part.added"
            | "response.content_part.done"
            | "response.output_text.delta"
            | "response.output_text.done"
            | "response.output_text.annotation.added"
            | "response.refusal.delta"
            | "response.refusal.done"
            | "response.function_call_arguments.delta"
            | "response.function_call_arguments.done"
            | "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done"
            | "response.reasoning_summary_text.delta"
            | "response.reasoning_summary_text.done"
            | "response.reasoning_text.delta"
            | "response.reasoning_text.done" => Ok(None),
            _ => {
                let kind = event["type"].as_str().filter(|kind| {
                    !kind.is_empty()
                        && kind.len() <= 80
                        && kind
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_'))
                });
                Err(match kind {
                    Some(kind) => format!("ChatGPT emitted unsupported event {kind}; strict mode stopped before tool execution."),
                    None => "ChatGPT emitted an unsupported event; strict mode stopped before tool execution.".into(),
                })
            }
        }
    }
}

fn sanitize_output_item(item: &Value) -> Result<Value, String> {
    validate_item_type(item)?;
    match item["type"].as_str().unwrap() {
        "function_call" => sanitize_call(item),
        "message" => sanitize_message(item),
        "reasoning" => sanitize_reasoning(item),
        _ => unreachable!(),
    }
}

fn identifier<'a>(item: &'a Value, field: &str) -> Result<&'a str, String> {
    item[field]
        .as_str()
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 256
                && value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        })
        .ok_or_else(|| "ChatGPT returned an invalid item identifier.".into())
}

fn validate_item_type(item: &Value) -> Result<(), String> {
    if matches!(
        item["type"].as_str(),
        Some("message" | "reasoning" | "function_call")
    ) {
        Ok(())
    } else {
        Err(
            "ChatGPT returned a non-VM tool or unsupported output item; strict mode stopped."
                .into(),
        )
    }
}

fn validate_tool_name(item: &Value) -> Result<&str, String> {
    if item
        .get("namespace")
        .is_some_and(|namespace| namespace != "picos")
    {
        return Err("ChatGPT requested a tool outside the picos namespace.".into());
    }
    let name = item["name"]
        .as_str()
        .ok_or("ChatGPT omitted the function name.")?;
    if !crate::mcp::tools()
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == name)
    {
        return Err("ChatGPT requested a tool outside the fixed VM allowlist.".into());
    }
    Ok(name)
}

fn sanitize_call(item: &Value) -> Result<Value, String> {
    if item
        .get("status")
        .is_some_and(|status| status != "completed")
        || item.get("async").is_some_and(|value| value != false)
        || item
            .get("caller")
            .is_some_and(|caller| !caller.is_null() && caller["type"] != "direct")
    {
        return Err("ChatGPT returned an incomplete or indirect function call.".into());
    }
    let name = validate_tool_name(item)?;
    let call_id = identifier(item, "call_id")?;
    let id = identifier(item, "id")?;
    let arguments = item["arguments"]
        .as_str()
        .filter(|value| value.len() <= 512 * 1024)
        .ok_or("ChatGPT returned invalid or oversized function arguments.")?;
    let parsed: Value = serde_json::from_str(arguments)
        .map_err(|_| "ChatGPT returned malformed function arguments.")?;
    crate::mcp::validate_arguments(name, &parsed)
        .map_err(|_| "ChatGPT returned arguments outside the VM tool schema.")?;
    let mut call =
        json!({"type":"function_call","id":id,"call_id":call_id,"name":name,"arguments":arguments});
    if item.get("namespace").is_some() {
        call["namespace"] = json!("picos");
    }
    Ok(call)
}

/// The runner calls this again immediately before dispatch. Non-call items are
/// history only; there is no way to interpret their text as an operation.
pub fn tool_call(item: &Value) -> Result<Option<(String, String, Value)>, String> {
    validate_item_type(item)?;
    if item["type"] != "function_call" {
        return Ok(None);
    }
    let call = sanitize_call(item)?;
    let arguments = serde_json::from_str(call["arguments"].as_str().unwrap())
        .map_err(|_| "ChatGPT returned malformed function arguments.".to_owned())?;
    Ok(Some((
        call["name"].as_str().unwrap().to_owned(),
        call["call_id"].as_str().unwrap().to_owned(),
        arguments,
    )))
}

fn sanitize_message(item: &Value) -> Result<Value, String> {
    if item["role"] != "assistant" {
        return Err("ChatGPT returned an unexpected message role.".into());
    }
    let phase = item.get("phase").and_then(Value::as_str);
    if !matches!(phase, None | Some("commentary" | "final_answer")) {
        return Err("ChatGPT returned a non-public message channel.".into());
    }
    if item
        .get("channel")
        .is_some_and(|channel| !channel.is_null() && channel != "commentary" && channel != "final")
    {
        return Err("ChatGPT returned a non-public message channel.".into());
    }
    let id = identifier(item, "id")?;
    let content = item["content"]
        .as_array()
        .filter(|parts| parts.len() <= 64)
        .ok_or("ChatGPT returned invalid message content.")?;
    let mut clean_content = Vec::new();
    let mut size = 0usize;
    for part in content {
        let (kind, key) = match part["type"].as_str().unwrap_or("") {
            "output_text" => ("output_text", "text"),
            "refusal" => ("refusal", "refusal"),
            _ => return Err("ChatGPT returned unsupported message content.".into()),
        };
        let text = part[key]
            .as_str()
            .ok_or("ChatGPT returned invalid public text.")?;
        size = size.saturating_add(text.len());
        if size > MAX_PUBLIC_MESSAGE_BYTES {
            return Err("ChatGPT public message exceeded its strict transport limit.".into());
        }
        let mut part = json!({"type":kind});
        part[key] = json!(text);
        if kind == "output_text" {
            part["annotations"] = json!([]);
        }
        clean_content.push(part);
    }
    let mut result = json!({"type":"message","id":id,"role":"assistant","status":"completed","content":clean_content});
    if let Some(phase) = phase {
        result["phase"] = json!(phase);
    }
    Ok(result)
}

fn sanitize_reasoning(item: &Value) -> Result<Value, String> {
    let id = identifier(item, "id")?;
    let mut clean = json!({"type":"reasoning","id":id,"summary":[]});
    if let Some(encrypted) = item
        .get("encrypted_content")
        .filter(|value| !value.is_null())
    {
        let encrypted = encrypted
            .as_str()
            .filter(|value| value.len() <= MAX_EVENT_BYTES)
            .ok_or("ChatGPT returned invalid encrypted continuity data.")?;
        clean["encrypted_content"] = json!(encrypted);
    }
    Ok(clean)
}

fn safe_usage(usage: &Value) -> Value {
    let mut clean = json!({});
    for field in ["input_tokens", "output_tokens", "total_tokens"] {
        if let Some(value) = usage[field].as_u64() {
            clean[field] = json!(value);
        }
    }
    for (field, nested) in [
        ("input_tokens_details", "cached_tokens"),
        ("output_tokens_details", "reasoning_tokens"),
    ] {
        if let Some(value) = usage[field][nested].as_u64() {
            clean[field] = json!({});
            clean[field][nested] = json!(value);
        }
    }
    clean
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(value: Value) -> Vec<u8> {
        format!("data: {value}\n\n").into_bytes()
    }
    fn complete(output: Value) -> Vec<u8> {
        event(
            json!({"type":"response.completed","response":{"status":"completed","output":output,"usage":{"input_tokens":12,"output_tokens":5,"secret":"not retained"}}}),
        )
    }
    fn call(name: &str) -> Value {
        json!({"type":"function_call","id":"fc_test","call_id":"call_test","name":name,"arguments":"{}"})
    }
    fn message(text: &str) -> Value {
        json!({"type":"message","id":"msg_test","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":text}]})
    }

    struct TransportFixture {
        url: String,
        requests: std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
        stop: std::sync::Arc<AtomicBool>,
        worker: Option<std::thread::JoinHandle<()>>,
    }
    impl TransportFixture {
        fn new(responses: Vec<Vec<u8>>) -> Self {
            use std::io::Write;
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/responses", listener.local_addr().unwrap());
            listener.set_nonblocking(true).unwrap();
            let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let stop = std::sync::Arc::new(AtomicBool::new(false));
            let seen = requests.clone();
            let stopping = stop.clone();
            let worker = std::thread::spawn(move || {
                let mut replies = responses.into_iter();
                while !stopping.load(Ordering::SeqCst) {
                    let (mut socket, _) = match listener.accept() {
                        Ok(connection) => connection,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(2));
                            continue;
                        }
                        Err(e) => panic!("fixture accept: {e}"),
                    };
                    socket.set_nonblocking(false).unwrap();
                    socket
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut request = Vec::new();
                    let mut buffer = [0; 4096];
                    loop {
                        let n = socket.read(&mut buffer).unwrap();
                        if n == 0 {
                            break;
                        }
                        request.extend_from_slice(&buffer[..n]);
                        if let Some(end) = request.windows(4).position(|s| s == b"\r\n\r\n") {
                            let headers = String::from_utf8_lossy(&request[..end]);
                            let length: usize = headers
                                .lines()
                                .find_map(|line| {
                                    let (name, value) = line.split_once(':')?;
                                    name.eq_ignore_ascii_case("content-length")
                                        .then(|| value.trim().parse().unwrap())
                                })
                                .unwrap();
                            if request.len() >= end + 4 + length {
                                seen.lock().unwrap().push(request[end + 4..].to_vec());
                                break;
                            }
                        }
                    }
                    if let Some(reply) = replies.next() {
                        let _ = socket.write_all(&reply);
                    }
                    // Empty reply models a connection reset before headers.
                }
            });
            Self {
                url,
                requests,
                stop,
                worker: Some(worker),
            }
        }
        fn count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }
        fn run(
            &self,
            cancel: &AtomicBool,
            mut event: impl FnMut(&str, &str),
        ) -> Result<ProviderResponse, String> {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let client = reqwest::Client::builder()
                        .no_proxy()
                        .timeout(Duration::from_secs(2))
                        .build()
                        .unwrap();
                    request_with_retries(
                        &client,
                        &self.url,
                        "fixture-only",
                        &json!({"fixture":true}),
                        cancel,
                        &mut event,
                        Duration::from_millis(1),
                    )
                    .await
                })
        }
    }
    impl Drop for TransportFixture {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            let result = self.worker.take().unwrap().join();
            if !std::thread::panicking() {
                result.unwrap();
            }
        }
    }
    fn http_stream(body: Vec<u8>) -> Vec<u8> {
        let mut bytes = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
        bytes.extend(body);
        bytes
    }
    #[test]
    fn connection_failure_retries_identical_pending_inference_only() {
        let fixture = TransportFixture::new(vec![
            vec![],
            http_stream(complete(json!([call("machine_info")]))),
        ]);
        let mut notices = Vec::new();
        let result = fixture
            .run(&AtomicBool::new(false), |kind, text| {
                notices.push((kind.to_owned(), text.to_owned()))
            })
            .unwrap();
        assert_eq!(fixture.count(), 2);
        let requests = fixture.requests.lock().unwrap();
        assert_eq!(requests[0], requests[1]);
        assert_eq!(result.output.len(), 1);
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].0, "agent.warning");
        assert!(notices[0].1.contains("before response headers"));
        assert!(!notices[0].1.contains("fixture-only"));
        assert!(!notices[0].1.contains("127.0.0.1"));
    }
    #[test]
    fn interrupted_stream_discards_buffered_tools_before_retry() {
        let mut abandoned = call("machine_info");
        abandoned["id"] = json!("fc_abandoned");
        let fixture = TransportFixture::new(vec![
            http_stream(item_done(0, abandoned)),
            http_stream(complete(json!([call("machine_info")]))),
        ]);
        let result = fixture.run(&AtomicBool::new(false), |_, _| {}).unwrap();
        assert_eq!(fixture.count(), 2);
        assert_eq!(result.output.len(), 1);
        assert_eq!(result.output[0]["id"], "fc_test");
    }
    #[test]
    fn transport_attempts_are_bounded_and_cancellation_stops_backoff() {
        let fixture = TransportFixture::new(vec![vec![], vec![], vec![]]);
        let error = fixture
            .run(&AtomicBool::new(false), |_, _| {})
            .err()
            .unwrap();
        assert!(error.contains("after 3 attempts"));
        assert_eq!(fixture.count(), MAX_ATTEMPTS);
        let fixture = TransportFixture::new(vec![vec![]]);
        let cancel = AtomicBool::new(false);
        let error = fixture
            .run(&cancel, |_, _| cancel.store(true, Ordering::SeqCst))
            .err()
            .unwrap();
        assert!(error.contains("stopped"));
        assert_eq!(fixture.count(), 1);
    }
    #[test]
    fn temporary_subscription_service_errors_retry_but_quota_and_auth_do_not() {
        let body = json!({"error":{"code":"subscription_sharing_usage_unavailable"}}).to_string();
        let unavailable = format!("HTTP/1.1 503 Service Unavailable\r\nRetry-After: 0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).into_bytes();
        let fixture = TransportFixture::new(vec![
            unavailable,
            http_stream(complete(json!([call("machine_info")]))),
        ]);
        assert_eq!(
            fixture
                .run(&AtomicBool::new(false), |_, _| {})
                .unwrap()
                .output
                .len(),
            1
        );
        assert_eq!(fixture.count(), 2);
        for code in [
            "subscription_sharing_usage_limit_exceeded",
            "insufficient_quota",
            "invalid_api_key",
            "unknown_error",
        ] {
            assert!(!http_failure(503, &json!({"code":code}), None).retryable);
        }
        for status in [400, 401, 403, 429] {
            assert!(
                !http_failure(
                    status,
                    &json!({"code":"subscription_sharing_usage_unavailable"}),
                    None
                )
                .retryable
            );
        }
        for value in ["120", "Wed, 21 Oct 2030 07:28:00 GMT", "invalid"] {
            assert!(!http_failure(503, &json!({}), Some(&value.parse().unwrap())).retryable);
        }
        let failure = http_failure(503, &json!({}), Some(&"12".parse().unwrap()));
        assert_eq!(failure.retry_after, Some(Duration::from_secs(12)));
        assert!(failure.retryable);
    }

    #[test]
    fn http_and_validation_errors_do_not_retry() {
        for response in [
            b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                .to_vec(),
            b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                .to_vec(),
            http_stream(complete(json!([call("exec_command")]))),
            http_stream(event(
                json!({"type":"response.failed","response":{"error":{}}}),
            )),
        ] {
            let fixture = TransportFixture::new(vec![response]);
            assert!(fixture.run(&AtomicBool::new(false), |_, _| {}).is_err());
            assert_eq!(fixture.count(), 1);
        }
    }

    #[test]
    fn missing_content_type_still_requires_a_valid_completed_vm_stream() {
        let mut headers = reqwest::header::HeaderMap::new();
        validate_stream_headers(&headers).unwrap();
        let mut parser = StreamParser::default();
        let added = event(json!({"type":"response.output_item.added","item":call("machine_info")}));
        assert!(parser.feed(&added, &mut |_| {}).unwrap().is_none());
        let result = parser
            .feed(&complete(json!([call("machine_info")])), &mut |_| {})
            .unwrap()
            .unwrap();
        assert_eq!(result.output[0]["name"], "machine_info");
        for invalid in [
            b"<html>error</html>\n\n".as_slice(),
            b"{\"status\":\"completed\"}\n\n",
            b"data: invalid\n\n",
        ] {
            assert!(StreamParser::default().feed(invalid, &mut |_| {}).is_err());
        }
        assert!(
            StreamParser::default()
                .feed(&complete(json!([call("exec_command")])), &mut |_| {})
                .is_err()
        );
        for content_type in ["text/event-stream", "Text/Event-Stream; charset=utf-8"] {
            headers.insert(reqwest::header::CONTENT_TYPE, content_type.parse().unwrap());
            validate_stream_headers(&headers).unwrap();
        }
        for content_type in [
            "text/html",
            "application/json",
            "application/octet-stream",
            "",
        ] {
            headers.insert(reqwest::header::CONTENT_TYPE, content_type.parse().unwrap());
            assert!(validate_stream_headers(&headers).is_err());
        }
    }

    #[test]
    fn request_exposes_only_namespaced_vm_functions_and_subscription_fields() {
        let body = request_body(
            "gpt-6-luna",
            "fixed instructions",
            &[json!({"role":"user","content":"build"})],
        )
        .unwrap();
        assert_eq!(body["model"], "gpt-6-luna");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(body["tools"].as_array().unwrap().len(), 1);
        let namespace = &body["tools"][0];
        assert_eq!(namespace["name"], "picos");
        assert_eq!(namespace["type"], "namespace");
        assert_eq!(namespace["tools"].as_array().unwrap().len(), 9);
        for tool in namespace["tools"].as_array().unwrap() {
            assert_eq!(tool["type"], "function");
            assert!(
                crate::mcp::tools()
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|t| t["name"] == tool["name"])
            );
        }
        for forbidden in [
            "previous_response_id",
            "conversation",
            "max_output_tokens",
            "background",
            "temperature",
        ] {
            assert!(body.get(forbidden).is_none());
        }
    }

    #[test]
    fn model_catalog_is_bounded_ordered_and_has_no_api_key_shape_fallback() {
        let catalog = json!({"models":[
            {"slug":"gpt-6-luna","display_name":"GPT-6 Luna","visibility":"list"},
            {"slug":"hidden","display_name":"Hidden","visibility":"hidden"},
            {"slug":"gpt-6.1-sol","display_name":"GPT-6.1 Sol","visibility":"list"}
        ]});
        assert_eq!(
            parse_models(&catalog).unwrap(),
            vec![
                ("gpt-6-luna".to_owned(), "GPT-6 Luna".to_owned()),
                ("gpt-6.1-sol".to_owned(), "GPT-6.1 Sol".to_owned())
            ]
        );
        assert!(parse_models(&json!({"data":[{"id":"gpt-6-luna"}]})).is_err());
        assert!(parse_models(&json!({"models":vec![json!({});1025]})).is_err());
        let mut invalid = catalog.clone();
        invalid["models"][0]["display_name"] = json!("model\u{1b}[31m");
        assert!(parse_models(&invalid).is_err());
        invalid["models"][0]["display_name"] = json!("Model");
        invalid["models"][0]["slug"] = json!("x".repeat(121));
        assert!(parse_models(&invalid).is_err());
        let mut duplicate = catalog;
        duplicate["models"][2]["slug"] = json!("gpt-6-luna");
        assert!(parse_models(&duplicate).is_err());
        assert!(parse_models(&json!({"models":[]})).unwrap().is_empty());
    }

    fn item_done(index: usize, item: Value) -> Vec<u8> {
        event(json!({"type":"response.output_item.done","output_index":index,"item":item}))
    }

    #[test]
    fn indexed_items_are_released_only_after_successful_empty_terminal_output() {
        let mut parser = StreamParser::default();
        assert!(
            parser
                .feed(
                    &event(json!({"type":"response.metadata","metadata":{"ignored":"value"}})),
                    &mut |_| {}
                )
                .unwrap()
                .is_none()
        );
        let reasoning = json!({"type":"reasoning","id":"rs_1","encrypted_content":"opaque","summary":[{"text":"private sentinel"}]});
        let mut call = call("machine_info");
        call["namespace"] = json!("picos");
        assert!(
            parser
                .feed(&item_done(0, reasoning), &mut |_| panic!(
                    "private text published"
                ))
                .unwrap()
                .is_none()
        );
        assert!(
            parser
                .feed(&item_done(1, call), &mut |_| {})
                .unwrap()
                .is_none()
        );
        let result = parser
            .feed(&complete(json!([])), &mut |_| {})
            .unwrap()
            .unwrap();
        assert_eq!(result.output.len(), 2);
        assert_eq!(result.output[0]["encrypted_content"], "opaque");
        assert!(
            !serde_json::to_string(&result.output)
                .unwrap()
                .contains("private sentinel")
        );
        assert_eq!(
            tool_call(&result.output[1]).unwrap().unwrap().0,
            "machine_info"
        );
        let mut parser = StreamParser::default();
        let mut messages = vec![];
        parser
            .feed(
                &item_done(0, message("Completed public report.")),
                &mut |s| messages.push(s.to_owned()),
            )
            .unwrap();
        parser
            .feed(&complete(json!([])), &mut |s| messages.push(s.to_owned()))
            .unwrap()
            .unwrap();
        assert_eq!(messages, ["Completed public report."]);
    }

    #[test]
    fn pending_completed_items_never_override_failure_or_conflicting_terminal() {
        for terminal in [
            event(
                json!({"type":"response.failed","response":{"error":{"code":"rate_limit_exceeded"}}}),
            ),
            event(json!({"type":"response.incomplete"})),
            event(
                json!({"type":"response.completed","response":{"status":"in_progress","output":[]}}),
            ),
            complete(json!([call("machine_screen")])),
            b"data: [DONE]\n\n".to_vec(),
        ] {
            let mut parser = StreamParser::default();
            assert!(
                parser
                    .feed(&item_done(0, call("machine_info")), &mut |_| {})
                    .unwrap()
                    .is_none()
            );
            assert!(parser.feed(&terminal, &mut |_| {}).is_err());
        }
        let mut parser = StreamParser::default();
        parser
            .feed(&item_done(0, call("machine_info")), &mut |_| {})
            .unwrap();
        assert!(
            parser
                .feed(&complete(json!([call("machine_info")])), &mut |_| {})
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn pending_items_are_bounded_unique_contiguous_and_vm_only() {
        for (index, item) in [
            (0, call("machine_screen")),
            (1, call("machine_info")),
            (128, message("out of range")),
        ] {
            let mut parser = StreamParser::default();
            parser
                .feed(&item_done(0, call("machine_info")), &mut |_| {})
                .unwrap();
            assert!(parser.feed(&item_done(index, item), &mut |_| {}).is_err());
        }
        let mut parser = StreamParser::default();
        parser
            .feed(&item_done(1, call("machine_info")), &mut |_| {})
            .unwrap();
        assert!(parser.feed(&complete(json!([])), &mut |_| {}).is_err());
        let mut parser = StreamParser {
            completed_bytes: MAX_STREAM_BYTES,
            ..Default::default()
        };
        assert!(
            parser
                .feed(&item_done(0, call("machine_info")), &mut |_| {})
                .is_err()
        );
        for item in [
            call("exec_command"),
            json!({"type":"web_search_call","id":"ws_1"}),
        ] {
            assert!(
                StreamParser::default()
                    .feed(&item_done(0, item), &mut |_| {})
                    .is_err()
            );
        }
        let mut parser = StreamParser::default();
        let mut second = call("machine_screen");
        second["id"] = json!("fc_other");
        second["call_id"] = json!("call_other");
        parser
            .feed(&item_done(0, call("machine_info")), &mut |_| {})
            .unwrap();
        parser.feed(&item_done(1, second), &mut |_| {}).unwrap();
        assert!(parser.feed(&complete(json!([])), &mut |_| {}).is_err());
    }

    #[test]
    fn fragmented_stream_only_releases_tools_after_completed() {
        let mut parser = StreamParser::default();
        let added = event(json!({"type":"response.output_item.added","item":call("machine_info")}));
        assert!(parser.feed(&added, &mut |_| {}).unwrap().is_none());
        let done = complete(json!([call("machine_info")]));
        for byte in &done[..done.len() - 1] {
            assert!(parser.feed(&[*byte], &mut |_| {}).unwrap().is_none());
        }
        let result = parser
            .feed(&done[done.len() - 1..], &mut |_| {})
            .unwrap()
            .unwrap();
        assert_eq!(result.output[0]["name"], "machine_info");
        assert_eq!(result.usage, json!({"input_tokens":12,"output_tokens":5}));
    }

    #[test]
    fn rejects_hosted_unknown_namespaces_and_invalid_arguments() {
        let mut foreign = call("machine_info");
        foreign["namespace"] = json!("host");
        let mut invalid = call("machine_run");
        invalid["arguments"] = json!("{\"cycles\":2000001}");
        let mut indirect = call("machine_info");
        indirect["caller"] = json!({"type":"program","caller_id":"program_1"});
        let mut incomplete = call("machine_info");
        incomplete["status"] = json!("incomplete");
        for item in [
            call("exec"),
            call("picos.machine_info"),
            foreign,
            invalid,
            indirect,
            incomplete,
            json!({"type":"code_interpreter_call"}),
            json!({"type":"web_search_call"}),
        ] {
            assert!(
                StreamParser::default()
                    .feed(&complete(json!([item])), &mut |_| {})
                    .is_err()
            );
        }
        let mut allowed = call("machine_info");
        allowed["namespace"] = json!("picos");
        assert!(
            StreamParser::default()
                .feed(&complete(json!([allowed])), &mut |_| {})
                .unwrap()
                .is_some()
        );
        assert!(
            StreamParser::default()
                .feed(
                    &complete(json!([call("machine_info"), call("machine_info")])),
                    &mut |_| {}
                )
                .is_err()
        );
    }

    #[test]
    fn public_messages_are_deduplicated_and_private_reasoning_is_stripped() {
        let mut parser = StreamParser::default();
        let mut visible = Vec::new();
        let msg = message("I will inspect the guest.");
        parser
            .feed(
                &event(json!({"type":"response.output_item.done","output_index":0,"item":msg})),
                &mut |s| visible.push(s.to_owned()),
            )
            .unwrap();
        parser
            .feed(
                &event(json!({"type":"response.reasoning_text.delta","delta":"private sentinel"})),
                &mut |s| visible.push(s.to_owned()),
            )
            .unwrap();
        let result = parser.feed(&complete(json!([msg,{"type":"reasoning","id":"rs_test","encrypted_content":"opaque","summary":[{"text":"private sentinel"}],"content":[{"text":"private sentinel"}]}])), &mut |s|visible.push(s.to_owned())).unwrap().unwrap();
        assert_eq!(visible, ["I will inspect the guest."]);
        assert!(
            !serde_json::to_string(&result.output)
                .unwrap()
                .contains("private sentinel")
        );
        assert_eq!(result.output[1]["encrypted_content"], "opaque");
        let mut analysis = message("private sentinel");
        analysis["phase"] = json!("analysis");
        assert!(
            StreamParser::default()
                .feed(&complete(json!([analysis])), &mut |_| panic!(
                    "private message published"
                ))
                .is_err()
        );
    }

    #[test]
    fn failed_incomplete_malformed_and_premature_end_never_release_tools() {
        for value in [
            json!({"type":"response.failed","response":{"error":{"code":"subscription_sharing_usage_limit_exceeded","message":"secret"}}}),
            json!({"type":"response.incomplete"}),
            json!({"type":"error","code":"unknown","message":"secret"}),
            json!({"type":"response.completed","response":{"status":"incomplete","output":[call("machine_info")]}}),
            json!({"type":"response.code_interpreter_call.in_progress"}),
        ] {
            let error = StreamParser::default()
                .feed(&event(value), &mut |_| {})
                .err()
                .unwrap();
            assert!(!error.contains("secret"));
        }
        assert!(
            StreamParser::default()
                .feed(b"data: [DONE]\n\n", &mut |_| {})
                .is_err()
        );
        assert!(
            StreamParser::default()
                .feed(b"data: {invalid}\n\n", &mut |_| {})
                .is_err()
        );
        assert!(
            StreamParser::default()
                .feed(
                    &event(json!({"type":"response.output_item.done","output_index":0,"item":call("machine_info")})),
                    &mut |_| {}
                )
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn unsupported_event_diagnostic_reports_only_a_bounded_safe_type() {
        let error = StreamParser::default()
            .feed(
                &event(json!({"type":"response.new_progress","message":"secret sentinel"})),
                &mut |_| {},
            )
            .err()
            .unwrap();
        assert!(error.contains("response.new_progress"));
        assert!(!error.contains("secret sentinel"));
        for kind in ["unsafe\nsecret".to_owned(), "x".repeat(81)] {
            let error = StreamParser::default()
                .feed(&event(json!({"type":kind})), &mut |_| {})
                .err()
                .unwrap();
            assert!(!error.contains(&kind));
        }
    }

    #[test]
    fn transport_memory_and_cancel_are_bounded() {
        let mut parser = StreamParser {
            total: MAX_STREAM_BYTES,
            ..Default::default()
        };
        assert!(parser.feed(b"x", &mut |_| {}).is_err());
        let mut parser = StreamParser::default();
        assert!(
            parser
                .feed(&vec![b'x'; MAX_EVENT_BYTES + 1], &mut |_| {})
                .is_err()
        );
        let cancel = AtomicBool::new(true);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert!(
            runtime
                .block_on(cancellable(std::future::pending::<()>(), &cancel))
                .is_err()
        );
        assert_eq!(RESPONSES_URL, "https://api.openai.com/v1/responses");
    }
}
