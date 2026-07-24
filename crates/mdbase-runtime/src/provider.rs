use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;

use crate::model::{ActionDispatch, ActionResponse, DispatchFailure};

#[async_trait]
pub trait ActionProvider: Send + Sync {
    async fn dispatch(&self, request: ActionDispatch) -> Result<ActionResponse, DispatchFailure>;

    async fn cancel(&self, _invocation_id: &str) -> Result<(), DispatchFailure> {
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
    providers: Arc<RwLock<BTreeMap<String, Arc<dyn ActionProvider>>>>,
}

impl ProviderRegistry {
    pub fn register(
        &self,
        action: impl Into<String>,
        provider: Arc<dyn ActionProvider>,
    ) -> Option<Arc<dyn ActionProvider>> {
        self.providers
            .write()
            .expect("provider registry lock poisoned")
            .insert(action.into(), provider)
    }

    pub fn register_many(
        &self,
        actions: impl IntoIterator<Item = String>,
        provider: Arc<dyn ActionProvider>,
    ) {
        let mut providers = self
            .providers
            .write()
            .expect("provider registry lock poisoned");
        for action in actions {
            providers.insert(action, provider.clone());
        }
    }

    pub fn unregister(&self, action: &str) -> Option<Arc<dyn ActionProvider>> {
        self.providers
            .write()
            .expect("provider registry lock poisoned")
            .remove(action)
    }

    pub fn get(&self, action: &str) -> Option<Arc<dyn ActionProvider>> {
        self.providers
            .read()
            .expect("provider registry lock poisoned")
            .get(action)
            .cloned()
    }

    pub fn actions(&self) -> Vec<String> {
        self.providers
            .read()
            .expect("provider registry lock poisoned")
            .keys()
            .cloned()
            .collect()
    }
}
