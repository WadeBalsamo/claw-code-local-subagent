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
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use runtime::permission_enforcer::PermissionEnforcer;
use runtime::{ConversationRuntime, PermissionMode, Session, ToolError, ToolExecutor};

use crate::{
    agent_permission_policy_for_mode, allowed_tools_for_subagent, build_agent_system_prompt,
    final_assistant_text, normalize_subagent_type, resolve_agent_model, ProviderRuntimeClient,
    SubagentToolExecutor, ToolSpec, DEFAULT_AGENT_MAX_ITERATIONS, DEFAULT_AGENT_MODEL,
};

/// Default cap on the returned `summary` text, in characters.
const DEFAULT_MAX_OUTPUT_CHARS: usize = 4000;
const DEFAULT_STALE_AFTER_SECS: u64 = 300;
const DEFAULT_STOP_GRACE_SECS: u64 = 2;
const HEARTBEAT_INTERVAL_SECS: u64 = 30;
const HEARTBEAT_STOP_POLL_MS: u64 = 100;

/// Input for the `run_subagent` MCP tool.
#[derive(Debug, Clone, Deserialize, Serialize)]
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
    /// Review effort/depth hint for review-style prompts: quick|standard|deep|exhaustive.
    #[serde(default)]
    pub review_depth: Option<String>,
    /// Comma-separated review focus areas, e.g. correctness,tests,security,scope.
    #[serde(default)]
    pub focus: Option<String>,
    /// Review artifact scope hint: diff_only|diff_plus_tests|full_repo_context.
    #[serde(default)]
    pub artifact_scope: Option<String>,
    /// Ask the reviewer to stop once it finds one concrete blocker.
    #[serde(default)]
    pub stop_on_first_blocker: Option<bool>,
    /// Ask the reviewer to cite concrete evidence for findings.
    #[serde(default)]
    pub require_evidence: Option<bool>,
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

/// Input for starting a pollable background sub-agent run.
pub type StartSubagentInput = RunSubagentInput;

/// Input for reading a background sub-agent run status file.
#[derive(Debug, Clone, Deserialize)]
pub struct GetSubagentInput {
    /// Run id returned by `start_subagent`.
    #[serde(default)]
    pub run_id: Option<String>,
    /// Optional explicit status file path returned by `start_subagent`.
    #[serde(default)]
    pub status_file: Option<String>,
    /// Limit returned activity events. Defaults to all events.
    #[serde(default)]
    pub activity_limit: Option<usize>,
    /// Limit returned JSONL status events. Defaults to all events.
    #[serde(default)]
    pub event_limit: Option<usize>,
    /// Return only JSONL events with seq greater than this value.
    #[serde(default)]
    pub since_seq: Option<usize>,
    /// Mark active runs stale when no activity has occurred for this many seconds.
    #[serde(default)]
    pub stale_after_secs: Option<u64>,
}

/// Input for cancelling a background sub-agent run.
#[derive(Debug, Clone, Deserialize)]
pub struct StopSubagentInput {
    /// Run id returned by `start_subagent`.
    #[serde(default)]
    pub run_id: Option<String>,
    /// Optional explicit status file path returned by `start_subagent`.
    #[serde(default)]
    pub status_file: Option<String>,
    /// Seconds to wait after graceful termination before force-killing.
    #[serde(default)]
    pub grace_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartSubagentOutput {
    pub status: String,
    pub run_id: String,
    pub provider: String,
    pub model: String,
    pub repo_dir: String,
    pub status_file: String,
    pub input_file: String,
    pub stdout_file: String,
    pub stderr_file: String,
    pub events_file: String,
    pub status_command: String,
    pub stop_command: String,
    pub pid: Option<u32>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentActivity {
    pub seq: usize,
    pub tool_name: String,
    pub status: String,
    pub input: Value,
    pub observed_target: Option<String>,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub is_error: Option<bool>,
    pub output_chars: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentEvent {
    pub seq: usize,
    pub timestamp: String,
    pub kind: String,
    pub phase: Option<String>,
    pub status: Option<String>,
    pub message: Option<String>,
    pub tool_name: Option<String>,
    pub observed_target: Option<String>,
    pub input: Option<Value>,
    pub is_error: Option<bool>,
    pub output_chars: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentStatusRecord {
    pub status: String,
    #[serde(default)]
    pub phase: Option<String>,
    pub run_id: String,
    pub provider: String,
    pub model: String,
    pub repo_dir: String,
    pub status_file: String,
    #[serde(default)]
    pub stdout_file: Option<String>,
    #[serde(default)]
    pub stderr_file: Option<String>,
    #[serde(default)]
    pub events_file: Option<String>,
    #[serde(default)]
    pub pid: Option<u32>,
    pub started_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub last_activity_at: Option<String>,
    #[serde(default)]
    pub heartbeat_at: Option<String>,
    pub completed_at: Option<String>,
    pub summary: String,
    pub truncated: bool,
    pub diff_stat: Option<String>,
    pub error: Option<String>,
    pub duration_ms: u64,
    #[serde(default)]
    pub event_seq: usize,
    pub activity: Vec<SubagentActivity>,
    pub read_paths: Vec<String>,
    #[serde(default)]
    pub grep_patterns: Vec<String>,
    #[serde(default)]
    pub web_queries: Vec<String>,
    #[serde(default)]
    pub stop_command: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SubagentWorkerInput {
    run_id: String,
    status_file: String,
    input: RunSubagentInput,
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
                "preset": { "type": "string", "description": "Preset name for isolated runs." },
                "review_depth": {
                    "type": "string",
                    "enum": ["quick", "standard", "deep", "exhaustive"],
                    "description": "Review-depth hint for review prompts. Shapes instructions, not runtime internals."
                },
                "focus": { "type": "string", "description": "Comma-separated review focus areas such as correctness,tests,security,scope." },
                "artifact_scope": {
                    "type": "string",
                    "enum": ["diff_only", "diff_plus_tests", "full_repo_context"],
                    "description": "Review scope hint for how broadly to inspect artifacts."
                },
                "stop_on_first_blocker": { "type": "boolean", "description": "Ask a reviewer to stop after one concrete blocker." },
                "require_evidence": { "type": "boolean", "description": "Ask a reviewer to cite concrete evidence for findings." }
            },
            "required": ["prompt"],
            "additionalProperties": false
        }),
        required_permission: PermissionMode::WorkspaceWrite,
    }
}

/// JSON schema for the `start_subagent` tool. Same input as `run_subagent`,
/// but it returns immediately with a run id and status file.
#[must_use]
pub fn start_subagent_tool_spec() -> ToolSpec {
    let mut spec = run_subagent_tool_spec();
    spec.name = "start_subagent";
    spec.description = "Start a pollable local or OpenRouter sub-agent run and return a run_id plus status_file. Use get_subagent to poll status, final result, and observed read/search tool activity.";
    spec
}

/// JSON schema for the `get_subagent` tool.
#[must_use]
pub fn get_subagent_tool_spec() -> ToolSpec {
    ToolSpec {
        name: "get_subagent",
        description: "Read the status for a pollable sub-agent run started by start_subagent, including liveness, phase, staleness, recent events, read paths, and tool activity.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "run_id": { "type": "string", "description": "Run id returned by start_subagent." },
                "status_file": { "type": "string", "description": "Explicit status file path returned by start_subagent." },
                "activity_limit": { "type": "integer", "minimum": 1, "description": "Limit returned activity events." },
                "event_limit": { "type": "integer", "minimum": 1, "description": "Limit returned JSONL status events." },
                "since_seq": { "type": "integer", "minimum": 0, "description": "Return only JSONL status events with seq greater than this value." },
                "stale_after_secs": { "type": "integer", "minimum": 1, "description": "Mark active runs stale after this many seconds without activity." }
            },
            "additionalProperties": false
        }),
        required_permission: PermissionMode::ReadOnly,
    }
}

/// JSON schema for the `stop_subagent` tool.
#[must_use]
pub fn stop_subagent_tool_spec() -> ToolSpec {
    ToolSpec {
        name: "stop_subagent",
        description: "Cancel a pollable sub-agent run started by start_subagent. Sends graceful termination, then force-kills after a short grace period.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "run_id": { "type": "string", "description": "Run id returned by start_subagent." },
                "status_file": { "type": "string", "description": "Explicit status file path returned by start_subagent." },
                "grace_secs": { "type": "integer", "minimum": 0, "description": "Seconds to wait before force-killing after graceful termination." }
            },
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
        description: "List the available sub-agent presets (name, description, provider, model, tier_ref, resource) discoverable from the repo's scripts/presets and ~/.lmcode/presets. `tier_ref` keys into config/model_tiers.json for the hybrid local->OpenRouter ladder; `resource` is the broker slot (gpu/cpu/gpu+cpu/remote).",
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
        "start_subagent" => {
            let input: StartSubagentInput = serde_json::from_value(args.clone())
                .map_err(|error| format!("invalid start_subagent input: {error}"))?;
            let output = start_subagent(input);
            serde_json::to_string_pretty(&output)
                .map_err(|error| format!("failed to serialize start_subagent output: {error}"))
        }
        "get_subagent" => {
            let input: GetSubagentInput = serde_json::from_value(args.clone())
                .map_err(|error| format!("invalid get_subagent input: {error}"))?;
            let output = get_subagent(input);
            serde_json::to_string_pretty(&output)
                .map_err(|error| format!("failed to serialize get_subagent output: {error}"))
        }
        "stop_subagent" => {
            let input: StopSubagentInput = serde_json::from_value(args.clone())
                .map_err(|error| format!("invalid stop_subagent input: {error}"))?;
            let output = stop_subagent(input);
            serde_json::to_string_pretty(&output)
                .map_err(|error| format!("failed to serialize stop_subagent output: {error}"))
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

/// Start a background sub-agent run that can be polled with [`get_subagent`].
#[must_use]
pub fn start_subagent(input: StartSubagentInput) -> StartSubagentOutput {
    let provider = input
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("auto")
        .to_lowercase();
    let model = resolve_agent_model(input.model.as_deref());
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
    let run_id = make_subagent_run_id();
    let run_dir = subagent_run_dir(&run_id);
    let status_file = run_dir.join("status.json");
    let input_file = run_dir.join("input.json");
    let stdout_file = run_dir.join("worker.stdout.log");
    let stderr_file = run_dir.join("worker.stderr.log");
    let events_file = run_dir.join("events.jsonl");
    let status_command = status_command_for_file(&status_file, &subagent_run_root());
    let stop_command = stop_command_for_file(&status_file, &subagent_run_root());

    let failed = |error: String| StartSubagentOutput {
        status: "failed".to_string(),
        run_id: run_id.clone(),
        provider: provider.clone(),
        model: model.clone(),
        repo_dir: repo_dir.clone(),
        status_file: status_file.display().to_string(),
        input_file: input_file.display().to_string(),
        stdout_file: stdout_file.display().to_string(),
        stderr_file: stderr_file.display().to_string(),
        events_file: events_file.display().to_string(),
        status_command: status_command.clone(),
        stop_command: stop_command.clone(),
        pid: None,
        error: Some(error),
    };

    if input.prompt.trim().is_empty() {
        return failed("prompt must not be empty".to_string());
    }
    if let Err(error) = fs::create_dir_all(&run_dir) {
        return failed(format!(
            "failed to create run dir {}: {error}",
            run_dir.display()
        ));
    }

    let now = timestamp_now();
    let record = SubagentStatusRecord {
        status: "starting".to_string(),
        phase: Some("queued".to_string()),
        run_id: run_id.clone(),
        provider: provider.clone(),
        model: model.clone(),
        repo_dir: repo_dir.clone(),
        status_file: status_file.display().to_string(),
        stdout_file: Some(stdout_file.display().to_string()),
        stderr_file: Some(stderr_file.display().to_string()),
        events_file: Some(events_file.display().to_string()),
        pid: None,
        started_at: now.clone(),
        updated_at: now.clone(),
        last_activity_at: Some(now.clone()),
        heartbeat_at: Some(now),
        completed_at: None,
        summary: String::new(),
        truncated: false,
        diff_stat: None,
        error: None,
        duration_ms: 0,
        event_seq: 0,
        activity: Vec::new(),
        read_paths: Vec::new(),
        grep_patterns: Vec::new(),
        web_queries: Vec::new(),
        stop_command: Some(stop_command.clone()),
    };
    if let Err(error) = write_status_record(&status_file, &record) {
        return failed(error);
    }
    let _ = record_status_event(
        &status_file,
        "phase_started",
        Some("queued"),
        Some("sub-agent run queued"),
        None,
    );

    let worker_input = SubagentWorkerInput {
        run_id: run_id.clone(),
        status_file: status_file.display().to_string(),
        input,
    };
    let serialized = match serde_json::to_string_pretty(&worker_input) {
        Ok(serialized) => serialized,
        Err(error) => return failed(format!("failed to serialize worker input: {error}")),
    };
    if let Err(error) = fs::write(&input_file, serialized) {
        return failed(format!(
            "failed to write worker input {}: {error}",
            input_file.display()
        ));
    }

    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => return failed(format!("failed to resolve current executable: {error}")),
    };
    match spawn_subagent_worker(
        &exe,
        &input_file,
        &stdout_file,
        &stderr_file,
        &status_file,
        &repo_dir,
    ) {
        Ok(pid) => {
            let _ = update_status_file(&status_file, |record| {
                record.pid = Some(pid);
                record.updated_at = timestamp_now();
            });
            StartSubagentOutput {
                status: "started".to_string(),
                run_id,
                provider,
                model,
                repo_dir,
                status_file: status_file.display().to_string(),
                input_file: input_file.display().to_string(),
                stdout_file: stdout_file.display().to_string(),
                stderr_file: stderr_file.display().to_string(),
                events_file: events_file.display().to_string(),
                status_command,
                stop_command,
                pid: Some(pid),
                error: None,
            }
        }
        Err(error) => {
            let _ = update_status_file(&status_file, |record| {
                record.status = "failed".to_string();
                record.error = Some(error.clone());
                record.completed_at = Some(timestamp_now());
            });
            failed(error)
        }
    }
}

/// Return the persisted status for a background sub-agent run.
#[must_use]
pub fn get_subagent(input: GetSubagentInput) -> Value {
    let status_path = match resolve_status_path(&input) {
        Ok(path) => path,
        Err(error) => {
            return json!({
                "status": "failed",
                "error": error,
            });
        }
    };
    let raw = match fs::read_to_string(&status_path) {
        Ok(raw) => raw,
        Err(error) => {
            return json!({
                "status": "missing",
                "status_file": status_path.display().to_string(),
                "error": format!("failed to read status file: {error}"),
            });
        }
    };
    let mut record: SubagentStatusRecord = match serde_json::from_str(&raw) {
        Ok(record) => record,
        Err(error) => {
            return json!({
                "status": "failed",
                "status_file": status_path.display().to_string(),
                "error": format!("failed to parse status file: {error}"),
            });
        }
    };
    let worker_alive = record
        .pid
        .filter(|_| is_active_subagent_status(&record.status))
        .and_then(process_is_alive);
    if worker_alive == Some(false) && is_active_subagent_status(&record.status) {
        let now = timestamp_now();
        record.status = "failed".to_string();
        record.phase = Some("failed".to_string());
        record.error = Some(
            "sub-agent worker exited before writing a terminal status; see stderr_file/stdout_file"
                .to_string(),
        );
        record.duration_ms = elapsed_ms_since_timestamp(&record.started_at);
        record.diff_stat = git_diff_stat(&record.repo_dir);
        record.completed_at = Some(now.clone());
        record.updated_at = now;
        let _ = write_status_record(&status_path, &record);
        let _ = record_status_event(
            &status_path,
            "terminal",
            Some("failed"),
            Some("sub-agent worker exited before terminal status"),
            None,
        );
    }
    let stale_after_secs = input.stale_after_secs.unwrap_or(DEFAULT_STALE_AFTER_SECS);
    let stale = is_active_subagent_status(&record.status)
        && millis_since_optional(record.last_activity_at.as_deref())
            .is_some_and(|elapsed| elapsed >= u128::from(stale_after_secs) * 1000);
    let stale_reason = stale.then(|| {
        format!(
            "no activity for at least {stale_after_secs}s; use stop_subagent/claw subagent stop to cancel if it is frozen"
        )
    });
    let event_count = record.event_seq;
    let events = read_status_events(&record, input.since_seq, input.event_limit);
    if let Some(limit) = input.activity_limit {
        if record.activity.len() > limit {
            let keep_from = record.activity.len() - limit;
            record.activity = record.activity.split_off(keep_from);
        }
    }
    let mut value = serde_json::to_value(record).expect("subagent status should serialize");
    if let Value::Object(object) = &mut value {
        object.insert(
            "worker_alive".to_string(),
            worker_alive.map_or(Value::Null, Value::Bool),
        );
        object.insert("stale".to_string(), Value::Bool(stale));
        object.insert(
            "stale_after_secs".to_string(),
            Value::from(stale_after_secs),
        );
        object.insert(
            "stale_reason".to_string(),
            stale_reason.map_or(Value::Null, Value::String),
        );
        object.insert("event_count".to_string(), Value::from(event_count));
        object.insert(
            "events".to_string(),
            serde_json::to_value(events).unwrap_or_else(|_| Value::Array(Vec::new())),
        );
    }
    value
}

/// Cancel a pollable sub-agent run.
#[must_use]
pub fn stop_subagent(input: StopSubagentInput) -> Value {
    let status_path = match resolve_stop_status_path(&input) {
        Ok(path) => path,
        Err(error) => {
            return json!({
                "status": "failed",
                "error": error,
            });
        }
    };
    let raw = match fs::read_to_string(&status_path) {
        Ok(raw) => raw,
        Err(error) => {
            return json!({
                "status": "missing",
                "status_file": status_path.display().to_string(),
                "error": format!("failed to read status file: {error}"),
            });
        }
    };
    let record: SubagentStatusRecord = match serde_json::from_str(&raw) {
        Ok(record) => record,
        Err(error) => {
            return json!({
                "status": "failed",
                "status_file": status_path.display().to_string(),
                "error": format!("failed to parse status file: {error}"),
            });
        }
    };
    if !is_active_subagent_status(&record.status) {
        return json!({
            "status": record.status,
            "status_file": status_path.display().to_string(),
            "message": "sub-agent run is already terminal",
        });
    }

    let grace_secs = input.grace_secs.unwrap_or(DEFAULT_STOP_GRACE_SECS);
    let mut termination_error = None;
    let mut force_killed = false;
    if let Some(pid) = record.pid {
        if process_is_alive(pid) == Some(true) {
            if let Err(error) = terminate_process(pid, false) {
                termination_error = Some(error);
            }
            if grace_secs > 0 {
                std::thread::sleep(Duration::from_secs(grace_secs));
            }
            if process_is_alive(pid) == Some(true) {
                force_killed = true;
                if let Err(error) = terminate_process(pid, true) {
                    termination_error = Some(error);
                }
            }
        }
    }

    let now = timestamp_now();
    let message = if let Some(error) = termination_error {
        format!("cancel requested, but process termination reported: {error}")
    } else if force_killed {
        "cancelled by parent agent after force-kill".to_string()
    } else {
        "cancelled by parent agent".to_string()
    };
    let _ = update_status_file(&status_path, |record| {
        record.status = "cancelled".to_string();
        record.phase = Some("cancelled".to_string());
        record.error = Some(message.clone());
        record.duration_ms = elapsed_ms_since_timestamp(&record.started_at);
        record.diff_stat = git_diff_stat(&record.repo_dir);
        record.completed_at = Some(now.clone());
        record.updated_at = now.clone();
        record.last_activity_at = Some(now.clone());
        record.heartbeat_at = Some(now.clone());
    });
    let _ = record_status_event(
        &status_path,
        "terminal",
        Some("cancelled"),
        Some(&message),
        None,
    );

    json!({
        "status": "cancelled",
        "status_file": status_path.display().to_string(),
        "message": message,
        "force_killed": force_killed,
    })
}

/// Execute a status-file-backed sub-agent worker. Intended for the hidden
/// `claw subagent run-worker --input-file <path>` CLI entrypoint.
pub fn run_subagent_worker_from_file(input_file: &Path) -> Result<(), String> {
    let raw = fs::read_to_string(input_file).map_err(|error| {
        format!(
            "failed to read worker input {}: {error}",
            input_file.display()
        )
    })?;
    let worker: SubagentWorkerInput = serde_json::from_str(&raw).map_err(|error| {
        format!(
            "failed to parse worker input {}: {error}",
            input_file.display()
        )
    })?;
    let status_path = PathBuf::from(&worker.status_file);
    run_observed_subagent(worker.input, worker.run_id, status_path);
    Ok(())
}

fn run_observed_subagent(input: RunSubagentInput, run_id: String, status_path: PathBuf) {
    let started = Instant::now();
    let provider = input
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("auto")
        .to_lowercase();
    let model = resolve_agent_model(input.model.as_deref());
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

    let _ = update_status_file(&status_path, |record| {
        record.status = "running".to_string();
        record.phase = Some("initializing".to_string());
        let now = timestamp_now();
        record.updated_at = now.clone();
        record.last_activity_at = Some(now.clone());
        record.heartbeat_at = Some(now);
    });
    let _ = record_status_event(
        &status_path,
        "phase_started",
        Some("initializing"),
        Some("sub-agent worker started"),
        None,
    );

    if input.isolated.unwrap_or(false) {
        let _ = record_status_event(
            &status_path,
            "phase_started",
            Some("isolated_launcher"),
            Some("starting isolated launcher"),
            None,
        );
        let effective_input = input_with_review_controls(&input);
        let output = run_subagent_isolated(&effective_input, &provider, &model, &repo_dir);
        let status = output.status.clone();
        let phase = if status == "completed" {
            "completed".to_string()
        } else {
            status.clone()
        };
        let _ = update_status_file(&status_path, |record| {
            record.status = status;
            record.phase = Some(phase.clone());
            record.summary = output.summary;
            record.truncated = output.truncated;
            record.error = output.error;
            record.duration_ms = elapsed_ms(started);
            record.diff_stat = output.diff_stat;
            let now = timestamp_now();
            record.completed_at = Some(now.clone());
            record.updated_at = now.clone();
            record.last_activity_at = Some(now.clone());
            record.heartbeat_at = Some(now);
        });
        let _ = record_status_event(
            &status_path,
            "terminal",
            Some(&phase),
            Some("isolated launcher finished"),
            None,
        );
        let _ = run_id;
        return;
    }

    let _env_guard = apply_provider_env(&provider, &model);
    let _cwd_guard = match CwdGuard::enter(input.repo_dir.as_deref()) {
        Ok(guard) => guard,
        Err(error) => {
            let _ = mark_status_terminal(
                &status_path,
                "failed",
                None,
                Some(error),
                elapsed_ms(started),
                &repo_dir,
            );
            return;
        }
    };

    let status_record = match fs::read_to_string(&status_path)
        .ok()
        .and_then(|raw| serde_json::from_str::<SubagentStatusRecord>(&raw).ok())
    {
        Some(record) => Arc::new(Mutex::new(record)),
        None => {
            let _ = mark_status_terminal(
                &status_path,
                "failed",
                None,
                Some("status file disappeared before worker started".to_string()),
                elapsed_ms(started),
                &repo_dir,
            );
            return;
        }
    };

    let subagent_type = normalize_subagent_type(input.subagent_type.as_deref());
    let permission_mode = parse_permission_mode(input.permission_mode.as_deref());
    let allowed_tools = allowed_tools_for_subagent(&subagent_type);
    let max_output_chars = input.max_output_chars.unwrap_or(DEFAULT_MAX_OUTPUT_CHARS);
    let timeout = input.timeout_secs.map(Duration::from_secs);
    let prompt = prompt_with_review_controls(&input);
    let max_iterations = max_iterations_for_input(&input);
    let heartbeat_stop = Arc::new(AtomicBool::new(false));
    let heartbeat = spawn_status_heartbeat(
        Arc::clone(&status_record),
        status_path.clone(),
        Arc::clone(&heartbeat_stop),
    );

    let run = {
        let status_record = Arc::clone(&status_record);
        let status_path = status_path.clone();
        let model = model.clone();
        move || -> Result<String, String> {
            {
                let mut record = lock_status(&status_record);
                record.phase = Some("model_turn".to_string());
                let event = status_event(
                    &mut record,
                    "phase_started",
                    Some("model_turn"),
                    Some("starting model/tool loop"),
                    None,
                );
                let _ = write_status_record(&status_path, &record);
                let _ = append_event_for_record(&record, &event);
            }
            let system_prompt = build_agent_system_prompt(&subagent_type, &model)?;
            let api_client = ProviderRuntimeClient::new(model.clone(), allowed_tools.clone())?;
            let permission_policy = agent_permission_policy_for_mode(permission_mode);
            let inner = SubagentToolExecutor::new(allowed_tools)
                .with_enforcer(PermissionEnforcer::new(permission_policy.clone()));
            let tool_executor = ObservedToolExecutor {
                inner,
                status_record,
                status_path,
            };
            let mut runtime = ConversationRuntime::new(
                Session::new(),
                api_client,
                tool_executor,
                permission_policy,
                system_prompt,
            )
            .with_max_iterations(max_iterations);
            let summary = runtime
                .run_turn(prompt, None)
                .map_err(|error| error.to_string())?;
            Ok(final_assistant_text(&summary))
        }
    };

    match run_with_optional_timeout(run, timeout) {
        TurnResult::Completed(text) => {
            heartbeat_stop.store(true, Ordering::SeqCst);
            if let Some(handle) = heartbeat {
                let _ = handle.join();
            }
            let (summary, truncated) = truncate_summary(&text, max_output_chars);
            let _ = update_status_file(&status_path, |record| {
                record.status = "completed".to_string();
                record.phase = Some("completed".to_string());
                record.summary = summary;
                record.truncated = truncated;
                record.error = None;
                record.duration_ms = elapsed_ms(started);
                record.diff_stat = git_diff_stat(&repo_dir);
                let now = timestamp_now();
                record.completed_at = Some(now.clone());
                record.updated_at = now.clone();
                record.last_activity_at = Some(now.clone());
                record.heartbeat_at = Some(now);
            });
            let _ = record_status_event(
                &status_path,
                "terminal",
                Some("completed"),
                Some("sub-agent completed"),
                None,
            );
        }
        TurnResult::Failed(error) => {
            heartbeat_stop.store(true, Ordering::SeqCst);
            if let Some(handle) = heartbeat {
                let _ = handle.join();
            }
            let _ = mark_status_terminal(
                &status_path,
                "failed",
                None,
                Some(error),
                elapsed_ms(started),
                &repo_dir,
            );
        }
        TurnResult::TimedOut => {
            heartbeat_stop.store(true, Ordering::SeqCst);
            if let Some(handle) = heartbeat {
                let _ = handle.join();
            }
            let _ = mark_status_terminal(
                &status_path,
                "timeout",
                None,
                Some(
                    "sub-agent turn exceeded timeout; the worker process marked the run timed out"
                        .to_string(),
                ),
                elapsed_ms(started),
                &repo_dir,
            );
        }
    }

    let _ = run_id;
}

struct ObservedToolExecutor {
    inner: SubagentToolExecutor,
    status_record: Arc<Mutex<SubagentStatusRecord>>,
    status_path: PathBuf,
}

impl ToolExecutor for ObservedToolExecutor {
    fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        let input_value = serde_json::from_str::<Value>(input)
            .unwrap_or_else(|_| Value::String(input.to_string()));
        let observed_target = observed_tool_target(tool_name, &input_value);
        let seq = {
            let mut record = lock_status(&self.status_record);
            let seq = record.activity.len() + 1;
            record.phase = Some(format!("tool:{tool_name}"));
            record.activity.push(SubagentActivity {
                seq,
                tool_name: tool_name.to_string(),
                status: "running".to_string(),
                input: input_value.clone(),
                observed_target: observed_target.clone(),
                started_at: timestamp_now(),
                finished_at: None,
                is_error: None,
                output_chars: None,
            });
            if tool_name == "read_file" {
                if let Some(path) = observed_target.as_deref() {
                    if !record.read_paths.iter().any(|existing| existing == path) {
                        record.read_paths.push(path.to_string());
                    }
                }
            }
            match tool_name {
                "grep_search" => {
                    if let Some(pattern) = observed_target.as_deref() {
                        if !record
                            .grep_patterns
                            .iter()
                            .any(|existing| existing == pattern)
                        {
                            record.grep_patterns.push(pattern.to_string());
                        }
                    }
                }
                "WebSearch" => {
                    if let Some(query) = observed_target.as_deref() {
                        if !record.web_queries.iter().any(|existing| existing == query) {
                            record.web_queries.push(query.to_string());
                        }
                    }
                }
                _ => {}
            }
            let event = status_event(
                &mut record,
                "tool_started",
                Some(&format!("tool:{tool_name}")),
                Some("tool call started"),
                Some(ToolEventPayload {
                    tool_name,
                    status: "running",
                    input: Some(input_value.clone()),
                    observed_target: observed_target.clone(),
                    is_error: None,
                    output_chars: None,
                }),
            );
            let _ = write_status_record(&self.status_path, &record);
            let _ = append_event_for_record(&record, &event);
            seq
        };

        let result = self.inner.execute(tool_name, input);
        let (is_error, output_chars) = match &result {
            Ok(output) => (false, output.chars().count()),
            Err(error) => (true, error.to_string().chars().count()),
        };
        {
            let mut record = lock_status(&self.status_record);
            if let Some(activity) = record
                .activity
                .iter_mut()
                .find(|activity| activity.seq == seq)
            {
                activity.status = if is_error { "failed" } else { "completed" }.to_string();
                activity.finished_at = Some(timestamp_now());
                activity.is_error = Some(is_error);
                activity.output_chars = Some(output_chars);
            }
            let event = status_event(
                &mut record,
                "tool_finished",
                Some(&format!("tool:{tool_name}")),
                Some("tool call finished"),
                Some(ToolEventPayload {
                    tool_name,
                    status: if is_error { "failed" } else { "completed" },
                    input: None,
                    observed_target,
                    is_error: Some(is_error),
                    output_chars: Some(output_chars),
                }),
            );
            let _ = write_status_record(&self.status_path, &record);
            let _ = append_event_for_record(&record, &event);
        }
        result
    }
}

fn lock_status(
    status_record: &Arc<Mutex<SubagentStatusRecord>>,
) -> std::sync::MutexGuard<'_, SubagentStatusRecord> {
    status_record
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn observed_tool_target(tool_name: &str, input: &Value) -> Option<String> {
    let field = match tool_name {
        "read_file" => "path",
        "glob_search" => "pattern",
        "grep_search" => "pattern",
        "WebFetch" => "url",
        "WebSearch" => "query",
        "Skill" => "skill",
        _ => return None,
    };
    input
        .get(field)
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

struct ToolEventPayload<'a> {
    tool_name: &'a str,
    status: &'a str,
    input: Option<Value>,
    observed_target: Option<String>,
    is_error: Option<bool>,
    output_chars: Option<usize>,
}

fn status_event(
    record: &mut SubagentStatusRecord,
    kind: &str,
    phase: Option<&str>,
    message: Option<&str>,
    tool: Option<ToolEventPayload<'_>>,
) -> SubagentEvent {
    let now = timestamp_now();
    record.event_seq += 1;
    record.updated_at = now.clone();
    record.last_activity_at = Some(now.clone());
    record.heartbeat_at = Some(now.clone());
    if let Some(phase) = phase {
        record.phase = Some(phase.to_string());
    }
    let (tool_name, status, input, observed_target, is_error, output_chars) = match tool {
        Some(tool) => (
            Some(tool.tool_name.to_string()),
            Some(tool.status.to_string()),
            tool.input,
            tool.observed_target,
            tool.is_error,
            tool.output_chars,
        ),
        None => (None, None, None, None, None, None),
    };
    SubagentEvent {
        seq: record.event_seq,
        timestamp: now,
        kind: kind.to_string(),
        phase: phase
            .map(ToString::to_string)
            .or_else(|| record.phase.clone()),
        status,
        message: message.map(ToString::to_string),
        tool_name,
        observed_target,
        input,
        is_error,
        output_chars,
    }
}

fn record_status_event(
    status_path: &Path,
    kind: &str,
    phase: Option<&str>,
    message: Option<&str>,
    tool: Option<ToolEventPayload<'_>>,
) -> Result<(), String> {
    let raw = fs::read_to_string(status_path).map_err(|error| {
        format!(
            "failed to read status file {}: {error}",
            status_path.display()
        )
    })?;
    let mut record: SubagentStatusRecord = serde_json::from_str(&raw).map_err(|error| {
        format!(
            "failed to parse status file {}: {error}",
            status_path.display()
        )
    })?;
    let event = status_event(&mut record, kind, phase, message, tool);
    write_status_record(status_path, &record)?;
    append_event_for_record(&record, &event)
}

fn append_event_for_record(
    record: &SubagentStatusRecord,
    event: &SubagentEvent,
) -> Result<(), String> {
    let Some(path) = record.events_file.as_deref() else {
        return Ok(());
    };
    let path = PathBuf::from(path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!("failed to create events dir {}: {error}", parent.display())
        })?;
    }
    let mut payload = serde_json::to_string(event)
        .map_err(|error| format!("failed to serialize sub-agent event: {error}"))?;
    payload.push('\n');
    use std::io::Write as _;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| format!("failed to open events file {}: {error}", path.display()))?;
    file.write_all(payload.as_bytes())
        .map_err(|error| format!("failed to append events file {}: {error}", path.display()))
}

fn read_status_events(
    record: &SubagentStatusRecord,
    since_seq: Option<usize>,
    limit: Option<usize>,
) -> Vec<SubagentEvent> {
    let Some(path) = record.events_file.as_deref() else {
        return Vec::new();
    };
    let Ok(raw) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let min_seq = since_seq.unwrap_or_default();
    let mut events = raw
        .lines()
        .filter_map(|line| serde_json::from_str::<SubagentEvent>(line).ok())
        .filter(|event| event.seq > min_seq)
        .collect::<Vec<_>>();
    if let Some(limit) = limit {
        if events.len() > limit {
            let keep_from = events.len() - limit;
            events = events.split_off(keep_from);
        }
    }
    events
}

fn spawn_status_heartbeat(
    status_record: Arc<Mutex<SubagentStatusRecord>>,
    status_path: PathBuf,
    stop: Arc<AtomicBool>,
) -> Option<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("claw-subagent-heartbeat".to_string())
        .spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                for _ in 0..(HEARTBEAT_INTERVAL_SECS * 1000 / HEARTBEAT_STOP_POLL_MS) {
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(HEARTBEAT_STOP_POLL_MS));
                }
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let mut record = lock_status(&status_record);
                if !is_active_subagent_status(&record.status) {
                    break;
                }
                record.heartbeat_at = Some(timestamp_now());
                record.updated_at = record.heartbeat_at.clone().unwrap_or_default();
                let _ = write_status_record(&status_path, &record);
            }
        })
        .ok()
}

fn mark_status_terminal(
    status_path: &Path,
    status: &str,
    summary: Option<String>,
    error: Option<String>,
    duration_ms: u64,
    repo_dir: &str,
) -> Result<(), String> {
    let result = update_status_file(status_path, |record| {
        record.status = status.to_string();
        record.phase = Some(status.to_string());
        if let Some(summary) = summary {
            record.summary = summary;
        }
        record.error = error;
        record.duration_ms = duration_ms;
        record.diff_stat = git_diff_stat(repo_dir);
        let now = timestamp_now();
        record.completed_at = Some(now.clone());
        record.updated_at = now.clone();
        record.last_activity_at = Some(now.clone());
        record.heartbeat_at = Some(now);
    });
    let _ = record_status_event(
        status_path,
        "terminal",
        Some(status),
        Some(&format!("sub-agent finished with status {status}")),
        None,
    );
    result
}

fn update_status_file(
    status_path: &Path,
    update: impl FnOnce(&mut SubagentStatusRecord),
) -> Result<(), String> {
    let raw = fs::read_to_string(status_path).map_err(|error| {
        format!(
            "failed to read status file {}: {error}",
            status_path.display()
        )
    })?;
    let mut record: SubagentStatusRecord = serde_json::from_str(&raw).map_err(|error| {
        format!(
            "failed to parse status file {}: {error}",
            status_path.display()
        )
    })?;
    update(&mut record);
    write_status_record(status_path, &record)
}

fn write_status_record(status_path: &Path, record: &SubagentStatusRecord) -> Result<(), String> {
    if let Some(parent) = status_path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!("failed to create status dir {}: {error}", parent.display())
        })?;
    }
    let tmp = status_path.with_extension("json.tmp");
    let payload = serde_json::to_string_pretty(record)
        .map_err(|error| format!("failed to serialize status record: {error}"))?;
    fs::write(&tmp, payload).map_err(|error| {
        format!(
            "failed to write status temp file {}: {error}",
            tmp.display()
        )
    })?;
    fs::rename(&tmp, status_path).map_err(|error| {
        format!(
            "failed to move status temp file {} to {}: {error}",
            tmp.display(),
            status_path.display()
        )
    })
}

fn resolve_status_path(input: &GetSubagentInput) -> Result<PathBuf, String> {
    if let Some(path) = input
        .status_file
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return resolve_explicit_status_path(path);
    }
    let run_id = input
        .run_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "get_subagent requires run_id or status_file".to_string())?;
    if !run_id
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Err(format!("invalid run_id `{run_id}`"));
    }
    Ok(subagent_run_dir(run_id).join("status.json"))
}

fn resolve_stop_status_path(input: &StopSubagentInput) -> Result<PathBuf, String> {
    resolve_status_path(&GetSubagentInput {
        run_id: input.run_id.clone(),
        status_file: input.status_file.clone(),
        activity_limit: None,
        event_limit: None,
        since_seq: None,
        stale_after_secs: None,
    })
}

fn resolve_explicit_status_path(path: &str) -> Result<PathBuf, String> {
    let status_path = PathBuf::from(path);
    if !status_path.is_absolute() {
        return Err("status_file must be an absolute path returned by start_subagent".to_string());
    }
    if status_path.file_name().and_then(|name| name.to_str()) != Some("status.json") {
        return Err("status_file must point to a sub-agent status.json file".to_string());
    }
    if has_parent_component(&status_path) {
        return Err("status_file must not contain parent-directory components".to_string());
    }

    let root = subagent_run_root();
    let root_for_check = fs::canonicalize(&root).unwrap_or(root);
    if status_path.exists() {
        let canonical_status = fs::canonicalize(&status_path).map_err(|error| {
            format!(
                "failed to canonicalize status_file {}: {error}",
                status_path.display()
            )
        })?;
        if !canonical_status.starts_with(&root_for_check) {
            return Err(format!(
                "status_file must be under {}",
                root_for_check.display()
            ));
        }
    } else if !status_path.starts_with(&root_for_check) {
        return Err(format!(
            "status_file must be under {}",
            root_for_check.display()
        ));
    }
    Ok(status_path)
}

fn has_parent_component(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, Component::ParentDir))
}

fn status_command_for_file(status_file: &Path, run_root: &Path) -> String {
    format!(
        "CLAW_SUBAGENT_RUN_DIR={} claw subagent status --status-file {}",
        shell_quote(&run_root.display().to_string()),
        shell_quote(&status_file.display().to_string())
    )
}

fn stop_command_for_file(status_file: &Path, run_root: &Path) -> String {
    format!(
        "CLAW_SUBAGENT_RUN_DIR={} claw subagent stop --status-file {}",
        shell_quote(&run_root.display().to_string()),
        shell_quote(&status_file.display().to_string())
    )
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '-' | '_' | ':' | '+'))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

fn make_subagent_run_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("subagent-{}-{nanos}", std::process::id())
}

fn subagent_run_dir(run_id: &str) -> PathBuf {
    subagent_run_root().join(run_id)
}

fn subagent_run_root() -> PathBuf {
    let root = std::env::var_os("CLAW_SUBAGENT_RUN_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("claw-subagent-runs"));
    if root.is_absolute() {
        root
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| std::env::temp_dir())
            .join(root)
    }
}

fn timestamp_now() -> String {
    format!("{}", now_millis())
}

fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

fn elapsed_ms_since_timestamp(started_at: &str) -> u64 {
    started_at
        .parse::<u128>()
        .ok()
        .map(|started| now_millis().saturating_sub(started))
        .and_then(|elapsed| u64::try_from(elapsed).ok())
        .unwrap_or_default()
}

fn millis_since_optional(started_at: Option<&str>) -> Option<u128> {
    started_at
        .and_then(|value| value.parse::<u128>().ok())
        .map(|started| now_millis().saturating_sub(started))
}

fn is_active_subagent_status(status: &str) -> bool {
    matches!(status, "starting" | "running")
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> Option<bool> {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()
        .map(|status| status.success())
}

#[cfg(windows)]
fn process_is_alive(pid: u32) -> Option<bool> {
    let output = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}")])
        .output()
        .ok()?;
    if !output.status.success() {
        return Some(false);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Some(stdout.lines().any(|line| line.contains(&pid.to_string())))
}

#[cfg(not(any(unix, windows)))]
fn process_is_alive(_pid: u32) -> Option<bool> {
    None
}

#[cfg(unix)]
fn terminate_process(pid: u32, force: bool) -> Result<(), String> {
    let signal = if force { "-9" } else { "-TERM" };
    let status = Command::new("kill")
        .arg(signal)
        .arg(pid.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| format!("failed to invoke kill for pid {pid}: {error}"))?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| format!("kill {signal} {pid} exited with {status}"))
}

#[cfg(windows)]
fn terminate_process(pid: u32, force: bool) -> Result<(), String> {
    let mut command = Command::new("taskkill");
    command.args(["/PID", &pid.to_string(), "/T"]);
    if force {
        command.arg("/F");
    }
    let status = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| format!("failed to invoke taskkill for pid {pid}: {error}"))?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| format!("taskkill for pid {pid} exited with {status}"))
}

#[cfg(not(any(unix, windows)))]
fn terminate_process(_pid: u32, _force: bool) -> Result<(), String> {
    Err("process termination is unsupported on this platform".to_string())
}

fn spawn_subagent_worker(
    exe: &Path,
    input_file: &Path,
    stdout_file: &Path,
    stderr_file: &Path,
    status_file: &Path,
    repo_dir: &str,
) -> Result<u32, String> {
    let stdout = fs::File::create(stdout_file).map_err(|error| {
        format!(
            "failed to create worker stdout log {}: {error}",
            stdout_file.display()
        )
    })?;
    let stderr = fs::File::create(stderr_file).map_err(|error| {
        format!(
            "failed to create worker stderr log {}: {error}",
            stderr_file.display()
        )
    })?;
    let mut command = Command::new(exe);
    command
        .arg("subagent")
        .arg("run-worker")
        .arg("--input-file")
        .arg(input_file)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    #[cfg(unix)]
    command.process_group(0);
    let child = command
        .spawn()
        .map_err(|error| format!("failed to spawn worker: {error}"))?;
    Ok(spawn_subagent_reaper(
        child,
        status_file.to_path_buf(),
        repo_dir.to_string(),
    ))
}

fn spawn_subagent_reaper(mut child: Child, status_path: PathBuf, repo_dir: String) -> u32 {
    let pid = child.id();
    let _ = std::thread::Builder::new()
        .name(format!("claw-subagent-reaper-{pid}"))
        .spawn(move || {
            let wait_result = child.wait();
            let raw = match fs::read_to_string(&status_path) {
                Ok(raw) => raw,
                Err(_) => return,
            };
            let mut record: SubagentStatusRecord = match serde_json::from_str(&raw) {
                Ok(record) => record,
                Err(_) => return,
            };
            if !is_active_subagent_status(&record.status) {
                return;
            }

            let now = timestamp_now();
            record.status = "failed".to_string();
            record.error = Some(match wait_result {
                Ok(status) => format!(
                    "sub-agent worker exited before writing a terminal status ({status}); see stderr_file/stdout_file"
                ),
                Err(error) => format!(
                    "failed to wait for sub-agent worker before terminal status: {error}; see stderr_file/stdout_file"
                ),
            });
            record.duration_ms = elapsed_ms_since_timestamp(&record.started_at);
            record.diff_stat = git_diff_stat(&repo_dir);
            record.completed_at = Some(now.clone());
            record.updated_at = now;
            let _ = write_status_record(&status_path, &record);
        });
    pid
}

fn input_with_review_controls(input: &RunSubagentInput) -> RunSubagentInput {
    let mut effective = input.clone();
    effective.prompt = prompt_with_review_controls(input);
    effective.review_depth = None;
    effective.focus = None;
    effective.artifact_scope = None;
    effective.stop_on_first_blocker = None;
    effective.require_evidence = None;
    effective
}

fn prompt_with_review_controls(input: &RunSubagentInput) -> String {
    let review_depth = input
        .review_depth
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let focus = input
        .focus
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let artifact_scope = input
        .artifact_scope
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let has_controls = review_depth.is_some()
        || focus.is_some()
        || artifact_scope.is_some()
        || input.stop_on_first_blocker.is_some()
        || input.require_evidence.is_some();
    if !has_controls {
        return input.prompt.clone();
    }

    let mut controls = Vec::new();
    controls.push("=== REVIEW CONTROLS ===".to_string());
    if let Some(value) = review_depth {
        controls.push(format!("Review depth: {value}"));
    }
    if let Some(value) = focus {
        controls.push(format!("Focus: {value}"));
    }
    if let Some(value) = artifact_scope {
        controls.push(format!("Artifact scope: {value}"));
    }
    if let Some(value) = input.stop_on_first_blocker {
        controls.push(format!("Stop on first blocker: {value}"));
    }
    if let Some(value) = input.require_evidence {
        controls.push(format!("Require evidence: {value}"));
    }
    controls.push(
        "Treat these as effort and scope controls for the review. They do not limit your ability to inspect necessary files.".to_string(),
    );
    controls.push("=== TASK ===".to_string());
    controls.push(input.prompt.clone());
    controls.join("\n")
}

fn max_iterations_for_input(input: &RunSubagentInput) -> usize {
    match input
        .review_depth
        .as_deref()
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("quick") => DEFAULT_AGENT_MAX_ITERATIONS,
        Some("standard") => DEFAULT_AGENT_MAX_ITERATIONS * 2,
        Some("deep") => DEFAULT_AGENT_MAX_ITERATIONS * 6,
        Some("exhaustive") => DEFAULT_AGENT_MAX_ITERATIONS * 12,
        _ => DEFAULT_AGENT_MAX_ITERATIONS,
    }
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
    let max_iterations = max_iterations_for_input(input);

    // Owned copies to move into the worker thread.
    let model_owned = model.to_string();
    let prompt = prompt_with_review_controls(input);

    let run = move || -> Result<String, String> {
        // Upstream's `build_agent_system_prompt` takes the model so it can
        // select the right model-family identity block; pass the resolved
        // model through (the fork's old 1-arg signature predated that).
        let system_prompt = build_agent_system_prompt(&subagent_type, &model_owned)?;
        let mut runtime =
            build_subagent_runtime(model_owned, allowed_tools, permission_mode, system_prompt)?
                .with_max_iterations(max_iterations);
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
            guard.set_if_absent(
                "OPENAI_BASE_URL",
                "https://openrouter.ai/api/v1/chat/completions",
            );
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
    /// Orchestrator key into `config/model_tiers.json` (the hybrid local→OpenRouter
    /// ladder). Empty for standalone presets that don't participate in tier routing.
    pub tier_ref: String,
    /// Resource-broker slot the local-first model holds: `gpu`/`cpu`/`gpu+cpu`/`remote`.
    pub resource: String,
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
            tier_ref: string_field(&value, "tier_ref"),
            resource: string_field(&value, "resource"),
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
///   risks leaking secrets to a world-readable temp file), so we **fail fast**
///   with `status: "failed"` rather than silently downgrade to the weaker
///   in-process backend (a caller asking for a hard kill must not lose it
///   unnoticed).
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
        // No preset: fail fast. The launcher is preset-driven, so we cannot
        // honor `isolated: true` (git worktree + hard `kill -9`) without one.
        // Silently downgrading to the in-process backend would hand the caller
        // a weaker isolation/cancellation model than they explicitly asked for,
        // so we refuse instead of pretending.
        return RunSubagentOutput::failed(
            provider,
            model,
            repo_dir,
            "isolated:true requires a `preset` (the run-claw-code.sh launcher is preset-driven and has no ad-hoc provider/model flag). Pass a `preset`, or drop `isolated` to run in-process.",
        );
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
        .arg(prompt_with_review_controls(input));
    // Forward EXPLICIT caller overrides only. This is how an orchestrator dispatches a
    // budget-gated OpenRouter rung onto a preset whose default is local. When the caller
    // didn't specify, we leave them off so the launcher uses the preset's local-first
    // defaults (passing the defaulted DeepSeek model here would wrongly override the preset).
    if let Some(provider) = input
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "auto")
    {
        command.arg("--provider").arg(provider);
    }
    if let Some(model) = input
        .model
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        command.arg("--model").arg(model);
    }
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

    fn restore_env_var(key: &str, previous: Option<std::ffi::OsString>) {
        match previous {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }

    #[test]
    fn run_subagent_rejects_empty_prompt() {
        let output = run_subagent(RunSubagentInput {
            provider: Some("openrouter".to_string()),
            model: Some("deepseek/deepseek-v4-pro:nitro".to_string()),
            prompt: "   ".to_string(),
            repo_dir: None,
            subagent_type: None,
            permission_mode: None,
            isolated: None,
            timeout_secs: None,
            max_output_chars: None,
            preset: None,
            review_depth: None,
            focus: None,
            artifact_scope: None,
            stop_on_first_blocker: None,
            require_evidence: None,
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
            review_depth: None,
            focus: None,
            artifact_scope: None,
            stop_on_first_blocker: None,
            require_evidence: None,
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
            review_depth: None,
            focus: None,
            artifact_scope: None,
            stop_on_first_blocker: None,
            require_evidence: None,
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
            let _env = apply_provider_env("openrouter", "deepseek/deepseek-v4-pro:nitro");
            assert_eq!(
                std::env::var("OPENAI_BASE_URL").as_deref(),
                Ok("https://openrouter.ai/api/v1/chat/completions")
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
    fn review_depth_scales_internal_iteration_budget_without_public_iteration_flag() {
        let input = RunSubagentInput {
            provider: None,
            model: None,
            prompt: "review".to_string(),
            repo_dir: None,
            subagent_type: None,
            permission_mode: None,
            isolated: None,
            timeout_secs: None,
            max_output_chars: None,
            preset: None,
            review_depth: Some("deep".to_string()),
            focus: None,
            artifact_scope: None,
            stop_on_first_blocker: None,
            require_evidence: None,
        };
        assert_eq!(
            max_iterations_for_input(&input),
            DEFAULT_AGENT_MAX_ITERATIONS * 6
        );

        let mut exhaustive = input.clone();
        exhaustive.review_depth = Some("exhaustive".to_string());
        assert_eq!(
            max_iterations_for_input(&exhaustive),
            DEFAULT_AGENT_MAX_ITERATIONS * 12
        );

        let mut unknown = input;
        unknown.review_depth = Some("surprise".to_string());
        assert_eq!(
            max_iterations_for_input(&unknown),
            DEFAULT_AGENT_MAX_ITERATIONS
        );
    }

    #[test]
    fn list_presets_parses_dir() {
        let _guard = env_guard();
        let root = unique_dir("presets-root");
        let presets_dir = root.join("scripts/presets");
        std::fs::create_dir_all(&presets_dir).expect("mkdir presets");
        std::fs::write(
            presets_dir.join("dev-coder.json"),
            r#"{"preset_name":"dev-coder","description":"Local coder","provider":"lmstudio","model":"qwen-coder","tier_ref":"presets.dev-coder-l0","resource":"gpu"}"#,
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
        assert_eq!(preset.tier_ref, "presets.dev-coder-l0");
        assert_eq!(preset.resource, "gpu");
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
    fn start_subagent_rejects_empty_prompt_without_spawning() {
        let result =
            handle_subagent_mcp_call("start_subagent", &json!({ "prompt": " " })).expect("ok json");
        let value: Value = serde_json::from_str(&result).expect("valid json");
        assert_eq!(value["status"], "failed");
        assert!(value["status_command"]
            .as_str()
            .unwrap_or_default()
            .contains("claw subagent status --status-file"));
        assert!(value["error"]
            .as_str()
            .unwrap_or_default()
            .contains("prompt must not be empty"));
    }

    #[test]
    fn status_command_quotes_paths_with_spaces() {
        let command = status_command_for_file(
            Path::new("/tmp/claw runs/subagent-1/status.json"),
            Path::new("/tmp/claw runs"),
        );
        assert_eq!(
            command,
            "CLAW_SUBAGENT_RUN_DIR='/tmp/claw runs' claw subagent status --status-file '/tmp/claw runs/subagent-1/status.json'"
        );
    }

    #[test]
    fn subagent_reaper_marks_active_status_failed_when_worker_exits() {
        let _guard = env_guard();
        let root = unique_dir("reaper-root");
        let run_dir = root.join("subagent-reaper");
        fs::create_dir_all(&run_dir).expect("mkdir status dir");
        let status_file = run_dir.join("status.json");
        let record = SubagentStatusRecord {
            status: "running".to_string(),
            phase: Some("running".to_string()),
            run_id: "subagent-reaper".to_string(),
            provider: "openrouter".to_string(),
            model: "deepseek/deepseek-v4-pro:nitro".to_string(),
            repo_dir: root.display().to_string(),
            status_file: status_file.display().to_string(),
            stdout_file: Some(run_dir.join("worker.stdout.log").display().to_string()),
            stderr_file: Some(run_dir.join("worker.stderr.log").display().to_string()),
            events_file: Some(run_dir.join("events.jsonl").display().to_string()),
            pid: None,
            started_at: (now_millis().saturating_sub(50)).to_string(),
            updated_at: timestamp_now(),
            last_activity_at: Some(timestamp_now()),
            heartbeat_at: Some(timestamp_now()),
            completed_at: None,
            summary: String::new(),
            truncated: false,
            diff_stat: None,
            error: None,
            duration_ms: 0,
            event_seq: 0,
            read_paths: Vec::new(),
            grep_patterns: Vec::new(),
            web_queries: Vec::new(),
            stop_command: None,
            activity: Vec::new(),
        };
        write_status_record(&status_file, &record).expect("write status");

        #[cfg(windows)]
        let child = Command::new("cmd")
            .args(["/C", "exit", "7"])
            .spawn()
            .expect("spawn short-lived child");
        #[cfg(not(windows))]
        let child = Command::new("sh")
            .args(["-c", "exit 7"])
            .spawn()
            .expect("spawn short-lived child");

        let pid = spawn_subagent_reaper(child, status_file.clone(), root.display().to_string());
        assert!(pid > 0);

        let mut final_status = String::new();
        for _ in 0..50 {
            let raw = fs::read_to_string(&status_file).expect("read status");
            let value: Value = serde_json::from_str(&raw).expect("status json");
            final_status = value["status"].as_str().unwrap_or_default().to_string();
            if final_status == "failed" {
                assert!(value["error"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("worker exited before writing a terminal status"));
                assert!(value["completed_at"].is_string());
                let _ = fs::remove_dir_all(root);
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let _ = fs::remove_dir_all(root);
        panic!("expected reaper to mark status failed, got {final_status}");
    }

    #[test]
    fn get_subagent_reads_status_file_and_limits_activity() {
        let _guard = env_guard();
        let previous_run_dir = std::env::var_os("CLAW_SUBAGENT_RUN_DIR");
        let root = unique_dir("status-root");
        let dir = root.join("subagent-test");
        fs::create_dir_all(&dir).expect("mkdir status dir");
        std::env::set_var("CLAW_SUBAGENT_RUN_DIR", &root);
        let status_file = dir.join("status.json");
        let record = SubagentStatusRecord {
            status: "running".to_string(),
            phase: Some("running".to_string()),
            run_id: "subagent-test".to_string(),
            provider: "openrouter".to_string(),
            model: "deepseek/deepseek-v4-pro:nitro".to_string(),
            repo_dir: "/tmp/repo".to_string(),
            status_file: status_file.display().to_string(),
            stdout_file: None,
            stderr_file: None,
            events_file: Some(dir.join("events.jsonl").display().to_string()),
            pid: None,
            started_at: "1".to_string(),
            updated_at: "2".to_string(),
            last_activity_at: Some("2".to_string()),
            heartbeat_at: Some("2".to_string()),
            completed_at: None,
            summary: String::new(),
            truncated: false,
            diff_stat: None,
            error: None,
            duration_ms: 0,
            event_seq: 2,
            read_paths: vec!["src/lib.rs".to_string()],
            grep_patterns: vec!["todo".to_string()],
            web_queries: Vec::new(),
            stop_command: None,
            activity: vec![
                SubagentActivity {
                    seq: 1,
                    tool_name: "read_file".to_string(),
                    status: "completed".to_string(),
                    input: json!({"path": "src/lib.rs"}),
                    observed_target: Some("src/lib.rs".to_string()),
                    started_at: "1".to_string(),
                    finished_at: Some("2".to_string()),
                    is_error: Some(false),
                    output_chars: Some(10),
                },
                SubagentActivity {
                    seq: 2,
                    tool_name: "grep_search".to_string(),
                    status: "running".to_string(),
                    input: json!({"pattern": "todo"}),
                    observed_target: Some("todo".to_string()),
                    started_at: "3".to_string(),
                    finished_at: None,
                    is_error: None,
                    output_chars: None,
                },
            ],
        };
        write_status_record(&status_file, &record).expect("write status");
        let events = [
            SubagentEvent {
                seq: 1,
                timestamp: "2".to_string(),
                kind: "phase_started".to_string(),
                phase: Some("running".to_string()),
                status: None,
                message: Some("started".to_string()),
                tool_name: None,
                observed_target: None,
                input: None,
                is_error: None,
                output_chars: None,
            },
            SubagentEvent {
                seq: 2,
                timestamp: "3".to_string(),
                kind: "tool_started".to_string(),
                phase: Some("tool:grep_search".to_string()),
                status: Some("running".to_string()),
                message: Some("tool call started".to_string()),
                tool_name: Some("grep_search".to_string()),
                observed_target: Some("todo".to_string()),
                input: Some(json!({"pattern": "todo"})),
                is_error: None,
                output_chars: None,
            },
        ];
        let events_payload = events
            .iter()
            .map(|event| serde_json::to_string(event).expect("event json"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(dir.join("events.jsonl"), format!("{events_payload}\n")).expect("write events");
        let value = get_subagent(GetSubagentInput {
            run_id: None,
            status_file: Some(status_file.display().to_string()),
            activity_limit: Some(1),
            event_limit: Some(1),
            since_seq: Some(1),
            stale_after_secs: None,
        });
        assert_eq!(value["status"], "running");
        assert_eq!(value["read_paths"][0], "src/lib.rs");
        assert_eq!(value["activity"].as_array().expect("activity").len(), 1);
        assert_eq!(value["activity"][0]["seq"], 2);
        assert_eq!(value["event_count"], 2);
        assert_eq!(value["events"].as_array().expect("events").len(), 1);
        assert_eq!(value["events"][0]["seq"], 2);

        restore_env_var("CLAW_SUBAGENT_RUN_DIR", previous_run_dir);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn get_subagent_rejects_status_file_outside_run_root() {
        let _guard = env_guard();
        let previous_run_dir = std::env::var_os("CLAW_SUBAGENT_RUN_DIR");
        let root = unique_dir("safe-status-root");
        let outside = unique_dir("outside-status-root");
        fs::create_dir_all(&root).expect("mkdir safe root");
        fs::create_dir_all(&outside).expect("mkdir outside root");
        let outside_status = outside.join("status.json");
        fs::write(&outside_status, "{}").expect("write outside status");
        std::env::set_var("CLAW_SUBAGENT_RUN_DIR", &root);

        let value = get_subagent(GetSubagentInput {
            run_id: None,
            status_file: Some(outside_status.display().to_string()),
            activity_limit: None,
            event_limit: None,
            since_seq: None,
            stale_after_secs: None,
        });

        assert_eq!(value["status"], "failed");
        assert!(value["error"]
            .as_str()
            .unwrap_or_default()
            .contains("status_file must be under"));

        restore_env_var("CLAW_SUBAGENT_RUN_DIR", previous_run_dir);
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(outside);
    }

    #[test]
    fn get_subagent_marks_dead_active_worker_failed() {
        let _guard = env_guard();
        let previous_run_dir = std::env::var_os("CLAW_SUBAGENT_RUN_DIR");
        let root = unique_dir("dead-worker-root");
        let run_dir = root.join("subagent-dead-worker");
        fs::create_dir_all(&run_dir).expect("mkdir status dir");
        std::env::set_var("CLAW_SUBAGENT_RUN_DIR", &root);
        let status_file = run_dir.join("status.json");
        let record = SubagentStatusRecord {
            status: "running".to_string(),
            phase: Some("running".to_string()),
            run_id: "subagent-dead-worker".to_string(),
            provider: "openrouter".to_string(),
            model: "deepseek/deepseek-v4-pro:nitro".to_string(),
            repo_dir: root.display().to_string(),
            status_file: status_file.display().to_string(),
            stdout_file: Some(run_dir.join("worker.stdout.log").display().to_string()),
            stderr_file: Some(run_dir.join("worker.stderr.log").display().to_string()),
            events_file: Some(run_dir.join("events.jsonl").display().to_string()),
            pid: Some(u32::MAX),
            started_at: (now_millis().saturating_sub(50)).to_string(),
            updated_at: timestamp_now(),
            last_activity_at: Some(timestamp_now()),
            heartbeat_at: Some(timestamp_now()),
            completed_at: None,
            summary: String::new(),
            truncated: false,
            diff_stat: None,
            error: None,
            duration_ms: 0,
            event_seq: 0,
            read_paths: Vec::new(),
            grep_patterns: Vec::new(),
            web_queries: Vec::new(),
            stop_command: None,
            activity: Vec::new(),
        };
        write_status_record(&status_file, &record).expect("write status");

        let value = get_subagent(GetSubagentInput {
            run_id: Some("subagent-dead-worker".to_string()),
            status_file: None,
            activity_limit: None,
            event_limit: None,
            since_seq: None,
            stale_after_secs: None,
        });

        assert_eq!(value["status"], "failed");
        assert_eq!(value["worker_alive"], false);
        assert!(value["error"]
            .as_str()
            .unwrap_or_default()
            .contains("worker exited"));

        restore_env_var("CLAW_SUBAGENT_RUN_DIR", previous_run_dir);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn stop_subagent_marks_active_run_cancelled() {
        let _guard = env_guard();
        let previous_run_dir = std::env::var_os("CLAW_SUBAGENT_RUN_DIR");
        let root = unique_dir("stop-root");
        let run_dir = root.join("subagent-stop");
        fs::create_dir_all(&run_dir).expect("mkdir status dir");
        std::env::set_var("CLAW_SUBAGENT_RUN_DIR", &root);
        let status_file = run_dir.join("status.json");
        let now = timestamp_now();
        let record = SubagentStatusRecord {
            status: "running".to_string(),
            phase: Some("model_turn".to_string()),
            run_id: "subagent-stop".to_string(),
            provider: "openrouter".to_string(),
            model: "deepseek/deepseek-v4-pro:nitro".to_string(),
            repo_dir: root.display().to_string(),
            status_file: status_file.display().to_string(),
            stdout_file: None,
            stderr_file: None,
            events_file: Some(run_dir.join("events.jsonl").display().to_string()),
            pid: None,
            started_at: now.clone(),
            updated_at: now.clone(),
            last_activity_at: Some(now.clone()),
            heartbeat_at: Some(now),
            completed_at: None,
            summary: String::new(),
            truncated: false,
            diff_stat: None,
            error: None,
            duration_ms: 0,
            event_seq: 0,
            activity: Vec::new(),
            read_paths: Vec::new(),
            grep_patterns: Vec::new(),
            web_queries: Vec::new(),
            stop_command: None,
        };
        write_status_record(&status_file, &record).expect("write status");

        let value = stop_subagent(StopSubagentInput {
            run_id: Some("subagent-stop".to_string()),
            status_file: None,
            grace_secs: Some(0),
        });

        assert_eq!(value["status"], "cancelled");
        let status = get_subagent(GetSubagentInput {
            run_id: None,
            status_file: Some(status_file.display().to_string()),
            activity_limit: None,
            event_limit: Some(1),
            since_seq: None,
            stale_after_secs: None,
        });
        assert_eq!(status["status"], "cancelled");
        assert_eq!(status["phase"], "cancelled");
        assert_eq!(status["events"][0]["kind"], "terminal");

        restore_env_var("CLAW_SUBAGENT_RUN_DIR", previous_run_dir);
        let _ = fs::remove_dir_all(root);
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
    fn isolated_without_preset_fails_fast_rather_than_downgrading() {
        // `isolated: true` with no `preset` must NOT silently fall back to the
        // in-process backend (which has no worktree / hard-kill). It returns a
        // structured failure that names the missing `preset` — no network call.
        let result = handle_subagent_mcp_call(
            "run_subagent",
            &json!({ "prompt": "review this", "isolated": true }),
        )
        .expect("ok json");
        let value: Value = serde_json::from_str(&result).expect("valid json");
        assert_eq!(value["status"], "failed");
        let error = value["error"].as_str().unwrap_or_default();
        assert!(
            error.contains("preset"),
            "error should explain the missing preset, got: {error}"
        );
    }

    #[test]
    fn parse_launcher_task_dir_reads_status_path() {
        let stdout = "task_id=abc-123\n/tmp/claw-runs/abc-123/status.json\n/tmp/claw-runs/abc-123/diff.patch\n/tmp/claw-runs/abc-123/summary.md\n";
        let dir = parse_launcher_task_dir(stdout).expect("task dir");
        assert_eq!(dir, PathBuf::from("/tmp/claw-runs/abc-123"));
    }
}
