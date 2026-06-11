// Integration tests for local model recovery functionality
use api::{
    ErrorClassifier, HealthProfileCache, ModelHealthProfile, ProviderCapabilities, RecoveryContext,
    RecoveryStateMachine, RetryableErrorKind,
};

#[test]
fn recovery_state_machine_handles_empty_stream_with_fallback() {
    // Simulate an empty stream error on first attempt with LM Studio
    let profile = ModelHealthProfile::new("test-model".to_string(), 45_000);
    let capabilities = ProviderCapabilities::lm_studio();

    let context = RecoveryContext::new(
        "lm_studio".to_string(),
        "test-model".to_string(),
        profile,
        capabilities,
    );

    let mut state_machine = RecoveryStateMachine::new(context);

    // First attempt should be able to attempt streaming (if capabilities allow and profile doesn't degrade)
    // LM Studio has supports_streaming = Some(false), so streaming is always disabled
    state_machine.next_attempt();
    assert_eq!(state_machine.context().attempt, 1);

    // LM Studio explicitly disables streaming
    assert!(!state_machine.should_use_streaming_for_attempt(1));

    // Handle empty stream error
    state_machine.handle_empty_stream();
    assert!(state_machine.context().health_profile.recent_empty_streams > 0);

    // Second attempt: should definitely NOT use streaming
    state_machine.next_attempt();
    assert_eq!(state_machine.context().attempt, 2);
    assert!(!state_machine.should_use_streaming_for_attempt(2));
}

#[test]
fn recovery_state_machine_handles_model_unload() {
    let profile = ModelHealthProfile::new("test-model".to_string(), 45_000);
    let capabilities = ProviderCapabilities::lm_studio();

    let context = RecoveryContext::new(
        "lm_studio".to_string(),
        "test-model".to_string(),
        profile,
        capabilities,
    );

    let mut state_machine = RecoveryStateMachine::new(context);

    state_machine.next_attempt();
    state_machine.handle_model_unloaded();

    // Should mark the error for retry
    assert!(state_machine.context().health_profile.recent_model_unloads > 0);
    assert!(state_machine.has_more_attempts());
}

#[test]
fn health_profile_cache_persists_degradation_across_models() {
    let cache = HealthProfileCache::new();

    // First model gets empty stream errors
    let mut profile1 = cache.get_or_create("model-1", 45_000);
    profile1.mark_empty_stream();
    profile1.mark_empty_stream();
    cache.update("model-1", profile1.clone());

    // Retrieve and verify degradation
    let retrieved1 = cache.get_or_create("model-1", 45_000);
    assert!(!retrieved1.should_use_streaming());

    // Second model should not be affected
    let retrieved2 = cache.get_or_create("model-2", 45_000);
    assert!(retrieved2.should_use_streaming());
}

#[test]
fn provider_capabilities_for_lm_studio_support_warmup() {
    let capabilities = ProviderCapabilities::for_provider("lm-studio", "some-model");
    assert!(capabilities.is_local);
    assert!(capabilities.supports_warmup);
    assert_eq!(capabilities.max_recovery_attempts, 3);
    assert_eq!(capabilities.first_token_timeout_ms, 45_000);
}

#[test]
fn provider_capabilities_for_nemotron_disable_streaming() {
    let capabilities = ProviderCapabilities::for_provider("nemotron-3-nano", "nemotron");
    assert!(capabilities.is_local);
    assert!(!capabilities.supports_warmup);
    assert_eq!(capabilities.supports_streaming, Some(false));
    assert_eq!(capabilities.max_recovery_attempts, 2);
}

#[test]
fn provider_capabilities_for_cloud_providers_minimal_retry() {
    let caps_openai = ProviderCapabilities::for_provider("openai", "gpt-4o");
    assert!(!caps_openai.is_local);
    assert_eq!(caps_openai.first_token_timeout_ms, 5_000);

    let caps_xai = ProviderCapabilities::for_provider("xai", "grok-3");
    assert!(!caps_xai.is_local);
    assert_eq!(caps_xai.first_token_timeout_ms, 5_000);
}

#[test]
fn error_classifier_recognizes_all_retryable_types() {
    use api::ApiError;

    // Empty stream
    let empty_stream_err = ApiError::EmptyAssistantStream {
        provider: "lm_studio".to_string(),
        model: "test".to_string(),
        attempt: 1,
    };
    assert_eq!(
        ErrorClassifier::classify(&empty_stream_err, "lm_studio", None),
        RetryableErrorKind::EmptyStream
    );

    // Model unloaded
    let unloaded_err = ApiError::LocalModelUnloaded {
        provider: "lm_studio".to_string(),
        model: "test".to_string(),
        attempt: 1,
    };
    assert_eq!(
        ErrorClassifier::classify(&unloaded_err, "lm_studio", None),
        RetryableErrorKind::ModelUnloaded
    );

    // First token timeout
    let timeout_err = ApiError::FirstTokenTimeout {
        provider: "lm_studio".to_string(),
        model: "test".to_string(),
        timeout_ms: 45_000,
    };
    assert_eq!(
        ErrorClassifier::classify(&timeout_err, "lm_studio", None),
        RetryableErrorKind::FirstTokenStalled
    );
}

#[test]
fn recovery_sequence_models_realistic_scenario() {
    // Simulate a realistic local model scenario with a provider that allows streaming initially
    // (unlike LM Studio which disables it by default)
    let profile = ModelHealthProfile::new("llama2".to_string(), 45_000);
    // Create custom capabilities where streaming is explicitly allowed on attempt 1
    let mut capabilities = ProviderCapabilities::local_generic();
    capabilities.supports_streaming = Some(true); // Allow streaming for this test

    let mut state_machine = RecoveryStateMachine::new(RecoveryContext::new(
        "local".to_string(),
        "llama2".to_string(),
        profile,
        capabilities,
    ));

    // Attempt 1: streaming should be available
    state_machine.next_attempt();
    let req1_streaming = state_machine.should_use_streaming_for_attempt(1);
    // Streaming is available on attempt 1 for this provider
    assert!(
        req1_streaming
            || !state_machine
                .context()
                .health_profile
                .should_use_streaming()
    );

    // Empty stream error
    state_machine.handle_empty_stream();

    // Attempt 2: streaming definitely disabled
    state_machine.next_attempt();
    let req2_streaming = state_machine.should_use_streaming_for_attempt(2);
    assert!(!req2_streaming);

    // Success - record it
    state_machine.record_success(false);

    // Verify health profile updated
    assert!(
        state_machine
            .context()
            .health_profile
            .last_success_streaming
            == false
    );
}

#[test]
fn backoff_respects_max_attempts() {
    let profile = ModelHealthProfile::new("test".to_string(), 30_000);
    let capabilities = ProviderCapabilities::nemotron_3_nano(); // max 2 attempts

    let context = RecoveryContext::new(
        "local".to_string(),
        "test".to_string(),
        profile,
        capabilities,
    );

    let mut state_machine = RecoveryStateMachine::new(context);

    assert!(state_machine.has_more_attempts()); // Before any attempts
    state_machine.next_attempt();
    assert!(state_machine.has_more_attempts()); // After attempt 1, still have attempt 2
    state_machine.next_attempt();
    assert!(!state_machine.has_more_attempts()); // After attempt 2, no more attempts
}

#[test]
fn timeout_increases_on_repeated_stalls() {
    let mut profile = ModelHealthProfile::new("test".to_string(), 30_000);
    let initial = profile.first_token_timeout_ms;

    profile.mark_first_token_timeout();
    let after_one_stall = profile.first_token_timeout_ms;

    assert!(after_one_stall > initial);
    assert!((after_one_stall as f64) > (initial as f64) * 1.4); // Should be ~1.5x
}

#[test]
fn local_vs_cloud_provider_detection() {
    // Local should be detected
    assert!(ProviderCapabilities::for_provider("lm-studio", "model").is_local);
    assert!(ProviderCapabilities::for_provider("localhost", "model").is_local);
    assert!(ProviderCapabilities::for_provider("local-llama", "model").is_local);

    // Cloud should not be local
    assert!(!ProviderCapabilities::for_provider("openai", "model").is_local);
    assert!(!ProviderCapabilities::for_provider("xai", "model").is_local);
}
