use std::io::{IsTerminal, Write};

use grok_search_rs::config::{self, AuthMode, Config, InitOutcome};

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // CLI shim: handle --version, --init before MCP server mode.
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args
        .iter()
        .any(|a| a == "--version" || a == "-V" || a == "-v")
    {
        println!("grok-search-rs {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    if args.iter().any(|a| a == "init" || a == "--init") {
        return run_init();
    }

    if args.first().map(String::as_str) == Some("login") {
        let cfg = Config::load();
        return run_login(&cfg).await;
    }

    if args.first().map(String::as_str) == Some("status") {
        let cfg = Config::load();
        return run_status(&cfg);
    }

    if args.first().map(String::as_str) == Some("logout") {
        let cfg = Config::load();
        return run_logout(&cfg);
    }

    let cfg = Config::load();

    if wants_http(&args) {
        let service = grok_search_rs::service::SearchService::new(cfg)?;
        grok_search_rs::mcp::run_http(service, &http_bind_addr()).await?;
        return Ok(());
    }

    // Detect interactive run with missing credentials and print a friendly
    // onboarding guide instead of a cryptic error. MCP clients always pipe
    // stdio, so a TTY here means the user ran the binary directly.
    if cfg.grok_auth_mode == AuthMode::ApiKey
        && cfg.grok_api_key.is_none()
        && std::io::stdin().is_terminal()
    {
        print_setup_guide();
        return Ok(());
    }

    let service = grok_search_rs::service::SearchService::new(cfg)?;
    grok_search_rs::mcp::run_stdio(service).await?;
    Ok(())
}

fn wants_http(args: &[String]) -> bool {
    let arg_http = args
        .first()
        .map(|arg| matches!(arg.as_str(), "serve-http" | "http" | "--http"))
        .unwrap_or(false);
    let env_http = std::env::var("GROK_SEARCH_MCP_TRANSPORT")
        .map(|value| value.eq_ignore_ascii_case("http"))
        .unwrap_or(false);
    arg_http || env_http
}

fn http_bind_addr() -> String {
    if let Ok(bind) = std::env::var("GROK_SEARCH_HTTP_BIND") {
        if !bind.trim().is_empty() {
            return bind;
        }
    }
    let host = std::env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
    let port = std::env::var("PORT")
        .or_else(|_| std::env::var("GROK_SEARCH_HTTP_PORT"))
        .unwrap_or_else(|_| "3000".to_string());
    format!("{host}:{port}")
}

async fn run_login(cfg: &Config) -> anyhow::Result<()> {
    let path = resolve_auth_path(cfg)?;
    let store = grok_search_rs::oauth::login::login(&path, true).await?;
    println!("Login successful.");
    println!("Auth file: {}", path.display());
    if let Some(exp) = grok_search_rs::oauth::token_store::jwt_exp(&store.access_token) {
        println!("Access token expires at unix time: {exp}");
    }
    Ok(())
}

fn run_status(cfg: &Config) -> anyhow::Result<()> {
    let path = resolve_auth_path(cfg)?;
    let status = grok_search_rs::oauth::token_store::auth_status(&path);
    println!("grok-search-rs OAuth status");
    println!("  Auth file: {}", status.path.display());
    println!(
        "  Authenticated: {}",
        if status.authenticated { "yes" } else { "no" }
    );
    println!(
        "  Refresh token: {}",
        if status.refresh_token_present {
            "present"
        } else {
            "missing"
        }
    );
    println!(
        "  Access expires at: {}",
        status
            .access_expires_at
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unknown".to_string())
    );
    println!(
        "  Base URL: {}",
        status.base_url.unwrap_or_else(|| "unknown".to_string())
    );
    Ok(())
}

fn run_logout(cfg: &Config) -> anyhow::Result<()> {
    let path = resolve_auth_path(cfg)?;
    let removed = grok_search_rs::oauth::token_store::delete_token_store(&path)?;
    if removed {
        println!("Removed OAuth token file: {}", path.display());
    } else {
        println!("No OAuth token file found: {}", path.display());
    }
    Ok(())
}

fn resolve_auth_path(cfg: &Config) -> anyhow::Result<std::path::PathBuf> {
    cfg.grok_auth_file
        .clone()
        .or_else(config::auth_path)
        .ok_or_else(|| anyhow::anyhow!("cannot resolve OAuth auth path; set GROK_SEARCH_AUTH_FILE"))
}

/// Scaffold the global config file. Idempotent: existing files are reported
/// and left untouched. Prints the resolved path so the user can `$EDITOR` it.
fn run_init() -> anyhow::Result<()> {
    let path = config::config_path().ok_or_else(|| {
        anyhow::anyhow!(
            "cannot resolve config path: set GROK_SEARCH_CONFIG to an explicit file path, \
             or ensure HOME (Unix / Git Bash) or USERPROFILE (Windows) is set"
        )
    })?;
    match config::write_template(&path)? {
        InitOutcome::Created => {
            println!("✓ wrote template: {}", path.display());
            println!("  edit it and uncomment the keys you need.");
        }
        InitOutcome::AlreadyExists => {
            println!("• config already exists: {}", path.display());
            println!("  not overwriting. delete the file first if you want a fresh template.");
        }
    }
    Ok(())
}

fn print_setup_guide() {
    let mut guide = String::from(
        r#"grok-search-rs is an MCP server. It speaks JSON-RPC over stdio and
should be launched by an MCP client (Claude Code, Codex CLI, Gemini CLI,
Cursor, VS Code, Windsurf, ...), not run directly.

Hosted HTTP mode
  grok-search-rs serve-http
  Exposes JSON-RPC MCP at http://HOST:PORT/mcp (default 0.0.0.0:3000).

Required keys
  GROK_SEARCH_API_KEY   xAI / Grok-compatible key   (https://x.ai/api)
  TAVILY_API_KEY        Tavily fetch + map          (https://tavily.com)
  FIRECRAWL_API_KEY     optional fetch fallback     (https://firecrawl.dev)

OAuth alternative
  grok-search-rs login
  Set GROK_SEARCH_AUTH_MODE=oauth in your MCP env or config.
  OAuth mode reuses Hermes' xAI client_id and may carry account / terms risk.

One-line install (Claude Code)
  claude mcp add-json grok-search-rs --scope user '{
    "type": "stdio",
    "command": "grok-search-rs",
    "env": {
      "GROK_SEARCH_API_KEY": "xai-...",
      "TAVILY_API_KEY": "tvly-..."
    }
  }'

"#,
    );

    // Hint the global config path only when the file is genuinely missing —
    // avoids nagging users who have already set one up.
    if let Some(path) = config::config_path() {
        if !path.exists() {
            guide.push_str(&format!(
                r#"Tip: set keys once for every MCP client
  grok-search-rs --init                  # scaffold {}
  $EDITOR {}    # uncomment and fill

"#,
                path.display(),
                path.display()
            ));
        }
    }

    guide.push_str(
        r#"Docs:    https://github.com/Episkey-G/GrokSearch-rs#readme
Issues:  https://github.com/Episkey-G/GrokSearch-rs/issues
"#,
    );

    let stdout = std::io::stdout();
    let _ = stdout.lock().write_all(guide.as_bytes());
}
