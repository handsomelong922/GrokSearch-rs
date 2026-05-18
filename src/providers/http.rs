use reqwest::{Client, Response};
use serde_json::Value;
use std::time::Duration;

use crate::error::{GrokSearchError, Result};

/// Build a tuned `reqwest::Client`. The same client is shared across providers
/// so TLS sessions and keep-alive connections can be reused between providers
/// that hit different hosts. Falls back to a bare `Client::new()` if the
/// builder errors (preserves prior behavior for tests that construct providers
/// without env-driven config).
pub fn build_client(timeout: Duration) -> Client {
    Client::builder()
        .timeout(timeout)
        .gzip(true)
        .pool_idle_timeout(Some(Duration::from_secs(90)))
        .tcp_keepalive(Some(Duration::from_secs(60)))
        .tcp_nodelay(true)
        .build()
        .unwrap_or_else(|_| Client::new())
}

/// Issue an authenticated JSON POST and normalize transport / status / parse
/// errors into `GrokSearchError`. `label` appears in error messages to
/// distinguish upstream providers (e.g. "Tavily", "Firecrawl", "Grok Responses").
pub async fn post_json(
    client: &Client,
    endpoint: &str,
    api_key: &str,
    body: &Value,
    label: &str,
) -> Result<Value> {
    let mut response = client
        .post(endpoint)
        .bearer_auth(api_key)
        .json(body)
        .send()
        .await
        .map_err(|err| {
            if err.is_timeout() {
                GrokSearchError::Timeout(format!("{label} request timed out: {err}"))
            } else {
                GrokSearchError::Provider(format!("{label} request failed: {err}"))
            }
        })?;

    let status = response.status();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();

    if status.is_success() && content_type.starts_with("text/event-stream") {
        return read_sse_json(&mut response, label).await;
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|err| GrokSearchError::Provider(format!("{label} body read failed: {err}")))?;

    if !status.is_success() {
        let text = String::from_utf8_lossy(&bytes);
        return Err(GrokSearchError::Provider(format!(
            "{label} returned HTTP {status}: {text}"
        )));
    }

    serde_json::from_slice(&bytes)
        .map_err(|err| GrokSearchError::Parse(format!("invalid {label} JSON: {err}")))
}

async fn read_sse_json(response: &mut Response, label: &str) -> Result<Value> {
    let mut buffer = Vec::new();
    let mut output_text = String::new();
    let mut chat_content = String::new();
    let mut last_json = None;

    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|err| GrokSearchError::Provider(format!("{label} stream read failed: {err}")))?
    {
        buffer.extend_from_slice(&chunk);

        while let Some((event, rest)) = split_sse_event(&buffer) {
            let event = event.to_vec();
            buffer = rest.to_vec();
            let event = parse_sse_event(&event, label)?;

            if event.name.as_deref().is_some_and(is_completion_event_name) && event.data.is_none() {
                return finish_sse_json(label, last_json, output_text, chat_content);
            }

            let Some(data) = event.data.as_deref().map(str::trim) else {
                continue;
            };
            if data.is_empty() {
                continue;
            }
            if data == "[DONE]" {
                return finish_sse_json(label, last_json, output_text, chat_content);
            }
            let value: Value = serde_json::from_str(data).map_err(|err| {
                GrokSearchError::Parse(format!("invalid {label} SSE JSON: {err}"))
            })?;
            collect_stream_delta(&value, &mut output_text, &mut chat_content);

            if value.get("type").and_then(Value::as_str) == Some("response.completed") {
                if let Some(response) = value.get("response") {
                    return Ok(response.clone());
                }
                if !output_text.is_empty() {
                    return Ok(serde_json::json!({ "output_text": output_text }));
                }
                return Ok(value);
            }

            last_json = Some(value);
        }
    }

    finish_sse_json(label, last_json, output_text, chat_content)
}

struct SseEvent {
    name: Option<String>,
    data: Option<String>,
}

fn split_sse_event(buffer: &[u8]) -> Option<(&[u8], &[u8])> {
    if let Some(index) = find_bytes(buffer, b"\n\n") {
        return Some((&buffer[..index], &buffer[index + 2..]));
    }
    if let Some(index) = find_bytes(buffer, b"\r\n\r\n") {
        return Some((&buffer[..index], &buffer[index + 4..]));
    }
    None
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn parse_sse_event(event: &[u8], label: &str) -> Result<SseEvent> {
    let event = std::str::from_utf8(event)
        .map_err(|err| GrokSearchError::Parse(format!("invalid {label} SSE UTF-8: {err}")))?;
    let mut name = None;
    let mut lines = Vec::new();
    for line in event.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(event_name) = line.strip_prefix("event:") {
            name = Some(event_name.trim_start().to_string());
            continue;
        }
        if let Some(data) = line.strip_prefix("data:") {
            lines.push(data.trim_start());
        }
    }

    Ok(SseEvent {
        name,
        data: if lines.is_empty() {
            None
        } else {
            Some(lines.join("\n"))
        },
    })
}

fn is_completion_event_name(name: &str) -> bool {
    matches!(
        name,
        "done" | "end" | "complete" | "completed" | "response.completed"
    )
}

fn synthesize_chat_json(last_json: Option<Value>, chat_content: String) -> Value {
    let mut raw = last_json.unwrap_or_else(|| serde_json::json!({}));
    if !raw.is_object() {
        raw = serde_json::json!({});
    }

    let message = raw
        .pointer("/choices/0/message")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let delta = raw
        .pointer("/choices/0/delta")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));

    let mut message = match message {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    if let Value::Object(delta) = delta {
        for key in ["annotations", "citations"] {
            if !message.contains_key(key) {
                if let Some(value) = delta.get(key) {
                    message.insert(key.to_string(), value.clone());
                }
            }
        }
    }
    message.insert("content".to_string(), Value::String(chat_content));

    let choice = serde_json::json!({ "message": Value::Object(message) });
    if let Some(object) = raw.as_object_mut() {
        object.insert("choices".to_string(), Value::Array(vec![choice]));
        raw
    } else {
        serde_json::json!({ "choices": [choice] })
    }
}

fn collect_stream_delta(value: &Value, output_text: &mut String, chat_content: &mut String) {
    if value.get("type").and_then(Value::as_str) == Some("response.output_text.delta") {
        if let Some(delta) = value.get("delta").and_then(Value::as_str) {
            output_text.push_str(delta);
        }
    }

    if let Some(content) = value.pointer("/choices/0/delta/content") {
        match content {
            Value::String(text) => chat_content.push_str(text),
            Value::Array(parts) => {
                for part in parts {
                    if let Some(text) = part
                        .get("text")
                        .or_else(|| part.get("content"))
                        .and_then(Value::as_str)
                    {
                        chat_content.push_str(text);
                    }
                }
            }
            _ => {}
        }
    }
}

fn finish_sse_json(
    label: &str,
    last_json: Option<Value>,
    output_text: String,
    chat_content: String,
) -> Result<Value> {
    if !output_text.is_empty() {
        return Ok(serde_json::json!({ "output_text": output_text }));
    }
    if !chat_content.is_empty() {
        return Ok(synthesize_chat_json(last_json, chat_content));
    }
    last_json.ok_or_else(|| {
        GrokSearchError::Parse(format!("{label} SSE stream ended without JSON data"))
    })
}
