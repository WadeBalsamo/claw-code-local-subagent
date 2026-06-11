/// `claw setup` — interactive setup for local and remote model providers.
///
/// Model selectors use crossterm-based arrow-key TUI when available
/// (standard terminal), falling back to numbered list + readline.
///
/// ## Unified model browser (`claw setup models`)
///
/// Scans all local providers (Ollama, LM Studio) plus recently-used
/// API models and presents a single interactive picker.
///
/// ## Per-provider (`claw setup ollama`, `claw setup lmstudio`)
///
/// Auto-discovers the provider, fetches available models, presents a
/// TUI selector, and launches the REPL with correct env vars.
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    style::{self, Color, Print, ResetColor, SetForegroundColor, Stylize},
    terminal::{self, Clear, ClearType},
};

// ── public API ────────────────────────────────────────────────────────

/// The resolved result of running one of the `claw setup` subcommands.
///
/// `main.rs` should apply the env var map and then launch the REPL with
/// the resolved model name, unless `SetupAction` is something other than
/// `LaunchRepl` (e.g. `ListModelsOnly`, `SetKey`, `PrintVersion`).
#[derive(Debug)]
pub struct SetupResult {
    pub action: SetupAction,
    /// Environment variables to set before launching the REPL.
    pub env: HashMap<String, String>,
    /// Optional — present when action is `LaunchRepl`.
    pub model: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SetupAction {
    LaunchRepl,
    ListModelsOnly,
    SetKey,
    PrintVersion,
}

// ── SetupTarget ───────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupTarget {
    /// `claw setup ollama [model]`
    Ollama,
    /// `claw setup lmstudio [model]`
    LmStudio,
    /// `claw setup openrouter [model] [--set-key KEY] [--list-models]`
    OpenRouter {
        set_key: Option<String>,
        list_models: bool,
    },
    /// `claw setup models` — unified browser across all local providers
    Models,
}

// ── entry point ───────────────────────────────────────────────────────

pub fn handle_setup(
    target: SetupTarget,
    model_hint: Option<String>,
) -> Result<SetupResult, Box<dyn std::error::Error>> {
    match target {
        SetupTarget::Ollama => setup_ollama(model_hint),
        SetupTarget::LmStudio => setup_lmstudio(model_hint),
        SetupTarget::OpenRouter {
            set_key,
            list_models,
        } => {
            if let Some(key) = set_key {
                save_openrouter_api_key(&key)?;
                return Ok(SetupResult {
                    action: SetupAction::SetKey,
                    model: None,
                    env: HashMap::new(),
                });
            }
            setup_openrouter(model_hint, list_models)
        }
        SetupTarget::Models => unified_model_browser(),
    }
}

// ── environment helpers ───────────────────────────────────────────────

fn default_home() -> PathBuf {
    env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

fn set_env(map: &mut HashMap<String, String>, key: &str, value: impl Into<String>) {
    map.insert(key.to_string(), value.into());
}

// ── TUI model selector (crossterm arrow-key) ──────────────────────────
//
// Presents an interactive multi-column list with arrow keys.
// Falls back to numbered stdin prompt if terminal is not interactive.

pub struct ModelEntry {
    pub id: String,
    pub provider: String,
    pub context: Option<u64>,
    pub note: String,
}

struct TuiPicker;

impl TuiPicker {
    /// Show an interactive arrow-key model selector. Returns the selected
    /// model id, or `None` if the user cancelled (Esc / Ctrl+C / q).
    fn pick(models: &[ModelEntry], title: &str) -> Option<String> {
        if models.is_empty() {
            return None;
        }
        if !atty::is(atty::Stream::Stdin) {
            // Fallback: auto-select first in non-interactive mode
            eprintln!("{} — auto-selecting first model: {}", title, models[0].id);
            return Some(models[0].id.clone());
        }
        Self::pick_interactive(models, title)
    }

    fn pick_interactive(models: &[ModelEntry], title: &str) -> Option<String> {
        // Try crossterm interactive picker, fall back to stdin prompt
        if terminal::is_raw_mode_enabled().unwrap_or(false) {
            return Self::pick_stdin(models, title);
        }
        match Self::pick_crossterm(models, title) {
            Some(id) => Some(id),
            None => Self::pick_stdin(models, title),
        }
    }

    fn pick_crossterm(models: &[ModelEntry], title: &str) -> Option<String> {
        let _guard = match RawModeGuard::enter() {
            Some(g) => g,
            None => return None,
        };

        let mut selected = 0usize;
        let mut scroll_offset = 0usize;
        let max_visible = (terminal::size().ok()?.1 as usize).saturating_sub(6).max(5);

        loop {
            Self::draw_models(models, title, selected, scroll_offset, max_visible);

            match Self::read_key() {
                TuiKey::Up => {
                    if selected > 0 {
                        selected -= 1;
                        if selected < scroll_offset {
                            scroll_offset = scroll_offset.saturating_sub(1);
                        }
                    }
                }
                TuiKey::Down => {
                    if selected + 1 < models.len() {
                        selected += 1;
                        if selected >= scroll_offset + max_visible {
                            scroll_offset += 1;
                        }
                    }
                }
                TuiKey::PageUp => {
                    selected = scroll_offset.saturating_sub(max_visible);
                    scroll_offset = scroll_offset.saturating_sub(max_visible);
                }
                TuiKey::PageDown => {
                    let new_scroll =
                        (scroll_offset + max_visible).min(models.len().saturating_sub(1));
                    scroll_offset = new_scroll;
                    selected = new_scroll.min(selected + max_visible);
                }
                TuiKey::Home => {
                    selected = 0;
                    scroll_offset = 0;
                }
                TuiKey::End => {
                    selected = models.len() - 1;
                    scroll_offset = models.len().saturating_sub(max_visible);
                }
                TuiKey::Enter => {
                    // Clear the picker output
                    let _ = execute!(io::stdout(), terminal::Clear(ClearType::FromCursorDown));
                    let _ = execute!(io::stdout(), cursor::MoveToPreviousLine(1));
                    return Some(models[selected].id.clone());
                }
                TuiKey::Esc | TuiKey::Quit => {
                    let _ = execute!(io::stdout(), terminal::Clear(ClearType::FromCursorDown));
                    return None;
                }
                _ => {}
            }
        }
    }

    fn draw_models(
        models: &[ModelEntry],
        title: &str,
        selected: usize,
        scroll: usize,
        max_visible: usize,
    ) {
        let mut out = io::stdout();
        let _ = execute!(out, cursor::MoveTo(0, 0), Clear(ClearType::All));

        // Title bar
        let _ = execute!(
            out,
            SetForegroundColor(Color::Cyan),
            style::Print(format!("╔══ {} ══╗\n", title)),
            ResetColor
        );

        // Column headers
        let _ = execute!(
            out,
            SetForegroundColor(Color::DarkGrey),
            style::Print(format!(
                " {:>3}  {:<45} {:<18} {:>8}  {}",
                "#", "Model", "Provider", "Ctx", "Notes"
            )),
            ResetColor,
            style::Print("\n")
        );
        let _ = execute!(
            out,
            SetForegroundColor(Color::DarkGrey),
            style::Print(format!(
                " {:-<3}  {:-<45} {:-<18} {:-<8}  {:-<20}\n",
                "", "", "", "", ""
            )),
            ResetColor
        );

        let end = (scroll + max_visible).min(models.len());
        for i in scroll..end {
            let m = &models[i];
            let ctx_str = m
                .context
                .map(|c| format_ctx(c))
                .unwrap_or_else(|| "?".to_string());
            let line = format!(
                " {:>3}  {:<45} {:<18} {:>8}  {}",
                i + 1,
                m.id,
                m.provider,
                ctx_str,
                m.note,
            );

            if i == selected {
                let _ = execute!(
                    out,
                    style::Print(" "),
                    style::SetAttribute(style::Attribute::Reverse),
                    style::Print(line),
                    style::SetAttribute(style::Attribute::NoReverse),
                    style::Print("\n"),
                );
            } else {
                let _ = execute!(
                    out,
                    style::Print(" "),
                    style::Print(line),
                    style::Print("\n"),
                );
            }
        }

        // Bottom bar
        let _ = execute!(
            out,
            SetForegroundColor(Color::DarkGrey),
            style::Print(format!(
                " {}/{} — ↑↓ pgup/pgdn home/end enter q\n",
                selected + 1,
                models.len()
            )),
            ResetColor
        );
        let _ = out.flush();
    }

    fn read_key() -> TuiKey {
        loop {
            match event::read() {
                Ok(Event::Key(k)) => match k.code {
                    KeyCode::Up | KeyCode::Char('k') => return TuiKey::Up,
                    KeyCode::Down | KeyCode::Char('j') => return TuiKey::Down,
                    KeyCode::PageUp => return TuiKey::PageUp,
                    KeyCode::PageDown => return TuiKey::PageDown,
                    KeyCode::Home => return TuiKey::Home,
                    KeyCode::End => return TuiKey::End,
                    KeyCode::Enter => return TuiKey::Enter,
                    KeyCode::Esc => return TuiKey::Esc,
                    KeyCode::Char('q' | 'Q') => return TuiKey::Quit,
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        return TuiKey::Quit;
                    }
                    _ => {}
                },
                Ok(Event::Resize(_, _)) => {
                    // Re-draw on resize is handled by next loop iteration
                    return TuiKey::Resize;
                }
                _ => {}
            }
        }
    }

    fn pick_stdin(models: &[ModelEntry], title: &str) -> Option<String> {
        eprintln!("\n── {} ──", title);
        for (i, m) in models.iter().enumerate() {
            let ctx_str = m.context.map(format_ctx).unwrap_or_else(|| "?".to_string());
            eprintln!(
                "  {:>3}. {:<50}  {}  ctx={}",
                i + 1,
                m.id,
                m.provider,
                ctx_str
            );
        }
        eprint!("Select model (1-{}, q=quit): ", models.len());
        let _ = io::stderr().flush();

        let mut input = String::new();
        if io::stdin().read_line(&mut input).is_err() {
            return None;
        }
        let input = input.trim();
        if input.eq_ignore_ascii_case("q") || input.eq_ignore_ascii_case("quit") {
            return None;
        }
        match input.parse::<usize>() {
            Ok(n) if n >= 1 && n <= models.len() => Some(models[n - 1].id.clone()),
            _ => None,
        }
    }
}

enum TuiKey {
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    Enter,
    Esc,
    Quit,
    Resize,
}

struct RawModeGuard;

impl RawModeGuard {
    fn enter() -> Option<Self> {
        terminal::enable_raw_mode().ok()?;
        Some(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(io::stdout(), cursor::Show);
        let _ = io::stdout().flush();
    }
}

fn format_ctx(ctx: u64) -> String {
    if ctx >= 1_000_000 {
        format!("{}M", ctx / 1_000_000)
    } else if ctx >= 1_000 {
        format!("{}K", ctx / 1_000)
    } else {
        ctx.to_string()
    }
}

// ── Ollama ────────────────────────────────────────────────────────────

const OLLAMA_DEFAULT_HOST: &str = "127.0.0.1";
const OLLAMA_DEFAULT_PORT: u16 = 11434;

fn ollama_base_url() -> String {
    let host = env::var("OLLAMA_HOST").unwrap_or_else(|_| OLLAMA_DEFAULT_HOST.to_string());
    let port: u16 = env::var("OLLAMA_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(OLLAMA_DEFAULT_PORT);
    format!("http://{host}:{port}")
}

async fn probe_ollama() -> bool {
    let base = ollama_base_url();
    let url = format!("{base}/v1/models");
    reqwest::Client::new()
        .get(&url)
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

async fn fetch_ollama_models() -> Result<Vec<ModelEntry>, String> {
    let base = ollama_base_url();
    let url = format!("{base}/v1/models");
    let resp = reqwest::Client::new()
        .get(&url)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("failed to fetch Ollama models: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("Ollama returned HTTP {}", resp.status()));
    }

    let data: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("failed to parse model list: {e}"))?;

    let models: Vec<ModelEntry> = data["data"]
        .as_array()
        .ok_or_else(|| "unexpected response format".to_string())?
        .iter()
        .filter_map(|item| {
            let id = item["id"].as_str()?.to_string();
            let ctx = item["details"]["context_length"].as_u64();
            // Ollama marks GGUF models with the parameter size in "details"
            let note = item["details"]["parameter_size"]
                .as_str()
                .map(|s| s.to_string())
                .unwrap_or_default();
            Some(ModelEntry {
                id,
                provider: "Ollama".to_string(),
                context: ctx,
                note,
            })
        })
        .collect();

    if models.is_empty() {
        return Err("No models found in Ollama — run `ollama pull <model>`".to_string());
    }
    Ok(models)
}

fn setup_ollama(model_hint: Option<String>) -> Result<SetupResult, Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Runtime::new()?;

    if !runtime.block_on(probe_ollama()) {
        eprintln!("Ollama server not reachable at {}.", ollama_base_url());
        eprintln!("Make sure Ollama is installed and running.");
        eprintln!("  ollama serve   # start the server");
        eprintln!("  ollama pull qwen3:14b   # pull a model");
        return Err("Ollama server not reachable".into());
    }

    let model = match model_hint {
        Some(m) => m,
        None => {
            let models = runtime.block_on(fetch_ollama_models())?;
            match TuiPicker::pick(&models, "Ollama — select a model") {
                Some(id) => id,
                None => return Err("No model selected".into()),
            }
        }
    };

    let mut env_map = HashMap::new();
    set_env(
        &mut env_map,
        "OPENAI_BASE_URL",
        format!("{}/v1", ollama_base_url()),
    );
    set_env(
        &mut env_map,
        "OPENAI_API_KEY",
        env::var("OPENAI_API_KEY").unwrap_or_else(|_| "ollama".to_string()),
    );
    set_env(&mut env_map, "CLAW_RESILIENCE", "force".to_string());

    eprintln!("\n  ✓ Launching with model: {model}");
    eprintln!("  Provider: Ollama at {}", ollama_base_url());
    eprintln!("  Resilience: enabled (force)\n");

    Ok(SetupResult {
        action: SetupAction::LaunchRepl,
        model: Some(model),
        env: env_map,
    })
}

// ── LM Studio ─────────────────────────────────────────────────────────

const LMSTUDIO_DEFAULT_HOST: &str = "127.0.0.1";
const LMSTUDIO_DEFAULT_PORT: u16 = 1234;
const LMSTUDIO_RECENT_FILE: &str = ".lmstudio_recent_ips";

fn lmstudio_recent_path() -> PathBuf {
    default_home().join(LMSTUDIO_RECENT_FILE)
}

fn load_lmstudio_recent() -> Vec<String> {
    let path = lmstudio_recent_path();
    if !path.exists() {
        return Vec::new();
    }
    fs::read_to_string(&path)
        .ok()
        .map(|c| {
            c.lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn save_lmstudio_recent(addr: &str) {
    let path = lmstudio_recent_path();
    let mut addrs: Vec<String> = path
        .exists()
        .then(|| {
            fs::read_to_string(&path)
                .ok()
                .map(|c| {
                    c.lines()
                        .map(|l| l.trim().to_string())
                        .filter(|l| !l.is_empty())
                        .collect()
                })
                .unwrap_or_default()
        })
        .unwrap_or_default();
    addrs.retain(|a| a != addr);
    addrs.insert(0, addr.to_string());
    addrs.truncate(10);
    let _ = fs::write(&path, addrs.join("\n"));
}

async fn probe_lmstudio(host_port: &str) -> bool {
    let url = format!("http://{host_port}/v1/models");
    reqwest::Client::new()
        .get(&url)
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

async fn discover_lmstudio_address() -> Result<String, String> {
    let configured_host =
        env::var("LM_STUDIO_HOST").unwrap_or_else(|_| LMSTUDIO_DEFAULT_HOST.to_string());
    let configured_port: u16 = env::var("LM_STUDIO_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(LMSTUDIO_DEFAULT_PORT);

    let configured_addr = format!("{configured_host}:{configured_port}");
    if probe_lmstudio(&configured_addr).await {
        save_lmstudio_recent(&configured_addr);
        return Ok(configured_addr);
    }

    for addr in load_lmstudio_recent() {
        if probe_lmstudio(&addr).await {
            save_lmstudio_recent(&addr);
            return Ok(addr);
        }
    }

    for candidate in &["127.0.0.1:1234", "localhost:1234"] {
        if probe_lmstudio(candidate).await {
            save_lmstudio_recent(candidate);
            return Ok(candidate.to_string());
        }
    }

    Err(format!(
        "Could not find LM Studio server at {configured_addr} or any default address.\n\
         Is LM Studio running with the local HTTP server enabled?\n\
         You can customise the address via LM_STUDIO_HOST (default 127.0.0.1) and\n\
         LM_STUDIO_PORT (default 1234) environment variables."
    ))
}

async fn fetch_lmstudio_models(host_port: &str) -> Result<Vec<ModelEntry>, String> {
    let url = format!("http://{host_port}/v1/models");
    let resp = reqwest::Client::new()
        .get(&url)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("failed to fetch model list: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!(
            "LM Studio model list returned HTTP {}",
            resp.status()
        ));
    }

    let data: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("failed to parse model list: {e}"))?;

    let models: Vec<ModelEntry> = data["data"]
        .as_array()
        .ok_or_else(|| "unexpected response format: 'data' field is not an array".to_string())?
        .iter()
        .filter_map(|item| {
            let id = item["id"].as_str()?.to_string();
            Some(ModelEntry {
                id,
                provider: "LM Studio".to_string(),
                context: None,
                note: String::new(),
            })
        })
        .collect();

    if models.is_empty() {
        return Err("No models found — have you loaded a model in LM Studio?".to_string());
    }
    Ok(models)
}

fn setup_lmstudio(model_hint: Option<String>) -> Result<SetupResult, Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Runtime::new()?;
    let address = runtime.block_on(discover_lmstudio_address())?;

    let model = match model_hint {
        Some(m) => m,
        None => {
            let models = runtime.block_on(fetch_lmstudio_models(&address))?;
            match TuiPicker::pick(&models, "LM Studio — select a model") {
                Some(id) => id,
                None => return Err("No model selected".into()),
            }
        }
    };

    let (host, port) = address.split_once(':').unwrap_or((&address, "1234"));

    let mut env_map = HashMap::new();
    set_env(
        &mut env_map,
        "OPENAI_BASE_URL",
        format!("http://{host}:{port}/v1"),
    );
    set_env(
        &mut env_map,
        "OPENAI_API_KEY",
        env::var("OPENAI_API_KEY").unwrap_or_else(|_| "local-model".to_string()),
    );
    set_env(&mut env_map, "CLAW_RESILIENCE", "force".to_string());

    eprintln!("\n  ✓ Launching with model: {model}");
    eprintln!("  Provider: LM Studio at {host}:{port}");
    eprintln!("  Resilience: enabled (force)\n");

    Ok(SetupResult {
        action: SetupAction::LaunchRepl,
        model: Some(model),
        env: env_map,
    })
}

// ── OpenRouter ────────────────────────────────────────────────────────

const OPENROUTER_CONFIG_SUBDIR: &str = ".config/openroutercode";
// Legacy location used before the launcher/config-dir rename to
// `openroutercode`. We still read from here so existing users' saved
// API keys keep working without re-entering them.
const OPENROUTER_LEGACY_CONFIG_SUBDIR: &str = ".config/opencode";
const OPENROUTER_ENV_FILE: &str = ".env";

fn openrouter_config_dir() -> PathBuf {
    default_home().join(OPENROUTER_CONFIG_SUBDIR)
}

fn openrouter_env_path() -> PathBuf {
    openrouter_config_dir().join(OPENROUTER_ENV_FILE)
}

fn openrouter_legacy_config_dir() -> PathBuf {
    default_home().join(OPENROUTER_LEGACY_CONFIG_SUBDIR)
}

fn openrouter_legacy_env_path() -> PathBuf {
    openrouter_legacy_config_dir().join(OPENROUTER_ENV_FILE)
}

/// Parse an `OPENROUTER_API_KEY=...` value out of a `.env`-style file.
/// Returns `None` if the file is missing, unreadable, or has no non-empty key.
fn read_key_from_env_file(path: &Path) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    for line in content.lines() {
        if let Some(value) = line.strip_prefix("OPENROUTER_API_KEY=") {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

pub fn save_openrouter_api_key(key: &str) -> Result<(), Box<dyn std::error::Error>> {
    let config_dir = openrouter_config_dir();
    fs::create_dir_all(&config_dir)?;
    let env_path = openrouter_env_path();
    fs::write(&env_path, format!("OPENROUTER_API_KEY={key}\n"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&env_path, fs::Permissions::from_mode(0o600))?;
    }
    eprintln!("OpenRouter API key saved to {}", env_path.display());
    Ok(())
}

fn load_openrouter_api_key() -> Result<String, String> {
    if let Ok(key) = env::var("OPENROUTER_API_KEY") {
        if !key.is_empty() {
            return Ok(key);
        }
    }
    // Prefer the current config dir, then fall back to the legacy
    // `~/.config/opencode/.env` so keys saved before the rename still work.
    if let Some(key) = read_key_from_env_file(&openrouter_env_path()) {
        return Ok(key);
    }
    if let Some(key) = read_key_from_env_file(&openrouter_legacy_env_path()) {
        return Ok(key);
    }
    Err(format!(
        "OpenRouter API key not found.\n\
         Set the OPENROUTER_API_KEY environment variable, or save a key with:\n\
         claw setup openrouter --set-key <your_key>\n\
         (Keys are stored in {path})",
        path = openrouter_env_path().display()
    ))
}

struct OpenRouterModel {
    id: String,
    name: String,
    context_length: u64,
    pricing_prompt: f64,
    pricing_completion: f64,
}

async fn fetch_openrouter_models(api_key: &str) -> Result<Vec<OpenRouterModel>, String> {
    let url = "https://openrouter.ai/api/v1/models?supported_parameters=tools";
    let resp = reqwest::Client::new()
        .get(url)
        .header("Authorization", format!("Bearer {api_key}"))
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("failed to fetch OpenRouter models: {e}"))?;

    if !resp.status().is_success() {
        if resp.status().as_u16() == 401 {
            return Err(
                "Invalid or expired OpenRouter API key – check OPENROUTER_API_KEY".to_string(),
            );
        }
        return Err(format!("OpenRouter API returned HTTP {}", resp.status()));
    }

    let data: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("failed to parse OpenRouter response: {e}"))?;

    let models: Vec<OpenRouterModel> = data["data"]
        .as_array()
        .ok_or_else(|| "unexpected response format: 'data' is not an array".to_string())?
        .iter()
        .filter(|m| {
            m.get("supported_parameters")
                .and_then(|p| p.as_array())
                .map(|arr| arr.iter().any(|v| v == "tools"))
                .unwrap_or(false)
        })
        .map(|m| OpenRouterModel {
            id: m["id"].as_str().unwrap_or("").to_string(),
            name: m["name"].as_str().unwrap_or("").to_string(),
            context_length: m["context_length"].as_u64().unwrap_or(0),
            pricing_prompt: m["pricing"]["prompt"].as_f64().unwrap_or(0.0),
            pricing_completion: m["pricing"]["completion"].as_f64().unwrap_or(0.0),
        })
        .collect();

    Ok(models)
}

fn setup_openrouter(
    model_hint: Option<String>,
    list_only: bool,
) -> Result<SetupResult, Box<dyn std::error::Error>> {
    let api_key = load_openrouter_api_key().map_err(|e| {
        Box::new(std::io::Error::new(std::io::ErrorKind::Other, e)) as Box<dyn std::error::Error>
    })?;

    if list_only {
        let runtime = tokio::runtime::Runtime::new()?;
        let models = runtime.block_on(fetch_openrouter_models(&api_key))?;
        if models.is_empty() {
            eprintln!("No tool-capable models found on OpenRouter.");
        } else {
            eprintln!("Tool-capable OpenRouter models ({}):", models.len());
            for m in &models {
                eprintln!(
                    "  {id:45} {name:50} ctx={ctx:>8}  prompt=${p:>7.5}/M  complete=${c:>7.5}/M",
                    id = m.id,
                    name = m.name,
                    ctx = m.context_length,
                    p = m.pricing_prompt * 1_000_000.0,
                    c = m.pricing_completion * 1_000_000.0,
                );
            }
        }
        return Ok(SetupResult {
            action: SetupAction::ListModelsOnly,
            model: None,
            env: HashMap::new(),
        });
    }

    let model = match model_hint {
        Some(m) => m,
        None => {
            let runtime = tokio::runtime::Runtime::new()?;
            let or_models = runtime.block_on(fetch_openrouter_models(&api_key))?;
            if or_models.is_empty() {
                return Err("No tool-capable models found on OpenRouter. Use `claw setup openrouter --list-models` to check.".into());
            }
            // Convert to ModelEntry for TUI
            let entries: Vec<ModelEntry> = or_models
                .iter()
                .map(|m| ModelEntry {
                    id: m.id.clone(),
                    provider: "OpenRouter".to_string(),
                    context: Some(m.context_length),
                    note: format!("${:.2}/M", m.pricing_prompt * 1_000_000.0),
                })
                .collect();
            match TuiPicker::pick(&entries, "OpenRouter — select a model") {
                Some(id) => id,
                None => return Err("No model selected".into()),
            }
        }
    };

    let mut env_map = HashMap::new();
    set_env(
        &mut env_map,
        "OPENAI_BASE_URL",
        "https://openrouter.ai/api/v1".to_string(),
    );
    set_env(&mut env_map, "OPENAI_API_KEY", api_key.clone());
    set_env(
        &mut env_map,
        "HTTP_REFERER",
        "https://localhost".to_string(),
    );
    set_env(&mut env_map, "X_TITLE", "claw-code".to_string());
    set_env(&mut env_map, "CLAW_RESILIENCE", "none".to_string());

    eprintln!("\n  ✓ Launching with model: {model}");
    eprintln!("  Provider: OpenRouter");
    eprintln!("  Resilience: disabled (none — cloud provider)\n");

    Ok(SetupResult {
        action: SetupAction::LaunchRepl,
        model: Some(model),
        env: env_map,
    })
}

// ── Unified model browser ─────────────────────────────────────────────
//
// `claw setup models` — scans all local providers plus recently-used
// API models and presents a single interactive picker.

fn unified_model_browser() -> Result<SetupResult, Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Runtime::new()?;

    // Collect models from all discoverable providers
    let mut all_models: Vec<ModelEntry> = Vec::new();

    // 1. Ollama
    if runtime.block_on(probe_ollama()) {
        match runtime.block_on(fetch_ollama_models()) {
            Ok(models) => all_models.extend(models),
            Err(e) => eprintln!("Ollama: {e}"),
        }
    } else {
        eprintln!("  Ollama: not reachable at {}", ollama_base_url());
    }

    // 2. LM Studio
    match runtime.block_on(discover_lmstudio_address()) {
        Ok(addr) => match runtime.block_on(fetch_lmstudio_models(&addr)) {
            Ok(models) => all_models.extend(models),
            Err(e) => eprintln!("LM Studio: {e}"),
        },
        Err(e) => eprintln!("  LM Studio: {e}"),
    }

    // 3. Recently-used API models from history file
    let recent_models = load_recent_api_models();
    for m in &recent_models {
        all_models.push(ModelEntry {
            id: m.clone(),
            provider: "Recent API".to_string(),
            context: None,
            note: String::new(),
        });
    }

    if all_models.is_empty() {
        return Err("No models found from any provider.\n\
             Make sure Ollama or LM Studio is running, or set a recent API model."
            .into());
    }

    let model = match TuiPicker::pick(&all_models, "All available models") {
        Some(id) => id,
        None => return Err("No model selected".into()),
    };

    // Determine which provider the selected model belongs to, to set correct env vars
    let (base_url, api_key, resilience, provider_name) =
        resolve_model_provider(&model, &all_models);

    let mut env_map = HashMap::new();
    set_env(&mut env_map, "OPENAI_BASE_URL", base_url);
    set_env(&mut env_map, "OPENAI_API_KEY", api_key);
    set_env(&mut env_map, "CLAW_RESILIENCE", resilience.clone());

    // Save to recent API models
    save_recent_api_model(&model);

    eprintln!("\n  ✓ Launching with model: {model}");
    eprintln!("  Provider: {provider_name}");
    eprintln!("  Resilience: {resilience}\n");

    Ok(SetupResult {
        action: SetupAction::LaunchRepl,
        model: Some(model),
        env: env_map,
    })
}

/// Resolve the correct env vars for a model selected from the unified browser.
fn resolve_model_provider(
    model: &str,
    all_models: &[ModelEntry],
) -> (String, String, String, String) {
    // Find which provider entry has this model
    let entry = all_models.iter().find(|m| m.id == model);
    let provider = entry.map(|m| m.provider.as_str()).unwrap_or("auto");

    match provider {
        "Ollama" => {
            let base = ollama_base_url();
            (
                format!("{base}/v1"),
                env::var("OPENAI_API_KEY").unwrap_or_else(|_| "ollama".to_string()),
                "force".to_string(),
                "Ollama".to_string(),
            )
        }
        "LM Studio" => {
            // Try to find the address from recent file
            let addr = load_lmstudio_recent()
                .first()
                .cloned()
                .unwrap_or_else(|| format!("{LMSTUDIO_DEFAULT_HOST}:{LMSTUDIO_DEFAULT_PORT}"));
            (
                format!("http://{addr}/v1"),
                env::var("OPENAI_API_KEY").unwrap_or_else(|_| "local-model".to_string()),
                "force".to_string(),
                "LM Studio".to_string(),
            )
        }
        "OpenRouter" => {
            let key = load_openrouter_api_key()
                .unwrap_or_else(|_| env::var("OPENAI_API_KEY").unwrap_or_default());
            (
                "https://openrouter.ai/api/v1".to_string(),
                key,
                "none".to_string(),
                "OpenRouter".to_string(),
            )
        }
        _ => {
            // Auto-detect from env vars
            if env::var("OPENAI_BASE_URL").is_ok() {
                (
                    env::var("OPENAI_BASE_URL").unwrap(),
                    env::var("OPENAI_API_KEY").unwrap_or_default(),
                    env::var("CLAW_RESILIENCE").unwrap_or_else(|_| "auto".to_string()),
                    "custom".to_string(),
                )
            } else {
                // Default to Ollama-like
                let base = ollama_base_url();
                (
                    format!("{base}/v1"),
                    env::var("OPENAI_API_KEY").unwrap_or_else(|_| "ollama".to_string()),
                    "force".to_string(),
                    "auto (Ollama)".to_string(),
                )
            }
        }
    }
}

// ── Recent API models tracking ────────────────────────────────────────

const RECENT_API_MODELS_FILE: &str = ".claw_recent_api_models";

fn recent_api_models_path() -> PathBuf {
    default_home().join(RECENT_API_MODELS_FILE)
}

fn load_recent_api_models() -> Vec<String> {
    let path = recent_api_models_path();
    if !path.exists() {
        return Vec::new();
    }
    fs::read_to_string(&path)
        .ok()
        .map(|c| {
            c.lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn save_recent_api_model(model: &str) {
    let path = recent_api_models_path();
    let mut models: Vec<String> = path
        .exists()
        .then(|| {
            fs::read_to_string(&path)
                .ok()
                .map(|c| {
                    c.lines()
                        .map(|l| l.trim().to_string())
                        .filter(|l| !l.is_empty())
                        .collect()
                })
                .unwrap_or_default()
        })
        .unwrap_or_default();
    models.retain(|m| m != model);
    models.insert(0, model.to_string());
    models.truncate(25);
    let _ = fs::write(&path, models.join("\n"));
}

// ── tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_api_models_roundtrip() {
        // Shares `recent_api_models_path()` (HOME-derived) with other tests;
        // hold the env lock so parallel runs don't clobber the file.
        let _env = home_env_lock();
        let path = recent_api_models_path();
        let _ = fs::remove_file(&path);

        save_recent_api_model("gpt-4o");
        let models = load_recent_api_models();
        assert_eq!(models, vec!["gpt-4o"]);

        save_recent_api_model("gpt-4o");
        let models = load_recent_api_models();
        assert_eq!(models.len(), 1, "duplicate should not add");

        save_recent_api_model("claude-opus-4-6");
        let models = load_recent_api_models();
        assert_eq!(models, vec!["claude-opus-4-6", "gpt-4o"]);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn recent_api_models_limited() {
        let _env = home_env_lock();
        let path = recent_api_models_path();
        let _ = fs::remove_file(&path);
        for i in 0..50 {
            save_recent_api_model(&format!("model-{i:02}"));
        }
        let models = load_recent_api_models();
        assert!(models.len() <= 25, "max 25 entries, got {}", models.len());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn setup_target_equality() {
        assert_eq!(SetupTarget::Ollama, SetupTarget::Ollama);
        assert_eq!(SetupTarget::LmStudio, SetupTarget::LmStudio);
        assert_eq!(SetupTarget::Models, SetupTarget::Models);
        assert_eq!(
            SetupTarget::OpenRouter {
                set_key: None,
                list_models: false
            },
            SetupTarget::OpenRouter {
                set_key: None,
                list_models: false
            }
        );
    }

    #[test]
    fn resolve_model_provider_defaults_to_ollama() {
        let all_models: Vec<ModelEntry> = vec![];
        let (url, _, resilience, name) = resolve_model_provider("unknown-model", &all_models);
        assert!(
            url.contains("11434"),
            "should default to Ollama port: {url}"
        );
        assert_eq!(resilience, "force");
        assert!(name.contains("Ollama") || name.contains("auto"));
    }

    #[test]
    fn test_format_ctx() {
        assert_eq!(format_ctx(0), "0");
        assert_eq!(format_ctx(999), "999");
        assert_eq!(format_ctx(1_000), "1K");
        assert_eq!(format_ctx(128_000), "128K");
        assert_eq!(format_ctx(1_000_000), "1M");
        assert_eq!(format_ctx(1_500_000), "1M");
    }

    // ── OpenRouter API key load/save (env-isolated) ───────────────────
    //
    // These tests mutate process-global env vars (`HOME`,
    // `OPENROUTER_API_KEY`), so they share a mutex and restore the prior
    // values on drop. Other tests in this binary run in the same process.
    use std::sync::{Mutex, MutexGuard, OnceLock};

    // Serializes every test in this module that depends on `HOME` (the
    // OpenRouter key tests below plus the recent-models tests above, which
    // share a single `recent_api_models_path()` file). Without this, those
    // tests race on process-global state when run in parallel.
    fn home_env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Restores `HOME` and `OPENROUTER_API_KEY` to their prior values when
    /// dropped, regardless of test outcome.
    struct OpenRouterEnvGuard {
        _lock: MutexGuard<'static, ()>,
        prev_home: Option<String>,
        prev_key: Option<String>,
    }

    impl OpenRouterEnvGuard {
        /// Acquire the shared lock, point `HOME` at `home`, and clear
        /// `OPENROUTER_API_KEY` so file-based loading is exercised.
        fn new(home: &Path) -> Self {
            let lock = home_env_lock();
            let prev_home = env::var("HOME").ok();
            let prev_key = env::var("OPENROUTER_API_KEY").ok();
            env::set_var("HOME", home);
            env::remove_var("OPENROUTER_API_KEY");
            Self {
                _lock: lock,
                prev_home,
                prev_key,
            }
        }
    }

    impl Drop for OpenRouterEnvGuard {
        fn drop(&mut self) {
            match &self.prev_home {
                Some(v) => env::set_var("HOME", v),
                None => env::remove_var("HOME"),
            }
            match &self.prev_key {
                Some(v) => env::set_var("OPENROUTER_API_KEY", v),
                None => env::remove_var("OPENROUTER_API_KEY"),
            }
        }
    }

    fn openrouter_temp_home() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time should be after epoch")
            .as_nanos();
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("claw-openrouter-{nanos}-{unique}"));
        fs::create_dir_all(&dir).expect("temp home should be creatable");
        dir
    }

    fn write_env_file(dir: &Path, key: &str) {
        fs::create_dir_all(dir).expect("config dir should be creatable");
        fs::write(
            dir.join(OPENROUTER_ENV_FILE),
            format!("OPENROUTER_API_KEY={key}\n"),
        )
        .expect("env file should write");
    }

    #[test]
    fn load_openrouter_api_key_prefers_new_dir() {
        let home = openrouter_temp_home();
        let _guard = OpenRouterEnvGuard::new(&home);
        write_env_file(&home.join(OPENROUTER_CONFIG_SUBDIR), "new-dir-key");

        let loaded = load_openrouter_api_key().expect("key should load from new dir");
        assert_eq!(loaded, "new-dir-key");

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn load_openrouter_api_key_falls_back_to_legacy_opencode_dir() {
        let home = openrouter_temp_home();
        let _guard = OpenRouterEnvGuard::new(&home);
        // Only the legacy ~/.config/opencode/.env exists.
        write_env_file(&home.join(OPENROUTER_LEGACY_CONFIG_SUBDIR), "legacy-key");

        let loaded = load_openrouter_api_key().expect("key should load from legacy opencode dir");
        assert_eq!(loaded, "legacy-key");

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn load_openrouter_api_key_new_dir_wins_over_legacy() {
        let home = openrouter_temp_home();
        let _guard = OpenRouterEnvGuard::new(&home);
        write_env_file(&home.join(OPENROUTER_CONFIG_SUBDIR), "new-dir-key");
        write_env_file(&home.join(OPENROUTER_LEGACY_CONFIG_SUBDIR), "legacy-key");

        let loaded = load_openrouter_api_key().expect("key should load");
        assert_eq!(loaded, "new-dir-key", "new dir must take precedence");

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn save_then_load_roundtrips_into_new_dir() {
        let home = openrouter_temp_home();
        let _guard = OpenRouterEnvGuard::new(&home);

        save_openrouter_api_key("saved-key").expect("save should succeed");

        // The key file must live under the new config dir.
        let expected = home
            .join(OPENROUTER_CONFIG_SUBDIR)
            .join(OPENROUTER_ENV_FILE);
        assert!(
            expected.exists(),
            "expected key file at {}",
            expected.display()
        );
        assert!(
            !home
                .join(OPENROUTER_LEGACY_CONFIG_SUBDIR)
                .join(OPENROUTER_ENV_FILE)
                .exists(),
            "save must not write to the legacy dir"
        );

        let loaded = load_openrouter_api_key().expect("key should load after save");
        assert_eq!(loaded, "saved-key");

        let _ = fs::remove_dir_all(&home);
    }
}
