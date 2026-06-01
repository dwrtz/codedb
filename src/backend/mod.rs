use serde_json::Value as JsonValue;

pub(crate) mod native;

pub(crate) use crate::artifact::ArtifactKind;
use crate::lowering::LoweredFunctionIr;

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

pub(crate) struct ObjectBackendInput<'a> {
    pub ir: &'a LoweredFunctionIr,
    pub target_triple: &'a str,
    pub exported_abi_names: Vec<String>,
}

pub(crate) struct ObjectBackendArtifact {
    pub artifact_hash: String,
    pub metadata: JsonValue,
    pub bytes: Vec<u8>,
}

pub(crate) trait ObjectBackend {
    fn backend_id(&self) -> &'static str;

    fn emit_object(&self, input: ObjectBackendInput<'_>) -> anyhow::Result<ObjectBackendArtifact>;
}
