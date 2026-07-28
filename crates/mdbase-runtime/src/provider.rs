use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use mdbase_interop::{ActionCancellation, ActionInvocation, ActionOutcome};

use crate::model::{ActionDispatch, DispatchFailure};

#[async_trait]
pub trait ActionProvider: Send + Sync {
    async fn dispatch(
        &self,
        invocation: ActionInvocation,
    ) -> Result<ActionOutcome, DispatchFailure>;

    async fn cancel(&self, _request: ActionCancellation) -> Result<(), DispatchFailure> {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorizationDecision {
    Allow,
    Deny { code: String, message: String },
}

#[async_trait]
pub trait DispatchAuthorizer: Send + Sync {
    async fn authorize(&self, request: &ActionDispatch) -> AuthorizationDecision;
}

/// Safe default used by [`crate::RuntimeBuilder`].
#[derive(Debug, Default)]
pub struct DenyAllAuthorizer;

#[async_trait]
impl DispatchAuthorizer for DenyAllAuthorizer {
    async fn authorize(&self, _request: &ActionDispatch) -> AuthorizationDecision {
        AuthorizationDecision::Deny {
            code: "host_authorization_required".to_string(),
            message: "The embedding host has not installed a dispatch authorizer.".to_string(),
        }
    }
}

#[derive(Default, Clone)]
pub struct ProviderRegistry {
    providers: Arc<RwLock<BTreeMap<ProviderBinding, Arc<dyn ActionProvider>>>>,
}

/// Executable host binding for one exact, already-verified interoperability
/// provider declaration and handler. Markdown records cannot create this key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProviderBinding {
    pub provider_declaration_digest: String,
    pub handler_id: String,
}

impl ProviderRegistry {
    pub fn register(
        &self,
        binding: ProviderBinding,
        provider: Arc<dyn ActionProvider>,
    ) -> Option<Arc<dyn ActionProvider>> {
        self.providers
            .write()
            .expect("provider registry lock poisoned")
            .insert(binding, provider)
    }

    pub fn register_handlers(
        &self,
        provider_declaration_digest: impl Into<String>,
        handler_ids: impl IntoIterator<Item = String>,
        provider: Arc<dyn ActionProvider>,
    ) {
        let provider_declaration_digest = provider_declaration_digest.into();
        let mut providers = self
            .providers
            .write()
            .expect("provider registry lock poisoned");
        for handler_id in handler_ids {
            providers.insert(
                ProviderBinding {
                    provider_declaration_digest: provider_declaration_digest.clone(),
                    handler_id,
                },
                provider.clone(),
            );
        }
    }

    pub fn unregister(&self, binding: &ProviderBinding) -> Option<Arc<dyn ActionProvider>> {
        self.providers
            .write()
            .expect("provider registry lock poisoned")
            .remove(binding)
    }

    pub fn get(
        &self,
        provider_declaration_digest: &str,
        handler_id: &str,
    ) -> Option<Arc<dyn ActionProvider>> {
        self.providers
            .read()
            .expect("provider registry lock poisoned")
            .get(&ProviderBinding {
                provider_declaration_digest: provider_declaration_digest.to_string(),
                handler_id: handler_id.to_string(),
            })
            .cloned()
    }

    pub fn bindings(&self) -> Vec<ProviderBinding> {
        self.providers
            .read()
            .expect("provider registry lock poisoned")
            .keys()
            .cloned()
            .collect()
    }
}
