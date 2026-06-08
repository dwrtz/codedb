use std::collections::BTreeSet;

use anyhow::{Result, anyhow, bail};
use serde_json::{Value as JsonValue, json};

use crate::artifact::CacheKeyInput;
use crate::backend::ArtifactKind;
use crate::model::{ProgramRootPayload, resolve_named_type_in_root};
use crate::store::{CodeDb, canonical_json};
use crate::types::{
    TypeDefinition, TypeFieldSpec, TypeMemberDef, TypeSpec, type_hash_for, type_payload_for_spec,
};
use crate::{ABI_TAG, APPLE_ARM64_TARGET, LINUX_X86_64_TARGET, MAIN_BRANCH};

pub(crate) const TYPE_LAYOUT_SCHEMA: &str = "codedb/type-layout/v2";
// The version tag (`:v2`) in `TYPE_LAYOUT_BACKEND_ID` is what versions the
// layout cache key: bumping it invalidates every cached layout. `LAYOUT_VERSION`
// records the same tag in artifact metadata. The two MUST carry the same tag, so
// a layout-format change invalidates caches rather than silently reusing stale
// layouts; `layout_cache_key_is_versioned` guards against drift.
pub(crate) const TYPE_LAYOUT_BACKEND_ID: &str = "type-layout:v2";
pub(crate) const LAYOUT_VERSION: &str = "layout:v2";

#[derive(Debug, Clone)]
pub(crate) struct TypeLayoutResult {
    pub(crate) metadata: JsonValue,
    pub(crate) dependency_type_def_hashes: Vec<String>,
}

#[derive(Debug, Clone)]
struct ComputedLayout {
    metadata: JsonValue,
    size_bytes: u64,
    align_bytes: u64,
    class: LayoutClass,
}

#[derive(Debug, Clone, Copy)]
struct TargetLayout {
    pointer_size_bytes: u64,
    pointer_align_bytes: u64,
    enum_tag_size_bytes: u64,
    enum_tag_align_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CopyKind {
    Copy,
    MoveOnly,
}

impl CopyKind {
    fn as_str(self) -> &'static str {
        match self {
            CopyKind::Copy => "copy",
            CopyKind::MoveOnly => "move_only",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DropKind {
    Trivial,
    NeedsDrop,
}

impl DropKind {
    fn as_str(self) -> &'static str {
        match self {
            DropKind::Trivial => "trivial",
            DropKind::NeedsDrop => "needs_drop",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LayoutClass {
    copy_kind: CopyKind,
    drop_kind: DropKind,
    contains_reference: bool,
    contains_mut_reference: bool,
    contains_raw_pointer: bool,
    contains_box: bool,
    contains_capability_handle: bool,
}

impl LayoutClass {
    fn copy() -> Self {
        Self {
            copy_kind: CopyKind::Copy,
            drop_kind: DropKind::Trivial,
            contains_reference: false,
            contains_mut_reference: false,
            contains_raw_pointer: false,
            contains_box: false,
            contains_capability_handle: false,
        }
    }

    fn shared_reference() -> Self {
        Self {
            contains_reference: true,
            ..Self::copy()
        }
    }

    fn mutable_reference() -> Self {
        Self {
            copy_kind: CopyKind::MoveOnly,
            contains_reference: true,
            contains_mut_reference: true,
            ..Self::copy()
        }
    }

    fn raw_pointer() -> Self {
        Self {
            contains_raw_pointer: true,
            ..Self::copy()
        }
    }

    fn boxed(payload: Self) -> Self {
        Self {
            copy_kind: CopyKind::MoveOnly,
            drop_kind: DropKind::NeedsDrop,
            contains_reference: payload.contains_reference,
            contains_mut_reference: payload.contains_mut_reference,
            contains_raw_pointer: payload.contains_raw_pointer,
            contains_box: true,
            contains_capability_handle: payload.contains_capability_handle,
        }
    }

    fn owned_buffer(payload: Self) -> Self {
        Self {
            copy_kind: CopyKind::MoveOnly,
            drop_kind: DropKind::NeedsDrop,
            contains_reference: payload.contains_reference,
            contains_mut_reference: payload.contains_mut_reference,
            contains_raw_pointer: payload.contains_raw_pointer,
            // Existing native drop helpers use this as the recursive owned-drop
            // fast path, not only for the literal `box<T>` type.
            contains_box: true,
            contains_capability_handle: payload.contains_capability_handle,
        }
    }

    fn merge(self, other: Self) -> Self {
        Self {
            copy_kind: if self.copy_kind == CopyKind::Copy && other.copy_kind == CopyKind::Copy {
                CopyKind::Copy
            } else {
                CopyKind::MoveOnly
            },
            drop_kind: if self.drop_kind == DropKind::NeedsDrop
                || other.drop_kind == DropKind::NeedsDrop
            {
                DropKind::NeedsDrop
            } else {
                DropKind::Trivial
            },
            contains_reference: self.contains_reference || other.contains_reference,
            contains_mut_reference: self.contains_mut_reference || other.contains_mut_reference,
            contains_raw_pointer: self.contains_raw_pointer || other.contains_raw_pointer,
            contains_box: self.contains_box || other.contains_box,
            contains_capability_handle: self.contains_capability_handle
                || other.contains_capability_handle,
        }
    }
}

#[derive(Debug, Clone)]
struct LayoutMember {
    symbol: Option<String>,
    name: String,
    type_hash: String,
}

struct LayoutComputer<'a> {
    db: &'a CodeDb,
    root: &'a ProgramRootPayload,
    target_triple: &'a str,
    target: TargetLayout,
    dependency_type_def_hashes: BTreeSet<String>,
    active_types: BTreeSet<String>,
}

impl CodeDb {
    pub fn emit_type_layout_main_branch(
        &mut self,
        type_source: &str,
        target: &str,
    ) -> Result<String> {
        self.ensure_initialized()?;
        let branch = self.branch(MAIN_BRANCH)?;
        let root = self.load_root(&branch.root_hash)?;
        let type_hash = match self.resolve_type_in_root(MAIN_BRANCH, &root, type_source) {
            Ok(type_hash) => type_hash,
            Err(err) => self
                .type_definition_layout_hash(&root, type_source)
                .map_err(|_| err)?,
        };
        let layout = self.compute_type_layout(&root, &type_hash, target)?;
        let key_input = type_layout_cache_key_input(
            &type_hash,
            target,
            layout.dependency_type_def_hashes.clone(),
        );
        self.write_cache_json_for_key(key_input, &layout.metadata)?;
        Ok(format!("{}\n", canonical_json(&layout.metadata)))
    }

    fn type_definition_layout_hash(
        &mut self,
        root: &ProgramRootPayload,
        type_name: &str,
    ) -> Result<String> {
        let type_symbol = resolve_named_type_in_root(root, MAIN_BRANCH, type_name)
            .ok_or_else(|| anyhow!("unknown type {type_name}"))?;
        let entry = self
            .root_type(root, &type_symbol)
            .ok_or_else(|| anyhow!("type {type_name} missing root definition"))?;
        let definition = self.type_definition(&entry.type_def)?;
        let payload = type_payload_for_spec(&TypeSpec::Named {
            type_symbol,
            region_args: definition
                .region_params()
                .iter()
                .map(|param| param.region.clone())
                .collect(),
        })?;
        self.put_object("Type", &payload)
    }

    pub(crate) fn compute_type_layout(
        &self,
        root: &ProgramRootPayload,
        type_hash: &str,
        target_triple: &str,
    ) -> Result<TypeLayoutResult> {
        let target = target_layout(target_triple)?;
        let mut computer = LayoutComputer {
            db: self,
            root,
            target_triple,
            target,
            dependency_type_def_hashes: BTreeSet::new(),
            active_types: BTreeSet::new(),
        };
        let mut layout = computer.layout_type(type_hash)?;
        let dependency_type_def_hashes = computer
            .dependency_type_def_hashes
            .into_iter()
            .collect::<Vec<_>>();
        layout
            .metadata
            .as_object_mut()
            .ok_or_else(|| anyhow!("computed type layout metadata is not a JSON object"))?
            .insert(
                "type_dependency_hashes".to_string(),
                json!(dependency_type_def_hashes),
            );
        Ok(TypeLayoutResult {
            metadata: layout.metadata,
            dependency_type_def_hashes,
        })
    }
}

pub(crate) fn type_layout_cache_key_input(
    type_hash: &str,
    target_triple: &str,
    dependency_type_def_hashes: Vec<String>,
) -> CacheKeyInput {
    CacheKeyInput::new(
        ArtifactKind::TypeLayout,
        type_hash,
        TYPE_LAYOUT_BACKEND_ID,
        target_triple,
    )
    .with_dependency_implementation_hashes(dependency_type_def_hashes)
}

impl LayoutComputer<'_> {
    fn layout_type(&mut self, type_hash: &str) -> Result<ComputedLayout> {
        if !self.active_types.insert(type_hash.to_string()) {
            bail!("recursive type layout is not supported for {type_hash}");
        }
        let result = self.layout_type_inner(type_hash);
        self.active_types.remove(type_hash);
        result
    }

    fn layout_type_inner(&mut self, type_hash: &str) -> Result<ComputedLayout> {
        match self.db.type_spec(type_hash)? {
            TypeSpec::Builtin(kind) => self.layout_builtin(type_hash, &kind),
            TypeSpec::Named {
                type_symbol,
                region_args,
            } => {
                let entry = self
                    .db
                    .root_type(self.root, &type_symbol)
                    .ok_or_else(|| anyhow!("named type missing from root {type_symbol}"))?;
                let definition = self.db.type_definition(&entry.type_def)?;
                if definition.region_params().len() != region_args.len() {
                    bail!(
                        "named type {type_symbol} expects {} region args, got {}",
                        definition.region_params().len(),
                        region_args.len()
                    );
                }
                self.dependency_type_def_hashes
                    .insert(entry.type_def.clone());
                match definition {
                    TypeDefinition::Record { fields, .. } => {
                        let TypeSpec::Record(expanded_fields) =
                            self.db.type_spec_in_root(self.root, type_hash)?
                        else {
                            bail!("named record did not expand to record layout members");
                        };
                        self.layout_record(
                            type_hash,
                            Some(type_symbol),
                            Some(entry.type_def.clone()),
                            record_members(fields, expanded_fields)?,
                        )
                    }
                    TypeDefinition::Enum { variants, .. } => {
                        let TypeSpec::Enum(expanded_variants) =
                            self.db.type_spec_in_root(self.root, type_hash)?
                        else {
                            bail!("named enum did not expand to enum layout members");
                        };
                        self.layout_enum(
                            type_hash,
                            Some(type_symbol),
                            Some(entry.type_def.clone()),
                            enum_members(variants, expanded_variants)?,
                        )
                    }
                }
            }
            TypeSpec::Reference {
                region,
                mutable,
                referent,
            } => {
                self.db.type_spec(&referent)?;
                let class = if mutable {
                    LayoutClass::mutable_reference()
                } else {
                    LayoutClass::shared_reference()
                };
                let mut metadata = self.base_metadata(
                    type_hash,
                    "reference",
                    self.target.pointer_size_bytes,
                    self.target.pointer_align_bytes,
                    class,
                );
                let object = metadata.as_object_mut().unwrap();
                object.insert("region".to_string(), json!(region));
                object.insert("mutable".to_string(), json!(mutable));
                object.insert("referent_type_hash".to_string(), json!(referent));
                Ok(ComputedLayout {
                    metadata,
                    size_bytes: self.target.pointer_size_bytes,
                    align_bytes: self.target.pointer_align_bytes,
                    class,
                })
            }
            TypeSpec::RawPointer { mutable, pointee } => {
                self.db.type_spec(&pointee)?;
                let class = LayoutClass::raw_pointer();
                let mut metadata = self.base_metadata(
                    type_hash,
                    "raw_pointer",
                    self.target.pointer_size_bytes,
                    self.target.pointer_align_bytes,
                    class,
                );
                let object = metadata.as_object_mut().unwrap();
                object.insert("mutable".to_string(), json!(mutable));
                object.insert("pointee_type_hash".to_string(), json!(pointee));
                Ok(ComputedLayout {
                    metadata,
                    size_bytes: self.target.pointer_size_bytes,
                    align_bytes: self.target.pointer_align_bytes,
                    class,
                })
            }
            TypeSpec::Box { element } => {
                self.db.type_spec(&element)?;
                let payload_class =
                    self.layout_class_flags_for_type(&element, &mut BTreeSet::new())?;
                let class = LayoutClass::boxed(payload_class);
                let mut metadata = self.base_metadata(
                    type_hash,
                    "box",
                    self.target.pointer_size_bytes,
                    self.target.pointer_align_bytes,
                    class,
                );
                let object = metadata.as_object_mut().unwrap();
                object.insert("element_type_hash".to_string(), json!(element));
                Ok(ComputedLayout {
                    metadata,
                    size_bytes: self.target.pointer_size_bytes,
                    align_bytes: self.target.pointer_align_bytes,
                    class,
                })
            }
            TypeSpec::Vec { element } => {
                let element_layout = self.layout_type(&element)?;
                let payload_class =
                    self.layout_class_flags_for_type(&element, &mut BTreeSet::new())?;
                let class = LayoutClass::owned_buffer(payload_class);
                let size_bytes = align_up(self.target.pointer_size_bytes, 8)?
                    .checked_add(16)
                    .ok_or_else(|| anyhow!("vec layout overflows for {type_hash}"))?;
                let align_bytes = self.target.pointer_align_bytes.max(8);
                let mut metadata =
                    self.base_metadata(type_hash, "vec", size_bytes, align_bytes, class);
                let object = metadata.as_object_mut().unwrap();
                object.insert("element_type_hash".to_string(), json!(element));
                object.insert("ptr_offset_bytes".to_string(), json!(0));
                object.insert(
                    "len_offset_bytes".to_string(),
                    json!(align_up(self.target.pointer_size_bytes, 8)?),
                );
                object.insert(
                    "capacity_offset_bytes".to_string(),
                    json!(align_up(self.target.pointer_size_bytes, 8)? + 8),
                );
                object.insert(
                    "element_size_bytes".to_string(),
                    json!(element_layout.size_bytes),
                );
                object.insert(
                    "element_align_bytes".to_string(),
                    json!(element_layout.align_bytes),
                );
                object.insert(
                    "element_stride_bytes".to_string(),
                    json!(align_up(
                        element_layout.size_bytes,
                        element_layout.align_bytes
                    )?),
                );
                Ok(ComputedLayout {
                    metadata,
                    size_bytes,
                    align_bytes,
                    class,
                })
            }
            TypeSpec::String => {
                let element = type_hash_for("U8");
                let element_layout = self.layout_type(&element)?;
                let class = LayoutClass::owned_buffer(LayoutClass::copy());
                let size_bytes = align_up(self.target.pointer_size_bytes, 8)?
                    .checked_add(16)
                    .ok_or_else(|| anyhow!("string layout overflows for {type_hash}"))?;
                let align_bytes = self.target.pointer_align_bytes.max(8);
                let mut metadata =
                    self.base_metadata(type_hash, "string", size_bytes, align_bytes, class);
                let object = metadata.as_object_mut().unwrap();
                object.insert("element_type_hash".to_string(), json!(element));
                object.insert("ptr_offset_bytes".to_string(), json!(0));
                object.insert(
                    "len_offset_bytes".to_string(),
                    json!(align_up(self.target.pointer_size_bytes, 8)?),
                );
                object.insert(
                    "capacity_offset_bytes".to_string(),
                    json!(align_up(self.target.pointer_size_bytes, 8)? + 8),
                );
                object.insert(
                    "element_size_bytes".to_string(),
                    json!(element_layout.size_bytes),
                );
                object.insert(
                    "element_align_bytes".to_string(),
                    json!(element_layout.align_bytes),
                );
                object.insert(
                    "element_stride_bytes".to_string(),
                    json!(align_up(
                        element_layout.size_bytes,
                        element_layout.align_bytes
                    )?),
                );
                Ok(ComputedLayout {
                    metadata,
                    size_bytes,
                    align_bytes,
                    class,
                })
            }
            TypeSpec::Slice {
                region,
                mutable,
                element,
            } => {
                let element_layout = self.layout_type(&element)?;
                let class = if mutable {
                    LayoutClass::mutable_reference()
                } else {
                    LayoutClass::shared_reference()
                };
                let size_bytes = align_up(self.target.pointer_size_bytes, 8)?
                    .checked_add(8)
                    .ok_or_else(|| anyhow!("slice layout overflows for {type_hash}"))?;
                let align_bytes = self.target.pointer_align_bytes.max(8);
                let mut metadata =
                    self.base_metadata(type_hash, "slice", size_bytes, align_bytes, class);
                let object = metadata.as_object_mut().unwrap();
                object.insert("region".to_string(), json!(region));
                object.insert("mutable".to_string(), json!(mutable));
                object.insert("element_type_hash".to_string(), json!(element));
                object.insert("ptr_offset_bytes".to_string(), json!(0));
                object.insert(
                    "len_offset_bytes".to_string(),
                    json!(align_up(self.target.pointer_size_bytes, 8)?),
                );
                object.insert(
                    "element_size_bytes".to_string(),
                    json!(element_layout.size_bytes),
                );
                object.insert(
                    "element_align_bytes".to_string(),
                    json!(element_layout.align_bytes),
                );
                object.insert(
                    "element_stride_bytes".to_string(),
                    json!(align_up(
                        element_layout.size_bytes,
                        element_layout.align_bytes
                    )?),
                );
                Ok(ComputedLayout {
                    metadata,
                    size_bytes,
                    align_bytes,
                    class,
                })
            }
            TypeSpec::FixedArray { element, len } => {
                let element_layout = self.layout_type(&element)?;
                let stride = align_up(element_layout.size_bytes, element_layout.align_bytes)?;
                let size_bytes = stride
                    .checked_mul(len)
                    .ok_or_else(|| anyhow!("fixed array layout overflows for {type_hash}"))?;
                let mut metadata = self.base_metadata(
                    type_hash,
                    "fixed_array",
                    size_bytes,
                    element_layout.align_bytes,
                    element_layout.class,
                );
                let object = metadata.as_object_mut().unwrap();
                object.insert("element_type_hash".to_string(), json!(element));
                object.insert("len".to_string(), json!(len));
                object.insert("stride_bytes".to_string(), json!(stride));
                object.insert(
                    "element_size_bytes".to_string(),
                    json!(element_layout.size_bytes),
                );
                object.insert(
                    "element_align_bytes".to_string(),
                    json!(element_layout.align_bytes),
                );
                Ok(ComputedLayout {
                    metadata,
                    size_bytes,
                    align_bytes: element_layout.align_bytes,
                    class: element_layout.class,
                })
            }
            TypeSpec::Record(fields) => {
                self.layout_record(type_hash, None, None, structural_members(fields))
            }
            TypeSpec::Enum(variants) => {
                self.layout_enum(type_hash, None, None, structural_members(variants))
            }
        }
    }

    fn layout_builtin(&self, type_hash: &str, kind: &str) -> Result<ComputedLayout> {
        let (layout_kind, size_bytes, align_bytes) = if type_hash == type_hash_for("I64") {
            ("scalar", 8, 8)
        } else if type_hash == type_hash_for("U8") {
            ("scalar", 1, 1)
        } else if type_hash == type_hash_for("Bool") {
            ("scalar", 1, 1)
        } else if type_hash == type_hash_for("Unit") {
            ("scalar", 0, 1)
        } else {
            bail!("unknown builtin type kind {kind}");
        };
        let class = LayoutClass::copy();
        Ok(ComputedLayout {
            metadata: self.base_metadata(type_hash, layout_kind, size_bytes, align_bytes, class),
            size_bytes,
            align_bytes,
            class,
        })
    }

    fn layout_record(
        &mut self,
        type_hash: &str,
        type_symbol: Option<String>,
        type_def_hash: Option<String>,
        fields: Vec<LayoutMember>,
    ) -> Result<ComputedLayout> {
        let mut offset = 0;
        let mut size_bytes = 0;
        let mut align_bytes = 1;
        let mut class = LayoutClass::copy();
        let mut field_metadata = Vec::with_capacity(fields.len());
        for field in fields {
            let field_layout = self.layout_type(&field.type_hash)?;
            offset = align_up(offset, field_layout.align_bytes)?;
            align_bytes = align_bytes.max(field_layout.align_bytes);
            size_bytes = offset
                .checked_add(field_layout.size_bytes)
                .ok_or_else(|| anyhow!("record layout overflows for {type_hash}"))?;
            class = class.merge(field_layout.class);

            let mut field_object = serde_json::Map::new();
            if let Some(symbol) = field.symbol {
                field_object.insert("field_symbol".to_string(), json!(symbol));
            }
            field_object.insert("name".to_string(), json!(field.name));
            field_object.insert("type_hash".to_string(), json!(field.type_hash));
            field_object.insert("offset_bytes".to_string(), json!(offset));
            field_object.insert("size_bytes".to_string(), json!(field_layout.size_bytes));
            field_object.insert("align_bytes".to_string(), json!(field_layout.align_bytes));
            field_metadata.push(JsonValue::Object(field_object));

            offset = size_bytes;
        }
        size_bytes = align_up(size_bytes, align_bytes)?;
        let mut metadata = self.base_metadata(type_hash, "record", size_bytes, align_bytes, class);
        let object = metadata.as_object_mut().unwrap();
        if let Some(type_symbol) = type_symbol {
            object.insert("type_symbol".to_string(), json!(type_symbol));
        }
        if let Some(type_def_hash) = type_def_hash {
            object.insert("type_def_hash".to_string(), json!(type_def_hash));
        }
        object.insert("fields".to_string(), json!(field_metadata));
        Ok(ComputedLayout {
            metadata,
            size_bytes,
            align_bytes,
            class,
        })
    }

    fn layout_enum(
        &mut self,
        type_hash: &str,
        type_symbol: Option<String>,
        type_def_hash: Option<String>,
        variants: Vec<LayoutMember>,
    ) -> Result<ComputedLayout> {
        let mut payload_size = 0;
        let mut payload_align = 1;
        let mut class = LayoutClass::copy();
        let mut payload_layouts = Vec::with_capacity(variants.len());
        for variant in &variants {
            let layout = self.layout_type(&variant.type_hash)?;
            payload_size = payload_size.max(layout.size_bytes);
            payload_align = payload_align.max(layout.align_bytes);
            class = class.merge(layout.class);
            payload_layouts.push(layout);
        }
        let align_bytes = self.target.enum_tag_align_bytes.max(payload_align);
        let payload_offset = align_up(self.target.enum_tag_size_bytes, payload_align)?;
        let size_bytes = align_up(
            payload_offset
                .checked_add(payload_size)
                .ok_or_else(|| anyhow!("enum layout overflows for {type_hash}"))?,
            align_bytes,
        )?;
        let mut variant_metadata = Vec::with_capacity(variants.len());
        for (idx, (variant, layout)) in variants.into_iter().zip(payload_layouts).enumerate() {
            let mut variant_object = serde_json::Map::new();
            if let Some(symbol) = variant.symbol {
                variant_object.insert("variant_symbol".to_string(), json!(symbol));
            }
            variant_object.insert("name".to_string(), json!(variant.name));
            variant_object.insert("type_hash".to_string(), json!(variant.type_hash));
            variant_object.insert("tag_value".to_string(), json!(idx as u64));
            variant_object.insert("payload_offset_bytes".to_string(), json!(payload_offset));
            variant_object.insert("payload_size_bytes".to_string(), json!(layout.size_bytes));
            variant_object.insert("payload_align_bytes".to_string(), json!(layout.align_bytes));
            variant_metadata.push(JsonValue::Object(variant_object));
        }

        let mut metadata = self.base_metadata(type_hash, "enum", size_bytes, align_bytes, class);
        let object = metadata.as_object_mut().unwrap();
        if let Some(type_symbol) = type_symbol {
            object.insert("type_symbol".to_string(), json!(type_symbol));
        }
        if let Some(type_def_hash) = type_def_hash {
            object.insert("type_def_hash".to_string(), json!(type_def_hash));
        }
        object.insert(
            "tag".to_string(),
            json!({
                "offset_bytes": 0,
                "size_bytes": self.target.enum_tag_size_bytes,
                "align_bytes": self.target.enum_tag_align_bytes,
                "type": "u64",
            }),
        );
        object.insert("payload_offset_bytes".to_string(), json!(payload_offset));
        object.insert("payload_size_bytes".to_string(), json!(payload_size));
        object.insert("variants".to_string(), json!(variant_metadata));
        Ok(ComputedLayout {
            metadata,
            size_bytes,
            align_bytes,
            class,
        })
    }

    fn layout_class_flags_for_type(
        &mut self,
        type_hash: &str,
        active_types: &mut BTreeSet<String>,
    ) -> Result<LayoutClass> {
        if !active_types.insert(type_hash.to_string()) {
            return Ok(LayoutClass::copy());
        }
        let result = self.layout_class_flags_for_type_inner(type_hash, active_types);
        active_types.remove(type_hash);
        result
    }

    fn layout_class_flags_for_type_inner(
        &mut self,
        type_hash: &str,
        active_types: &mut BTreeSet<String>,
    ) -> Result<LayoutClass> {
        match self.db.type_spec_in_root(self.root, type_hash)? {
            TypeSpec::Builtin(_) => Ok(LayoutClass::copy()),
            TypeSpec::Named { .. } => Ok(LayoutClass::copy()),
            TypeSpec::Reference { mutable, .. } => {
                if mutable {
                    Ok(LayoutClass::mutable_reference())
                } else {
                    Ok(LayoutClass::shared_reference())
                }
            }
            TypeSpec::RawPointer { .. } => Ok(LayoutClass::raw_pointer()),
            TypeSpec::Box { element } => {
                let payload = self.layout_class_flags_for_type(&element, active_types)?;
                Ok(LayoutClass::boxed(payload))
            }
            TypeSpec::Vec { element } => {
                let payload = self.layout_class_flags_for_type(&element, active_types)?;
                Ok(LayoutClass::owned_buffer(payload))
            }
            TypeSpec::String => Ok(LayoutClass::owned_buffer(LayoutClass::copy())),
            TypeSpec::Slice {
                mutable, element, ..
            } => {
                let base = if mutable {
                    LayoutClass::mutable_reference()
                } else {
                    LayoutClass::shared_reference()
                };
                Ok(base.merge(self.layout_class_flags_for_type(&element, active_types)?))
            }
            TypeSpec::FixedArray { element, .. } => {
                self.layout_class_flags_for_type(&element, active_types)
            }
            TypeSpec::Record(fields) | TypeSpec::Enum(fields) => {
                let mut class = LayoutClass::copy();
                for field in fields {
                    class = class
                        .merge(self.layout_class_flags_for_type(&field.type_hash, active_types)?);
                }
                Ok(class)
            }
        }
    }

    fn base_metadata(
        &self,
        type_hash: &str,
        kind: &str,
        size_bytes: u64,
        align_bytes: u64,
        class: LayoutClass,
    ) -> JsonValue {
        json!({
            "schema": TYPE_LAYOUT_SCHEMA,
            "type_hash": type_hash,
            "target_triple": self.target_triple,
            "layout_version": LAYOUT_VERSION,
            "abi_version": ABI_TAG,
            "kind": kind,
            "size_bytes": size_bytes,
            "align_bytes": align_bytes,
            "copy_kind": class.copy_kind.as_str(),
            "drop_kind": class.drop_kind.as_str(),
            "contains_reference": class.contains_reference,
            "contains_mut_reference": class.contains_mut_reference,
            "contains_raw_pointer": class.contains_raw_pointer,
            "contains_box": class.contains_box,
            "contains_capability_handle": class.contains_capability_handle,
            "abi": abi_metadata(kind, size_bytes),
        })
    }
}

fn abi_metadata(kind: &str, size_bytes: u64) -> JsonValue {
    match (kind, size_bytes <= 8) {
        ("record", true) => json!({
            "pass": "by_value",
            "return": "by_value",
        }),
        ("record" | "enum" | "fixed_array" | "slice" | "vec" | "string", _) => json!({
            "pass": "by_indirect",
            "return": "hidden_return_slot",
        }),
        (_, _) => json!({
            "pass": "by_value",
            "return": "by_value",
        }),
    }
}

fn record_members(
    fields: Vec<TypeMemberDef>,
    expanded_fields: Vec<TypeFieldSpec>,
) -> Result<Vec<LayoutMember>> {
    members_with_expanded_types("record field", fields, expanded_fields)
}

fn enum_members(
    variants: Vec<TypeMemberDef>,
    expanded_variants: Vec<TypeFieldSpec>,
) -> Result<Vec<LayoutMember>> {
    members_with_expanded_types("enum variant", variants, expanded_variants)
}

fn members_with_expanded_types(
    label: &str,
    members: Vec<TypeMemberDef>,
    expanded: Vec<TypeFieldSpec>,
) -> Result<Vec<LayoutMember>> {
    if members.len() != expanded.len() {
        bail!("{label} expansion changed member count");
    }
    members
        .into_iter()
        .zip(expanded)
        .map(|(member, expanded)| {
            if member.name != expanded.name {
                bail!(
                    "{label} expansion order mismatch: expected {}, got {}",
                    member.name,
                    expanded.name
                );
            }
            Ok(LayoutMember {
                symbol: Some(member.member_symbol),
                name: member.name,
                type_hash: expanded.type_hash,
            })
        })
        .collect()
}

fn structural_members(fields: Vec<TypeFieldSpec>) -> Vec<LayoutMember> {
    fields
        .into_iter()
        .map(|field| LayoutMember {
            symbol: None,
            name: field.name,
            type_hash: field.type_hash,
        })
        .collect()
}

fn target_layout(target_triple: &str) -> Result<TargetLayout> {
    match target_triple {
        LINUX_X86_64_TARGET | APPLE_ARM64_TARGET => Ok(TargetLayout {
            pointer_size_bytes: 8,
            pointer_align_bytes: 8,
            enum_tag_size_bytes: 8,
            enum_tag_align_bytes: 8,
        }),
        other => bail!(
            "unsupported layout target {other}; supported targets: {LINUX_X86_64_TARGET}, {APPLE_ARM64_TARGET}"
        ),
    }
}

fn align_up(value: u64, align: u64) -> Result<u64> {
    if align == 0 {
        bail!("layout alignment must not be zero");
    }
    let addend = align - 1;
    let rounded = value
        .checked_add(addend)
        .ok_or_else(|| anyhow!("layout alignment overflows"))?;
    Ok((rounded / align) * align)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use crate::DEFAULT_NATIVE_TARGET;
    use crate::model::resolve_function_name_in_root;
    use tempfile::tempdir;

    #[test]
    fn layout_cache_key_is_versioned() {
        // The layout cache key must encode the layout-format version so that a
        // format bump invalidates cached layouts (rather than silently reusing
        // stale ones). The backend id carries the version tag, and
        // LAYOUT_VERSION metadata must use the same tag; this fails if they drift.
        let backend_tag = TYPE_LAYOUT_BACKEND_ID.rsplit(':').next().unwrap();
        let version_tag = LAYOUT_VERSION.rsplit(':').next().unwrap();
        assert_eq!(
            backend_tag, version_tag,
            "layout backend id and layout_version must share a version tag"
        );
        let key = type_layout_cache_key_input(&type_hash_for("I64"), DEFAULT_NATIVE_TARGET, vec![]);
        assert!(
            key.backend_id.ends_with(version_tag),
            "layout cache key backend_id must encode the layout version tag"
        );
    }

    #[test]
    fn named_record_layout_fields_use_instantiated_region_args() {
        let temp = tempdir().unwrap();
        let db_path = temp.path().join("layout-regions.sqlite");
        let source_path = temp.path().join("layout-regions.cdb");
        fs::write(
            &source_path,
            r#"
record Line {
  price_cents: i64
}

record LineView<'view> {
  line: &'view Line
}

fn id_view<'call>(view: LineView<'call>) -> LineView<'call> = view
"#,
        )
        .unwrap();

        let mut db = CodeDb::open(&db_path).unwrap();
        db.init().unwrap();
        db.import_file(&source_path).unwrap();

        let branch = db.branch(MAIN_BRANCH).unwrap();
        let root = db.load_root(&branch.root_hash).unwrap();
        let symbol = resolve_function_name_in_root(&root, MAIN_BRANCH, "id_view").unwrap();
        let entry = db.root_symbol(&root, &symbol).unwrap();
        let (_, return_type) = db.signature_parts(&entry.signature).unwrap();
        let function_region = db.signature_region_params(&entry.signature).unwrap()[0]
            .region
            .clone();

        let layout = db
            .compute_type_layout(&root, &return_type, DEFAULT_NATIVE_TARGET)
            .unwrap()
            .metadata;
        let layout_field_type = layout["fields"][0]["type_hash"].as_str().unwrap();
        let TypeSpec::Record(fields) = db.type_spec_in_root(&root, &return_type).unwrap() else {
            panic!("return type should expand to a record");
        };
        assert_eq!(layout_field_type, fields[0].type_hash);

        let TypeSpec::Reference { region, .. } = db.type_spec(layout_field_type).unwrap() else {
            panic!("layout field should be a reference");
        };
        assert_eq!(region, function_region);
    }
}
