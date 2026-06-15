//! Resilience configuration for local-model recovery (fork feature).
//!
//! In the fork's original tree this type was defined in the `runtime` crate
//! and re-exported here. Upstream's `runtime` crate does not define a
//! `ResilienceConfig`, and the `api` crate may not introduce a dependency
//! cycle, so the canonical definition now lives directly in the `api` crate.
//!
//! Configurable via the `CLAW_RESILIENCE` environment variable:
//! - `force` — force enable resilience on all providers/URLs
//! - `none`  — force disable resilience on all providers/URLs
//! - `auto` or unset — default behavior (auto-detect localhost)

use std::time::Duration;

/// Per-error retry budgets, backoff tuning, and provider/URL gating for the
/// local-model resilience layer.
#[derive(Debug, Clone)]
pub struct ResilienceConfig {
    /// Force enable resilience recovery regardless of provider/URL.
    pub force_enable: bool,
    /// Force disable resilience recovery regardless of provider/URL.
    pub force_disable: bool,
    /// Auto-enable for localhost endpoints (default: true).
    pub auto_enable_for_local: bool,
    /// Enable for Anthropic API endpoints (default: false).
    pub enable_for_anthropic: bool,
    /// Enable for OpenAI-compatible endpoints (default: true).
    pub enable_for_openai_compat: bool,

    // Error-specific retry configurations.
    /// Maximum retries for model reloaded errors.
    pub model_reloaded_max_retries: u32,
    /// Maximum retries for context size exceeded errors.
    pub context_exceeded_max_retries: u32,
    /// Maximum retries for empty stream errors.
    pub stream_empty_max_retries: u32,
    /// Maximum retries for decoding errors.
    pub decoding_error_max_retries: u32,
    /// Maximum retries for model unloaded errors.
    pub model_unloaded_max_retries: u32,
    /// Maximum retries for tool sequence errors.
    pub tool_sequence_error_max_retries: u32,

    // Backoff configurations (initial backoff duration).
    /// Initial backoff for model reloaded errors.
    pub model_reloaded_initial_backoff: Duration,
    /// Initial backoff for context exceeded errors.
    pub context_exceeded_initial_backoff: Duration,
    /// Initial backoff for stream empty errors.
    pub stream_empty_initial_backoff: Duration,
    /// Initial backoff for decoding errors.
    pub decoding_error_initial_backoff: Duration,
    /// Initial backoff for model unloaded errors.
    pub model_unloaded_initial_backoff: Duration,
    /// Initial backoff for tool sequence errors.
    pub tool_sequence_error_initial_backoff: Duration,

    // Context management thresholds (0.0 to 1.0).
    /// Warning threshold for context usage percentage (default: 0.8 = 80%).
    pub context_warning_threshold: f32,
    /// Critical threshold for context usage percentage (default: 0.95 = 95%).
    pub context_critical_threshold: f32,

    // Compaction strategies.
    /// Preserve recent messages count for aggressive compaction.
    pub aggressive_compaction_preserve_recent: usize,
    /// Preserve recent messages count for conservative compaction.
    pub conservative_compaction_preserve_recent: usize,

    // Backoff tuning.
    /// Backoff multiplier for each retry attempt (default: 2.0).
    pub backoff_multiplier: f64,
    /// Maximum backoff duration (default: 30s).
    pub max_backoff: Duration,
}

impl ResilienceConfig {
    /// Create the default resilience configuration.
    // Inherent `default()` is intentional and pre-existing public API; an
    // explicit allow keeps it without globally disabling the lint.
    #[allow(clippy::should_implement_trait)]
    #[must_use]
    pub fn default() -> Self {
        Self {
            force_enable: false,
            force_disable: false,
            auto_enable_for_local: true,
            enable_for_anthropic: false,
            enable_for_openai_compat: true,
            model_reloaded_max_retries: 3,
            context_exceeded_max_retries: 2,
            stream_empty_max_retries: 3,
            decoding_error_max_retries: 2,
            model_unloaded_max_retries: 5,
            tool_sequence_error_max_retries: 2,
            model_reloaded_initial_backoff: Duration::from_secs(1),
            context_exceeded_initial_backoff: Duration::from_secs(2),
            stream_empty_initial_backoff: Duration::from_secs(1),
            decoding_error_initial_backoff: Duration::from_secs(1),
            model_unloaded_initial_backoff: Duration::from_secs(3),
            tool_sequence_error_initial_backoff: Duration::from_secs(1),
            context_warning_threshold: 0.8,
            context_critical_threshold: 0.95,
            aggressive_compaction_preserve_recent: 1,
            conservative_compaction_preserve_recent: 3,
            backoff_multiplier: 2.0,
            max_backoff: Duration::from_secs(30),
        }
    }

    /// Force enable resilience on all providers.
    #[must_use]
    pub fn force_enable() -> Self {
        Self {
            force_enable: true,
            force_disable: false,
            auto_enable_for_local: true,
            enable_for_anthropic: true,
            enable_for_openai_compat: true,
            model_reloaded_max_retries: 5,
            context_exceeded_max_retries: 3,
            stream_empty_max_retries: 5,
            decoding_error_max_retries: 3,
            model_unloaded_max_retries: 10,
            tool_sequence_error_max_retries: 3,
            model_reloaded_initial_backoff: Duration::from_secs(1),
            context_exceeded_initial_backoff: Duration::from_secs(2),
            stream_empty_initial_backoff: Duration::from_secs(1),
            decoding_error_initial_backoff: Duration::from_secs(1),
            model_unloaded_initial_backoff: Duration::from_secs(3),
            tool_sequence_error_initial_backoff: Duration::from_secs(1),
            context_warning_threshold: 0.8,
            context_critical_threshold: 0.95,
            aggressive_compaction_preserve_recent: 1,
            conservative_compaction_preserve_recent: 3,
            backoff_multiplier: 2.0,
            max_backoff: Duration::from_secs(60),
        }
    }

    /// Force disable resilience on all providers.
    #[must_use]
    pub fn force_disable() -> Self {
        Self {
            force_enable: false,
            force_disable: true,
            auto_enable_for_local: false,
            enable_for_anthropic: false,
            enable_for_openai_compat: false,
            model_reloaded_max_retries: 0,
            context_exceeded_max_retries: 0,
            stream_empty_max_retries: 0,
            decoding_error_max_retries: 0,
            model_unloaded_max_retries: 0,
            tool_sequence_error_max_retries: 0,
            model_reloaded_initial_backoff: Duration::from_secs(0),
            context_exceeded_initial_backoff: Duration::from_secs(0),
            stream_empty_initial_backoff: Duration::from_secs(0),
            decoding_error_initial_backoff: Duration::from_secs(0),
            model_unloaded_initial_backoff: Duration::from_secs(0),
            tool_sequence_error_initial_backoff: Duration::from_secs(0),
            context_warning_threshold: 0.8,
            context_critical_threshold: 0.95,
            aggressive_compaction_preserve_recent: 1,
            conservative_compaction_preserve_recent: 3,
            backoff_multiplier: 1.0,
            max_backoff: Duration::from_secs(0),
        }
    }

    /// Create a resilience configuration from the `CLAW_RESILIENCE` env var.
    #[must_use]
    pub fn from_env() -> Self {
        match std::env::var("CLAW_RESILIENCE")
            .ok()
            .map(|s| s.to_lowercase())
        {
            Some(s) if s == "force" => Self::force_enable(),
            Some(s) if s == "none" => Self::force_disable(),
            _ => Self::default(),
        }
    }

    /// Check if resilience is enabled for any provider type.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        if self.force_disable {
            return false;
        }
        if self.force_enable {
            return true;
        }
        self.enable_for_anthropic || self.enable_for_openai_compat || self.auto_enable_for_local
    }

    /// Enable resilience for the Anthropic API.
    #[must_use]
    pub fn with_anthropic_enabled(mut self, enabled: bool) -> Self {
        self.enable_for_anthropic = enabled;
        self
    }

    /// Enable resilience for OpenAI-compatible endpoints.
    #[must_use]
    pub fn with_openai_compat_enabled(mut self, enabled: bool) -> Self {
        self.enable_for_openai_compat = enabled;
        self
    }

    /// Force enable resilience (overrides all other settings).
    #[must_use]
    pub fn with_force_enable(mut self, enabled: bool) -> Self {
        self.force_enable = enabled;
        self
    }

    /// Force disable resilience (overrides all other settings).
    #[must_use]
    pub fn with_force_disable(mut self, enabled: bool) -> Self {
        self.force_disable = enabled;
        self
    }

    /// Check if resilience should be enabled for a provider.
    #[must_use]
    pub fn should_enable_for_provider(&self, provider_name: &str) -> bool {
        if self.force_enable {
            return true;
        }
        if self.force_disable {
            return false;
        }
        match provider_name.to_lowercase().as_str() {
            "anthropic" => self.enable_for_anthropic,
            "openai" | "xai" | "dashscope" | "lm_studio" | "local" => self.enable_for_openai_compat,
            _ => false,
        }
    }

    /// Check if resilience should be enabled for a specific URL.
    #[must_use]
    pub fn should_enable_for_url(&self, base_url: &str) -> bool {
        if self.force_enable {
            return true;
        }
        if self.force_disable {
            return false;
        }
        if self.auto_enable_for_local {
            let lower = base_url.to_lowercase();
            if lower.contains("localhost") || lower.contains("127.0.0.1") || lower.contains("local")
            {
                return true;
            }
        }
        false
    }

    /// Maximum retries for an error class, gated on the provider being enabled.
    #[must_use]
    pub fn max_retries_for_provider(&self, error_class: &str, provider_name: &str) -> u32 {
        if !self.should_enable_for_provider(provider_name) {
            return 0;
        }
        self.max_retries_by_class(error_class)
    }

    /// Maximum retries for an error class, gated on the URL being enabled.
    #[must_use]
    pub fn max_retries_for_url(&self, error_class: &str, base_url: &str) -> u32 {
        if !self.should_enable_for_url(base_url) {
            return 0;
        }
        self.max_retries_by_class(error_class)
    }

    /// Maximum retries for an error class, gated on global enablement.
    #[must_use]
    pub fn max_retries_for(&self, error_class: &str) -> u32 {
        if !self.is_enabled() {
            return 0;
        }
        self.max_retries_by_class(error_class)
    }

    fn max_retries_by_class(&self, error_class: &str) -> u32 {
        match error_class {
            "stream_empty" | "empty_stream" | "no_content" => self.stream_empty_max_retries,
            "context_window" | "context_exceeded" => self.context_exceeded_max_retries,
            "model_reloaded" => self.model_reloaded_max_retries,
            "model_unloaded" | "local_model_unloaded" => self.model_unloaded_max_retries,
            "decoding_error" | "decode" => self.decoding_error_max_retries,
            "tool_sequence" => self.tool_sequence_error_max_retries,
            "first_token_timeout" => self.model_reloaded_max_retries,
            _ => 1,
        }
    }

    /// Get the initial backoff duration for a specific error class.
    #[must_use]
    pub fn initial_backoff_for(&self, error_class: &str) -> Duration {
        match error_class {
            "stream_empty" | "empty_stream" | "no_content" => self.stream_empty_initial_backoff,
            "context_window" | "context_exceeded" => self.context_exceeded_initial_backoff,
            "model_reloaded" => self.model_reloaded_initial_backoff,
            "model_unloaded" | "local_model_unloaded" => self.model_unloaded_initial_backoff,
            "decoding_error" | "decode" => self.decoding_error_initial_backoff,
            "tool_sequence" => self.tool_sequence_error_initial_backoff,
            "first_token_timeout" => self.model_unloaded_initial_backoff,
            _ => Duration::from_secs(1),
        }
    }

    /// Calculate the backoff duration for a given attempt number, using the
    /// error-class-specific initial backoff and the shared multiplier/max.
    #[must_use]
    pub fn backoff_for_attempt(&self, error_class: &str, attempt: u32) -> Duration {
        if attempt == 0 {
            return Duration::from_secs(0);
        }
        let initial = self.initial_backoff_for(error_class);
        let backoff_ms =
            (initial.as_millis() as f64) * self.backoff_multiplier.powi(attempt as i32 - 1);
        let capped = std::cmp::min(backoff_ms as u128, self.max_backoff.as_millis());
        Duration::from_millis(capped as u64)
    }

    /// Convenience: calculate backoff using the legacy single-argument form
    /// (uses the stream_empty initial backoff as the default).
    #[must_use]
    pub fn backoff_for_attempt_legacy(&self, attempt: u32) -> Duration {
        self.backoff_for_attempt("stream_empty", attempt)
    }
}

impl Default for ResilienceConfig {
    fn default() -> Self {
        Self::default()
    }
}
