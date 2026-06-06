//! Purpose-built `run_subagent` MCP tool surface.
//!
//! `claw mcp serve` exposes this curated `run_subagent` + `list_presets` pair
//! so a parent agent (Claude Code / openclaw) can spawn a LOCAL or OPENROUTER
//! sub-agent over stdio MCP and receive a single, bounded structured result.
//!
//! Unlike the raw `Agent` tool (which detaches a background thread and returns
//! a manifest), `run_subagent` runs the agent turn **synchronously and inline**
//! so the parent gets the final answer in the same `tools/call` response.
//!
//! Two execution backends:
//! - In-process (default, `isolated` false/absent): build a
//!   [`ConversationRuntime`] locally — mirroring [`crate::run_agent_job`] — and
//!   run a single turn, restoring any process env / cwd we mutate on the way
//!   out (the server is long-lived; one call must never leak provider state
//!   into the next).
//! - Isolated (`isolated: true`): shell out to
//!   `scripts/launchers/run-claw-code.sh`, which gives a git worktree, a hard
//!   `kill -9` timeout, and the 4-line `/tmp/claw-runs/<id>/` contract. See
//!   [`run_subagent_isolated`] for the preset limitation.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use runtime::permission_enforcer::PermissionEnforcer;
use runtime::{ConversationRuntime, PermissionMode, Session};

use crate::{
    agent_permission_policy_for_mode, allowed_tools_for_subagent, build_agent_system_prompt,
    final_assistant_text, normalize_subagent_type, resolve_agent_model, ProviderRuntimeClient,
    SubagentToolExecutor, ToolSpec, DEFAULT_AGENT_MAX_ITERATIONS, DEFAULT_AGENT_MODEL,
};

/// Default cap on the returned `summary` text, in characters.
const DEFAULT_MAX_OUTPUT_CHARS: usize = 4000;

/// Input for the `run_subagent` MCP tool.
#[derive(Debug, Clone, Deserialize)]
pub struct RunSubagentInput {
    /// Provider routing: `local|openrouter|ollama|lmstudio|anthropic|openai|auto`.
    /// Absent/empty is treated as `auto` (use whatever env is already present).
    #[serde(default)]
    pub provider: Option<String>,
    /// Model id. Empty/absent falls back to `CLAW_SUBAGENT_MODEL` then the
    /// `DeepSeek` default.
    #[serde(default)]
    pub model: Option<String>,
    /// The task prompt. Required and non-empty.
    pub prompt: String,
    /// Working directory for the sub-agent. Defaults to the server's cwd.
    #[serde(default)]
    pub repo_dir: Option<String>,
    /// Sub-agent type that selects the allowed-tool set / system prompt
    /// (e.g. `Explore`, `Plan`, `general-purpose`).
    #[serde(default)]
    pub subagent_type: Option<String>,
    /// `read-only|workspace-write|danger-full-access`; default `workspace-write`.
    #[serde(default)]
    pub permission_mode: Option<String>,
    /// When true, run via the isolated launcher (git worktree + hard kill).
    #[serde(default)]
    pub isolated: Option<bool>,
    /// Best-effort wall-clock timeout for the turn.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Cap on returned summary characters (default 4000).
    #[serde(default)]
    pub max_output_chars: Option<usize>,
    /// Preset name for `isolated: true` runs (see [`run_subagent_isolated`]).
    #[serde(default)]
    pub preset: Option<String>,
}

/// Structured result of a `run_subagent` call.
///
/// Always serializable; failures and timeouts are encoded in `status`/`error`
/// rather than surfaced as an MCP transport error, so the parent always gets
/// machine-readable detail.
#[derive(Debug, Clone, Serialize)]
pub struct RunSubagentOutput {
    /// `completed|failed|timeout`.
    pub status: String,
    /// Provider that was selected (after defaulting).
    pub provider: String,
    /// Model that was used (after defaulting).
    pub model: String,
    /// Resolved working directory.
    pub repo_dir: String,
    /// Final assistant text, truncated to `max_output_chars`.
    pub summary: String,
    /// True when `summary` was truncated.
    pub truncated: bool,
    /// `git diff --stat HEAD` of the repo, when it is a git repo.
    pub diff_stat: Option<String>,
    /// Error detail when `status` is `failed`/`timeout`.
    pub error: Option<String>,
    /// Wall-clock duration of the call.
    pub duration_ms: u64,
}

impl RunSubagentOutput {
    fn failed(provider: &str, model: &str, repo_dir: &str, error: impl Into<String>) -> Self {
        Self {
            status: "failed".to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            repo_dir: repo_dir.to_string(),
            summary: String::new(),
            truncated: false,
            diff_stat: None,
            error: Some(error.into()),
            duration_ms: 0,
        }
    }
}

/// JSON schema for the `run_subagent` tool. `prompt` required;
/// `additionalProperties:false`.
#[must_use]
pub fn run_subagent_tool_spec() -> ToolSpec {
    ToolSpec {
        name: "run_subagent",
        description: "Spawn a local or OpenRouter sub-agent on a delegated task and return a single bounded result. Defaults to a DeepSeek model via OpenRouter; set provider=local/lmstudio/ollama (with an explicit model) to use a local server, or provider=anthropic for a Claude model.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "prompt": { "type": "string", "description": "The task for the sub-agent." },
                "provider": {
                    "type": "string",
                    "enum": ["local", "openrouter", "ollama", "lmstudio", "anthropic", "openai", "auto"],
                    "description": "Provider routing. Default: whatever env is configured (auto)."
                },
                "model": { "type": "string", "description": "Model id. Defaults to CLAW_SUBAGENT_MODEL then a DeepSeek model." },
                "repo_dir": { "type": "string", "description": "Working directory. Defaults to the server cwd." },
                "subagent_type": { "type": "string", "description": "Explore|Plan|Verification|general-purpose, etc." },
                "permission_mode": {
                    "type": "string",
                    "enum": ["read-only", "workspace-write", "danger-full-access"],
                    "description": "Default: workspace-write."
                },
                "isolated": { "type": "boolean", "description": "Run in a git worktree via the launcher (hard kill on timeout)." },
                "timeout_secs": { "type": "integer", "minimum": 1, "description": "Best-effort wall-clock timeout." },
                "max_output_chars": { "type": "integer", "minimum": 1, "description": "Cap on returned summary chars (default 4000)." },
                "preset": { "type": "string", "description": "Preset name for isolated runs." }
            },
            "required": ["prompt"],
            "additionalProperties": false
        }),
        required_permission: PermissionMode::WorkspaceWrite,
    }
}

/// JSON schema for the `list_presets` tool (no inputs).
#[must_use]
pub fn list_presets_tool_spec() -> ToolSpec {
    ToolSpec {
        name: "list_presets",
        description: "List the available sub-agent presets (name, description, provider, model) discoverable from the repo's scripts/presets and ~/.lmcode/presets.",
        input_schema: json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
        required_permission: PermissionMode::ReadOnly,
    }
}

/// Dispatch an MCP `tools/call` for the curated sub-agent surface.
///
/// Returns `Ok(json)` for both tools — even when `run_subagent` ends in a
/// `failed`/`timeout` status — so the parent always receives structured
/// detail rather than an opaque MCP error. Only an unknown tool name (or a
/// serialization failure) yields `Err`.
///
/// # Errors
/// Returns `Err` when `name` is not a known tool, when `run_subagent` input
/// cannot be deserialized, or when output serialization fails.
pub fn handle_subagent_mcp_call(name: &str, args: &Value) -> Result<String, String> {
    match name {
        "run_subagent" => {
            let input: RunSubagentInput = serde_json::from_value(args.clone())
                .map_err(|error| format!("invalid run_subagent input: {error}"))?;
            let output = run_subagent(input);
            serde_json::to_string_pretty(&output)
                .map_err(|error| format!("failed to serialize run_subagent output: {error}"))
        }
        "list_presets" => {
            let presets = list_presets();
            serde_json::to_string_pretty(&presets)
                .map_err(|error| format!("failed to serialize presets: {error}"))
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

/// Run a sub-agent and return a bounded structured result.
///
/// Never panics: validation, provider, runtime, and timeout failures are all
/// encoded in [`RunSubagentOutput::status`] / [`RunSubagentOutput::error`].
#[must_use]
#[allow(clippy::needless_pass_by_value)] // public MCP entrypoint takes owned input
pub fn run_subagent(input: RunSubagentInput) -> RunSubagentOutput {
    let started = Instant::now();

    let provider = input
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("auto")
        .to_lowercase();

    // Resolve the model: explicit input > CLAW_SUBAGENT_MODEL > DeepSeek default.
    // `resolve_agent_model` already encodes that precedence.
    let model = resolve_agent_model(input.model.as_deref());
    let caller_gave_model = input
        .model
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| !value.is_empty());

    // Resolve the working directory up front so failure outputs can report it.
    let repo_dir = input
        .repo_dir
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map_or_else(
            || {
                std::env::current_dir()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default()
            },
            ToString::to_string,
        );

    // Validate: empty prompt.
    if input.prompt.trim().is_empty() {
        let mut output =
            RunSubagentOutput::failed(&provider, &model, &repo_dir, "prompt must not be empty");
        output.duration_ms = elapsed_ms(started);
        return output;
    }

    // Providers without a usable default model.
    if !caller_gave_model {
        if matches!(provider.as_str(), "local" | "ollama" | "lmstudio") {
            let mut output = RunSubagentOutput::failed(
                &provider,
                &model,
                &repo_dir,
                format!(
                    "provider `{provider}` requires an explicit `model` (local servers do not have a default; the DeepSeek default `{DEFAULT_AGENT_MODEL}` only applies to OpenRouter)"
                ),
            );
            output.duration_ms = elapsed_ms(started);
            return output;
        }
        if provider == "anthropic" {
            let mut output = RunSubagentOutput::failed(
                &provider,
                &model,
                &repo_dir,
                format!(
                    "provider `anthropic` requires an explicit Claude `model`; the DeepSeek default `{DEFAULT_AGENT_MODEL}` is not an Anthropic model"
                ),
            );
            output.duration_ms = elapsed_ms(started);
            return output;
        }
    }

    if input.isolated.unwrap_or(false) {
        let mut output = run_subagent_isolated(&input, &provider, &model, &repo_dir);
        output.duration_ms = elapsed_ms(started);
        return output;
    }

    run_subagent_in_process(&input, &provider, &model, &repo_dir, started)
}

/// In-process backend: build a runtime locally and run a single turn inline.
fn run_subagent_in_process(
    input: &RunSubagentInput,
    provider: &str,
    model: &str,
    repo_dir: &str,
    started: Instant,
) -> RunSubagentOutput {
    // RAII guards: provider env + cwd are restored when these drop, so the
    // long-lived server never leaks one call's config into the next. Dispatch
    // is serial (the MCP run loop processes calls one at a time), so mutating
    // process-global cwd here is safe.
    let _env_guard = apply_provider_env(provider, model);
    let _cwd_guard = match CwdGuard::enter(input.repo_dir.as_deref()) {
        Ok(guard) => guard,
        Err(error) => {
            let mut output = RunSubagentOutput::failed(provider, model, repo_dir, error);
            output.duration_ms = elapsed_ms(started);
            return output;
        }
    };

    let subagent_type = normalize_subagent_type(input.subagent_type.as_deref());
    let permission_mode = parse_permission_mode(input.permission_mode.as_deref());
    let allowed_tools = allowed_tools_for_subagent(&subagent_type);

    let max_output_chars = input.max_output_chars.unwrap_or(DEFAULT_MAX_OUTPUT_CHARS);
    let timeout = input.timeout_secs.map(Duration::from_secs);

    // Owned copies to move into the worker thread.
    let model_owned = model.to_string();
    let prompt = input.prompt.clone();

    let run = move || -> Result<String, String> {
        let system_prompt = build_agent_system_prompt(&subagent_type)?;
        let mut runtime =
            build_subagent_runtime(model_owned, allowed_tools, permission_mode, system_prompt)?
                .with_max_iterations(DEFAULT_AGENT_MAX_ITERATIONS);
        let summary = runtime
            .run_turn(prompt, None)
            .map_err(|error| error.to_string())?;
        Ok(final_assistant_text(&summary))
    };

    // Best-effort timeout: run the turn on a worker thread and wait with a
    // recv_timeout. NOTE: on timeout the worker thread cannot be hard-killed
    // in-process (it keeps running until the API call returns). Use
    // `isolated: true` when a hard kill is required.
    let result = run_with_optional_timeout(run, timeout);

    let mut output = match result {
        TurnResult::Completed(text) => {
            let (summary, truncated) = truncate_summary(&text, max_output_chars);
            RunSubagentOutput {
                status: "completed".to_string(),
                provider: provider.to_string(),
                model: model.to_string(),
                repo_dir: repo_dir.to_string(),
                summary,
                truncated,
                diff_stat: git_diff_stat(repo_dir),
                error: None,
                duration_ms: 0,
            }
        }
        TurnResult::Failed(error) => {
            let mut out = RunSubagentOutput::failed(provider, model, repo_dir, error);
            out.diff_stat = git_diff_stat(repo_dir);
            out
        }
        TurnResult::TimedOut => RunSubagentOutput {
            status: "timeout".to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            repo_dir: repo_dir.to_string(),
            summary: String::new(),
            truncated: false,
            diff_stat: git_diff_stat(repo_dir),
            error: Some(
                "sub-agent turn exceeded timeout; the worker thread continues in-process (use isolated:true for a hard kill)"
                    .to_string(),
            ),
            duration_ms: 0,
        },
    };
    output.duration_ms = elapsed_ms(started);
    output
}

/// Build a [`ConversationRuntime`] for an inline sub-agent turn.
///
/// Mirrors [`crate::build_agent_runtime`] but takes the model / allowed tools /
/// permission mode / system prompt directly instead of an `AgentJob`.
fn build_subagent_runtime(
    model: String,
    allowed_tools: BTreeSet<String>,
    permission_mode: PermissionMode,
    system_prompt: Vec<String>,
) -> Result<ConversationRuntime<ProviderRuntimeClient, SubagentToolExecutor>, String> {
    let api_client = ProviderRuntimeClient::new(model, allowed_tools.clone())?;
    let permission_policy = agent_permission_policy_for_mode(permission_mode);
    let tool_executor = SubagentToolExecutor::new(allowed_tools)
        .with_enforcer(PermissionEnforcer::new(permission_policy.clone()));
    Ok(ConversationRuntime::new(
        Session::new(),
        api_client,
        tool_executor,
        permission_policy,
        system_prompt,
    ))
}

enum TurnResult {
    Completed(String),
    Failed(String),
    TimedOut,
}

/// Run `run` on a worker thread; when `timeout` is set, give up waiting after
/// it elapses (returning [`TurnResult::TimedOut`]). The worker thread is
/// detached on timeout — it cannot be cancelled from here.
fn run_with_optional_timeout<F>(run: F, timeout: Option<Duration>) -> TurnResult
where
    F: FnOnce() -> Result<String, String> + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    let handle = std::thread::Builder::new()
        .name("claw-run-subagent".to_string())
        .spawn(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run))
                .unwrap_or_else(|_| Err("sub-agent worker thread panicked".to_string()));
            let _ = tx.send(outcome);
        });
    let handle = match handle {
        Ok(handle) => handle,
        Err(error) => return TurnResult::Failed(format!("failed to spawn worker thread: {error}")),
    };

    match timeout {
        Some(limit) => match rx.recv_timeout(limit) {
            Ok(Ok(text)) => {
                let _ = handle.join();
                TurnResult::Completed(text)
            }
            Ok(Err(error)) => {
                let _ = handle.join();
                TurnResult::Failed(error)
            }
            Err(mpsc::RecvTimeoutError::Timeout) => TurnResult::TimedOut,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                TurnResult::Failed("sub-agent worker thread disconnected".to_string())
            }
        },
        None => match rx.recv() {
            Ok(Ok(text)) => {
                let _ = handle.join();
                TurnResult::Completed(text)
            }
            Ok(Err(error)) => {
                let _ = handle.join();
                TurnResult::Failed(error)
            }
            Err(_) => TurnResult::Failed("sub-agent worker thread disconnected".to_string()),
        },
    }
}

/// Map a textual permission mode to [`PermissionMode`]; default
/// `workspace-write`. Unknown values fall back to the default.
fn parse_permission_mode(mode: Option<&str>) -> PermissionMode {
    match mode.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        Some("read-only" | "readonly" | "read_only") => PermissionMode::ReadOnly,
        Some("danger-full-access" | "danger" | "full" | "dangerfullaccess") => {
            PermissionMode::DangerFullAccess
        }
        // "workspace-write", empty, unknown -> default.
        _ => PermissionMode::WorkspaceWrite,
    }
}

/// Truncate `text` to at most `max_chars` characters (on a char boundary),
/// appending a marker when truncated. Returns `(text, truncated)`.
fn truncate_summary(text: &str, max_chars: usize) -> (String, bool) {
    if text.chars().count() <= max_chars {
        return (text.to_string(), false);
    }
    let dropped = text.chars().count() - max_chars;
    let truncated: String = text.chars().take(max_chars).collect();
    (
        format!("{truncated}\n\n[...truncated {dropped} chars...]"),
        true,
    )
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Best-effort `git -C <dir> diff --stat HEAD`. Returns `None` when the
/// directory is not a git repo or git is unavailable.
fn git_diff_stat(repo_dir: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["-C", repo_dir, "diff", "--stat", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

// ---------------------------------------------------------------------------
// Provider env guard
// ---------------------------------------------------------------------------

/// RAII guard that sets provider env vars on construction and restores their
/// previous values (or removes them if previously unset) on drop.
///
/// The MCP server is long-lived and dispatches calls serially, so we must not
/// leak one call's `OPENAI_BASE_URL`/`OPENAI_API_KEY`/`CLAW_RESILIENCE` into
/// the next.
pub struct ProviderEnvGuard {
    /// (key, previous value) — `None` previous means the var was unset.
    saved: Vec<(String, Option<String>)>,
}

impl ProviderEnvGuard {
    /// Set `key=value` only if `key` is not already present, recording the
    /// prior state so [`Drop`] can restore it. Mirrors the shell
    /// `${VAR:-default}` / `setdefault` behavior in run-claw-code.sh.
    fn set_if_absent(&mut self, key: &str, value: &str) {
        if std::env::var_os(key).is_some() {
            return;
        }
        self.saved.push((key.to_string(), None));
        std::env::set_var(key, value);
    }
}

impl Drop for ProviderEnvGuard {
    fn drop(&mut self) {
        // Restore in reverse order so repeated sets of the same key unwind
        // correctly back to the original value.
        for (key, previous) in self.saved.drain(..).rev() {
            match previous {
                Some(value) => std::env::set_var(&key, value),
                None => std::env::remove_var(&key),
            }
        }
    }
}

/// Apply provider->env mapping (mirroring run-claw-code.sh:397-419) and return
/// a guard that restores the previous env on drop. Variables are only set when
/// absent so an explicit caller-provided `OPENAI_*` is respected.
#[must_use]
pub fn apply_provider_env(provider: &str, model: &str) -> ProviderEnvGuard {
    let mut guard = ProviderEnvGuard { saved: Vec::new() };
    let _ = model; // model does not affect env mapping today
    match provider {
        "openrouter" => {
            guard.set_if_absent("OPENAI_BASE_URL", "https://openrouter.ai/api/v1");
            if std::env::var_os("OPENAI_API_KEY").is_none() {
                if let Some(key) = read_openrouter_key() {
                    guard.set_if_absent("OPENAI_API_KEY", &key);
                }
            }
            guard.set_if_absent("CLAW_RESILIENCE", "none");
        }
        "local" | "ollama" => {
            guard.set_if_absent("OPENAI_BASE_URL", "http://localhost:11434/v1");
            guard.set_if_absent("OPENAI_API_KEY", "ollama");
            guard.set_if_absent("CLAW_RESILIENCE", "force");
        }
        "lmstudio" => {
            guard.set_if_absent("OPENAI_BASE_URL", "http://localhost:1234/v1");
            guard.set_if_absent("OPENAI_API_KEY", "lmstudio");
            guard.set_if_absent("CLAW_RESILIENCE", "force");
        }
        // anthropic -> uses ANTHROPIC_API_KEY, set nothing.
        // openai|auto|unknown -> set nothing beyond what's present.
        _ => {}
    }
    guard
}

/// Read an `OpenRouter` API key from the env or the on-disk config files used
/// by the launchers, in the same order as `run-claw-code.sh`:
/// 1. `OPENROUTER_API_KEY` env var.
/// 2. `~/.config/openroutercode/.env`.
/// 3. `~/.config/opencode/.env` (legacy).
///
/// `.env` files are parsed for an `OPENAI_API_KEY=` or `OPENROUTER_API_KEY=`
/// line (optionally `export`-prefixed, optionally quoted).
#[must_use]
pub fn read_openrouter_key() -> Option<String> {
    if let Ok(key) = std::env::var("OPENROUTER_API_KEY") {
        if !key.trim().is_empty() {
            return Some(key.trim().to_string());
        }
    }
    let home = std::env::var("HOME").ok()?;
    let candidates = [
        PathBuf::from(&home).join(".config/openroutercode/.env"),
        PathBuf::from(&home).join(".config/opencode/.env"),
    ];
    for path in candidates {
        if let Some(key) = read_key_from_env_file(&path) {
            return Some(key);
        }
    }
    None
}

fn read_key_from_env_file(path: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    for line in contents.lines() {
        let line = line.trim();
        let line = line.strip_prefix("export ").unwrap_or(line);
        for key_name in ["OPENAI_API_KEY", "OPENROUTER_API_KEY"] {
            if let Some(rest) = line.strip_prefix(key_name) {
                if let Some(value) = rest.strip_prefix('=') {
                    let value = value.trim().trim_matches(|c| c == '"' || c == '\'').trim();
                    if !value.is_empty() {
                        return Some(value.to_string());
                    }
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// cwd guard
// ---------------------------------------------------------------------------

/// RAII guard that changes the process cwd and restores it on drop. Safe under
/// the MCP server's serial dispatch.
struct CwdGuard {
    previous: Option<PathBuf>,
}

impl CwdGuard {
    fn enter(repo_dir: Option<&str>) -> Result<Self, String> {
        let target = repo_dir.map(str::trim).filter(|value| !value.is_empty());
        let Some(target) = target else {
            return Ok(Self { previous: None });
        };
        let path = PathBuf::from(target);
        if !path.is_dir() {
            return Err(format!("repo_dir `{target}` is not a directory"));
        }
        let previous = std::env::current_dir().ok();
        std::env::set_current_dir(&path)
            .map_err(|error| format!("failed to set cwd to `{target}`: {error}"))?;
        Ok(Self { previous })
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            let _ = std::env::set_current_dir(previous);
        }
    }
}

// ---------------------------------------------------------------------------
// Presets
// ---------------------------------------------------------------------------

/// A preset descriptor surfaced by `list_presets`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PresetInfo {
    pub name: String,
    pub description: String,
    pub provider: String,
    pub model: String,
}

/// Enumerate presets from `<repo_root>/scripts/presets/*.json` and
/// `~/.lmcode/presets/*.json`, parse each, and emit
/// `[{name, description, provider, model}]`.
#[must_use]
pub fn list_presets() -> Vec<PresetInfo> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(root) = locate_repo_root() {
        dirs.push(root.join("scripts/presets"));
    }
    if let Ok(home) = std::env::var("HOME") {
        dirs.push(PathBuf::from(home).join(".lmcode/presets"));
    }
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for dir in dirs {
        collect_presets_from_dir(&dir, &mut seen, &mut out);
    }
    out
}

fn collect_presets_from_dir(dir: &Path, seen: &mut BTreeSet<String>, out: &mut Vec<PresetInfo>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    for path in paths {
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&contents) else {
            continue;
        };
        let file_stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default()
            .to_string();
        let name = value
            .get("preset_name")
            .and_then(Value::as_str)
            .map_or(file_stem, ToString::to_string);
        if !seen.insert(name.clone()) {
            continue;
        }
        out.push(PresetInfo {
            name,
            description: string_field(&value, "description"),
            provider: string_field(&value, "provider"),
            model: string_field(&value, "model"),
        });
    }
}

fn string_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Locate the repo root: prefer `CLAW_CODE_ROOT`, else walk up from cwd to a
/// directory that contains `scripts/presets/`.
fn locate_repo_root() -> Option<PathBuf> {
    if let Ok(root) = std::env::var("CLAW_CODE_ROOT") {
        if !root.trim().is_empty() {
            return Some(PathBuf::from(root));
        }
    }
    let mut dir = std::env::current_dir().ok()?;
    loop {
        if dir.join("scripts/presets").is_dir() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

// ---------------------------------------------------------------------------
// Isolated backend
// ---------------------------------------------------------------------------

/// Isolated backend: shell out to `scripts/launchers/run-claw-code.sh`.
///
/// LIMITATION: that launcher is **preset-driven** — it requires `--agent
/// <preset>` and resolves provider/model/permissions from the preset JSON; it
/// has no flag for an ad-hoc provider/model. So:
/// - If `preset` is supplied, we invoke the launcher with it (the launcher's
///   own git-worktree + hard `kill -9` timeout + 4-line contract apply, and we
///   parse `status.json` + `summary.md` + `diff.patch` back into the output).
/// - If `preset` is absent, we cannot honor `isolated: true` faithfully (a
///   synthesized temp preset would still need a resolvable model/provider and
///   risks leaking secrets to a world-readable temp file), so we fall back to
///   the in-process backend and note this in `error`.
fn run_subagent_isolated(
    input: &RunSubagentInput,
    provider: &str,
    model: &str,
    repo_dir: &str,
) -> RunSubagentOutput {
    let Some(preset) = input
        .preset
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        // No preset: fall back to in-process and explain.
        let mut output = run_subagent_in_process(input, provider, model, repo_dir, Instant::now());
        let note = "isolated:true requires a `preset` (the run-claw-code.sh launcher is preset-driven and has no ad-hoc provider/model flag); ran in-process instead";
        output.error = Some(match output.error.take() {
            Some(existing) => format!("{note}; underlying: {existing}"),
            None => note.to_string(),
        });
        return output;
    };

    let Some(script) = locate_launcher_script() else {
        return RunSubagentOutput::failed(
            provider,
            model,
            repo_dir,
            "could not locate scripts/launchers/run-claw-code.sh",
        );
    };

    let mut command = Command::new(&script);
    command
        .arg("--agent")
        .arg(preset)
        .arg("--dir")
        .arg(repo_dir)
        .arg("--plan")
        .arg(&input.prompt);
    if let Some(timeout) = input.timeout_secs {
        command.arg("--timeout").arg(timeout.to_string());
    }

    let output = match command.output() {
        Ok(output) => output,
        Err(error) => {
            return RunSubagentOutput::failed(
                provider,
                model,
                repo_dir,
                format!("failed to run launcher: {error}"),
            );
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let task_dir = parse_launcher_task_dir(&stdout);
    let Some(task_dir) = task_dir else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return RunSubagentOutput::failed(
            provider,
            model,
            repo_dir,
            format!(
                "launcher did not report a task dir; stderr: {}",
                stderr.trim()
            ),
        );
    };

    parse_isolated_artifacts(&task_dir, provider, model, repo_dir, input)
}

/// Parse `/tmp/claw-runs/<id>/{status.json,summary.md,diff.patch}` into output.
fn parse_isolated_artifacts(
    task_dir: &Path,
    provider: &str,
    model: &str,
    repo_dir: &str,
    input: &RunSubagentInput,
) -> RunSubagentOutput {
    let status_json = std::fs::read_to_string(task_dir.join("status.json")).ok();
    let status_value: Option<Value> = status_json
        .as_deref()
        .and_then(|raw| serde_json::from_str(raw).ok());
    let launcher_status = status_value
        .as_ref()
        .and_then(|value| value.get("status"))
        .and_then(Value::as_str)
        .unwrap_or("failed");
    // Map launcher status -> our status vocabulary.
    let status = match launcher_status {
        "done" => "completed",
        "timeout" => "timeout",
        other => other, // "failed" etc.
    };

    let summary_raw = std::fs::read_to_string(task_dir.join("summary.md")).unwrap_or_default();
    let max_output_chars = input.max_output_chars.unwrap_or(DEFAULT_MAX_OUTPUT_CHARS);
    let (summary, truncated) = truncate_summary(summary_raw.trim(), max_output_chars);

    let diff_stat = std::fs::read_to_string(task_dir.join("diff.patch"))
        .ok()
        .map(|diff| {
            // diff.patch begins with `--stat` output then the full diff; keep
            // the leading stat lines (up to the first `diff --git`).
            diff.lines()
                .take_while(|line| !line.starts_with("diff --git"))
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string()
        })
        .filter(|stat| !stat.is_empty());

    let error = if status == "completed" {
        None
    } else {
        Some(format!(
            "isolated run ended with launcher status `{launcher_status}` (see {})",
            task_dir.display()
        ))
    };

    RunSubagentOutput {
        status: status.to_string(),
        provider: provider.to_string(),
        model: model.to_string(),
        repo_dir: repo_dir.to_string(),
        summary,
        truncated,
        diff_stat,
        error,
        duration_ms: 0,
    }
}

/// First stdout line of the launcher is `task_id=<uuid>`; the second is the
/// absolute path to `status.json`. Derive the task dir from that.
fn parse_launcher_task_dir(stdout: &str) -> Option<PathBuf> {
    for line in stdout.lines() {
        let line = line.trim();
        if line.ends_with("status.json") {
            return Path::new(line).parent().map(Path::to_path_buf);
        }
    }
    // Fallback: task_id=<id> -> /tmp/claw-runs/<id>.
    for line in stdout.lines() {
        if let Some(id) = line.trim().strip_prefix("task_id=") {
            if !id.is_empty() {
                return Some(PathBuf::from("/tmp/claw-runs").join(id));
            }
        }
    }
    None
}

fn locate_launcher_script() -> Option<PathBuf> {
    let root = locate_repo_root()?;
    let script = root.join("scripts/launchers/run-claw-code.sh");
    script.exists().then_some(script)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn env_guard() -> MutexGuard<'static, ()> {
        env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn unique_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        std::env::temp_dir().join(format!("claw-subagent-{tag}-{nanos}"))
    }

    #[test]
    fn run_subagent_rejects_empty_prompt() {
        let output = run_subagent(RunSubagentInput {
            provider: Some("openrouter".to_string()),
            model: Some("deepseek/deepseek-v4-pro".to_string()),
            prompt: "   ".to_string(),
            repo_dir: None,
            subagent_type: None,
            permission_mode: None,
            isolated: None,
            timeout_secs: None,
            max_output_chars: None,
            preset: None,
        });
        assert_eq!(output.status, "failed");
        assert!(output
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("prompt must not be empty"));
    }

    #[test]
    fn run_subagent_local_requires_explicit_model() {
        // No model + local provider -> failed before any LLM call.
        let output = run_subagent(RunSubagentInput {
            provider: Some("local".to_string()),
            model: None,
            prompt: "do a thing".to_string(),
            repo_dir: None,
            subagent_type: None,
            permission_mode: None,
            isolated: None,
            timeout_secs: None,
            max_output_chars: None,
            preset: None,
        });
        assert_eq!(output.status, "failed");
        assert!(output
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("requires an explicit `model`"));
    }

    #[test]
    fn run_subagent_anthropic_requires_explicit_model() {
        let output = run_subagent(RunSubagentInput {
            provider: Some("anthropic".to_string()),
            model: None,
            prompt: "do a thing".to_string(),
            repo_dir: None,
            subagent_type: None,
            permission_mode: None,
            isolated: None,
            timeout_secs: None,
            max_output_chars: None,
            preset: None,
        });
        assert_eq!(output.status, "failed");
        assert!(output
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("Anthropic"));
    }

    #[test]
    fn apply_provider_env_sets_openrouter_and_restores() {
        let _guard = env_guard();
        // Arrange: known prior state.
        std::env::set_var("OPENROUTER_API_KEY", "sk-or-test");
        std::env::remove_var("OPENAI_BASE_URL");
        std::env::remove_var("OPENAI_API_KEY");
        std::env::remove_var("CLAW_RESILIENCE");

        {
            let _env = apply_provider_env("openrouter", "deepseek/deepseek-v4-pro");
            assert_eq!(
                std::env::var("OPENAI_BASE_URL").as_deref(),
                Ok("https://openrouter.ai/api/v1")
            );
            assert_eq!(std::env::var("OPENAI_API_KEY").as_deref(), Ok("sk-or-test"));
            assert_eq!(std::env::var("CLAW_RESILIENCE").as_deref(), Ok("none"));
        }
        // After drop: restored to unset.
        assert!(std::env::var_os("OPENAI_BASE_URL").is_none());
        assert!(std::env::var_os("OPENAI_API_KEY").is_none());
        assert!(std::env::var_os("CLAW_RESILIENCE").is_none());

        std::env::remove_var("OPENROUTER_API_KEY");
    }

    #[test]
    fn apply_provider_env_preserves_existing_base_url() {
        let _guard = env_guard();
        std::env::set_var("OPENAI_BASE_URL", "http://preset.example/v1");
        std::env::set_var("OPENAI_API_KEY", "preset-key");
        std::env::remove_var("CLAW_RESILIENCE");

        {
            let _env = apply_provider_env("lmstudio", "qwen-coder");
            // Existing values are respected (set_if_absent).
            assert_eq!(
                std::env::var("OPENAI_BASE_URL").as_deref(),
                Ok("http://preset.example/v1")
            );
            assert_eq!(std::env::var("OPENAI_API_KEY").as_deref(), Ok("preset-key"));
            assert_eq!(std::env::var("CLAW_RESILIENCE").as_deref(), Ok("force"));
        }
        assert_eq!(
            std::env::var("OPENAI_BASE_URL").as_deref(),
            Ok("http://preset.example/v1")
        );
        assert!(std::env::var_os("CLAW_RESILIENCE").is_none());

        std::env::remove_var("OPENAI_BASE_URL");
        std::env::remove_var("OPENAI_API_KEY");
    }

    #[test]
    fn run_subagent_output_truncates_summary() {
        let text = "abcdefghij".repeat(10); // 100 chars
        let (summary, truncated) = truncate_summary(&text, 20);
        assert!(truncated);
        assert!(summary.starts_with(&"abcdefghij".repeat(2)));
        assert!(summary.contains("truncated"));

        let (short, truncated_short) = truncate_summary("hello", 20);
        assert!(!truncated_short);
        assert_eq!(short, "hello");
    }

    #[test]
    fn list_presets_parses_dir() {
        let _guard = env_guard();
        let root = unique_dir("presets-root");
        let presets_dir = root.join("scripts/presets");
        std::fs::create_dir_all(&presets_dir).expect("mkdir presets");
        std::fs::write(
            presets_dir.join("dev-coder.json"),
            r#"{"preset_name":"dev-coder","description":"Local coder","provider":"lmstudio","model":"qwen-coder"}"#,
        )
        .expect("write preset");
        std::fs::write(presets_dir.join("not-json.txt"), "ignore me").expect("write noise");

        // Point repo-root resolution at our temp dir; clear HOME so the
        // ~/.lmcode path does not contribute.
        let prev_root = std::env::var("CLAW_CODE_ROOT").ok();
        let prev_home = std::env::var("HOME").ok();
        std::env::set_var("CLAW_CODE_ROOT", &root);
        std::env::remove_var("HOME");

        let presets = list_presets();

        match prev_root {
            Some(value) => std::env::set_var("CLAW_CODE_ROOT", value),
            None => std::env::remove_var("CLAW_CODE_ROOT"),
        }
        match prev_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        let _ = std::fs::remove_dir_all(&root);

        assert_eq!(presets.len(), 1, "only the one json preset is parsed");
        let preset = &presets[0];
        assert_eq!(preset.name, "dev-coder");
        assert_eq!(preset.description, "Local coder");
        assert_eq!(preset.provider, "lmstudio");
        assert_eq!(preset.model, "qwen-coder");
    }

    #[test]
    fn run_subagent_tool_spec_requires_prompt() {
        let spec = run_subagent_tool_spec();
        assert_eq!(spec.name, "run_subagent");
        let required = spec.input_schema["required"]
            .as_array()
            .expect("required array");
        assert!(required.iter().any(|value| value == "prompt"));
        assert_eq!(spec.input_schema["additionalProperties"], json!(false));
    }

    #[test]
    fn list_presets_tool_spec_has_no_required_inputs() {
        let spec = list_presets_tool_spec();
        assert_eq!(spec.name, "list_presets");
        assert_eq!(spec.input_schema["additionalProperties"], json!(false));
    }

    #[test]
    fn handle_subagent_mcp_call_unknown_tool_errors() {
        let result = handle_subagent_mcp_call("nope", &json!({}));
        assert!(result.is_err());
    }

    #[test]
    fn handle_subagent_mcp_call_run_subagent_returns_structured_failure() {
        // Empty prompt -> Ok(json) with status:"failed" (not an Err).
        let result =
            handle_subagent_mcp_call("run_subagent", &json!({ "prompt": "" })).expect("ok json");
        let value: Value = serde_json::from_str(&result).expect("valid json");
        assert_eq!(value["status"], "failed");
    }

    #[test]
    fn handle_subagent_mcp_call_list_presets_returns_array() {
        let result = handle_subagent_mcp_call("list_presets", &json!({})).expect("ok json");
        let value: Value = serde_json::from_str(&result).expect("valid json");
        assert!(value.is_array());
    }

    #[test]
    fn parse_permission_mode_defaults_to_workspace_write() {
        assert_eq!(parse_permission_mode(None), PermissionMode::WorkspaceWrite);
        assert_eq!(
            parse_permission_mode(Some("garbage")),
            PermissionMode::WorkspaceWrite
        );
        assert_eq!(
            parse_permission_mode(Some("read-only")),
            PermissionMode::ReadOnly
        );
        assert_eq!(
            parse_permission_mode(Some("danger-full-access")),
            PermissionMode::DangerFullAccess
        );
    }

    #[test]
    fn parse_launcher_task_dir_reads_status_path() {
        let stdout = "task_id=abc-123\n/tmp/claw-runs/abc-123/status.json\n/tmp/claw-runs/abc-123/diff.patch\n/tmp/claw-runs/abc-123/summary.md\n";
        let dir = parse_launcher_task_dir(stdout).expect("task dir");
        assert_eq!(dir, PathBuf::from("/tmp/claw-runs/abc-123"));
    }
}
