mod catalog;

use std::path::PathBuf;
use std::sync::Arc;

use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_model_provider_info::ModelProviderInfo;
use codex_models_manager::manager::SharedModelsManager;
use codex_models_manager::manager::StaticModelsManager;
use codex_protocol::openai_models::ModelsResponse;

use crate::provider::ModelProvider;
use crate::provider::ProviderAccountResult;
use crate::provider::ProviderAccountState;
use crate::provider::ProviderCapabilities;
use catalog::static_model_catalog;

/// Runtime provider for Sarvam AI's Chat Completions endpoint.
///
/// Uses a built-in static model catalog so users can switch between
/// sarvam-30b and sarvam-105b in the TUI without any external config.
#[derive(Clone, Debug)]
pub(crate) struct SarvamModelProvider {
    info: ModelProviderInfo,
}

impl SarvamModelProvider {
    pub(crate) fn new(info: ModelProviderInfo) -> Self {
        Self { info }
    }
}

#[async_trait::async_trait]
impl ModelProvider for SarvamModelProvider {
    fn info(&self) -> &ModelProviderInfo {
        &self.info
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            // Sarvam only supports function tools — no namespace/image/web search.
            namespace_tools: false,
            image_generation: false,
            web_search: false,
        }
    }

    fn auth_manager(&self) -> Option<Arc<AuthManager>> {
        None
    }

    async fn auth(&self) -> Option<CodexAuth> {
        None
    }

    fn account_state(&self) -> ProviderAccountResult {
        Ok(ProviderAccountState {
            account: None,
            requires_openai_auth: false,
        })
    }

    fn models_manager(
        &self,
        _codex_home: PathBuf,
        config_model_catalog: Option<ModelsResponse>,
    ) -> SharedModelsManager {
        // User can still override with model_catalog_json; otherwise use built-in catalog.
        Arc::new(StaticModelsManager::new(
            /*auth_manager*/ None,
            config_model_catalog.unwrap_or_else(static_model_catalog),
        ))
    }
}
