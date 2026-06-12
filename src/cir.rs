//! CIR — the flat binary serialization of a lowered-IR closure (Phase 8 / ladder
//! rung 0, SPEC_V3 §5).
//!
//! The CodeDB-hosted reference evaluator consumes the lowered IR of a program.
//! The canonical lowered-IR artifact is JSON; parsing JSON in `.cdb` would be a
//! large detour, so the Rust side re-encodes the *same information* as flat
//! bytes that a `.cdb` program can decode with byte reads: fixed-width integers,
//! interned strings, and dense indices. The encoding is a faithful re-spelling,
//! not a new compilation artifact:
//!
//! - every semantic decision (op meanings, control flow, storage, traps) stays
//!   with the consumer; the encoder only renames references that are already
//!   explicit in the IR (symbol hashes -> function indices, type hashes ->
//!   per-function type-table indices, value-id strings -> dense value indices);
//! - `emit_cir_main_branch` decodes its own output and bails unless the decoded
//!   `LoweredFunctionIr`s are structurally identical to the originals, so a CIR
//!   file provably carries the whole IR (the built-in honesty gate);
//! - encoding is deterministic: the same root, entry, and target produce the
//!   same bytes (pool interning follows one canonical encode walk).
//!
//! ## File layout (all integers little-endian; `str` = u32 string-pool index)
//!
//! ```text
//! magic "CDIR" | version u32
//! string pool: count u32, then count x (byte_len u32), then concatenated bytes
//! data pool:   count u32, then count x (byte_len u32), then concatenated bytes
//! target str | entry_index u32 | function count u32
//! function table: count x (symbol str, section_offset u32, section_len u32)
//!   (offsets are relative to the start of the section region, in table order)
//! function sections, concatenated in table order
//! ```
//!
//! ## Function section
//!
//! ```text
//! schema str | symbol str | function_def_hash str | function_sig_hash str
//! typed_body_expr_hash str
//! layout table: count u32, then per layout:
//!   type_hash str, kind str, size_bytes u64, align_bytes u64,
//!   abi_pass str, abi_return str, metadata canonical-JSON str
//! type table: count u32, then per type:
//!   type_hash str, layout_index u32, meta_kind u8, meta_size u64
//!   (layout_index = u32::MAX when the type has no layout entry; scalar types)
//! value table: count u32, then per value: value_id str
//! return_type tref | params count u32 x (slot u32, tref)
//! locals count u32 x (slot u32, tref, size_bytes u64)
//! ops count u32 x op | debug map
//! ```
//!
//! ## Consumer columns
//!
//! The type-table `meta_kind`/`meta_size` pair and the `verb`/`width`/`signed`
//! triple on `binary`/`unary` ops are **consumer columns**: derived metadata
//! pre-classified at encode time so the `.cdb` walker never compares hash
//! strings or operator-kind names. They are renamings of facts already
//! explicit in the IR — the well-known scalar type hashes (`typemeta`), the
//! layout rows' size/ABI, and the operator registry's `SemOp` (`opverb`) —
//! never new semantics, exactly like the pre-resolved call-target indices.
//! The decode half of the round-trip honesty gate recomputes every consumer
//! column from the same inputs and fails on any mismatch. Param and local
//! slots are validated dense (`slot == row index`) at encode AND decode so a
//! consumer may index frame tables by slot.
//!
//! `tref` and `vref` are u32 indices into the function's type and value tables.
//! Optional fields are a u8 presence flag followed by the payload when 1.
//! A block is `count u32, count x op, result vref`. Each op is an opcode byte
//! (the `opcode` module below is the stable table) followed by its fields in
//! struct order. `Call` targets are u32 indices into the function table —
//! resolved at encode time, so the consumer never handles symbol hashes.
//! External functions are not encodable (fail-closed): the rung-0 corpus is
//! the reference evaluator's domain, which cannot execute externs either.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value as JsonValue};

use crate::lowering::{
    LoweredBlock, LoweredCaseArm, LoweredDebugMap, LoweredDebugOp, LoweredExprOpMap,
    LoweredFunctionIr, LoweredLocalSlot, LoweredOp, LoweredParamSlot, LoweredPlace, LoweredTrap,
    LoweredTypeAbi, LoweredTypeLayout,
};
use crate::op_registry::{sem_for_kind, ArithOp, BitOp, Cmp, SemOp, ShiftOp};
use crate::oracle::bytes_oracle_hash;
use crate::store::canonical_json;
use crate::types::{bytes_to_hex, hex_to_bytes, type_hash_for};
use crate::{CodeDb, MAIN_BRANCH};

pub const CIR_SCHEMA: &str = "codedb/cir/v0";
const CIR_MAGIC: &[u8; 4] = b"CDIR";
const CIR_VERSION: u32 = 0;
/// Sentinel for "no layout entry" in a type-table row (scalar types like i64
/// are special-cased by consumers and carry no `LoweredTypeLayout`).
const NO_LAYOUT: u32 = u32::MAX;

/// The stable opcode table. Append-only: a new lowered op takes the next free
/// code; existing codes never renumber (the `.cdb` decoder is keyed on them).
mod opcode {
    pub const PARAM: u8 = 0;
    pub const CONST_I64: u8 = 1;
    pub const CONST_BOOL: u8 = 2;
    pub const CONST_UNIT: u8 = 3;
    pub const UNARY: u8 = 4;
    pub const INT_CAST: u8 = 5;
    pub const BINARY: u8 = 6;
    pub const CALL: u8 = 7;
    pub const IF: u8 = 8;
    pub const CASE: u8 = 9;
    pub const FOLD: u8 = 10;
    pub const LOOP: u8 = 11;
    pub const BORROW_SHARED: u8 = 12;
    pub const BORROW_MUT: u8 = 13;
    pub const DEREF_SHARED: u8 = 14;
    pub const DEREF_MUT: u8 = 15;
    pub const DEREF_BOX: u8 = 16;
    pub const UNBOX_MOVE: u8 = 17;
    pub const HEAP_ALLOC: u8 = 18;
    pub const PTR_CAST: u8 = 19;
    pub const DEREF_RAW: u8 = 20;
    pub const ADDR_OF_PARAM: u8 = 21;
    pub const ADDR_OF_LOCAL: u8 = 22;
    pub const ADDR_OF_FIELD: u8 = 23;
    pub const ADDR_OF_ENUM_PAYLOAD: u8 = 24;
    pub const ADDR_OF_INDEX: u8 = 25;
    pub const STATIC_DATA_ADDRESS: u8 = 26;
    pub const CONSTRUCT_SLICE: u8 = 27;
    pub const SLICE_LEN: u8 = 28;
    pub const SLICE_DATA: u8 = 29;
    pub const VEC_NEW: u8 = 30;
    pub const VEC_PUSH: u8 = 31;
    pub const VEC_GET: u8 = 32;
    pub const VEC_LEN: u8 = 33;
    pub const STRING_NEW: u8 = 34;
    pub const STRING_LEN: u8 = 35;
    pub const STRING_WITH_CAPACITY: u8 = 36;
    pub const STRING_PUSH: u8 = 37;
    pub const STRING_GET: u8 = 38;
    pub const STRING_SET: u8 = 39;
    pub const ARG_COUNT: u8 = 40;
    pub const ARG_LEN: u8 = 41;
    pub const ARG_BYTE: u8 = 42;
    pub const BOUNDS_CHECK: u8 = 43;
    pub const SLICE_RANGE_CHECK: u8 = 44;
    pub const LOAD_ENUM_TAG: u8 = 45;
    pub const STORE_ENUM_TAG: u8 = 46;
    pub const LOAD: u8 = 47;
    pub const STORE: u8 = 48;
    pub const COPY: u8 = 49;
    pub const MOVE: u8 = 50;
    pub const DROP: u8 = 51;
    pub const FREE_BOX_SHELL: u8 = 52;
    pub const BORROW_DEBUG: u8 = 53;
    pub const RETURN: u8 = 54;
    pub const EARLY_RETURN: u8 = 55;
}

mod placekind {
    pub const PARAM: u8 = 0;
    pub const LOCAL: u8 = 1;
    pub const FIELD: u8 = 2;
    pub const ENUM_PAYLOAD: u8 = 3;
    pub const INDEX: u8 = 4;
}

/// Type-table consumer column: how a value of this type lives in a cell/slot.
/// 0-9 are the scalar kinds (cells hold the canonically extended value), 10 is
/// pointer-like (box / reference / raw pointer — an 8-byte address cell),
/// 11/12 are layout-bearing aggregates split by ABI pass mode (cells hold the
/// address of the value's bytes; `meta_size` is the byte size of the bytes
/// themselves), and 13 is the slice sub-kind of 12 (a `{data_ptr, len}` pair;
/// a fold target of this kind iterates the pointed-to data, not the pair).
mod typemeta {
    pub const UNIT: u8 = 0;
    pub const BOOL: u8 = 1;
    pub const I8: u8 = 2;
    pub const I16: u8 = 3;
    pub const I32: u8 = 4;
    pub const I64: u8 = 5;
    pub const U8: u8 = 6;
    pub const U16: u8 = 7;
    pub const U32: u8 = 8;
    pub const U64: u8 = 9;
    pub const POINTER: u8 = 10;
    pub const AGG_BY_VALUE: u8 = 11;
    pub const AGG_INDIRECT: u8 = 12;
    pub const SLICE: u8 = 13;
}

/// Binary/unary consumer column: the registry kind's verb, decoupled from its
/// width (the `width`/`signed` columns carry the [`crate::op_registry::IntKind`];
/// the boolean verbs are width-free and encode width 0).
mod opverb {
    pub const ADD: u8 = 1;
    pub const SUB: u8 = 2;
    pub const MUL: u8 = 3;
    pub const DIV: u8 = 4;
    pub const REM: u8 = 5;
    pub const BIT_AND: u8 = 6;
    pub const BIT_OR: u8 = 7;
    pub const BIT_XOR: u8 = 8;
    pub const SHL: u8 = 9;
    pub const SHR: u8 = 10;
    pub const EQ: u8 = 11;
    pub const NE: u8 = 12;
    pub const LT: u8 = 13;
    pub const LE: u8 = 14;
    pub const GT: u8 = 15;
    pub const GE: u8 = 16;
    pub const NEG: u8 = 17;
    pub const BIT_NOT: u8 = 18;
    pub const AND_BOOL: u8 = 19;
    pub const OR_BOOL: u8 = 20;
    pub const NOT_BOOL: u8 = 21;
}

/// The well-known scalar type hashes -> their type-meta columns. Computed once;
/// these are the exact hashes the registry and backend special-case, so the
/// classification cannot drift from the type system's hashing.
fn scalar_type_meta(type_hash: &str) -> Option<(u8, u64)> {
    static SCALARS: OnceLock<BTreeMap<String, (u8, u64)>> = OnceLock::new();
    let map = SCALARS.get_or_init(|| {
        BTreeMap::from([
            (type_hash_for("Unit"), (typemeta::UNIT, 0)),
            (type_hash_for("Bool"), (typemeta::BOOL, 1)),
            (type_hash_for("I8"), (typemeta::I8, 1)),
            (type_hash_for("I16"), (typemeta::I16, 2)),
            (type_hash_for("I32"), (typemeta::I32, 4)),
            (type_hash_for("I64"), (typemeta::I64, 8)),
            (type_hash_for("U8"), (typemeta::U8, 1)),
            (type_hash_for("U16"), (typemeta::U16, 2)),
            (type_hash_for("U32"), (typemeta::U32, 4)),
            (type_hash_for("U64"), (typemeta::U64, 8)),
        ])
    });
    map.get(type_hash).copied()
}

/// Derive a type-table row's consumer columns from facts already in the IR:
/// the well-known scalar hashes, or the row's layout (kind + size + ABI pass).
/// A layout-less non-scalar type is pointer-like (references and raw pointers
/// live in 8-byte cells), as is a `box` layout (its value IS the pointer).
fn type_meta_columns(type_hash: &str, layout: Option<&LoweredTypeLayout>) -> (u8, u64) {
    if let Some(meta) = scalar_type_meta(type_hash) {
        return meta;
    }
    match layout {
        Some(layout) if layout.kind == "box" => (typemeta::POINTER, 8),
        Some(layout) if layout.kind == "slice" => (typemeta::SLICE, layout.size_bytes),
        Some(layout) if layout.abi.pass == "by_indirect" => {
            (typemeta::AGG_INDIRECT, layout.size_bytes)
        }
        Some(layout) => (typemeta::AGG_BY_VALUE, layout.size_bytes),
        None => (typemeta::POINTER, 8),
    }
}

/// Derive a binary/unary op's consumer columns (verb, width-in-bytes, signed)
/// from the registry's semantic for its kind string. Unknown kinds fail closed.
fn sem_columns(kind: &str) -> Result<(u8, u8, u8)> {
    let sem = sem_for_kind(kind)
        .ok_or_else(|| anyhow!("CIR operator kind {kind} is not in the registry"))?;
    let (verb, int) = match sem {
        SemOp::Arith(op, k) => (
            match op {
                ArithOp::Add => opverb::ADD,
                ArithOp::Sub => opverb::SUB,
                ArithOp::Mul => opverb::MUL,
                ArithOp::Div => opverb::DIV,
                ArithOp::Rem => opverb::REM,
            },
            Some(k),
        ),
        SemOp::Bit(op, k) => (
            match op {
                BitOp::And => opverb::BIT_AND,
                BitOp::Or => opverb::BIT_OR,
                BitOp::Xor => opverb::BIT_XOR,
            },
            Some(k),
        ),
        SemOp::Shift(op, k) => (
            match op {
                ShiftOp::Shl => opverb::SHL,
                ShiftOp::Shr => opverb::SHR,
            },
            Some(k),
        ),
        SemOp::Cmp(op, k) => (
            match op {
                Cmp::Eq => opverb::EQ,
                Cmp::Ne => opverb::NE,
                Cmp::Lt => opverb::LT,
                Cmp::Le => opverb::LE,
                Cmp::Gt => opverb::GT,
                Cmp::Ge => opverb::GE,
            },
            Some(k),
        ),
        SemOp::Neg(k) => (opverb::NEG, Some(k)),
        SemOp::BitNot(k) => (opverb::BIT_NOT, Some(k)),
        SemOp::AndBool => (opverb::AND_BOOL, None),
        SemOp::OrBool => (opverb::OR_BOOL, None),
        SemOp::NotBool => (opverb::NOT_BOOL, None),
    };
    match int {
        Some(k) => {
            let width = u8::try_from(k.width)
                .map_err(|_| anyhow!("CIR operator kind {kind} has an unencodable width"))?;
            Ok((verb, width, u8::from(k.signed)))
        }
        None => Ok((verb, 0, 0)),
    }
}

// ---------------------------------------------------------------------------
// byte-writer / byte-reader primitives
// ---------------------------------------------------------------------------

fn write_u8(out: &mut Vec<u8>, value: u8) {
    out.push(value);
}

fn write_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn write_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn write_i64(out: &mut Vec<u8>, value: i64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn write_usize_u32(out: &mut Vec<u8>, value: usize, what: &str) -> Result<()> {
    let value = u32::try_from(value).map_err(|_| anyhow!("CIR {what} exceeds u32 range"))?;
    write_u32(out, value);
    Ok(())
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(len)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| anyhow!("CIR truncated at offset {}", self.pos))?;
        let out = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("4 bytes")))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().expect("8 bytes")))
    }

    fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().expect("8 bytes")))
    }

    fn done(&self) -> bool {
        self.pos == self.bytes.len()
    }
}

// ---------------------------------------------------------------------------
// interned pools (file-global) and per-function tables
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Pools {
    strings: Vec<String>,
    string_index: BTreeMap<String, u32>,
    data: Vec<Vec<u8>>,
    data_index: BTreeMap<Vec<u8>, u32>,
}

impl Pools {
    fn istr(&mut self, value: &str) -> u32 {
        if let Some(index) = self.string_index.get(value) {
            return *index;
        }
        let index = u32::try_from(self.strings.len()).expect("string pool fits u32");
        self.strings.push(value.to_string());
        self.string_index.insert(value.to_string(), index);
        index
    }

    fn idata(&mut self, value: &[u8]) -> u32 {
        if let Some(index) = self.data_index.get(value) {
            return *index;
        }
        let index = u32::try_from(self.data.len()).expect("data pool fits u32");
        self.data.push(value.to_vec());
        self.data_index.insert(value.to_vec(), index);
        index
    }
}

#[derive(Default)]
struct FnTables {
    values: Vec<String>,
    value_index: BTreeMap<String, u32>,
    types: Vec<String>,
    type_index: BTreeMap<String, u32>,
}

impl FnTables {
    fn vref(&mut self, id: &str) -> u32 {
        if let Some(index) = self.value_index.get(id) {
            return *index;
        }
        let index = u32::try_from(self.values.len()).expect("value table fits u32");
        self.values.push(id.to_string());
        self.value_index.insert(id.to_string(), index);
        index
    }

    fn tref(&mut self, type_hash: &str) -> u32 {
        if let Some(index) = self.type_index.get(type_hash) {
            return *index;
        }
        let index = u32::try_from(self.types.len()).expect("type table fits u32");
        self.types.push(type_hash.to_string());
        self.type_index.insert(type_hash.to_string(), index);
        index
    }
}

// ---------------------------------------------------------------------------
// encoder
// ---------------------------------------------------------------------------

struct FnEncoder<'p, 'f> {
    pools: &'p mut Pools,
    tables: FnTables,
    fn_index: &'f BTreeMap<String, u32>,
}

impl FnEncoder<'_, '_> {
    fn ostr(&mut self, out: &mut Vec<u8>, value: Option<&str>) {
        match value {
            Some(value) => {
                write_u8(out, 1);
                write_u32(out, self.pools.istr(value));
            }
            None => write_u8(out, 0),
        }
    }

    fn ovref(&mut self, out: &mut Vec<u8>, value: Option<&str>) {
        match value {
            Some(value) => {
                write_u8(out, 1);
                write_u32(out, self.tables.vref(value));
            }
            None => write_u8(out, 0),
        }
    }

    fn encode_trap(&mut self, out: &mut Vec<u8>, trap: Option<&LoweredTrap>) {
        match trap {
            Some(trap) => {
                write_u8(out, 1);
                write_u32(out, self.pools.istr(&trap.condition));
                write_u32(out, self.pools.istr(&trap.code));
            }
            None => write_u8(out, 0),
        }
    }

    fn encode_block(&mut self, out: &mut Vec<u8>, block: &LoweredBlock) -> Result<()> {
        write_usize_u32(out, block.operations.len(), "block op count")?;
        for op in &block.operations {
            self.encode_op(out, op)?;
        }
        write_u32(out, self.tables.vref(&block.result));
        Ok(())
    }

    fn encode_place(&mut self, out: &mut Vec<u8>, place: &LoweredPlace) -> Result<()> {
        match place {
            LoweredPlace::Param {
                slot,
                type_hash,
                indirect,
            } => {
                write_u8(out, placekind::PARAM);
                write_usize_u32(out, *slot, "param slot")?;
                write_u32(out, self.tables.tref(type_hash));
                write_u8(out, u8::from(*indirect));
            }
            LoweredPlace::Local { slot, type_hash } => {
                write_u8(out, placekind::LOCAL);
                write_usize_u32(out, *slot, "local slot")?;
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredPlace::Field {
                base,
                field,
                field_symbol,
                owner_type_hash,
                offset_bytes,
                type_hash,
            } => {
                write_u8(out, placekind::FIELD);
                write_u32(out, self.tables.vref(base));
                write_u32(out, self.pools.istr(field));
                self.ostr(out, field_symbol.as_deref());
                write_u32(out, self.tables.tref(owner_type_hash));
                write_u64(out, *offset_bytes);
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredPlace::EnumPayload {
                base,
                variant,
                variant_symbol,
                owner_type_hash,
                tag_value,
                payload_offset_bytes,
                type_hash,
            } => {
                write_u8(out, placekind::ENUM_PAYLOAD);
                write_u32(out, self.tables.vref(base));
                write_u32(out, self.pools.istr(variant));
                self.ostr(out, variant_symbol.as_deref());
                write_u32(out, self.tables.tref(owner_type_hash));
                write_u64(out, *tag_value);
                write_u64(out, *payload_offset_bytes);
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredPlace::Index {
                base,
                index,
                element_type_hash,
                type_hash,
            } => {
                write_u8(out, placekind::INDEX);
                write_u32(out, self.tables.vref(base));
                write_u32(out, self.tables.vref(index));
                write_u32(out, self.tables.tref(element_type_hash));
                write_u32(out, self.tables.tref(type_hash));
            }
        }
        Ok(())
    }

    fn encode_static_data(&mut self, out: &mut Vec<u8>, bytes_hex: &str) -> Result<()> {
        let bytes = hex_to_bytes(bytes_hex)?;
        if bytes_to_hex(&bytes) != bytes_hex {
            bail!("CIR static data hex is not canonical lowercase");
        }
        write_u32(out, self.pools.idata(&bytes));
        Ok(())
    }

    fn encode_op(&mut self, out: &mut Vec<u8>, op: &LoweredOp) -> Result<()> {
        match op {
            LoweredOp::Param {
                id,
                slot,
                type_hash,
            } => {
                write_u8(out, opcode::PARAM);
                write_u32(out, self.tables.vref(id));
                write_usize_u32(out, *slot, "param slot")?;
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::ConstI64 {
                id,
                value,
                type_hash,
            } => {
                let parsed: i64 = value
                    .parse()
                    .map_err(|_| anyhow!("CIR const_i64 value {value} is not a canonical i64"))?;
                if parsed.to_string() != *value {
                    bail!("CIR const_i64 value {value} is not in canonical decimal form");
                }
                write_u8(out, opcode::CONST_I64);
                write_u32(out, self.tables.vref(id));
                write_i64(out, parsed);
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::ConstBool {
                id,
                value,
                type_hash,
            } => {
                write_u8(out, opcode::CONST_BOOL);
                write_u32(out, self.tables.vref(id));
                write_u8(out, u8::from(*value));
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::ConstUnit { id, type_hash } => {
                write_u8(out, opcode::CONST_UNIT);
                write_u32(out, self.tables.vref(id));
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::Unary {
                id,
                kind,
                value,
                type_hash,
            } => {
                write_u8(out, opcode::UNARY);
                write_u32(out, self.tables.vref(id));
                write_u32(out, self.pools.istr(kind));
                let (verb, width, signed) = sem_columns(kind)?;
                write_u8(out, verb);
                write_u8(out, width);
                write_u8(out, signed);
                write_u32(out, self.tables.vref(value));
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::IntCast {
                id,
                value,
                type_hash,
            } => {
                write_u8(out, opcode::INT_CAST);
                write_u32(out, self.tables.vref(id));
                write_u32(out, self.tables.vref(value));
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::Binary {
                id,
                kind,
                left,
                right,
                type_hash,
                trap,
            } => {
                write_u8(out, opcode::BINARY);
                write_u32(out, self.tables.vref(id));
                write_u32(out, self.pools.istr(kind));
                let (verb, width, signed) = sem_columns(kind)?;
                write_u8(out, verb);
                write_u8(out, width);
                write_u8(out, signed);
                write_u32(out, self.tables.vref(left));
                write_u32(out, self.tables.vref(right));
                write_u32(out, self.tables.tref(type_hash));
                self.encode_trap(out, trap.as_ref());
            }
            LoweredOp::Call {
                id,
                target_symbol_hash,
                target_abi_symbol,
                args,
                return_address,
                type_hash,
            } => {
                let target = self.fn_index.get(target_symbol_hash).ok_or_else(|| {
                    anyhow!("CIR call target {target_symbol_hash} is not in the function table")
                })?;
                write_u8(out, opcode::CALL);
                write_u32(out, self.tables.vref(id));
                write_u32(out, *target);
                self.ostr(out, target_abi_symbol.as_deref());
                write_usize_u32(out, args.len(), "call arg count")?;
                for arg in args {
                    write_u32(out, self.tables.vref(arg));
                }
                self.ovref(out, return_address.as_deref());
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::If {
                id,
                cond,
                then_block,
                else_block,
                type_hash,
            } => {
                write_u8(out, opcode::IF);
                write_u32(out, self.tables.vref(id));
                write_u32(out, self.tables.vref(cond));
                self.encode_block(out, then_block)?;
                self.encode_block(out, else_block)?;
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::Case {
                id,
                scrutinee,
                enum_type_hash,
                arms,
                type_hash,
            } => {
                write_u8(out, opcode::CASE);
                write_u32(out, self.tables.vref(id));
                write_u32(out, self.tables.vref(scrutinee));
                write_u32(out, self.tables.tref(enum_type_hash));
                write_usize_u32(out, arms.len(), "case arm count")?;
                for arm in arms {
                    write_u32(out, self.pools.istr(&arm.variant));
                    self.ostr(out, arm.variant_symbol.as_deref());
                    write_u64(out, arm.tag_value);
                    write_u32(out, self.tables.tref(&arm.payload_type_hash));
                    write_u64(out, arm.payload_offset_bytes);
                    self.encode_block(out, &arm.block)?;
                }
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::Fold {
                id,
                target_address,
                target_type_hash,
                len,
                init,
                index_slot,
                acc_slot,
                item_slot,
                body,
                element_type_hash,
                acc_type_hash,
                type_hash,
            } => {
                write_u8(out, opcode::FOLD);
                write_u32(out, self.tables.vref(id));
                write_u32(out, self.tables.vref(target_address));
                write_u32(out, self.tables.tref(target_type_hash));
                write_u32(out, self.tables.vref(len));
                write_u32(out, self.tables.vref(init));
                write_usize_u32(out, *index_slot, "fold index slot")?;
                write_usize_u32(out, *acc_slot, "fold acc slot")?;
                write_usize_u32(out, *item_slot, "fold item slot")?;
                self.encode_block(out, body)?;
                write_u32(out, self.tables.tref(element_type_hash));
                write_u32(out, self.tables.tref(acc_type_hash));
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::Loop {
                id,
                acc_slot,
                init,
                cond,
                body,
                acc_type_hash,
                type_hash,
            } => {
                write_u8(out, opcode::LOOP);
                write_u32(out, self.tables.vref(id));
                write_usize_u32(out, *acc_slot, "loop acc slot")?;
                write_u32(out, self.tables.vref(init));
                self.encode_block(out, cond)?;
                self.encode_block(out, body)?;
                write_u32(out, self.tables.tref(acc_type_hash));
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::BorrowShared {
                id,
                address,
                region,
                referent_type_hash,
                type_hash,
            } => {
                write_u8(out, opcode::BORROW_SHARED);
                write_u32(out, self.tables.vref(id));
                write_u32(out, self.tables.vref(address));
                write_u32(out, self.pools.istr(region));
                write_u32(out, self.tables.tref(referent_type_hash));
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::BorrowMut {
                id,
                address,
                region,
                referent_type_hash,
                type_hash,
            } => {
                write_u8(out, opcode::BORROW_MUT);
                write_u32(out, self.tables.vref(id));
                write_u32(out, self.tables.vref(address));
                write_u32(out, self.pools.istr(region));
                write_u32(out, self.tables.tref(referent_type_hash));
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::DerefShared {
                id,
                reference,
                referent_type_hash,
            } => {
                write_u8(out, opcode::DEREF_SHARED);
                write_u32(out, self.tables.vref(id));
                write_u32(out, self.tables.vref(reference));
                write_u32(out, self.tables.tref(referent_type_hash));
            }
            LoweredOp::DerefMut {
                id,
                reference,
                referent_type_hash,
            } => {
                write_u8(out, opcode::DEREF_MUT);
                write_u32(out, self.tables.vref(id));
                write_u32(out, self.tables.vref(reference));
                write_u32(out, self.tables.tref(referent_type_hash));
            }
            LoweredOp::DerefBox {
                id,
                box_value,
                box_type_hash,
                element_type_hash,
            } => {
                write_u8(out, opcode::DEREF_BOX);
                write_u32(out, self.tables.vref(id));
                write_u32(out, self.tables.vref(box_value));
                write_u32(out, self.tables.tref(box_type_hash));
                write_u32(out, self.tables.tref(element_type_hash));
            }
            LoweredOp::UnboxMove {
                id,
                box_value,
                box_type_hash,
                element_type_hash,
                dest_slot,
            } => {
                write_u8(out, opcode::UNBOX_MOVE);
                write_u32(out, self.tables.vref(id));
                write_u32(out, self.tables.vref(box_value));
                write_u32(out, self.tables.tref(box_type_hash));
                write_u32(out, self.tables.tref(element_type_hash));
                write_usize_u32(out, *dest_slot, "unbox dest slot")?;
            }
            LoweredOp::HeapAlloc {
                id,
                size_bytes,
                align_bytes,
                element_type_hash,
                type_hash,
            } => {
                write_u8(out, opcode::HEAP_ALLOC);
                write_u32(out, self.tables.vref(id));
                write_u64(out, *size_bytes);
                write_u64(out, *align_bytes);
                write_u32(out, self.tables.tref(element_type_hash));
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::PtrCast {
                id,
                value,
                source_type_hash,
                type_hash,
            } => {
                write_u8(out, opcode::PTR_CAST);
                write_u32(out, self.tables.vref(id));
                write_u32(out, self.tables.vref(value));
                write_u32(out, self.tables.tref(source_type_hash));
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::DerefRaw {
                id,
                pointer,
                pointer_type_hash,
                pointee_type_hash,
                mutable,
            } => {
                write_u8(out, opcode::DEREF_RAW);
                write_u32(out, self.tables.vref(id));
                write_u32(out, self.tables.vref(pointer));
                write_u32(out, self.tables.tref(pointer_type_hash));
                write_u32(out, self.tables.tref(pointee_type_hash));
                write_u8(out, u8::from(*mutable));
            }
            LoweredOp::AddrOfParam { id, place } => {
                write_u8(out, opcode::ADDR_OF_PARAM);
                write_u32(out, self.tables.vref(id));
                self.encode_place(out, place)?;
            }
            LoweredOp::AddrOfLocal { id, place } => {
                write_u8(out, opcode::ADDR_OF_LOCAL);
                write_u32(out, self.tables.vref(id));
                self.encode_place(out, place)?;
            }
            LoweredOp::AddrOfField { id, place } => {
                write_u8(out, opcode::ADDR_OF_FIELD);
                write_u32(out, self.tables.vref(id));
                self.encode_place(out, place)?;
            }
            LoweredOp::AddrOfEnumPayload { id, place } => {
                write_u8(out, opcode::ADDR_OF_ENUM_PAYLOAD);
                write_u32(out, self.tables.vref(id));
                self.encode_place(out, place)?;
            }
            LoweredOp::AddrOfIndex { id, place } => {
                write_u8(out, opcode::ADDR_OF_INDEX);
                write_u32(out, self.tables.vref(id));
                self.encode_place(out, place)?;
            }
            LoweredOp::StaticDataAddress {
                id,
                static_data_hash,
                bytes_hex,
                len,
                element_type_hash,
            } => {
                write_u8(out, opcode::STATIC_DATA_ADDRESS);
                write_u32(out, self.tables.vref(id));
                write_u32(out, self.pools.istr(static_data_hash));
                self.encode_static_data(out, bytes_hex)?;
                write_u64(out, *len);
                write_u32(out, self.tables.tref(element_type_hash));
            }
            LoweredOp::ConstructSlice {
                id,
                address,
                data_address,
                len,
                element_type_hash,
                type_hash,
            } => {
                write_u8(out, opcode::CONSTRUCT_SLICE);
                write_u32(out, self.tables.vref(id));
                write_u32(out, self.tables.vref(address));
                write_u32(out, self.tables.vref(data_address));
                write_u32(out, self.tables.vref(len));
                write_u32(out, self.tables.tref(element_type_hash));
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::SliceLen {
                id,
                slice,
                slice_type_hash,
                type_hash,
            } => {
                write_u8(out, opcode::SLICE_LEN);
                write_u32(out, self.tables.vref(id));
                write_u32(out, self.tables.vref(slice));
                write_u32(out, self.tables.tref(slice_type_hash));
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::SliceData {
                id,
                slice,
                slice_type_hash,
                element_type_hash,
            } => {
                write_u8(out, opcode::SLICE_DATA);
                write_u32(out, self.tables.vref(id));
                write_u32(out, self.tables.vref(slice));
                write_u32(out, self.tables.tref(slice_type_hash));
                write_u32(out, self.tables.tref(element_type_hash));
            }
            LoweredOp::VecNew {
                id,
                address,
                capacity,
                element_type_hash,
                type_hash,
            } => {
                write_u8(out, opcode::VEC_NEW);
                write_u32(out, self.tables.vref(id));
                write_u32(out, self.tables.vref(address));
                write_u64(out, *capacity);
                write_u32(out, self.tables.tref(element_type_hash));
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::VecPush {
                id,
                vec_address,
                value,
                vec_type_hash,
                element_type_hash,
                type_hash,
            } => {
                write_u8(out, opcode::VEC_PUSH);
                write_u32(out, self.tables.vref(id));
                write_u32(out, self.tables.vref(vec_address));
                write_u32(out, self.tables.vref(value));
                write_u32(out, self.tables.tref(vec_type_hash));
                write_u32(out, self.tables.tref(element_type_hash));
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::VecGet {
                id,
                vec_address,
                index,
                vec_type_hash,
                element_type_hash,
                type_hash,
            } => {
                write_u8(out, opcode::VEC_GET);
                write_u32(out, self.tables.vref(id));
                write_u32(out, self.tables.vref(vec_address));
                write_u32(out, self.tables.vref(index));
                write_u32(out, self.tables.tref(vec_type_hash));
                write_u32(out, self.tables.tref(element_type_hash));
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::VecLen {
                id,
                vec_address,
                vec_type_hash,
                type_hash,
            } => {
                write_u8(out, opcode::VEC_LEN);
                write_u32(out, self.tables.vref(id));
                write_u32(out, self.tables.vref(vec_address));
                write_u32(out, self.tables.tref(vec_type_hash));
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::StringNew {
                id,
                address,
                static_data_hash,
                bytes_hex,
                len,
                type_hash,
            } => {
                write_u8(out, opcode::STRING_NEW);
                write_u32(out, self.tables.vref(id));
                write_u32(out, self.tables.vref(address));
                write_u32(out, self.pools.istr(static_data_hash));
                self.encode_static_data(out, bytes_hex)?;
                write_u64(out, *len);
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::StringLen {
                id,
                string_address,
                string_type_hash,
                type_hash,
            } => {
                write_u8(out, opcode::STRING_LEN);
                write_u32(out, self.tables.vref(id));
                write_u32(out, self.tables.vref(string_address));
                write_u32(out, self.tables.tref(string_type_hash));
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::StringWithCapacity {
                id,
                address,
                capacity,
                type_hash,
            } => {
                write_u8(out, opcode::STRING_WITH_CAPACITY);
                write_u32(out, self.tables.vref(id));
                write_u32(out, self.tables.vref(address));
                write_u32(out, self.tables.vref(capacity));
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::StringPush {
                id,
                string_address,
                value,
                string_type_hash,
                type_hash,
            } => {
                write_u8(out, opcode::STRING_PUSH);
                write_u32(out, self.tables.vref(id));
                write_u32(out, self.tables.vref(string_address));
                write_u32(out, self.tables.vref(value));
                write_u32(out, self.tables.tref(string_type_hash));
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::StringGet {
                id,
                string_address,
                index,
                string_type_hash,
                type_hash,
            } => {
                write_u8(out, opcode::STRING_GET);
                write_u32(out, self.tables.vref(id));
                write_u32(out, self.tables.vref(string_address));
                write_u32(out, self.tables.vref(index));
                write_u32(out, self.tables.tref(string_type_hash));
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::StringSet {
                id,
                string_address,
                index,
                value,
                string_type_hash,
                type_hash,
            } => {
                write_u8(out, opcode::STRING_SET);
                write_u32(out, self.tables.vref(id));
                write_u32(out, self.tables.vref(string_address));
                write_u32(out, self.tables.vref(index));
                write_u32(out, self.tables.vref(value));
                write_u32(out, self.tables.tref(string_type_hash));
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::ArgCount { id, type_hash } => {
                write_u8(out, opcode::ARG_COUNT);
                write_u32(out, self.tables.vref(id));
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::ArgLen {
                id,
                index,
                type_hash,
            } => {
                write_u8(out, opcode::ARG_LEN);
                write_u32(out, self.tables.vref(id));
                write_u32(out, self.tables.vref(index));
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::ArgByte {
                id,
                index,
                byte,
                type_hash,
            } => {
                write_u8(out, opcode::ARG_BYTE);
                write_u32(out, self.tables.vref(id));
                write_u32(out, self.tables.vref(index));
                write_u32(out, self.tables.vref(byte));
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::BoundsCheck {
                id,
                index,
                len,
                len_value,
                type_hash,
            } => {
                write_u8(out, opcode::BOUNDS_CHECK);
                write_u32(out, self.tables.vref(id));
                write_u32(out, self.tables.vref(index));
                write_u64(out, *len);
                self.ovref(out, len_value.as_deref());
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::SliceRangeCheck {
                id,
                start,
                len,
                source_len,
                type_hash,
            } => {
                write_u8(out, opcode::SLICE_RANGE_CHECK);
                write_u32(out, self.tables.vref(id));
                write_u32(out, self.tables.vref(start));
                write_u32(out, self.tables.vref(len));
                write_u32(out, self.tables.vref(source_len));
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::LoadEnumTag {
                id,
                address,
                type_hash,
            } => {
                write_u8(out, opcode::LOAD_ENUM_TAG);
                write_u32(out, self.tables.vref(id));
                write_u32(out, self.tables.vref(address));
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::StoreEnumTag {
                address,
                type_hash,
                variant,
                variant_symbol,
                tag_value,
            } => {
                write_u8(out, opcode::STORE_ENUM_TAG);
                write_u32(out, self.tables.vref(address));
                write_u32(out, self.tables.tref(type_hash));
                write_u32(out, self.pools.istr(variant));
                self.ostr(out, variant_symbol.as_deref());
                write_u64(out, *tag_value);
            }
            LoweredOp::Load {
                id,
                address,
                type_hash,
            } => {
                write_u8(out, opcode::LOAD);
                write_u32(out, self.tables.vref(id));
                write_u32(out, self.tables.vref(address));
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::Store {
                address,
                value,
                type_hash,
            } => {
                write_u8(out, opcode::STORE);
                write_u32(out, self.tables.vref(address));
                write_u32(out, self.tables.vref(value));
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::Copy {
                id,
                value,
                type_hash,
            } => {
                write_u8(out, opcode::COPY);
                write_u32(out, self.tables.vref(id));
                write_u32(out, self.tables.vref(value));
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::Move {
                id,
                address,
                type_hash,
            } => {
                write_u8(out, opcode::MOVE);
                write_u32(out, self.tables.vref(id));
                write_u32(out, self.tables.vref(address));
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::Drop { address, type_hash } => {
                write_u8(out, opcode::DROP);
                write_u32(out, self.tables.vref(address));
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::FreeBoxShell {
                address,
                box_type_hash,
            } => {
                write_u8(out, opcode::FREE_BOX_SHELL);
                write_u32(out, self.tables.vref(address));
                write_u32(out, self.tables.tref(box_type_hash));
            }
            LoweredOp::BorrowDebug {
                address,
                mutable,
                region,
                type_hash,
            } => {
                write_u8(out, opcode::BORROW_DEBUG);
                write_u32(out, self.tables.vref(address));
                write_u8(out, u8::from(*mutable));
                self.ostr(out, region.as_deref());
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::Return { value, type_hash } => {
                write_u8(out, opcode::RETURN);
                write_u32(out, self.tables.vref(value));
                write_u32(out, self.tables.tref(type_hash));
            }
            LoweredOp::EarlyReturn { value, type_hash } => {
                write_u8(out, opcode::EARLY_RETURN);
                write_u32(out, self.tables.vref(value));
                write_u32(out, self.tables.tref(type_hash));
            }
        }
        Ok(())
    }

    fn encode_debug_map(&mut self, out: &mut Vec<u8>, debug: &LoweredDebugMap) -> Result<()> {
        write_u32(out, self.pools.istr(&debug.schema));
        write_usize_u32(out, debug.operations.len(), "debug op count")?;
        for op in &debug.operations {
            write_u32(out, self.pools.istr(&op.lowered_op_id));
            write_u32(out, self.pools.istr(&op.value_id));
            write_u32(out, self.pools.istr(&op.lowered_op_kind));
            write_u32(out, self.pools.istr(&op.expr_hash));
        }
        write_usize_u32(out, debug.expr_to_ops.len(), "debug expr map count")?;
        for entry in &debug.expr_to_ops {
            write_u32(out, self.pools.istr(&entry.expr_hash));
            write_usize_u32(out, entry.lowered_op_ids.len(), "debug expr op-id count")?;
            for id in &entry.lowered_op_ids {
                write_u32(out, self.pools.istr(id));
            }
        }
        Ok(())
    }
}

fn encode_function(
    pools: &mut Pools,
    fn_index: &BTreeMap<String, u32>,
    ir: &LoweredFunctionIr,
) -> Result<Vec<u8>> {
    let mut encoder = FnEncoder {
        pools,
        tables: FnTables::default(),
        fn_index,
    };

    // The `.cdb` consumer indexes its frame tables by slot, so slots must be
    // dense and in row order (lowering allocates them that way; fail closed if
    // that ever changes rather than emit an unconsumable file).
    for (index, param) in ir.params.iter().enumerate() {
        if param.slot != index {
            bail!(
                "CIR param slots must be dense and in order (row {index} has slot {})",
                param.slot
            );
        }
    }
    for (index, local) in ir.locals.iter().enumerate() {
        if local.slot != index {
            bail!(
                "CIR local slots must be dense and in order (row {index} has slot {})",
                local.slot
            );
        }
    }

    // Encode the table-referencing tail first so the type/value tables are
    // complete, then assemble the section with the tables ahead of the tail.
    let mut tail = Vec::new();
    write_u32(&mut tail, encoder.tables.tref(&ir.return_type_hash));
    write_usize_u32(&mut tail, ir.params.len(), "param count")?;
    for param in &ir.params {
        write_usize_u32(&mut tail, param.slot, "param slot")?;
        write_u32(&mut tail, encoder.tables.tref(&param.type_hash));
    }
    write_usize_u32(&mut tail, ir.locals.len(), "local count")?;
    for local in &ir.locals {
        write_usize_u32(&mut tail, local.slot, "local slot")?;
        write_u32(&mut tail, encoder.tables.tref(&local.type_hash));
        write_u64(&mut tail, local.size_bytes);
    }
    write_usize_u32(&mut tail, ir.operations.len(), "op count")?;
    for op in &ir.operations {
        encoder.encode_op(&mut tail, op)?;
    }
    encoder.encode_debug_map(&mut tail, &ir.debug_map)?;

    let mut out = Vec::new();
    write_u32(&mut out, encoder.pools.istr(&ir.schema));
    write_u32(&mut out, encoder.pools.istr(&ir.symbol_hash));
    write_u32(&mut out, encoder.pools.istr(&ir.function_def_hash));
    write_u32(&mut out, encoder.pools.istr(&ir.function_sig_hash));
    write_u32(&mut out, encoder.pools.istr(&ir.typed_body_expr_hash));
    write_usize_u32(&mut out, ir.type_layouts.len(), "layout count")?;
    for layout in &ir.type_layouts {
        write_u32(&mut out, encoder.pools.istr(&layout.type_hash));
        write_u32(&mut out, encoder.pools.istr(&layout.kind));
        write_u64(&mut out, layout.size_bytes);
        write_u64(&mut out, layout.align_bytes);
        write_u32(&mut out, encoder.pools.istr(&layout.abi.pass));
        write_u32(&mut out, encoder.pools.istr(&layout.abi.return_));
        write_u32(&mut out, encoder.pools.istr(&canonical_json(&layout.metadata)));
    }
    let layout_index: BTreeMap<&str, u32> = ir
        .type_layouts
        .iter()
        .enumerate()
        .map(|(index, layout)| (layout.type_hash.as_str(), index as u32))
        .collect();
    // The table vectors are read out directly while their strings are interned
    // into the pool, so split the field borrows explicitly.
    let tables = &encoder.tables;
    let pools = &mut *encoder.pools;
    write_usize_u32(&mut out, tables.types.len(), "type table count")?;
    for type_hash in &tables.types {
        write_u32(&mut out, pools.istr(type_hash));
        let layout_ref = layout_index
            .get(type_hash.as_str())
            .copied()
            .unwrap_or(NO_LAYOUT);
        write_u32(&mut out, layout_ref);
        let layout = (layout_ref != NO_LAYOUT).then(|| &ir.type_layouts[layout_ref as usize]);
        let (meta_kind, meta_size) = type_meta_columns(type_hash, layout);
        write_u8(&mut out, meta_kind);
        write_u64(&mut out, meta_size);
    }
    write_usize_u32(&mut out, tables.values.len(), "value table count")?;
    for id in &tables.values {
        write_u32(&mut out, pools.istr(id));
    }
    out.extend_from_slice(&tail);
    Ok(out)
}

/// Encode a lowered-IR closure as CIR bytes. `functions[entry_index]` is the
/// entry; `functions[i].symbol_hash` must be unique and sorted ascending (the
/// canonical function-table order).
pub(crate) fn encode_cir(
    target: &str,
    entry_index: u32,
    functions: &[LoweredFunctionIr],
) -> Result<Vec<u8>> {
    if functions.is_empty() {
        bail!("CIR requires at least one function");
    }
    if entry_index as usize >= functions.len() {
        bail!("CIR entry index {entry_index} out of range");
    }
    let mut fn_index = BTreeMap::new();
    for (index, function) in functions.iter().enumerate() {
        if index > 0 && functions[index - 1].symbol_hash >= function.symbol_hash {
            bail!("CIR function table must be sorted by symbol hash");
        }
        fn_index.insert(function.symbol_hash.clone(), index as u32);
    }

    let mut pools = Pools::default();
    let target_ref = pools.istr(target);
    let symbol_refs: Vec<u32> = functions
        .iter()
        .map(|function| pools.istr(&function.symbol_hash))
        .collect();
    let sections: Vec<Vec<u8>> = functions
        .iter()
        .map(|function| encode_function(&mut pools, &fn_index, function))
        .collect::<Result<Vec<_>>>()?;

    let mut out = Vec::new();
    out.extend_from_slice(CIR_MAGIC);
    write_u32(&mut out, CIR_VERSION);
    write_usize_u32(&mut out, pools.strings.len(), "string pool count")?;
    for string in &pools.strings {
        write_usize_u32(&mut out, string.len(), "string byte len")?;
    }
    for string in &pools.strings {
        out.extend_from_slice(string.as_bytes());
    }
    write_usize_u32(&mut out, pools.data.len(), "data pool count")?;
    for blob in &pools.data {
        write_usize_u32(&mut out, blob.len(), "data blob len")?;
    }
    for blob in &pools.data {
        out.extend_from_slice(blob);
    }
    write_u32(&mut out, target_ref);
    write_u32(&mut out, entry_index);
    write_usize_u32(&mut out, functions.len(), "function count")?;
    let mut offset = 0u32;
    for (symbol_ref, section) in symbol_refs.iter().zip(&sections) {
        write_u32(&mut out, *symbol_ref);
        write_u32(&mut out, offset);
        write_usize_u32(&mut out, section.len(), "function section len")?;
        offset = offset
            .checked_add(u32::try_from(section.len()).expect("section fits u32"))
            .ok_or_else(|| anyhow!("CIR section region exceeds u32 range"))?;
    }
    for section in &sections {
        out.extend_from_slice(section);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// decoder
// ---------------------------------------------------------------------------

pub(crate) struct CirProgram {
    pub(crate) target: String,
    pub(crate) entry_index: u32,
    pub(crate) functions: Vec<LoweredFunctionIr>,
}

struct FnDecoder<'a> {
    strings: &'a [String],
    data: &'a [Vec<u8>],
    fn_symbols: &'a [String],
    types: Vec<String>,
    values: Vec<String>,
}

impl FnDecoder<'_> {
    fn pstr(&self, index: u32) -> Result<String> {
        self.strings
            .get(index as usize)
            .cloned()
            .ok_or_else(|| anyhow!("CIR string index {index} out of range"))
    }

    fn rstr(&self, reader: &mut Reader) -> Result<String> {
        let index = reader.u32()?;
        self.pstr(index)
    }

    fn rostr(&self, reader: &mut Reader) -> Result<Option<String>> {
        match reader.u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.rstr(reader)?)),
            other => bail!("CIR invalid option flag {other}"),
        }
    }

    fn rtype(&self, reader: &mut Reader) -> Result<String> {
        let index = reader.u32()?;
        self.types
            .get(index as usize)
            .cloned()
            .ok_or_else(|| anyhow!("CIR type index {index} out of range"))
    }

    fn rvalue(&self, reader: &mut Reader) -> Result<String> {
        let index = reader.u32()?;
        self.values
            .get(index as usize)
            .cloned()
            .ok_or_else(|| anyhow!("CIR value index {index} out of range"))
    }

    fn rovalue(&self, reader: &mut Reader) -> Result<Option<String>> {
        match reader.u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.rvalue(reader)?)),
            other => bail!("CIR invalid option flag {other}"),
        }
    }

    fn rdata_hex(&self, reader: &mut Reader) -> Result<String> {
        let index = reader.u32()?;
        let blob = self
            .data
            .get(index as usize)
            .ok_or_else(|| anyhow!("CIR data index {index} out of range"))?;
        Ok(bytes_to_hex(blob))
    }

    fn rtrap(&self, reader: &mut Reader) -> Result<Option<LoweredTrap>> {
        match reader.u8()? {
            0 => Ok(None),
            1 => Ok(Some(LoweredTrap {
                condition: self.rstr(reader)?,
                code: self.rstr(reader)?,
            })),
            other => bail!("CIR invalid option flag {other}"),
        }
    }

    fn rusize(&self, reader: &mut Reader) -> Result<usize> {
        Ok(reader.u32()? as usize)
    }

    /// Read a binary/unary op's consumer columns and fail unless they equal
    /// what the registry reproduces for `kind` (the honesty half of the
    /// consumer columns; see the module doc).
    fn rcheck_sem_columns(&self, reader: &mut Reader, kind: &str) -> Result<()> {
        let stored = (reader.u8()?, reader.u8()?, reader.u8()?);
        if stored != sem_columns(kind)? {
            bail!("CIR operator consumer columns are inconsistent for kind {kind}");
        }
        Ok(())
    }

    fn rblock(&self, reader: &mut Reader) -> Result<LoweredBlock> {
        let count = self.rusize(reader)?;
        let mut operations = Vec::with_capacity(count);
        for _ in 0..count {
            operations.push(self.rop(reader)?);
        }
        let result = self.rvalue(reader)?;
        Ok(LoweredBlock { operations, result })
    }

    fn rplace(&self, reader: &mut Reader) -> Result<LoweredPlace> {
        let kind = reader.u8()?;
        Ok(match kind {
            placekind::PARAM => LoweredPlace::Param {
                slot: self.rusize(reader)?,
                type_hash: self.rtype(reader)?,
                indirect: reader.u8()? != 0,
            },
            placekind::LOCAL => LoweredPlace::Local {
                slot: self.rusize(reader)?,
                type_hash: self.rtype(reader)?,
            },
            placekind::FIELD => LoweredPlace::Field {
                base: self.rvalue(reader)?,
                field: self.rstr(reader)?,
                field_symbol: self.rostr(reader)?,
                owner_type_hash: self.rtype(reader)?,
                offset_bytes: reader.u64()?,
                type_hash: self.rtype(reader)?,
            },
            placekind::ENUM_PAYLOAD => LoweredPlace::EnumPayload {
                base: self.rvalue(reader)?,
                variant: self.rstr(reader)?,
                variant_symbol: self.rostr(reader)?,
                owner_type_hash: self.rtype(reader)?,
                tag_value: reader.u64()?,
                payload_offset_bytes: reader.u64()?,
                type_hash: self.rtype(reader)?,
            },
            placekind::INDEX => LoweredPlace::Index {
                base: self.rvalue(reader)?,
                index: self.rvalue(reader)?,
                element_type_hash: self.rtype(reader)?,
                type_hash: self.rtype(reader)?,
            },
            other => bail!("CIR unknown place kind {other}"),
        })
    }

    fn rop(&self, reader: &mut Reader) -> Result<LoweredOp> {
        let code = reader.u8()?;
        Ok(match code {
            opcode::PARAM => LoweredOp::Param {
                id: self.rvalue(reader)?,
                slot: self.rusize(reader)?,
                type_hash: self.rtype(reader)?,
            },
            opcode::CONST_I64 => LoweredOp::ConstI64 {
                id: self.rvalue(reader)?,
                value: reader.i64()?.to_string(),
                type_hash: self.rtype(reader)?,
            },
            opcode::CONST_BOOL => LoweredOp::ConstBool {
                id: self.rvalue(reader)?,
                value: reader.u8()? != 0,
                type_hash: self.rtype(reader)?,
            },
            opcode::CONST_UNIT => LoweredOp::ConstUnit {
                id: self.rvalue(reader)?,
                type_hash: self.rtype(reader)?,
            },
            opcode::UNARY => {
                let id = self.rvalue(reader)?;
                let kind = self.rstr(reader)?;
                self.rcheck_sem_columns(reader, &kind)?;
                LoweredOp::Unary {
                    id,
                    kind,
                    value: self.rvalue(reader)?,
                    type_hash: self.rtype(reader)?,
                }
            }
            opcode::INT_CAST => LoweredOp::IntCast {
                id: self.rvalue(reader)?,
                value: self.rvalue(reader)?,
                type_hash: self.rtype(reader)?,
            },
            opcode::BINARY => {
                let id = self.rvalue(reader)?;
                let kind = self.rstr(reader)?;
                self.rcheck_sem_columns(reader, &kind)?;
                LoweredOp::Binary {
                    id,
                    kind,
                    left: self.rvalue(reader)?,
                    right: self.rvalue(reader)?,
                    type_hash: self.rtype(reader)?,
                    trap: self.rtrap(reader)?,
                }
            }
            opcode::CALL => {
                let id = self.rvalue(reader)?;
                let target = reader.u32()?;
                let target_symbol_hash = self
                    .fn_symbols
                    .get(target as usize)
                    .cloned()
                    .ok_or_else(|| anyhow!("CIR call target index {target} out of range"))?;
                let target_abi_symbol = self.rostr(reader)?;
                let arg_count = self.rusize(reader)?;
                let mut args = Vec::with_capacity(arg_count);
                for _ in 0..arg_count {
                    args.push(self.rvalue(reader)?);
                }
                LoweredOp::Call {
                    id,
                    target_symbol_hash,
                    target_abi_symbol,
                    args,
                    return_address: self.rovalue(reader)?,
                    type_hash: self.rtype(reader)?,
                }
            }
            opcode::IF => LoweredOp::If {
                id: self.rvalue(reader)?,
                cond: self.rvalue(reader)?,
                then_block: self.rblock(reader)?,
                else_block: self.rblock(reader)?,
                type_hash: self.rtype(reader)?,
            },
            opcode::CASE => {
                let id = self.rvalue(reader)?;
                let scrutinee = self.rvalue(reader)?;
                let enum_type_hash = self.rtype(reader)?;
                let arm_count = self.rusize(reader)?;
                let mut arms = Vec::with_capacity(arm_count);
                for _ in 0..arm_count {
                    arms.push(LoweredCaseArm {
                        variant: self.rstr(reader)?,
                        variant_symbol: self.rostr(reader)?,
                        tag_value: reader.u64()?,
                        payload_type_hash: self.rtype(reader)?,
                        payload_offset_bytes: reader.u64()?,
                        block: self.rblock(reader)?,
                    });
                }
                LoweredOp::Case {
                    id,
                    scrutinee,
                    enum_type_hash,
                    arms,
                    type_hash: self.rtype(reader)?,
                }
            }
            opcode::FOLD => LoweredOp::Fold {
                id: self.rvalue(reader)?,
                target_address: self.rvalue(reader)?,
                target_type_hash: self.rtype(reader)?,
                len: self.rvalue(reader)?,
                init: self.rvalue(reader)?,
                index_slot: self.rusize(reader)?,
                acc_slot: self.rusize(reader)?,
                item_slot: self.rusize(reader)?,
                body: self.rblock(reader)?,
                element_type_hash: self.rtype(reader)?,
                acc_type_hash: self.rtype(reader)?,
                type_hash: self.rtype(reader)?,
            },
            opcode::LOOP => LoweredOp::Loop {
                id: self.rvalue(reader)?,
                acc_slot: self.rusize(reader)?,
                init: self.rvalue(reader)?,
                cond: self.rblock(reader)?,
                body: self.rblock(reader)?,
                acc_type_hash: self.rtype(reader)?,
                type_hash: self.rtype(reader)?,
            },
            opcode::BORROW_SHARED => LoweredOp::BorrowShared {
                id: self.rvalue(reader)?,
                address: self.rvalue(reader)?,
                region: self.rstr(reader)?,
                referent_type_hash: self.rtype(reader)?,
                type_hash: self.rtype(reader)?,
            },
            opcode::BORROW_MUT => LoweredOp::BorrowMut {
                id: self.rvalue(reader)?,
                address: self.rvalue(reader)?,
                region: self.rstr(reader)?,
                referent_type_hash: self.rtype(reader)?,
                type_hash: self.rtype(reader)?,
            },
            opcode::DEREF_SHARED => LoweredOp::DerefShared {
                id: self.rvalue(reader)?,
                reference: self.rvalue(reader)?,
                referent_type_hash: self.rtype(reader)?,
            },
            opcode::DEREF_MUT => LoweredOp::DerefMut {
                id: self.rvalue(reader)?,
                reference: self.rvalue(reader)?,
                referent_type_hash: self.rtype(reader)?,
            },
            opcode::DEREF_BOX => LoweredOp::DerefBox {
                id: self.rvalue(reader)?,
                box_value: self.rvalue(reader)?,
                box_type_hash: self.rtype(reader)?,
                element_type_hash: self.rtype(reader)?,
            },
            opcode::UNBOX_MOVE => LoweredOp::UnboxMove {
                id: self.rvalue(reader)?,
                box_value: self.rvalue(reader)?,
                box_type_hash: self.rtype(reader)?,
                element_type_hash: self.rtype(reader)?,
                dest_slot: self.rusize(reader)?,
            },
            opcode::HEAP_ALLOC => LoweredOp::HeapAlloc {
                id: self.rvalue(reader)?,
                size_bytes: reader.u64()?,
                align_bytes: reader.u64()?,
                element_type_hash: self.rtype(reader)?,
                type_hash: self.rtype(reader)?,
            },
            opcode::PTR_CAST => LoweredOp::PtrCast {
                id: self.rvalue(reader)?,
                value: self.rvalue(reader)?,
                source_type_hash: self.rtype(reader)?,
                type_hash: self.rtype(reader)?,
            },
            opcode::DEREF_RAW => LoweredOp::DerefRaw {
                id: self.rvalue(reader)?,
                pointer: self.rvalue(reader)?,
                pointer_type_hash: self.rtype(reader)?,
                pointee_type_hash: self.rtype(reader)?,
                mutable: reader.u8()? != 0,
            },
            opcode::ADDR_OF_PARAM => LoweredOp::AddrOfParam {
                id: self.rvalue(reader)?,
                place: self.rplace(reader)?,
            },
            opcode::ADDR_OF_LOCAL => LoweredOp::AddrOfLocal {
                id: self.rvalue(reader)?,
                place: self.rplace(reader)?,
            },
            opcode::ADDR_OF_FIELD => LoweredOp::AddrOfField {
                id: self.rvalue(reader)?,
                place: self.rplace(reader)?,
            },
            opcode::ADDR_OF_ENUM_PAYLOAD => LoweredOp::AddrOfEnumPayload {
                id: self.rvalue(reader)?,
                place: self.rplace(reader)?,
            },
            opcode::ADDR_OF_INDEX => LoweredOp::AddrOfIndex {
                id: self.rvalue(reader)?,
                place: self.rplace(reader)?,
            },
            opcode::STATIC_DATA_ADDRESS => {
                let id = self.rvalue(reader)?;
                let static_data_hash = self.rstr(reader)?;
                let bytes_hex = self.rdata_hex(reader)?;
                let len = reader.u64()?;
                LoweredOp::StaticDataAddress {
                    id,
                    static_data_hash,
                    bytes_hex,
                    len,
                    element_type_hash: self.rtype(reader)?,
                }
            }
            opcode::CONSTRUCT_SLICE => LoweredOp::ConstructSlice {
                id: self.rvalue(reader)?,
                address: self.rvalue(reader)?,
                data_address: self.rvalue(reader)?,
                len: self.rvalue(reader)?,
                element_type_hash: self.rtype(reader)?,
                type_hash: self.rtype(reader)?,
            },
            opcode::SLICE_LEN => LoweredOp::SliceLen {
                id: self.rvalue(reader)?,
                slice: self.rvalue(reader)?,
                slice_type_hash: self.rtype(reader)?,
                type_hash: self.rtype(reader)?,
            },
            opcode::SLICE_DATA => LoweredOp::SliceData {
                id: self.rvalue(reader)?,
                slice: self.rvalue(reader)?,
                slice_type_hash: self.rtype(reader)?,
                element_type_hash: self.rtype(reader)?,
            },
            opcode::VEC_NEW => LoweredOp::VecNew {
                id: self.rvalue(reader)?,
                address: self.rvalue(reader)?,
                capacity: reader.u64()?,
                element_type_hash: self.rtype(reader)?,
                type_hash: self.rtype(reader)?,
            },
            opcode::VEC_PUSH => LoweredOp::VecPush {
                id: self.rvalue(reader)?,
                vec_address: self.rvalue(reader)?,
                value: self.rvalue(reader)?,
                vec_type_hash: self.rtype(reader)?,
                element_type_hash: self.rtype(reader)?,
                type_hash: self.rtype(reader)?,
            },
            opcode::VEC_GET => LoweredOp::VecGet {
                id: self.rvalue(reader)?,
                vec_address: self.rvalue(reader)?,
                index: self.rvalue(reader)?,
                vec_type_hash: self.rtype(reader)?,
                element_type_hash: self.rtype(reader)?,
                type_hash: self.rtype(reader)?,
            },
            opcode::VEC_LEN => LoweredOp::VecLen {
                id: self.rvalue(reader)?,
                vec_address: self.rvalue(reader)?,
                vec_type_hash: self.rtype(reader)?,
                type_hash: self.rtype(reader)?,
            },
            opcode::STRING_NEW => {
                let id = self.rvalue(reader)?;
                let address = self.rvalue(reader)?;
                let static_data_hash = self.rstr(reader)?;
                let bytes_hex = self.rdata_hex(reader)?;
                let len = reader.u64()?;
                LoweredOp::StringNew {
                    id,
                    address,
                    static_data_hash,
                    bytes_hex,
                    len,
                    type_hash: self.rtype(reader)?,
                }
            }
            opcode::STRING_LEN => LoweredOp::StringLen {
                id: self.rvalue(reader)?,
                string_address: self.rvalue(reader)?,
                string_type_hash: self.rtype(reader)?,
                type_hash: self.rtype(reader)?,
            },
            opcode::STRING_WITH_CAPACITY => LoweredOp::StringWithCapacity {
                id: self.rvalue(reader)?,
                address: self.rvalue(reader)?,
                capacity: self.rvalue(reader)?,
                type_hash: self.rtype(reader)?,
            },
            opcode::STRING_PUSH => LoweredOp::StringPush {
                id: self.rvalue(reader)?,
                string_address: self.rvalue(reader)?,
                value: self.rvalue(reader)?,
                string_type_hash: self.rtype(reader)?,
                type_hash: self.rtype(reader)?,
            },
            opcode::STRING_GET => LoweredOp::StringGet {
                id: self.rvalue(reader)?,
                string_address: self.rvalue(reader)?,
                index: self.rvalue(reader)?,
                string_type_hash: self.rtype(reader)?,
                type_hash: self.rtype(reader)?,
            },
            opcode::STRING_SET => LoweredOp::StringSet {
                id: self.rvalue(reader)?,
                string_address: self.rvalue(reader)?,
                index: self.rvalue(reader)?,
                value: self.rvalue(reader)?,
                string_type_hash: self.rtype(reader)?,
                type_hash: self.rtype(reader)?,
            },
            opcode::ARG_COUNT => LoweredOp::ArgCount {
                id: self.rvalue(reader)?,
                type_hash: self.rtype(reader)?,
            },
            opcode::ARG_LEN => LoweredOp::ArgLen {
                id: self.rvalue(reader)?,
                index: self.rvalue(reader)?,
                type_hash: self.rtype(reader)?,
            },
            opcode::ARG_BYTE => LoweredOp::ArgByte {
                id: self.rvalue(reader)?,
                index: self.rvalue(reader)?,
                byte: self.rvalue(reader)?,
                type_hash: self.rtype(reader)?,
            },
            opcode::BOUNDS_CHECK => LoweredOp::BoundsCheck {
                id: self.rvalue(reader)?,
                index: self.rvalue(reader)?,
                len: reader.u64()?,
                len_value: self.rovalue(reader)?,
                type_hash: self.rtype(reader)?,
            },
            opcode::SLICE_RANGE_CHECK => LoweredOp::SliceRangeCheck {
                id: self.rvalue(reader)?,
                start: self.rvalue(reader)?,
                len: self.rvalue(reader)?,
                source_len: self.rvalue(reader)?,
                type_hash: self.rtype(reader)?,
            },
            opcode::LOAD_ENUM_TAG => LoweredOp::LoadEnumTag {
                id: self.rvalue(reader)?,
                address: self.rvalue(reader)?,
                type_hash: self.rtype(reader)?,
            },
            opcode::STORE_ENUM_TAG => LoweredOp::StoreEnumTag {
                address: self.rvalue(reader)?,
                type_hash: self.rtype(reader)?,
                variant: self.rstr(reader)?,
                variant_symbol: self.rostr(reader)?,
                tag_value: reader.u64()?,
            },
            opcode::LOAD => LoweredOp::Load {
                id: self.rvalue(reader)?,
                address: self.rvalue(reader)?,
                type_hash: self.rtype(reader)?,
            },
            opcode::STORE => LoweredOp::Store {
                address: self.rvalue(reader)?,
                value: self.rvalue(reader)?,
                type_hash: self.rtype(reader)?,
            },
            opcode::COPY => LoweredOp::Copy {
                id: self.rvalue(reader)?,
                value: self.rvalue(reader)?,
                type_hash: self.rtype(reader)?,
            },
            opcode::MOVE => LoweredOp::Move {
                id: self.rvalue(reader)?,
                address: self.rvalue(reader)?,
                type_hash: self.rtype(reader)?,
            },
            opcode::DROP => LoweredOp::Drop {
                address: self.rvalue(reader)?,
                type_hash: self.rtype(reader)?,
            },
            opcode::FREE_BOX_SHELL => LoweredOp::FreeBoxShell {
                address: self.rvalue(reader)?,
                box_type_hash: self.rtype(reader)?,
            },
            opcode::BORROW_DEBUG => LoweredOp::BorrowDebug {
                address: self.rvalue(reader)?,
                mutable: reader.u8()? != 0,
                region: self.rostr(reader)?,
                type_hash: self.rtype(reader)?,
            },
            opcode::RETURN => LoweredOp::Return {
                value: self.rvalue(reader)?,
                type_hash: self.rtype(reader)?,
            },
            opcode::EARLY_RETURN => LoweredOp::EarlyReturn {
                value: self.rvalue(reader)?,
                type_hash: self.rtype(reader)?,
            },
            other => bail!("CIR unknown opcode {other}"),
        })
    }

    fn rdebug_map(&self, reader: &mut Reader) -> Result<LoweredDebugMap> {
        let schema = self.rstr(reader)?;
        let op_count = self.rusize(reader)?;
        let mut operations = Vec::with_capacity(op_count);
        for _ in 0..op_count {
            operations.push(LoweredDebugOp {
                lowered_op_id: self.rstr(reader)?,
                value_id: self.rstr(reader)?,
                lowered_op_kind: self.rstr(reader)?,
                expr_hash: self.rstr(reader)?,
            });
        }
        let map_count = self.rusize(reader)?;
        let mut expr_to_ops = Vec::with_capacity(map_count);
        for _ in 0..map_count {
            let expr_hash = self.rstr(reader)?;
            let id_count = self.rusize(reader)?;
            let mut lowered_op_ids = Vec::with_capacity(id_count);
            for _ in 0..id_count {
                lowered_op_ids.push(self.rstr(reader)?);
            }
            expr_to_ops.push(LoweredExprOpMap {
                expr_hash,
                lowered_op_ids,
            });
        }
        Ok(LoweredDebugMap {
            schema,
            operations,
            expr_to_ops,
        })
    }
}

fn decode_function(
    strings: &[String],
    data: &[Vec<u8>],
    fn_symbols: &[String],
    section: &[u8],
) -> Result<LoweredFunctionIr> {
    let mut reader = Reader::new(section);
    let mut decoder = FnDecoder {
        strings,
        data,
        fn_symbols,
        types: Vec::new(),
        values: Vec::new(),
    };
    let schema = decoder.rstr(&mut reader)?;
    let symbol_hash = decoder.rstr(&mut reader)?;
    let function_def_hash = decoder.rstr(&mut reader)?;
    let function_sig_hash = decoder.rstr(&mut reader)?;
    let typed_body_expr_hash = decoder.rstr(&mut reader)?;
    let layout_count = decoder.rusize(&mut reader)?;
    let mut type_layouts = Vec::with_capacity(layout_count);
    for _ in 0..layout_count {
        let type_hash = decoder.rstr(&mut reader)?;
        let kind = decoder.rstr(&mut reader)?;
        let size_bytes = reader.u64()?;
        let align_bytes = reader.u64()?;
        let pass = decoder.rstr(&mut reader)?;
        let return_ = decoder.rstr(&mut reader)?;
        let metadata_json = decoder.rstr(&mut reader)?;
        let metadata: JsonValue = serde_json::from_str(&metadata_json)
            .map_err(|err| anyhow!("CIR layout metadata is not valid JSON: {err}"))?;
        type_layouts.push(LoweredTypeLayout {
            type_hash,
            kind,
            size_bytes,
            align_bytes,
            abi: LoweredTypeAbi { pass, return_ },
            metadata,
        });
    }
    let type_count = decoder.rusize(&mut reader)?;
    let mut types = Vec::with_capacity(type_count);
    for _ in 0..type_count {
        let type_hash = decoder.rstr(&mut reader)?;
        let layout_ref = reader.u32()?;
        let mut layout = None;
        if layout_ref != NO_LAYOUT {
            let linked = type_layouts.get(layout_ref as usize).ok_or_else(|| {
                anyhow!("CIR type-table layout index {layout_ref} out of range")
            })?;
            if linked.type_hash != type_hash {
                bail!("CIR type-table layout link mismatch for {type_hash}");
            }
            layout = Some(linked);
        }
        // Consumer-column honesty: the stored classification must equal what
        // the same inputs reproduce.
        let meta_kind = reader.u8()?;
        let meta_size = reader.u64()?;
        if (meta_kind, meta_size) != type_meta_columns(&type_hash, layout) {
            bail!("CIR type-meta consumer columns are inconsistent for {type_hash}");
        }
        types.push(type_hash);
    }
    decoder.types = types;
    let value_count = decoder.rusize(&mut reader)?;
    let mut values = Vec::with_capacity(value_count);
    for _ in 0..value_count {
        values.push(decoder.rstr(&mut reader)?);
    }
    decoder.values = values;

    let return_type_hash = decoder.rtype(&mut reader)?;
    let param_count = decoder.rusize(&mut reader)?;
    let mut params = Vec::with_capacity(param_count);
    for index in 0..param_count {
        let slot = decoder.rusize(&mut reader)?;
        if slot != index {
            bail!("CIR param slots must be dense and in order (row {index} has slot {slot})");
        }
        params.push(LoweredParamSlot {
            slot,
            type_hash: decoder.rtype(&mut reader)?,
        });
    }
    let local_count = decoder.rusize(&mut reader)?;
    let mut locals = Vec::with_capacity(local_count);
    for index in 0..local_count {
        let slot = decoder.rusize(&mut reader)?;
        if slot != index {
            bail!("CIR local slots must be dense and in order (row {index} has slot {slot})");
        }
        locals.push(LoweredLocalSlot {
            slot,
            type_hash: decoder.rtype(&mut reader)?,
            size_bytes: reader.u64()?,
        });
    }
    let op_count = decoder.rusize(&mut reader)?;
    let mut operations = Vec::with_capacity(op_count);
    for _ in 0..op_count {
        operations.push(decoder.rop(&mut reader)?);
    }
    let debug_map = decoder.rdebug_map(&mut reader)?;
    if !reader.done() {
        bail!("CIR function section has trailing bytes");
    }
    Ok(LoweredFunctionIr {
        schema,
        symbol_hash,
        function_def_hash,
        function_sig_hash,
        typed_body_expr_hash,
        params,
        locals,
        return_type_hash,
        type_layouts,
        operations,
        debug_map,
    })
}

pub(crate) fn decode_cir(bytes: &[u8]) -> Result<CirProgram> {
    let mut reader = Reader::new(bytes);
    if reader.take(4)? != CIR_MAGIC {
        bail!("CIR magic mismatch (not a CIR file)");
    }
    let version = reader.u32()?;
    if version != CIR_VERSION {
        bail!("CIR version {version} unsupported (expected {CIR_VERSION})");
    }
    let string_count = reader.u32()? as usize;
    let mut string_lens = Vec::with_capacity(string_count);
    for _ in 0..string_count {
        string_lens.push(reader.u32()? as usize);
    }
    let mut strings = Vec::with_capacity(string_count);
    for len in string_lens {
        let bytes = reader.take(len)?;
        strings.push(
            String::from_utf8(bytes.to_vec())
                .map_err(|_| anyhow!("CIR string pool entry is not UTF-8"))?,
        );
    }
    let data_count = reader.u32()? as usize;
    let mut data_lens = Vec::with_capacity(data_count);
    for _ in 0..data_count {
        data_lens.push(reader.u32()? as usize);
    }
    let mut data = Vec::with_capacity(data_count);
    for len in data_lens {
        data.push(reader.take(len)?.to_vec());
    }
    let target_ref = reader.u32()?;
    let target = strings
        .get(target_ref as usize)
        .cloned()
        .ok_or_else(|| anyhow!("CIR target string index out of range"))?;
    let entry_index = reader.u32()?;
    let function_count = reader.u32()? as usize;
    if function_count == 0 {
        bail!("CIR has no functions");
    }
    if entry_index as usize >= function_count {
        bail!("CIR entry index {entry_index} out of range");
    }
    let mut fn_symbols = Vec::with_capacity(function_count);
    let mut fn_spans = Vec::with_capacity(function_count);
    for _ in 0..function_count {
        let symbol_ref = reader.u32()?;
        let symbol = strings
            .get(symbol_ref as usize)
            .cloned()
            .ok_or_else(|| anyhow!("CIR function symbol index out of range"))?;
        let offset = reader.u32()? as usize;
        let len = reader.u32()? as usize;
        fn_symbols.push(symbol);
        fn_spans.push((offset, len));
    }
    let region_start = reader.pos;
    let mut functions = Vec::with_capacity(function_count);
    let mut expected_offset = 0usize;
    for (index, (offset, len)) in fn_spans.iter().enumerate() {
        if *offset != expected_offset {
            bail!("CIR function sections must be contiguous and in table order");
        }
        let start = region_start
            .checked_add(*offset)
            .filter(|start| {
                start
                    .checked_add(*len)
                    .is_some_and(|end| end <= bytes.len())
            })
            .ok_or_else(|| anyhow!("CIR function section out of range"))?;
        let section = &bytes[start..start + len];
        let function = decode_function(&strings, &data, &fn_symbols, section)?;
        if function.symbol_hash != fn_symbols[index] {
            bail!("CIR function section symbol mismatch");
        }
        functions.push(function);
        expected_offset += len;
    }
    if region_start + expected_offset != bytes.len() {
        bail!("CIR has trailing bytes after the last function section");
    }
    Ok(CirProgram {
        target,
        entry_index,
        functions,
    })
}

// ---------------------------------------------------------------------------
// emission (closure assembly + the built-in round-trip honesty gate)
// ---------------------------------------------------------------------------

pub struct CirEmission {
    pub bytes: Vec<u8>,
    pub summary: JsonValue,
}

impl CodeDb {
    /// Emit the CIR artifact for `entry_name`'s lowered-IR closure on the main
    /// branch. Fail-closed: external functions in the closure are rejected
    /// (the rung-0 corpus is the reference evaluator's domain), and the
    /// encoded bytes are decoded and compared back to the source IR before
    /// being returned, so an emission that loses information cannot succeed.
    pub fn emit_cir_main_branch(&mut self, entry_name: &str, target: &str) -> Result<CirEmission> {
        self.ensure_initialized()?;
        let branch = self.branch(MAIN_BRANCH)?;
        let entry_symbol = self
            .resolve_symbol_or_name(&branch.root_hash, entry_name)
            .map_err(|err| anyhow!("unknown entry function {entry_name}: {err}"))?;
        let root = self.load_root(&branch.root_hash)?;
        let mut symbols = self.reachable_symbols(&branch.root_hash, &entry_symbol)?;
        symbols.sort();
        symbols.dedup();
        for symbol in &symbols {
            let entry = self
                .root_symbol(&root, symbol)
                .ok_or_else(|| anyhow!("CIR reachable symbol missing from root {symbol}"))?;
            if self.definition_is_external(&entry.definition)? {
                bail!(
                    "CIR v0 does not encode external functions ({symbol} is reachable from {entry_name})"
                );
            }
        }
        let mut functions = Vec::with_capacity(symbols.len());
        let mut function_summaries = Vec::with_capacity(symbols.len());
        for symbol in &symbols {
            let artifact = self.lower_symbol_for_target(&branch.root_hash, symbol, target)?;
            function_summaries.push(json!({
                "symbol": symbol,
                "lowered_ir_hash": artifact.lowered_ir_hash,
                "op_count": artifact.ir.operations.len(),
            }));
            functions.push(artifact.ir);
        }
        let entry_index = symbols
            .iter()
            .position(|symbol| symbol == &entry_symbol)
            .map(|index| index as u32)
            .ok_or_else(|| anyhow!("CIR entry symbol missing from its own closure"))?;
        let bytes = encode_cir(target, entry_index, &functions)?;

        let decoded = decode_cir(&bytes)?;
        if decoded.target != target
            || decoded.entry_index != entry_index
            || decoded.functions != functions
        {
            bail!("CIR round-trip mismatch: decoded IR differs from the lowered IR (encoder bug)");
        }

        let entry_ir = &functions[entry_index as usize];
        let summary = json!({
            "schema": CIR_SCHEMA,
            "target": target,
            "entry": entry_name,
            "entry_symbol": entry_symbol,
            "entry_index": entry_index,
            "entry_op_count": entry_ir.operations.len(),
            "entry_param_count": entry_ir.params.len(),
            "entry_local_count": entry_ir.locals.len(),
            "function_count": functions.len(),
            "byte_len": bytes.len(),
            "cir_hash": bytes_oracle_hash(&bytes),
            "functions": function_summaries,
        });
        Ok(CirEmission { bytes, summary })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scalar_function(symbol: &str) -> LoweredFunctionIr {
        LoweredFunctionIr {
            schema: "codedb/lowered-function-ir/v2".to_string(),
            symbol_hash: symbol.to_string(),
            function_def_hash: "sha256:def".to_string(),
            function_sig_hash: "sha256:sig".to_string(),
            typed_body_expr_hash: "sha256:body".to_string(),
            params: vec![LoweredParamSlot {
                slot: 0,
                type_hash: "sha256:i64".to_string(),
            }],
            locals: vec![LoweredLocalSlot {
                slot: 0,
                type_hash: "sha256:i64".to_string(),
                size_bytes: 8,
            }],
            return_type_hash: "sha256:i64".to_string(),
            type_layouts: Vec::new(),
            operations: vec![
                LoweredOp::ConstI64 {
                    id: "v0".to_string(),
                    value: "-7".to_string(),
                    type_hash: "sha256:i64".to_string(),
                },
                LoweredOp::Binary {
                    id: "v1".to_string(),
                    kind: "add_i64".to_string(),
                    left: "v0".to_string(),
                    right: "v0".to_string(),
                    type_hash: "sha256:i64".to_string(),
                    trap: Some(LoweredTrap {
                        condition: "overflow".to_string(),
                        code: "int_overflow".to_string(),
                    }),
                },
                LoweredOp::If {
                    id: "v2".to_string(),
                    cond: "v1".to_string(),
                    then_block: LoweredBlock {
                        operations: vec![LoweredOp::ConstI64 {
                            id: "v3".to_string(),
                            value: "1".to_string(),
                            type_hash: "sha256:i64".to_string(),
                        }],
                        result: "v3".to_string(),
                    },
                    else_block: LoweredBlock {
                        operations: vec![LoweredOp::ConstI64 {
                            id: "v4".to_string(),
                            value: "2".to_string(),
                            type_hash: "sha256:i64".to_string(),
                        }],
                        result: "v4".to_string(),
                    },
                    type_hash: "sha256:i64".to_string(),
                },
                LoweredOp::Return {
                    value: "v2".to_string(),
                    type_hash: "sha256:i64".to_string(),
                },
            ],
            debug_map: LoweredDebugMap::default(),
        }
    }

    #[test]
    fn round_trips_a_synthetic_function_exactly() {
        let function = scalar_function("sha256:fn-a");
        let bytes = encode_cir("test-target", 0, std::slice::from_ref(&function)).unwrap();
        let decoded = decode_cir(&bytes).unwrap();
        assert_eq!(decoded.target, "test-target");
        assert_eq!(decoded.entry_index, 0);
        assert_eq!(decoded.functions, vec![function]);
    }

    #[test]
    fn encoding_is_deterministic() {
        let functions = vec![scalar_function("sha256:fn-a"), scalar_function("sha256:fn-b")];
        let first = encode_cir("test-target", 1, &functions).unwrap();
        let second = encode_cir("test-target", 1, &functions).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn rejects_unsorted_function_tables_and_bad_entry() {
        let functions = vec![scalar_function("sha256:fn-b"), scalar_function("sha256:fn-a")];
        assert!(encode_cir("t", 0, &functions).is_err());
        let one = vec![scalar_function("sha256:fn-a")];
        assert!(encode_cir("t", 1, &one).is_err());
    }

    #[test]
    fn rejects_truncated_and_corrupt_bytes() {
        let function = scalar_function("sha256:fn-a");
        let bytes = encode_cir("test-target", 0, std::slice::from_ref(&function)).unwrap();
        assert!(decode_cir(&bytes[..bytes.len() - 1]).is_err());
        let mut corrupt = bytes.clone();
        corrupt[0] = b'X';
        assert!(decode_cir(&corrupt).is_err());
    }
}
