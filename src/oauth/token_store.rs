use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{GrokSearchError, Result};
use crate::oauth::constants::{
    DEFAULT_XAI_OAUTH_BASE_URL, XAI_ACCESS_TOKEN_REFRESH_SKEW_SECONDS, XAI_OAUTH_CLIENT_ID,
    XAI_OAUTH_ISSUER,
};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenStore {
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default)]
    pub id_token: String,
    pub token_endpoint: String,
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default)]
    pub last_refresh: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthStatus {
    pub path: PathBuf,
    pub authenticated: bool,
    pub access_expires_at: Option<u64>,
    pub refresh_token_present: bool,
    pub base_url: Option<String>,
}

fn default_base_url() -> String {
    DEFAULT_XAI_OAUTH_BASE_URL.to_string()
}

pub fn load_token_store(path: &Path) -> Result<Option<TokenStore>> {
    if !path.exists() {
        return Ok(None);
    }
    let body = std::fs::read_to_string(path)
        .map_err(|err| GrokSearchError::OAuth(format!("oauth_token_read_failed: {err}")))?;
    let store = serde_json::from_str::<TokenStore>(&body)
        .map_err(|err| GrokSearchError::OAuth(format!("oauth_token_parse_failed: {err}")))?;
    Ok(Some(store))
}

pub fn save_token_store(path: &Path, store: &TokenStore) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| GrokSearchError::OAuth(format!("oauth_token_dir_failed: {err}")))?;
    }
    let payload = serde_json::to_string_pretty(store)
        .map_err(|err| GrokSearchError::OAuth(format!("oauth_token_serialize_failed: {err}")))?;
    let tmp = path.with_file_name(format!(
        "{}.tmp.{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("auth.json"),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&tmp, payload)
        .map_err(|err| GrokSearchError::OAuth(format!("oauth_token_write_failed: {err}")))?;
    commit_token_file(&tmp, path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn commit_token_file(tmp: &Path, path: &Path) -> Result<()> {
    match std::fs::rename(tmp, path) {
        Ok(_) => Ok(()),
        Err(first_err) if path.exists() => {
            std::fs::remove_file(path).map_err(|err| {
                GrokSearchError::OAuth(format!("oauth_token_commit_failed: {err}"))
            })?;
            std::fs::rename(tmp, path).map_err(|err| {
                GrokSearchError::OAuth(format!(
                    "oauth_token_commit_failed: {err}; previous commit error: {first_err}"
                ))
            })
        }
        Err(err) => Err(GrokSearchError::OAuth(format!(
            "oauth_token_commit_failed: {err}"
        ))),
    }
}

pub fn delete_token_store(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    std::fs::remove_file(path)
        .map_err(|err| GrokSearchError::OAuth(format!("oauth_logout_failed: {err}")))?;
    Ok(true)
}

pub fn auth_status(path: &Path) -> AuthStatus {
    let store = load_token_store(path).ok().flatten();
    let access = store.as_ref().map(|s| s.access_token.as_str()).unwrap_or("");
    AuthStatus {
        path: path.to_path_buf(),
        authenticated: !access.is_empty(),
        access_expires_at: jwt_exp(access),
        refresh_token_present: store
            .as_ref()
            .map(|s| !s.refresh_token.is_empty())
            .unwrap_or(false),
        base_url: store
            .as_ref()
            .map(|s| s.base_url.clone())
            .filter(|s| !s.is_empty()),
    }
}

pub async fn get_access_token(client: &Client, path: &Path) -> Result<String> {
    let mut store = load_token_store(path)?.ok_or_else(|| {
        GrokSearchError::OAuth(
            "oauth_not_logged_in: run `grok-search-rs login` before using OAuth mode".to_string(),
        )
    })?;
    if store.access_token.is_empty() {
        return Err(GrokSearchError::OAuth(
            "oauth_not_logged_in: stored token is empty; run `grok-search-rs login`".to_string(),
        ));
    }
    if token_is_expiring(&store.access_token, XAI_ACCESS_TOKEN_REFRESH_SKEW_SECONDS) {
        refresh_token(client, path, &mut store).await?;
    }
    Ok(store.access_token)
}

pub fn jwt_exp(access_token: &str) -> Option<u64> {
    let payload = access_token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload.as_bytes()).ok()?;
    let value = serde_json::from_slice::<Value>(&decoded).ok()?;
    value.get("exp")?.as_u64()
}

pub fn token_is_expiring(access_token: &str, skew_seconds: u64) -> bool {
    let Some(exp) = jwt_exp(access_token) else {
        return false;
    };
    exp <= now_unix().saturating_add(skew_seconds)
}

async fn refresh_token(client: &Client, path: &Path, store: &mut TokenStore) -> Result<()> {
    if store.refresh_token.is_empty() || store.token_endpoint.is_empty() {
        return Err(GrokSearchError::OAuth(
            "oauth_refresh_missing_token: run `grok-search-rs login` again".to_string(),
        ));
    }
    if !store
        .token_endpoint
        .starts_with(&format!("{XAI_OAUTH_ISSUER}/"))
    {
        return Err(GrokSearchError::OAuth(
            "oauth_refresh_invalid_endpoint: token endpoint is not auth.x.ai".to_string(),
        ));
    }

    let response = client
        .post(&store.token_endpoint)
        .header("Accept", "application/json")
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", XAI_OAUTH_CLIENT_ID),
            ("refresh_token", store.refresh_token.as_str()),
        ])
        .send()
        .await
        .map_err(|err| GrokSearchError::OAuth(format!("oauth_refresh_failed: {err}")))?;

    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|err| GrokSearchError::OAuth(format!("oauth_refresh_body_failed: {err}")))?;
    if !status.is_success() {
        return Err(GrokSearchError::OAuth(format!(
            "oauth_refresh_failed: upstream returned HTTP {status}"
        )));
    }
    let payload = serde_json::from_str::<Value>(&body)
        .map_err(|err| GrokSearchError::OAuth(format!("oauth_refresh_parse_failed: {err}")))?;
    let access_token = payload
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            GrokSearchError::OAuth(
                "oauth_refresh_missing_access_token: run `grok-search-rs login` again".to_string(),
            )
        })?;

    store.access_token = access_token.to_string();
    if let Some(refresh) = payload.get("refresh_token").and_then(Value::as_str) {
        if !refresh.is_empty() {
            store.refresh_token = refresh.to_string();
        }
    }
    if let Some(id_token) = payload.get("id_token").and_then(Value::as_str) {
        store.id_token = id_token.to_string();
    }
    store.last_refresh = now_unix().to_string();
    save_token_store(path, store)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;

    fn fake_jwt(exp: u64) -> String {
        let payload = URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#));
        format!("header.{payload}.sig")
    }

    #[test]
    fn parses_jwt_exp() {
        assert_eq!(jwt_exp(&fake_jwt(12345)), Some(12345));
    }

    #[test]
    fn detects_expiring_tokens() {
        assert!(token_is_expiring(&fake_jwt(1), 120));
        assert!(!token_is_expiring(&fake_jwt(now_unix() + 10_000), 120));
    }

    #[test]
    fn damaged_token_file_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        std::fs::write(&path, "{not json").unwrap();
        assert!(load_token_store(&path).is_err());
    }

    #[test]
    fn save_and_load_token_store_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let store = TokenStore {
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            id_token: "id".into(),
            token_endpoint: "https://auth.x.ai/token".into(),
            base_url: DEFAULT_XAI_OAUTH_BASE_URL.into(),
            last_refresh: "now".into(),
        };
        save_token_store(&path, &store).unwrap();
        assert_eq!(load_token_store(&path).unwrap(), Some(store));
    }
}
