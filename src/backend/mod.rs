use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ArtifactKind {
    CanonicalSource,
    CProjection,
    TypedExpression,
    FunctionDependencySet,
    InterfaceHash,
    ImplementationHash,
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
                | ArtifactKind::ObjectFile
                | ArtifactKind::LinkPlan
                | ArtifactKind::Executable
        )
    }
}

impl fmt::Display for ArtifactKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[allow(dead_code)]
pub(crate) struct CompilerBackendInput<'a> {
    pub artifact_kind: ArtifactKind,
    pub input_hash: &'a str,
    pub target: &'a str,
    pub options: JsonValue,
}

#[allow(dead_code)]
pub(crate) struct CompilerBackendArtifact {
    pub artifact_kind: ArtifactKind,
    pub artifact_hash: String,
    pub metadata: JsonValue,
    pub bytes: Option<Vec<u8>>,
}

#[allow(dead_code)]
pub(crate) trait CompilerBackend {
    fn backend_id(&self) -> &'static str;

    fn emit_artifact(
        &self,
        input: CompilerBackendInput<'_>,
    ) -> anyhow::Result<CompilerBackendArtifact>;
}
