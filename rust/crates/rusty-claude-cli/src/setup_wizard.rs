//! `claw setup` (and the in-REPL `/setup`) — a single guided wizard that
//! walks the user through choosing a **provider** and a **model**.
//!
//! It unifies the two historically-separate setup paths:
//!   * the fork's live provider detection / model pickers (in `setup.rs`), and
//!   * upstream's persisted provider config (`~/.claw/settings.json`).
//!
//! Behaviour:
//!   1. Detect which providers are reachable right now — Ollama and LM Studio
//!      are probed locally; OpenRouter is "available" when an API key is found
//!      in the environment or saved config.
//!   2. Scan the process environment and nearby `.env` files for existing API
//!      keys / base URLs and *suggest* them (masked), or prompt for new ones.
//!   3. Present a provider chooser annotated with that availability.
//!   4. For local / OpenRouter providers, fetch the live model list and show
//!      an interactive picker; for cloud providers, offer common aliases or a
//!      free-text model name.
//!   5. Persist the choice to `~/.claw/settings.json` (durable default) and
//!      offer to launch the REPL immediately.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;

use runtime::{save_user_provider_settings, ConfigLoader, RuntimeProviderConfig};

use crate::setup;

// ── public API ────────────────────────────────────────────────────────

/// Provider env + model to start the REPL with when the user opts to launch.
pub struct LaunchSpec {
    pub model: String,
    pub env: HashMap<String, String>,
}

/// Outcome of the wizard. The provider is always persisted; `launch` is set
/// only when the user asked to start a session immediately.
pub struct WizardOutcome {
    pub launch: Option<LaunchSpec>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Provider {
    Ollama,
    LmStudio,
    OpenRouter,
    Anthropic,
    OpenAi,
    Xai,
    DashScope,
    Custom,
}

/// Internal: the fully-resolved provider selection.
struct ProviderChoice {
    /// `provider.kind` persisted to settings.json (e.g. "openai", "anthropic").
    kind: String,
    api_key: String,
    base_url: Option<String>,
    model: Option<String>,
    /// Env vars to export if the user launches immediately.
    launch_env: HashMap<String, String>,
    display: String,
}

pub fn run_setup_wizard() -> Result<WizardOutcome, Box<dyn std::error::Error>> {
    if !io::stdin().is_terminal() {
        return Err("setup wizard requires an interactive terminal".into());
    }

    println!();
    println!("  \x1b[1mClaw Code Setup Wizard\x1b[0m");
    println!("  Detecting available providers…");

    let detected = setup::detect_all();
    let scan = EnvScan::collect();
    let current = load_current_provider_config();

    let provider = choose_provider(&detected)?;

    let choice = match provider {
        Provider::Ollama => configure_ollama(&detected.ollama)?,
        Provider::LmStudio => configure_lmstudio(&detected.lmstudio)?,
        Provider::OpenRouter => configure_openrouter(&detected, &scan)?,
        Provider::Anthropic => configure_cloud("anthropic", "Anthropic", &scan, &current)?,
        Provider::OpenAi => configure_cloud("openai", "OpenAI", &scan, &current)?,
        Provider::Xai => configure_cloud("xai", "xAI / Grok", &scan, &current)?,
        Provider::DashScope => {
            configure_cloud("dashscope", "DashScope (Qwen/Kimi)", &scan, &current)?
        }
        Provider::Custom => configure_custom(&scan)?,
    };

    save_user_provider_settings(
        &choice.kind,
        &choice.api_key,
        choice.base_url.as_deref(),
        choice.model.as_deref(),
    )?;

    // Optional smaller model used by the Agent tool for sub-tasks.
    let fast = prompt_fast_model(choice.model.as_deref())?;
    if let Some(fast) = &fast {
        save_settings_field("subagentModel", fast)?;
    }

    println!();
    println!("  \x1b[32m✓ Provider saved to ~/.claw/settings.json\x1b[0m");
    println!("  Provider: {}", choice.display);
    if let Some(model) = &choice.model {
        println!("  Model:    {model}");
    }
    println!();

    // Offer to launch immediately.
    let mut launch = None;
    if let Some(model) = choice.model.clone() {
        let input = read_line("  Launch claw now with this provider? [Y/n]: ")?;
        let answer = input.trim();
        if answer.is_empty() || answer.eq_ignore_ascii_case("y") {
            launch = Some(LaunchSpec {
                model,
                env: choice.launch_env,
            });
        } else {
            println!(
                "  Run \x1b[1mclaw\x1b[0m to start, or \x1b[1m/model {}\x1b[0m in a session.",
                choice.model.as_deref().unwrap_or("")
            );
        }
    }

    Ok(WizardOutcome { launch })
}

// ── provider chooser ──────────────────────────────────────────────────

fn provider_menu(detected: &setup::Detected) -> Vec<(Provider, String, String)> {
    let ollama_status = if detected.ollama.running {
        format!(
            "✓ running ({} models) at {}",
            detected.ollama.models.len(),
            detected.ollama.address
        )
    } else {
        format!("✗ not detected at {}", detected.ollama.address)
    };
    let lm_status = if detected.lmstudio.running {
        format!(
            "✓ running ({} models) at {}",
            detected.lmstudio.models.len(),
            detected.lmstudio.address
        )
    } else {
        "✗ not detected".to_string()
    };
    let or_status = match &detected.openrouter_key {
        Some(_) => "✓ API key found".to_string(),
        None => "⚠ needs API key".to_string(),
    };

    vec![
        (
            Provider::Ollama,
            "Ollama (local)".to_string(),
            ollama_status,
        ),
        (
            Provider::LmStudio,
            "LM Studio (local)".to_string(),
            lm_status,
        ),
        (Provider::OpenRouter, "OpenRouter".to_string(), or_status),
        (Provider::Anthropic, "Anthropic".to_string(), String::new()),
        (Provider::OpenAi, "OpenAI".to_string(), String::new()),
        (Provider::Xai, "xAI / Grok".to_string(), String::new()),
        (
            Provider::DashScope,
            "DashScope (Qwen/Kimi)".to_string(),
            String::new(),
        ),
        (
            Provider::Custom,
            "Custom (OpenAI-compatible)…".to_string(),
            String::new(),
        ),
    ]
}

fn choose_provider(detected: &setup::Detected) -> Result<Provider, Box<dyn std::error::Error>> {
    let menu = provider_menu(detected);

    println!();
    println!("  \x1b[1mProvider\x1b[0m");
    for (i, (_, label, status)) in menu.iter().enumerate() {
        if status.is_empty() {
            println!("    [{}] {label}", i + 1);
        } else {
            println!("    [{}] {label:<28} {status}", i + 1);
        }
    }

    // Default to the first reachable local provider, then OpenRouter if a key
    // is already configured, otherwise the first entry.
    let default_idx = menu
        .iter()
        .position(|(p, _, _)| match p {
            Provider::Ollama => detected.ollama.running,
            Provider::LmStudio => detected.lmstudio.running,
            _ => false,
        })
        .or_else(|| {
            if detected.openrouter_key.is_some() {
                menu.iter().position(|(p, _, _)| *p == Provider::OpenRouter)
            } else {
                None
            }
        })
        .unwrap_or(0);
    let default = default_idx + 1;

    let input = read_line(&format!("  Select provider [{default}]: "))?;
    let choice: usize = if input.trim().is_empty() {
        default
    } else {
        input
            .trim()
            .parse()
            .map_err(|_| format!("invalid provider choice: {}", input.trim()))?
    };

    choice
        .checked_sub(1)
        .and_then(|i| menu.get(i))
        .map(|(p, _, _)| *p)
        .ok_or_else(|| format!("invalid provider choice: {choice}").into())
}

// ── local providers ───────────────────────────────────────────────────

fn configure_ollama(
    status: &setup::LocalProviderStatus,
) -> Result<ProviderChoice, Box<dyn std::error::Error>> {
    let base = setup::ollama_base_url();
    let base_v1 = format!("{base}/v1");

    let model = if status.running && !status.models.is_empty() {
        match setup::pick_model(&status.models, "Ollama — select a model") {
            Some(m) => m,
            None => return Err("No model selected".into()),
        }
    } else {
        println!("  Ollama not reachable at {base}.");
        println!("  Start it with `ollama serve` and pull a model (e.g. `ollama pull qwen3:14b`).");
        let input = read_line("  Model name to configure anyway (Enter to cancel): ")?;
        let model = input.trim().to_string();
        if model.is_empty() {
            return Err("No model selected".into());
        }
        model
    };

    Ok(ProviderChoice {
        kind: "openai".to_string(),
        api_key: "ollama".to_string(),
        base_url: Some(base_v1.clone()),
        model: Some(model),
        launch_env: local_launch_env(&base_v1, "ollama"),
        display: format!("Ollama ({base})"),
    })
}

fn configure_lmstudio(
    status: &setup::LocalProviderStatus,
) -> Result<ProviderChoice, Box<dyn std::error::Error>> {
    let address = if status.running {
        status.address.clone()
    } else {
        println!("  LM Studio not detected.");
        println!("  Start its local server (Developer → Start Server; default 127.0.0.1:1234).");
        let input = read_line("  LM Studio host:port [127.0.0.1:1234]: ")?;
        let trimmed = input.trim();
        if trimmed.is_empty() {
            "127.0.0.1:1234".to_string()
        } else {
            trimmed.to_string()
        }
    };
    let base_v1 = format!("http://{address}/v1");

    let model = if status.running && !status.models.is_empty() {
        match setup::pick_model(&status.models, "LM Studio — select a model") {
            Some(m) => m,
            None => return Err("No model selected".into()),
        }
    } else {
        let input = read_line("  Model name to configure anyway (Enter to cancel): ")?;
        let model = input.trim().to_string();
        if model.is_empty() {
            return Err("No model selected".into());
        }
        model
    };

    Ok(ProviderChoice {
        kind: "openai".to_string(),
        api_key: "local-model".to_string(),
        base_url: Some(base_v1.clone()),
        model: Some(model),
        launch_env: local_launch_env(&base_v1, "local-model"),
        display: format!("LM Studio ({address})"),
    })
}

fn local_launch_env(base_v1: &str, key: &str) -> HashMap<String, String> {
    let mut env = HashMap::new();
    env.insert("OPENAI_BASE_URL".to_string(), base_v1.to_string());
    env.insert("OPENAI_API_KEY".to_string(), key.to_string());
    // Local servers benefit from the fork's self-healing resilience layer.
    env.insert("CLAW_RESILIENCE".to_string(), "force".to_string());
    env
}

// ── OpenRouter ────────────────────────────────────────────────────────

fn configure_openrouter(
    detected: &setup::Detected,
    scan: &EnvScan,
) -> Result<ProviderChoice, Box<dyn std::error::Error>> {
    let already_saved = detected.openrouter_key.clone();

    let key = if let Some(existing) = &already_saved {
        println!(
            "  Using existing OpenRouter API key {} (environment or saved config).",
            mask(existing)
        );
        existing.clone()
    } else if let Some((value, source)) = scan.get("OPENROUTER_API_KEY") {
        println!("  Found OPENROUTER_API_KEY in {source}.");
        let input = read_line(&format!(
            "  Use this key {}? [Y/n] (or paste a different key): ",
            mask(value)
        ))?;
        let answer = input.trim();
        if answer.is_empty() || answer.eq_ignore_ascii_case("y") {
            value.clone()
        } else if answer.eq_ignore_ascii_case("n") {
            prompt_new_openrouter_key()?
        } else {
            answer.to_string()
        }
    } else {
        prompt_new_openrouter_key()?
    };

    // Offer to persist a freshly-entered key for next time.
    if already_saved.as_deref() != Some(key.as_str()) {
        let input = read_line("  Save this key to ~/.config/openroutercode/.env? [Y/n]: ")?;
        let answer = input.trim();
        if answer.is_empty() || answer.eq_ignore_ascii_case("y") {
            if let Err(e) = setup::save_openrouter_api_key(&key) {
                eprintln!("  Warning: could not save key: {e}");
            }
        }
    }

    println!("  Fetching tool-capable OpenRouter models…");
    let models = setup::list_openrouter_models(&key)?;
    if models.is_empty() {
        return Err("No tool-capable models found on OpenRouter.".into());
    }
    let model = match setup::pick_model(&models, "OpenRouter — select a model") {
        Some(m) => m,
        None => return Err("No model selected".into()),
    };

    let mut env = HashMap::new();
    env.insert(
        "OPENAI_BASE_URL".to_string(),
        "https://openrouter.ai/api/v1".to_string(),
    );
    env.insert("OPENAI_API_KEY".to_string(), key.clone());
    env.insert("HTTP_REFERER".to_string(), "https://localhost".to_string());
    env.insert("X_TITLE".to_string(), "claw-code".to_string());
    env.insert("CLAW_RESILIENCE".to_string(), "none".to_string());

    Ok(ProviderChoice {
        kind: "openai".to_string(),
        api_key: key,
        base_url: Some("https://openrouter.ai/api/v1".to_string()),
        model: Some(model),
        launch_env: env,
        display: "OpenRouter".to_string(),
    })
}

fn prompt_new_openrouter_key() -> Result<String, Box<dyn std::error::Error>> {
    println!("  Create a key at https://openrouter.ai/keys");
    loop {
        let input = read_line("  Paste your OpenRouter API key: ")?;
        let key = input.trim().to_string();
        if !key.is_empty() {
            return Ok(key);
        }
        println!("  Key cannot be empty.");
    }
}

// ── cloud providers (Anthropic / OpenAI / xAI / DashScope) ────────────

fn configure_cloud(
    kind: &str,
    display: &str,
    scan: &EnvScan,
    current: &RuntimeProviderConfig,
) -> Result<ProviderChoice, Box<dyn std::error::Error>> {
    let current_key = if current.kind() == Some(kind) {
        current.api_key()
    } else {
        None
    };
    let current_base = if current.kind() == Some(kind) {
        current.base_url()
    } else {
        None
    };
    let current_model = if current.kind() == Some(kind) {
        current.model()
    } else {
        None
    };

    let api_key = prompt_key_with_suggestion(api_key_var(kind), scan, current_key)?;
    let base_url = prompt_base_with_suggestion(
        base_url_var(kind),
        default_base_url(kind),
        scan,
        current_base,
    )?;
    let model = prompt_model_for(kind, current_model)?;

    let mut env = HashMap::new();
    if kind == "anthropic" {
        env.insert("ANTHROPIC_API_KEY".to_string(), api_key.clone());
        if let Some(base) = &base_url {
            env.insert("ANTHROPIC_BASE_URL".to_string(), base.clone());
        }
    } else {
        env.insert("OPENAI_API_KEY".to_string(), api_key.clone());
        env.insert(
            "OPENAI_BASE_URL".to_string(),
            base_url
                .clone()
                .unwrap_or_else(|| default_base_url(kind).to_string()),
        );
    }
    env.insert("CLAW_RESILIENCE".to_string(), "none".to_string());

    Ok(ProviderChoice {
        kind: kind.to_string(),
        api_key,
        base_url,
        model,
        launch_env: env,
        display: display.to_string(),
    })
}

fn configure_custom(scan: &EnvScan) -> Result<ProviderChoice, Box<dyn std::error::Error>> {
    println!("  \x1b[1mCustom OpenAI-compatible provider\x1b[0m");
    let name_input = read_line("  Display name [custom]: ")?;
    let display = {
        let trimmed = name_input.trim();
        if trimmed.is_empty() {
            "custom".to_string()
        } else {
            trimmed.to_string()
        }
    };

    let suggestion = scan.get("OPENAI_BASE_URL").map(|(v, _)| v.clone());
    if let Some((_, source)) = scan.get("OPENAI_BASE_URL") {
        println!("  Found OPENAI_BASE_URL in {source}.");
    }
    let base_url = loop {
        let prompt = match &suggestion {
            Some(s) => format!("  Base URL [{s}]: "),
            None => "  Base URL (e.g. http://host:port/v1): ".to_string(),
        };
        let input = read_line(&prompt)?;
        let trimmed = input.trim();
        if !trimmed.is_empty() {
            break trimmed.to_string();
        }
        if let Some(s) = &suggestion {
            break s.clone();
        }
        println!("  Base URL is required.");
    };

    let api_key = prompt_key_with_suggestion("OPENAI_API_KEY", scan, None)?;
    let key = if api_key.is_empty() {
        "local-model".to_string()
    } else {
        api_key
    };

    let model_input = read_line("  Model name: ")?;
    let model = model_input.trim().to_string();
    if model.is_empty() {
        return Err("Model name is required".into());
    }

    let mut env = HashMap::new();
    env.insert("OPENAI_BASE_URL".to_string(), base_url.clone());
    env.insert("OPENAI_API_KEY".to_string(), key.clone());
    // Unknown endpoint: let the resilience layer auto-detect (localhost → on).
    env.insert("CLAW_RESILIENCE".to_string(), "auto".to_string());

    Ok(ProviderChoice {
        kind: "openai".to_string(),
        api_key: key,
        base_url: Some(base_url),
        model: Some(model),
        launch_env: env,
        display,
    })
}

// ── shared prompt helpers ─────────────────────────────────────────────

fn prompt_key_with_suggestion(
    var: &str,
    scan: &EnvScan,
    current: Option<&str>,
) -> Result<String, Box<dyn std::error::Error>> {
    if let Some((value, source)) = scan.get(var) {
        let input = read_line(&format!(
            "  {var} found in {source} {}. Use it? [Y/n] (or paste new): ",
            mask(value)
        ))?;
        let answer = input.trim();
        if answer.is_empty() || answer.eq_ignore_ascii_case("y") {
            return Ok(value.clone());
        }
        if !answer.eq_ignore_ascii_case("n") {
            return Ok(answer.to_string());
        }
    } else if let Some(stored) = current.filter(|k| !k.is_empty()) {
        let input = read_line(&format!(
            "  Stored {var} {}. Use it? [Y/n] (or paste new): ",
            mask(stored)
        ))?;
        let answer = input.trim();
        if answer.is_empty() || answer.eq_ignore_ascii_case("y") {
            return Ok(stored.to_string());
        }
        if !answer.eq_ignore_ascii_case("n") {
            return Ok(answer.to_string());
        }
    }

    let input = read_line(&format!("  Enter {var} (blank to skip): "))?;
    Ok(input.trim().to_string())
}

fn prompt_base_with_suggestion(
    var: &str,
    default_base: &str,
    scan: &EnvScan,
    current: Option<&str>,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let suggestion = scan
        .get(var)
        .map(|(v, _)| v.clone())
        .or_else(|| current.filter(|s| !s.is_empty()).map(|s| s.to_string()))
        .unwrap_or_else(|| default_base.to_string());

    if let Some((_, source)) = scan.get(var) {
        println!("  {var} found in {source}.");
    }

    let input = read_line(&format!("  Base URL [{suggestion}]: "))?;
    let chosen = if input.trim().is_empty() {
        suggestion
    } else {
        input.trim().to_string()
    };

    // Persist nothing when it matches the provider default — keeps the stored
    // config minimal and lets upstream defaults move underneath us.
    if chosen.is_empty() || chosen == default_base {
        Ok(None)
    } else {
        Ok(Some(chosen))
    }
}

fn prompt_model_for(
    kind: &str,
    current: Option<&str>,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let aliases = provider_model_aliases(kind);
    let default = current
        .filter(|m| !m.is_empty())
        .map(|s| s.to_string())
        .or_else(|| aliases.first().map(|s| s.to_string()))
        .unwrap_or_default();

    println!("  \x1b[1mModel\x1b[0m");
    if !aliases.is_empty() {
        println!("    Common: {}", aliases.join(", "));
    }
    println!("    Or enter any model name (e.g. openai/gpt-4.1-mini for custom routing).");

    let hint = if default.is_empty() { "none" } else { &default };
    let input = read_line(&format!("  Model [{hint}]: "))?;
    let chosen = if input.trim().is_empty() {
        default
    } else {
        input.trim().to_string()
    };

    if chosen.is_empty() {
        Ok(None)
    } else {
        Ok(Some(chosen))
    }
}

fn prompt_fast_model(
    main_model: Option<&str>,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    println!();
    println!("  \x1b[1mFast model for Agent sub-tasks\x1b[0m (optional)");
    println!("    A smaller/cheaper model the Agent tool uses for Explore/Plan sub-agents.");

    let current_fast = load_current_settings_field("subagentModel");
    let hint = current_fast
        .as_deref()
        .or(main_model)
        .unwrap_or("same as main");

    let input = read_line(&format!("  Fast model [{hint}] (Enter to skip): "))?;
    if input.trim().is_empty() {
        Ok(current_fast)
    } else {
        Ok(Some(input.trim().to_string()))
    }
}

// ── provider metadata ─────────────────────────────────────────────────

fn api_key_var(kind: &str) -> &'static str {
    match kind {
        "anthropic" => "ANTHROPIC_API_KEY",
        "xai" => "XAI_API_KEY",
        "dashscope" => "DASHSCOPE_API_KEY",
        _ => "OPENAI_API_KEY",
    }
}

fn base_url_var(kind: &str) -> &'static str {
    match kind {
        "anthropic" => "ANTHROPIC_BASE_URL",
        "xai" => "XAI_BASE_URL",
        "dashscope" => "DASHSCOPE_BASE_URL",
        _ => "OPENAI_BASE_URL",
    }
}

fn default_base_url(kind: &str) -> &'static str {
    match kind {
        "anthropic" => "https://api.anthropic.com",
        "xai" => "https://api.x.ai/v1",
        "openai" => "https://api.openai.com/v1",
        "dashscope" => "https://dashscope.aliyuncs.com/compatible-mode/v1",
        _ => "",
    }
}

fn provider_model_aliases(kind: &str) -> Vec<&'static str> {
    match kind {
        "anthropic" => vec!["opus", "sonnet", "haiku"],
        "xai" => vec!["grok", "grok-mini", "grok-2"],
        "openai" => vec!["gpt-4.1", "gpt-4.1-mini", "gpt-4.1-nano"],
        "dashscope" => vec!["qwen-plus", "qwen-max", "kimi"],
        _ => vec![],
    }
}

// ── environment / .env discovery ──────────────────────────────────────

/// Provider keys and hosts discovered in the process environment and nearby
/// `.env` files, used to *suggest* values during the wizard.
struct EnvScan {
    /// VAR -> (value, human-readable source label).
    vars: HashMap<String, (String, String)>,
}

impl EnvScan {
    fn collect() -> Self {
        let mut vars: HashMap<String, (String, String)> = HashMap::new();

        // `.env` files, lowest priority first; process env overrides below.
        let mut files: Vec<PathBuf> = Vec::new();
        if let Ok(cwd) = env::current_dir() {
            files.push(cwd.join(".env"));
        }
        if let Ok(home) = env::var("HOME") {
            let home = PathBuf::from(home);
            files.push(home.join(".env"));
            files.push(home.join(".config/openroutercode/.env"));
            files.push(home.join(".config/opencode/.env"));
        }

        for path in files {
            let Ok(content) = fs::read_to_string(&path) else {
                continue;
            };
            let label = format!(".env ({})", path.display());
            for (key, value) in parse_env_file(&content) {
                vars.entry(key).or_insert((value, label.clone()));
            }
        }

        // Process environment takes precedence over any file.
        for (key, value) in env::vars() {
            if is_interesting(&key) && !value.is_empty() {
                vars.insert(key, (value, "environment".to_string()));
            }
        }

        Self { vars }
    }

    fn get(&self, key: &str) -> Option<&(String, String)> {
        self.vars.get(key)
    }
}

fn is_interesting(key: &str) -> bool {
    key.ends_with("_API_KEY")
        || key.ends_with("_BASE_URL")
        || key.ends_with("_HOST")
        || key == "OLLAMA_PORT"
        || key == "LM_STUDIO_PORT"
}

fn parse_env_file(content: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().to_string();
        let mut value = value.trim();
        if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            value = &value[1..value.len() - 1];
        }
        if is_interesting(&key) && !value.is_empty() {
            out.push((key, value.to_string()));
        }
    }
    out
}

fn mask(key: &str) -> String {
    if key.len() > 4 {
        format!("****{}", &key[key.len() - 4..])
    } else {
        "****".to_string()
    }
}

// ── settings.json helpers ─────────────────────────────────────────────

fn load_current_provider_config() -> RuntimeProviderConfig {
    let cwd = std::env::current_dir().unwrap_or_default();
    ConfigLoader::default_for(&cwd)
        .load()
        .map(|c| c.provider().clone())
        .unwrap_or_default()
}

fn load_current_settings_field(field: &str) -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let settings_path = std::path::Path::new(&home).join(".claw/settings.json");
    let content = std::fs::read_to_string(&settings_path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    json.get(field)?.as_str().map(|s| s.to_string())
}

fn save_settings_field(field: &str, value: &str) -> Result<(), Box<dyn std::error::Error>> {
    let home = std::env::var("HOME")?;
    let settings_dir = std::path::Path::new(&home).join(".claw");
    let settings_path = settings_dir.join("settings.json");

    let mut settings: serde_json::Value = if settings_path.exists() {
        let content = std::fs::read_to_string(&settings_path)?;
        serde_json::from_str(&content)?
    } else {
        serde_json::json!({})
    };

    if let Some(obj) = settings.as_object_mut() {
        obj.insert(
            field.to_string(),
            serde_json::Value::String(value.to_string()),
        );
    }

    std::fs::create_dir_all(&settings_dir)?;
    std::fs::write(&settings_path, serde_json::to_string_pretty(&settings)?)?;
    Ok(())
}

fn read_line(prompt: &str) -> Result<String, Box<dyn std::error::Error>> {
    let mut stdout = io::stdout();
    write!(stdout, "{prompt}")?;
    stdout.flush()?;
    let mut buffer = String::new();
    io::stdin().read_line(&mut buffer)?;
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_env_file_extracts_interesting_keys() {
        let content = "\
# a comment\n\
export OPENROUTER_API_KEY=\"sk-or-123\"\n\
OPENAI_BASE_URL='http://localhost:1234/v1'\n\
RANDOM_THING=ignored\n\
ANTHROPIC_API_KEY=sk-ant-456\n";
        let parsed: HashMap<String, String> = parse_env_file(content).into_iter().collect();
        assert_eq!(parsed.get("OPENROUTER_API_KEY").unwrap(), "sk-or-123");
        assert_eq!(
            parsed.get("OPENAI_BASE_URL").unwrap(),
            "http://localhost:1234/v1"
        );
        assert_eq!(parsed.get("ANTHROPIC_API_KEY").unwrap(), "sk-ant-456");
        assert!(!parsed.contains_key("RANDOM_THING"));
    }

    #[test]
    fn is_interesting_matches_keys_and_hosts() {
        assert!(is_interesting("OPENAI_API_KEY"));
        assert!(is_interesting("ANTHROPIC_BASE_URL"));
        assert!(is_interesting("OLLAMA_HOST"));
        assert!(is_interesting("OLLAMA_PORT"));
        assert!(is_interesting("LM_STUDIO_PORT"));
        assert!(!is_interesting("PATH"));
        assert!(!is_interesting("HOME"));
    }

    #[test]
    fn mask_hides_all_but_last_four() {
        assert_eq!(mask("sk-or-1234567890"), "****7890");
        assert_eq!(mask("abc"), "****");
    }

    #[test]
    fn metadata_tables_are_consistent() {
        assert_eq!(api_key_var("anthropic"), "ANTHROPIC_API_KEY");
        assert_eq!(api_key_var("openai"), "OPENAI_API_KEY");
        assert_eq!(api_key_var("custom"), "OPENAI_API_KEY");
        assert_eq!(base_url_var("dashscope"), "DASHSCOPE_BASE_URL");
        assert_eq!(default_base_url("openai"), "https://api.openai.com/v1");
        assert!(provider_model_aliases("anthropic").contains(&"opus"));
        assert!(provider_model_aliases("custom").is_empty());
    }
}
