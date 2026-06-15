//! End-to-end tests for Firecrawl multi-key rotation against a local mock HTTP
//! server. Mirrors Tavily rotation coverage without adding test dependencies.

use grok_search_rs::providers::firecrawl::FirecrawlProvider;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn spawn_mock_server(responses: Vec<(u16, &'static str)>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
    let base = format!("http://{}", listener.local_addr().expect("local addr"));
    let seen_auth = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&seen_auth);

    std::thread::spawn(move || {
        for (status, body) in responses {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("set read timeout");

            let mut raw = Vec::new();
            let mut buf = [0u8; 1024];
            let header_end = loop {
                let n = stream.read(&mut buf).expect("read request");
                if n == 0 {
                    break raw.len();
                }
                raw.extend_from_slice(&buf[..n]);
                if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos + 4;
                }
            };
            let head = String::from_utf8_lossy(&raw[..header_end]).to_string();
            let content_length = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            while raw.len() < header_end + content_length {
                let n = stream.read(&mut buf).expect("read body");
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&buf[..n]);
            }

            let auth = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("authorization")
                        .then(|| value.trim().to_string())
                })
                .unwrap_or_default();
            seen.lock().expect("auth log lock").push(auth);

            let reason = match status {
                200 => "OK",
                402 => "Payment Required",
                429 => "Too Many Requests",
                500 => "Internal Server Error",
                _ => "Mock",
            };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("write response");
        }
    });

    (base, seen_auth)
}

const OK_BODY: &str = r#"{"data":{"markdown":"ok"}}"#;

fn provider_for(base: &str, keys: &str) -> FirecrawlProvider {
    FirecrawlProvider::new(base, keys, Duration::from_secs(5))
}

#[tokio::test]
async fn key_scoped_failure_rotates_to_next_key_and_succeeds() {
    let (base, auth_log) =
        spawn_mock_server(vec![(429, r#"{"error":"rate limited"}"#), (200, OK_BODY)]);
    let provider = provider_for(&base, "key-a,key-b");

    let content = provider
        .scrape("https://example.com")
        .await
        .expect("second key should succeed after 429 on first");

    assert_eq!(content, "ok");
    assert_eq!(
        *auth_log.lock().expect("auth log lock"),
        vec!["Bearer key-a".to_string(), "Bearer key-b".to_string()]
    );
}

#[tokio::test]
async fn successive_requests_round_robin_across_keys() {
    let (base, auth_log) = spawn_mock_server(vec![(200, OK_BODY), (200, OK_BODY)]);
    let provider = provider_for(&base, "key-a,key-b");

    for _ in 0..2 {
        provider
            .scrape("https://example.com")
            .await
            .expect("mock returns 200");
    }

    assert_eq!(
        *auth_log.lock().expect("auth log lock"),
        vec!["Bearer key-a".to_string(), "Bearer key-b".to_string()]
    );
}

#[tokio::test]
async fn upstream_5xx_fails_fast_without_rotation() {
    let (base, auth_log) = spawn_mock_server(vec![(500, r#"{"error":"boom"}"#)]);
    let provider = provider_for(&base, "key-a,key-b");

    let err = provider
        .scrape("https://example.com")
        .await
        .expect_err("500 is upstream-wide and must not rotate");

    assert!(err.to_string().contains("500"), "got: {err}");
    assert_eq!(auth_log.lock().expect("auth log lock").len(), 1);
}
