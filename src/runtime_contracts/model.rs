use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Independently versioned runtime contract profile implemented by this module.
pub const RUNTIME_PROFILE_VERSION: &str = "0.1.0";

/// Canonical runtime record kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ContractKind {
    Provider,
    Action,
    Event,
    Capability,
    Workflow,
    RuntimePolicy,
    RuntimeRun,
    RuntimeCheckpoint,
    RuntimeTimer,
    RuntimeDiagnostic,
}

impl ContractKind {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "provider" => Some(Self::Provider),
            "action" => Some(Self::Action),
            "event" => Some(Self::Event),
            "capability" => Some(Self::Capability),
            "workflow" => Some(Self::Workflow),
            "runtime_policy" => Some(Self::RuntimePolicy),
            "runtime_run" => Some(Self::RuntimeRun),
            "runtime_checkpoint" => Some(Self::RuntimeCheckpoint),
            "runtime_timer" => Some(Self::RuntimeTimer),
            "runtime_diagnostic" => Some(Self::RuntimeDiagnostic),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Provider => "provider",
            Self::Action => "action",
            Self::Event => "event",
            Self::Capability => "capability",
            Self::Workflow => "workflow",
            Self::RuntimePolicy => "runtime_policy",
            Self::RuntimeRun => "runtime_run",
            Self::RuntimeCheckpoint => "runtime_checkpoint",
            Self::RuntimeTimer => "runtime_timer",
            Self::RuntimeDiagnostic => "runtime_diagnostic",
        }
    }

    pub fn is_registry_contract(self) -> bool {
        matches!(
            self,
            Self::Provider
                | Self::Action
                | Self::Event
                | Self::Capability
                | Self::Workflow
                | Self::RuntimePolicy
        )
    }
}

/// One materialized or virtual contract document.
#[derive(Debug, Clone, PartialEq)]
pub struct ContractDocument {
    pub path: String,
    pub frontmatter: Value,
    pub body: String,
}

impl ContractDocument {
    pub fn new(path: impl Into<String>, frontmatter: Value) -> Self {
        Self {
            path: path.into(),
            frontmatter,
            body: String::new(),
        }
    }

    /// Construct a non-materialized contract. No collection path is required.
    pub fn virtual_contract(frontmatter: Value) -> Self {
        let id = frontmatter
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        Self::new(format!("<virtual:{id}>"), frontmatter)
    }

    pub fn kind(&self) -> Option<ContractKind> {
        self.frontmatter
            .get("type")
            .and_then(Value::as_str)
            .and_then(ContractKind::parse)
    }

    pub fn id(&self) -> Option<&str> {
        self.frontmatter.get("id").and_then(Value::as_str)
    }
}

/// Deterministic registry source tiers, ordered by the runtime profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceKind {
    BuiltIn,
    Provider { id: String },
    Pack { id: String },
    Collection,
}

impl SourceKind {
    pub(crate) fn sort_key(&self) -> (u8, &str) {
        match self {
            Self::BuiltIn => (0, ""),
            Self::Provider { id } => (1, id),
            Self::Pack { id } => (2, id),
            Self::Collection => (3, ""),
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::BuiltIn => "built-in".to_string(),
            Self::Provider { id } => format!("provider:{id}"),
            Self::Pack { id } => format!("pack:{id}"),
            Self::Collection => "collection".to_string(),
        }
    }
}

/// A set of documents supplied atomically by one registry source.
#[derive(Debug, Clone, PartialEq)]
pub struct ContractSource {
    pub kind: SourceKind,
    pub documents: Vec<ContractDocument>,
}

impl ContractSource {
    pub fn built_in(documents: Vec<ContractDocument>) -> Self {
        Self {
            kind: SourceKind::BuiltIn,
            documents,
        }
    }

    pub fn provider(id: impl Into<String>, documents: Vec<ContractDocument>) -> Self {
        Self {
            kind: SourceKind::Provider { id: id.into() },
            documents,
        }
    }

    pub fn pack(id: impl Into<String>, documents: Vec<ContractDocument>) -> Self {
        Self {
            kind: SourceKind::Pack { id: id.into() },
            documents,
        }
    }

    pub fn collection(documents: Vec<ContractDocument>) -> Self {
        Self {
            kind: SourceKind::Collection,
            documents,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractOrigin {
    pub source: String,
    pub location: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeDiagnostic {
    pub severity: String,
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl RuntimeDiagnostic {
    pub(crate) fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: "error".to_string(),
            code: code.into(),
            message: message.into(),
            path: None,
            id: None,
            field: None,
            details: None,
        }
    }

    pub(crate) fn at_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    pub(crate) fn for_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    pub(crate) fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValidationResult {
    pub valid: bool,
    pub diagnostics: Vec<RuntimeDiagnostic>,
}

impl ValidationResult {
    pub(crate) fn new(diagnostics: Vec<RuntimeDiagnostic>) -> Self {
        Self {
            valid: !diagnostics
                .iter()
                .any(|diagnostic| diagnostic.severity == "error"),
            diagnostics,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct RuntimePackage {
    pub type_files: Vec<ContractDocument>,
    pub records: Vec<ContractDocument>,
    pub providers: Vec<ContractDocument>,
    pub actions: Vec<ContractDocument>,
    pub events: Vec<ContractDocument>,
    pub capabilities: Vec<ContractDocument>,
    pub workflows: Vec<ContractDocument>,
    pub policies: Vec<ContractDocument>,
    pub runs: Vec<ContractDocument>,
    pub checkpoints: Vec<ContractDocument>,
    pub timers: Vec<ContractDocument>,
    pub runtime_diagnostics: Vec<ContractDocument>,
    pub diagnostics: Vec<RuntimeDiagnostic>,
}

impl RuntimePackage {
    pub fn valid(&self) -> bool {
        !self
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == "error")
    }

    pub(crate) fn push(&mut self, document: ContractDocument) {
        match document.kind() {
            Some(ContractKind::Provider) => self.providers.push(document.clone()),
            Some(ContractKind::Action) => self.actions.push(document.clone()),
            Some(ContractKind::Event) => self.events.push(document.clone()),
            Some(ContractKind::Capability) => self.capabilities.push(document.clone()),
            Some(ContractKind::Workflow) => self.workflows.push(document.clone()),
            Some(ContractKind::RuntimePolicy) => self.policies.push(document.clone()),
            Some(ContractKind::RuntimeRun) => self.runs.push(document.clone()),
            Some(ContractKind::RuntimeCheckpoint) => self.checkpoints.push(document.clone()),
            Some(ContractKind::RuntimeTimer) => self.timers.push(document.clone()),
            Some(ContractKind::RuntimeDiagnostic) => {
                self.runtime_diagnostics.push(document.clone())
            }
            None => return,
        }
        self.records.push(document);
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContractEntry {
    pub contract: Value,
    pub origins: Vec<ContractOrigin>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkflowResolution {
    pub workflow: String,
    pub events: BTreeSet<String>,
    pub actions: BTreeSet<String>,
    pub providers: BTreeSet<String>,
    pub capabilities: BTreeSet<String>,
    pub executor: Option<String>,
    pub valid: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PreflightReport {
    pub valid: bool,
    pub workflows: BTreeMap<String, WorkflowResolution>,
    pub diagnostics: Vec<RuntimeDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicySelector {
    Id(String),
    Path(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ComposeOptions {
    /// Locally selected policies. Normal collection config supplies at most
    /// one; accepting a list lets preflight diagnose ambiguous hosts.
    pub selected_policies: Vec<PolicySelector>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoadOptions {
    /// Overrides `runtime.policy` from `mdbase.yaml` when non-empty.
    pub selected_policies: Vec<PolicySelector>,
}

#[derive(Debug)]
pub struct RuntimeLoadResult {
    pub contracts: RuntimePackage,
    pub registry: super::RuntimeRegistry,
    pub preflight: PreflightReport,
}

impl RuntimeLoadResult {
    pub fn valid(&self) -> bool {
        self.preflight.valid
    }
}
