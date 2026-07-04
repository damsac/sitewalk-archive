//! `MurmurEngine` + `EngineConfig` + provider routing (Plan 07 D11). The
//! entry point Swift constructs once per app; `begin_walk` (Task 7) hands out
//! per-session `WalkSession` objects.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use harness::{AnthropicProvider, FileMemoryStore, LlmProvider, Memory, MemoryStore};
use murmur_core::Store;

/// Config crossing the FFI boundary. `api_key` is an opaque `String` from the
/// iOS Keychain and must NEVER be logged — `Debug` is hand-written (never
/// derived) so it always redacts the key, even if a field is added later.
#[derive(uniffi::Record, Clone)]
pub struct EngineConfig {
    pub db_path: String,
    pub device_id: String,
    pub api_key: String,
    pub base_url: Option<String>,
    pub model_live: String,
    pub model_processing: String,
    pub model_reflection: String,
}

impl std::fmt::Debug for EngineConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineConfig")
            .field("db_path", &self.db_path)
            .field("device_id", &self.device_id)
            .field("api_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .field("model_live", &self.model_live)
            .field("model_processing", &self.model_processing)
            .field("model_reflection", &self.model_reflection)
            .finish()
    }
}

/// Three routing purposes (D11): `live` (cheap), `processing` (strong),
/// `reflection` (cheap). One `AnthropicProvider` per distinct (model, key,
/// base_url), `Arc`-deduped across purposes that share a model.
///
/// `pub` (not `pub(crate)`) so `crates/ffi/tests/bridge_e2e.rs` can inject
/// mock providers via `MurmurEngine::with_providers` — never crosses FFI (no
/// `#[uniffi::export]`), so it doesn't affect the generated Swift bindings.
#[doc(hidden)]
pub struct Providers {
    pub live: Arc<dyn LlmProvider>,
    pub processing: Arc<dyn LlmProvider>,
    pub reflection: Arc<dyn LlmProvider>,
}

fn build_providers(config: &EngineConfig) -> Providers {
    let mut cache: HashMap<String, Arc<dyn LlmProvider>> = HashMap::new();
    let mut make = |model: &str| -> Arc<dyn LlmProvider> {
        cache
            .entry(model.to_string())
            .or_insert_with(|| {
                let mut provider = AnthropicProvider::new(config.api_key.clone(), model.to_string());
                if let Some(base) = &config.base_url {
                    provider = provider.with_base_url(base.clone());
                }
                Arc::new(provider) as Arc<dyn LlmProvider>
            })
            .clone()
    };
    Providers {
        live: make(&config.model_live),
        processing: make(&config.model_processing),
        reflection: make(&config.model_reflection),
    }
}

/// The FFI entry point. One per app; `begin_walk` (Task 7) hands out
/// per-session `WalkSession`s.
// Fields are read by `begin_walk` (Task 7), which is deliberately deferred
// out of this task so `cargo test -p ffi engine` compiles standalone.
#[derive(uniffi::Object)]
pub struct MurmurEngine {
    pub(crate) store: Arc<Mutex<Store>>,
    pub(crate) memory: Arc<Mutex<Memory>>,
    pub(crate) memory_store: Arc<dyn MemoryStore>,
    pub(crate) providers: Providers,
    /// Handle used to spawn live-extraction ticks from the SYNC
    /// `append_transcript` export (D7: fire-and-forget — the tick runs off
    /// whatever executor called us, which for a plain sync FFI export is not
    /// guaranteed to be a tokio context). Production owns the `Runtime` that
    /// backs this handle (`_runtime`, kept alive for the engine's lifetime);
    /// tests borrow the `#[tokio::test]` runtime instead of spinning up a
    /// second one.
    pub(crate) runtime_handle: tokio::runtime::Handle,
    _runtime: Option<Arc<tokio::runtime::Runtime>>,
}

#[uniffi::export]
impl MurmurEngine {
    #[uniffi::constructor]
    pub fn new(config: EngineConfig) -> Arc<Self> {
        let store = Store::open(&config.db_path, config.device_id.clone())
            .expect("cannot open store at the given db_path");
        let memory_store: Arc<dyn MemoryStore> =
            Arc::new(FileMemoryStore::new(format!("{}.memory.json", config.db_path)));
        let memory = memory_store.load().unwrap_or_default();
        let providers = build_providers(&config);
        let runtime = Arc::new(
            tokio::runtime::Runtime::new().expect("cannot start the bridge's tokio runtime"),
        );
        let runtime_handle = runtime.handle().clone();
        Arc::new(MurmurEngine {
            store: Arc::new(Mutex::new(store)),
            memory: Arc::new(Mutex::new(memory)),
            memory_store,
            providers,
            runtime_handle,
            _runtime: Some(runtime),
        })
    }
}

impl MurmurEngine {
    /// Test-only constructor injecting mock providers (never crosses FFI —
    /// no `#[uniffi::export]`). Lets unit tests AND the `bridge_e2e`
    /// integration test exercise the bridge without a network provider.
    /// Borrows the calling `#[tokio::test]` runtime rather than spinning up a
    /// second one. `pub`, not `#[cfg(test)]`, because an integration test
    /// binary compiles this crate as an ordinary dependency — `#[cfg(test)]`
    /// items would not exist for it to call.
    #[doc(hidden)]
    pub fn with_providers(
        store: Store,
        memory: Memory,
        memory_store: Arc<dyn MemoryStore>,
        providers: Providers,
    ) -> Arc<Self> {
        Arc::new(MurmurEngine {
            store: Arc::new(Mutex::new(store)),
            memory: Arc::new(Mutex::new(memory)),
            memory_store,
            providers,
            runtime_handle: tokio::runtime::Handle::current(),
            _runtime: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_debug_redacts_the_api_key() {
        let cfg = EngineConfig {
            db_path: ":memory:".into(),
            device_id: "dev".into(),
            api_key: "sk-super-secret".into(),
            base_url: None,
            model_live: "claude-haiku-4-5".into(),
            model_processing: "claude-sonnet-4-5".into(),
            model_reflection: "claude-haiku-4-5".into(),
        };
        assert!(!format!("{cfg:?}").contains("sk-super-secret"), "api key must never be printable");
    }

    #[test]
    fn providers_dedupe_by_model() {
        let cfg = EngineConfig {
            db_path: ":memory:".into(),
            device_id: "dev".into(),
            api_key: "sk-test".into(),
            base_url: None,
            model_live: "claude-haiku-4-5".into(),
            model_processing: "claude-sonnet-4-5".into(),
            model_reflection: "claude-haiku-4-5".into(),
        };
        let providers = build_providers(&cfg);
        assert!(Arc::ptr_eq(&providers.live, &providers.reflection), "same model shares one Arc");
        assert!(!Arc::ptr_eq(&providers.live, &providers.processing));
    }
}
