use std::fmt;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::{ABI_TAG, COMPILER_VERSION, PIPELINE_VERSION};

pub(crate) const CACHE_KEY_SCHEMA: &str = "codedb/cache-key/v1";
pub(crate) const ARTIFACT_METADATA_SCHEMA: &str = "codedb/artifact-metadata/v1";
pub(crate) const DEFAULT_RELOCATION_MODEL: &str = "relocation:default";
pub(crate) const DEFAULT_CODE_MODEL: &str = "code-model:default";
pub(crate) const DEFAULT_OPTIMIZATION_LEVEL: &str = "opt:none";
pub(crate) const NO_RUNTIME_SENTINEL: &str = "runtime:none";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ArtifactKind {
    CanonicalSource,
    CProjection,
    TypedExpression,
    FunctionDependencySet,
    InterfaceHash,
    ImplementationHash,
    TypeLayout,
    LoweredIr,
    ObjectFile,
    LinkPlan,
    Executable,
}

impl ArtifactKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ArtifactKind::CanonicalSource => "canonical_source",
            ArtifactKind::CProjection => "c_projection",
            ArtifactKind::TypedExpression => "typed_expression",
            ArtifactKind::FunctionDependencySet => "function_dependency_set",
            ArtifactKind::InterfaceHash => "interface_hash",
            ArtifactKind::ImplementationHash => "implementation_hash",
            ArtifactKind::TypeLayout => "type_layout",
            ArtifactKind::LoweredIr => "lowered_ir",
            ArtifactKind::ObjectFile => "object_file",
            ArtifactKind::LinkPlan => "link_plan",
            ArtifactKind::Executable => "executable",
        }
    }

    pub(crate) fn from_str(value: &str) -> Option<Self> {
        match value {
            "canonical_source" => Some(ArtifactKind::CanonicalSource),
            "c_projection" => Some(ArtifactKind::CProjection),
            "typed_expression" => Some(ArtifactKind::TypedExpression),
            "function_dependency_set" => Some(ArtifactKind::FunctionDependencySet),
            "interface_hash" => Some(ArtifactKind::InterfaceHash),
            "implementation_hash" => Some(ArtifactKind::ImplementationHash),
            "type_layout" => Some(ArtifactKind::TypeLayout),
            "lowered_ir" => Some(ArtifactKind::LoweredIr),
            "object_file" => Some(ArtifactKind::ObjectFile),
            "link_plan" => Some(ArtifactKind::LinkPlan),
            "executable" => Some(ArtifactKind::Executable),
            _ => None,
        }
    }

    pub(crate) fn is_compiler_artifact(self) -> bool {
        matches!(
            self,
            ArtifactKind::LoweredIr
                | ArtifactKind::TypeLayout
                | ArtifactKind::ObjectFile
                | ArtifactKind::LinkPlan
                | ArtifactKind::Executable
        )
    }

    pub(crate) fn requires_artifact_bytes(self) -> bool {
        matches!(self, ArtifactKind::ObjectFile | ArtifactKind::Executable)
    }
}

impl fmt::Display for ArtifactKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CacheKeyInput {
    pub schema: String,
    pub artifact_kind: ArtifactKind,
    pub input_hash: String,
    pub dependency_interface_hashes: Vec<String>,
    pub dependency_implementation_hashes: Vec<String>,
    pub backend_id: String,
    pub target_triple: String,
    pub abi_tag: String,
    pub relocation_model: String,
    pub code_model: String,
    pub optimization_level: String,
    pub compiler_version: String,
    pub pipeline_version: String,
    pub runtime_sentinel: String,
}

impl CacheKeyInput {
    pub(crate) fn new(
        artifact_kind: ArtifactKind,
        input_hash: impl Into<String>,
        backend_id: impl Into<String>,
        target_triple: impl Into<String>,
    ) -> Self {
        Self {
            schema: CACHE_KEY_SCHEMA.to_string(),
            artifact_kind,
            input_hash: input_hash.into(),
            dependency_interface_hashes: vec![],
            dependency_implementation_hashes: vec![],
            backend_id: backend_id.into(),
            target_triple: target_triple.into(),
            abi_tag: ABI_TAG.to_string(),
            relocation_model: DEFAULT_RELOCATION_MODEL.to_string(),
            code_model: DEFAULT_CODE_MODEL.to_string(),
            optimization_level: DEFAULT_OPTIMIZATION_LEVEL.to_string(),
            compiler_version: COMPILER_VERSION.to_string(),
            pipeline_version: PIPELINE_VERSION.to_string(),
            runtime_sentinel: NO_RUNTIME_SENTINEL.to_string(),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn with_dependency_interface_hashes(
        mut self,
        dependency_interface_hashes: Vec<String>,
    ) -> Self {
        self.dependency_interface_hashes = dependency_interface_hashes;
        self.normalized()
    }

    #[allow(dead_code)]
    pub(crate) fn with_dependency_implementation_hashes(
        mut self,
        dependency_implementation_hashes: Vec<String>,
    ) -> Self {
        self.dependency_implementation_hashes = dependency_implementation_hashes;
        self.normalized()
    }

    pub(crate) fn normalized(mut self) -> Self {
        self.dependency_interface_hashes.sort();
        self.dependency_interface_hashes.dedup();
        self.dependency_implementation_hashes.sort();
        self.dependency_implementation_hashes.dedup();
        self
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if self.schema != CACHE_KEY_SCHEMA {
            bail!("cache key schema must be {CACHE_KEY_SCHEMA}");
        }
        for (label, value) in [
            ("input_hash", &self.input_hash),
            ("backend_id", &self.backend_id),
            ("target_triple", &self.target_triple),
            ("abi_tag", &self.abi_tag),
            ("relocation_model", &self.relocation_model),
            ("code_model", &self.code_model),
            ("optimization_level", &self.optimization_level),
            ("compiler_version", &self.compiler_version),
            ("pipeline_version", &self.pipeline_version),
            ("runtime_sentinel", &self.runtime_sentinel),
        ] {
            if value.is_empty() {
                bail!("cache key {label} is empty");
            }
        }
        if !self.runtime_sentinel.starts_with("runtime:") {
            bail!("cache key runtime_sentinel must be explicit");
        }
        Ok(())
    }
}
