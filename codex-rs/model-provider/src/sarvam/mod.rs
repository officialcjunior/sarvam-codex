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
    auth_manager: Option<Arc<AuthManager>>,
}

impl SarvamModelProvider {
    pub(crate) fn new(info: ModelProviderInfo, auth_manager: Option<Arc<AuthManager>>) -> Self {
        Self { info, auth_manager }
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
        self.auth_manager.clone()
    }

    async fn auth(&self) -> Option<CodexAuth> {
        self.auth_manager.as_ref().and_then(|am| am.auth_cached())
    }

    fn account_state(&self) -> ProviderAccountResult {
        let account = if self.info.requires_openai_auth {
            self.auth_manager
                .as_ref()
                .and_then(|auth_manager| {
                    let auth = auth_manager.auth_cached()?;
                    Some(auth)
                })
                .map(|auth| match &auth {
                    CodexAuth::ApiKey(_) => Ok(codex_protocol::account::ProviderAccount::ApiKey),
                    _ => Ok(codex_protocol::account::ProviderAccount::ApiKey),
                })
                .transpose()?
        } else {
            None
        };

        Ok(ProviderAccountState {
            account,
            requires_openai_auth: self.info.requires_openai_auth,
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
