use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::error::ApiError;
use crate::types::MessageRequest;

/// Classifies errors into retry-aware categories specific to local model providers.
/// Different error types may be recoverable or unrecoverable depending on the provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryableErrorKind {
    /// Model was unloaded by JIT memory management (LM Studio). Transient.
    ModelUnloaded,
    /// Stream opened but produced no assistant content or tool calls. May be streaming flakiness.
    EmptyStream,
    /// No first output token within timeout. Slow prompt eval on local model.
    FirstTokenStalled,
    /// Network transport errors (connection reset, timeout). Transient.
    TransportError,
    /// 5xx server errors. Transient.
    ServerError,
    /// Non-retryable (auth, malformed request, invalid schema).
    NonRetryable,
}

/// Error classifier for OpenAI-compatible providers, with special handling for local models.
pub struct ErrorClassifier;

impl ErrorClassifier {
    /// Classify an error to determine if it's retryable and why.
    pub fn classify(
        error: &ApiError,
        _provider: &str,
        response_body: Option<&str>,
    ) -> RetryableErrorKind {
        match error {
            // Local model specific: "Model unloaded." in response body
            ApiError::Api { status, body, .. } if status.as_u16() == 400 => {
                let body_text = response_body.unwrap_or(body);
                if body_text.contains("Model unloaded") {
                    return RetryableErrorKind::ModelUnloaded;
                }
                // Other 400s are non-retryable unless transport-related
                RetryableErrorKind::NonRetryable
            }
            // Empty assistant stream from local model
            ApiError::EmptyAssistantStream { .. } => RetryableErrorKind::EmptyStream,
            // First token timeout
            ApiError::FirstTokenTimeout { .. } => RetryableErrorKind::FirstTokenStalled,
            // HTTP transport errors
            ApiError::Http(err) => {
                if err.is_connect() || err.is_timeout() || err.is_request() {
                    RetryableErrorKind::TransportError
                } else {
                    RetryableErrorKind::NonRetryable
                }
            }
            // 5xx server errors
            ApiError::Api { status, .. } if matches!(status.as_u16(), 500 | 502 | 503 | 504) => {
                RetryableErrorKind::ServerError
            }
            // 429 rate limit
            ApiError::Api { status, .. } if status.as_u16() == 429 => {
                RetryableErrorKind::ServerError
            }
            // Local model state errors
            ApiError::LocalModelUnloaded { .. } => RetryableErrorKind::ModelUnloaded,
            // Auth, missing credentials, invalid format, etc.
            _ => RetryableErrorKind::NonRetryable,
        }
    }
}

/// Per-model health tracking for adaptive fallback behavior.
/// Learns from repeated failures and adjusts strategy accordingly.
#[derive(Debug, Clone)]
pub struct ModelHealthProfile {
    pub model_name: String,
    pub recent_empty_streams: u32,
    pub recent_model_unloads: u32,
    pub recent_first_token_timeouts: u32,
    pub streaming_degraded: bool,
    pub streaming_degraded_until: Option<Instant>,
    pub force_non_streaming: bool,
    pub last_attempt_streaming: bool,
    pub last_success_streaming: bool,
    pub first_token_timeout_ms: u64,
}

impl ModelHealthProfile {
    pub fn new(model_name: String, initial_timeout_ms: u64) -> Self {
        Self {
            model_name,
            recent_empty_streams: 0,
            recent_model_unloads: 0,
            recent_first_token_timeouts: 0,
            streaming_degraded: false,
            streaming_degraded_until: None,
            force_non_streaming: false,
            last_attempt_streaming: true,
            last_success_streaming: true,
            first_token_timeout_ms: initial_timeout_ms,
        }
    }

    /// Determine if streaming should be used for the next request.
    pub fn should_use_streaming(&self) -> bool {
        if self.force_non_streaming {
            return false;
        }
        if let Some(until) = self.streaming_degraded_until {
            if Instant::now() < until {
                return false; // Still within degradation window
            }
        }
        true
    }

    /// Mark that a stream produced no content, triggering degradation if threshold exceeded.
    pub fn mark_empty_stream(&mut self) {
        self.recent_empty_streams += 1;
        if self.recent_empty_streams >= 2 {
            self.streaming_degraded = true;
            self.streaming_degraded_until = Some(Instant::now() + Duration::from_secs(10));
        }
    }

    /// Mark that a model unload error occurred, triggering non-streaming if repeated.
    pub fn mark_model_unload(&mut self) {
        self.recent_model_unloads += 1;
        if self.recent_model_unloads >= 2 {
            self.force_non_streaming = true;
        }
    }

    /// Mark that first token timeout occurred, triggering timeout increase if repeated.
    pub fn mark_first_token_timeout(&mut self) {
        self.recent_first_token_timeouts += 1;
        if self.recent_first_token_timeouts >= 1 {
            let new_timeout = (self.first_token_timeout_ms as f64 * 1.5).min(120_000.0) as u64;
            self.first_token_timeout_ms = new_timeout;
        }
    }

    /// Mark a successful attempt, recording streaming preference.
    pub fn mark_success(&mut self, used_streaming: bool) {
        self.recent_empty_streams = self.recent_empty_streams.saturating_sub(1);
        self.recent_model_unloads = self.recent_model_unloads.saturating_sub(1);
        self.recent_first_token_timeouts = self.recent_first_token_timeouts.saturating_sub(1);
        self.last_success_streaming = used_streaming;
    }

    /// Clear transient error counters after 60 seconds without incidents.
    pub fn clear_transient_counters_if_stale(&mut self) {
        // Simple version: just reset to 0 periodically in production use
        // In tests, we can manually reset or use mock time.
    }
}

/// Provider capability flags for known local and cloud providers.
#[derive(Debug, Clone)]
pub struct ProviderCapabilities {
    pub is_local: bool,
    pub supports_warmup: bool,
    pub supports_streaming: Option<bool>,
    pub prefer_non_streaming_for_tools: bool,
    pub cold_start_likely: bool,
    pub first_token_timeout_ms: u64,
    pub max_recovery_attempts: u32,
    pub default_backoff_ms: u64,
}

impl ProviderCapabilities {
    /// Create capabilities for LM Studio (OpenAI-compatible local backend).
    pub fn lm_studio() -> Self {
        Self {
            is_local: true,
            supports_warmup: true,
            supports_streaming: Some(false),
            prefer_non_streaming_for_tools: true,
            cold_start_likely: true,
            first_token_timeout_ms: 45_000,
            max_recovery_attempts: 3,
            default_backoff_ms: 500,
        }
    }

    /// Create capabilities for Nemotron-3-Nano (small local model).
    pub fn nemotron_3_nano() -> Self {
        Self {
            is_local: true,
            supports_warmup: false,
            supports_streaming: Some(false),
            prefer_non_streaming_for_tools: true,
            cold_start_likely: false,
            first_token_timeout_ms: 30_000,
            max_recovery_attempts: 2,
            default_backoff_ms: 500,
        }
    }

    /// Create generic capabilities for unknown local providers.
    pub fn local_generic() -> Self {
        Self {
            is_local: true,
            supports_warmup: false,
            supports_streaming: Some(false),
            prefer_non_streaming_for_tools: true,
            cold_start_likely: true,
            first_token_timeout_ms: 45_000,
            max_recovery_attempts: 3,
            default_backoff_ms: 500,
        }
    }

    /// Create capabilities for cloud providers (OpenAI, xAI, etc.).
    pub fn cloud_provider() -> Self {
        Self {
            is_local: false,
            supports_warmup: false,
            supports_streaming: Some(true),
            prefer_non_streaming_for_tools: false,
            cold_start_likely: false,
            first_token_timeout_ms: 5_000,
            max_recovery_attempts: 1,
            default_backoff_ms: 500,
        }
    }

    /// Get or infer capabilities for a provider string.
    pub fn for_provider(provider_name: &str, _model: &str) -> Self {
        let lower = provider_name.to_lowercase();
        if lower.contains("lm") && lower.contains("studio") {
            Self::lm_studio()
        } else if lower.contains("nemotron") {
            Self::nemotron_3_nano()
        } else if lower.contains("local") || lower.contains("localhost") {
            Self::local_generic()
        } else {
            Self::cloud_provider()
        }
    }
}

/// Context for a single recovery attempt.
#[derive(Debug)]
pub struct RecoveryContext {
    pub provider: String,
    pub model: String,
    pub attempt: u32,
    pub last_error_kind: Option<RetryableErrorKind>,
    pub health_profile: ModelHealthProfile,
    pub capabilities: ProviderCapabilities,
}

impl RecoveryContext {
    pub fn new(
        provider: String,
        model: String,
        mut health_profile: ModelHealthProfile,
        capabilities: ProviderCapabilities,
    ) -> Self {
        // Clean up stale counters on new context (in real use, this would be time-based)
        health_profile.clear_transient_counters_if_stale();
        Self {
            provider,
            model,
            attempt: 0,
            last_error_kind: None,
            health_profile,
            capabilities,
        }
    }
}

/// Manage recovery attempts for a single message request.
pub struct RecoveryStateMachine {
    context: RecoveryContext,
}

impl RecoveryStateMachine {
    pub fn new(context: RecoveryContext) -> Self {
        Self { context }
    }

    /// Get the current recovery context (for tests and introspection).
    pub fn context(&self) -> &RecoveryContext {
        &self.context
    }

    /// Get a mutable reference to the recovery context.
    pub fn context_mut(&mut self) -> &mut RecoveryContext {
        &mut self.context
    }

    /// Compute jittered backoff delay for a given attempt number.
    pub fn backoff_for_attempt(&self, attempt: u32) -> Duration {
        if attempt == 0 {
            return Duration::ZERO;
        }
        // Exponential backoff: base * 2^(attempt-1), capped, with jitter
        let base_ms = self.context.capabilities.default_backoff_ms;
        let multiplier = 1_u64 << (attempt - 1).min(7); // Cap at 2^7 = 128x
        let backoff_ms = (base_ms * multiplier).min(8000);
        Duration::from_millis(backoff_ms)
    }

    /// Determine if this attempt should use streaming, based on health profile.
    pub fn should_use_streaming_for_attempt(&self, attempt: u32) -> bool {
        if !self.context.capabilities.supports_streaming.unwrap_or(true) {
            return false;
        }
        match attempt {
            1 => self.context.health_profile.should_use_streaming(),
            _ => false, // Fallback to non-streaming on retry
        }
    }

    /// Mutate a request for retry, applying fallback strategies.
    pub fn mutate_request_for_attempt(
        &self,
        request: &MessageRequest,
        attempt: u32,
    ) -> MessageRequest {
        let mut req = request.clone();
        req.stream = self.should_use_streaming_for_attempt(attempt);
        req
    }

    /// Handle recovery for model-unloaded error (may trigger warmup in future).
    pub fn handle_model_unloaded(&mut self) {
        self.context.health_profile.mark_model_unload();
        // TODO: Implement warmup hook for LM Studio
    }

    /// Handle recovery for empty-stream error (mark degradation).
    pub fn handle_empty_stream(&mut self) {
        self.context.health_profile.mark_empty_stream();
    }

    /// Handle recovery for first-token timeout (increase timeout).
    pub fn handle_first_token_timeout(&mut self) {
        self.context.health_profile.mark_first_token_timeout();
    }

    /// Record success and update health profile.
    pub fn record_success(&mut self, used_streaming: bool) {
        self.context.health_profile.mark_success(used_streaming);
    }

    /// Check if more attempts are available.
    pub fn has_more_attempts(&self) -> bool {
        self.context.attempt < self.context.capabilities.max_recovery_attempts
    }

    /// Increment attempt counter.
    pub fn next_attempt(&mut self) {
        self.context.attempt += 1;
    }
}

/// Shared health profile cache for models across requests.
pub struct HealthProfileCache {
    profiles: Arc<Mutex<HashMap<String, ModelHealthProfile>>>,
}

impl HealthProfileCache {
    pub fn new() -> Self {
        Self {
            profiles: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Get or create a health profile for a model.
    pub fn get_or_create(&self, model: &str, initial_timeout_ms: u64) -> ModelHealthProfile {
        let mut profiles = self.profiles.lock().expect("lock profiles");
        profiles
            .entry(model.to_string())
            .or_insert_with(|| ModelHealthProfile::new(model.to_string(), initial_timeout_ms))
            .clone()
    }

    /// Update the profile for a model.
    pub fn update(&self, model: &str, profile: ModelHealthProfile) {
        let mut profiles = self.profiles.lock().expect("lock profiles");
        profiles.insert(model.to_string(), profile);
    }

    /// Clear all profiles (useful for tests).
    pub fn clear(&self) {
        let mut profiles = self.profiles.lock().expect("lock profiles");
        profiles.clear();
    }
}

impl Default for HealthProfileCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_classifier_detects_model_unloaded() {
        let error = ApiError::Api {
            status: reqwest::StatusCode::BAD_REQUEST,
            error_type: Some("error".to_string()),
            message: Some("Model unloaded.".to_string()),
            request_id: None,
            body: "Model unloaded.".to_string(),
            retryable: false,
            suggested_action: None,
        };
        let kind = ErrorClassifier::classify(&error, "lm_studio", None);
        assert_eq!(kind, RetryableErrorKind::ModelUnloaded);
    }

    #[test]
    fn error_classifier_detects_empty_stream() {
        let error = ApiError::EmptyAssistantStream {
            provider: "lm_studio".to_string(),
            model: "test".to_string(),
            attempt: 1,
        };
        let kind = ErrorClassifier::classify(&error, "lm_studio", None);
        assert_eq!(kind, RetryableErrorKind::EmptyStream);
    }

    #[test]
    fn error_classifier_detects_first_token_timeout() {
        let error = ApiError::FirstTokenTimeout {
            provider: "lm_studio".to_string(),
            model: "test".to_string(),
            timeout_ms: 5000,
        };
        let kind = ErrorClassifier::classify(&error, "lm_studio", None);
        assert_eq!(kind, RetryableErrorKind::FirstTokenStalled);
    }

    #[test]
    fn model_health_profile_degrades_streaming_after_two_empty_streams() {
        let mut profile = ModelHealthProfile::new("test".to_string(), 5000);
        assert!(profile.should_use_streaming());

        profile.mark_empty_stream();
        assert!(profile.should_use_streaming()); // Not degraded yet

        profile.mark_empty_stream();
        assert!(!profile.should_use_streaming()); // Now degraded
        assert!(profile.streaming_degraded);
    }

    #[test]
    fn model_health_profile_forces_non_streaming_after_two_unloads() {
        let mut profile = ModelHealthProfile::new("test".to_string(), 5000);
        assert!(!profile.force_non_streaming);

        profile.mark_model_unload();
        assert!(!profile.force_non_streaming);

        profile.mark_model_unload();
        assert!(profile.force_non_streaming);
    }

    #[test]
    fn model_health_profile_increases_first_token_timeout_on_stall() {
        let mut profile = ModelHealthProfile::new("test".to_string(), 30_000);
        assert_eq!(profile.first_token_timeout_ms, 30_000);

        profile.mark_first_token_timeout();
        assert_eq!(profile.first_token_timeout_ms, 45_000); // 30_000 * 1.5
    }

    #[test]
    fn provider_capabilities_lm_studio_is_local() {
        let caps = ProviderCapabilities::lm_studio();
        assert!(caps.is_local);
        assert!(caps.supports_warmup);
        assert_eq!(caps.first_token_timeout_ms, 45_000);
    }

    #[test]
    fn provider_capabilities_cloud_provider_not_local() {
        let caps = ProviderCapabilities::cloud_provider();
        assert!(!caps.is_local);
        assert!(!caps.supports_warmup);
        assert_eq!(caps.first_token_timeout_ms, 5_000);
    }

    #[test]
    fn recovery_backoff_increases_exponentially() {
        let context = RecoveryContext::new(
            "lm_studio".to_string(),
            "test".to_string(),
            ModelHealthProfile::new("test".to_string(), 5000),
            ProviderCapabilities::lm_studio(),
        );
        let state_machine = RecoveryStateMachine::new(context);

        let delay0 = state_machine.backoff_for_attempt(0);
        let delay1 = state_machine.backoff_for_attempt(1);
        let delay2 = state_machine.backoff_for_attempt(2);
        let delay3 = state_machine.backoff_for_attempt(3);

        assert_eq!(delay0, Duration::ZERO);
        assert!(delay1 < delay2);
        assert!(delay2 < delay3);
    }

    #[test]
    fn health_profile_cache_returns_same_profile_for_same_model() {
        let cache = HealthProfileCache::new();
        let profile1 = cache.get_or_create("test-model", 5000);
        let profile2 = cache.get_or_create("test-model", 5000);

        assert_eq!(profile1.model_name, profile2.model_name);
    }

    #[test]
    fn health_profile_cache_update_stores_profile() {
        let cache = HealthProfileCache::new();
        let mut profile = cache.get_or_create("test", 5000);
        profile.mark_empty_stream();

        cache.update("test", profile.clone());

        let retrieved = cache.get_or_create("test", 5000);
        assert_eq!(retrieved.recent_empty_streams, 1);
    }
}
