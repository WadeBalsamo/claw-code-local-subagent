// ResilienceConfig is now canonically defined in the `runtime` crate.
// This module re-exports it so existing `api` crate consumers continue to work.
pub use runtime::ResilienceConfig;
