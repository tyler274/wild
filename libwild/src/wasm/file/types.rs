use super::super::STANDARD_SECTION_LOOKUP_LEN;
use super::super::relocations::*;
use super::super::section_id;
use super::super::symbols::*;
use crate::alignment::Alignment;
use std::borrow::Cow;
use std::ops::Range;
use wasmparser::ConstExpr;
use wasmparser::DataKind;
use wasmparser::GlobalType;

#[derive(derive_more::Debug)]
pub(crate) struct File<'data> {
    #[debug(skip)]
    pub(crate) data: &'data [u8],

    #[debug(skip)]
    pub(crate) sections: Vec<SectionHeader>,

    /// For each standard Wasm section id, the index into `sections`, if present.
    #[debug(skip)]
    pub(crate) standard_section_index: [Option<u32>; STANDARD_SECTION_LOOKUP_LEN],

    #[debug(skip)]
    pub(crate) symbols: Vec<WasmSymbol>,

    /// Per-data-segment alignments from the linking `SegmentInfo` subsection.
    #[debug(skip)]
    pub(crate) segment_alignments: Vec<Alignment>,

    /// Init functions from the linking section (`InitFuncs`), in input order.
    #[debug(skip)]
    pub(crate) init_funcs: Vec<WasmInitFunc>,

    #[debug(skip)]
    pub(crate) reloc_sections: Vec<WasmRelocSection>,

    /// Entries from the `target_features` custom section, if present.
    #[debug(skip)]
    pub(crate) target_features: Vec<WasmTargetFeature<'data>>,

    pub(crate) num_function_imports: u32,
    pub(crate) num_global_imports: u32,
    pub(crate) num_defined_functions: u32,
    pub(crate) num_defined_globals: u32,
    pub(crate) num_data_segments: u32,
}

/// One entry of the Wasm tool-conventions `target_features` custom section.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WasmTargetFeature<'data> {
    pub(crate) prefix: u8,
    pub(crate) name: &'data str,
}

/// A constructor from the linking `InitFuncs` subsection.
///
/// `symbol_index` indexes the linking symbol table.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WasmInitFunc {
    pub(crate) priority: u32,
    pub(crate) symbol_index: u32,
}

/// A single section of a Wasm module.
#[derive(Debug, Default, Clone)]
pub(crate) struct SectionHeader {
    /// The wasm section id.
    pub(crate) id: u8,

    /// Byte range of the section (id + size + payload) within the original Wasm binary.
    pub(crate) payload_range: Range<u32>,

    /// For custom sections, the byte range within the input data of the section's name string.
    /// `None` for standard sections, whose canonical name is derived from `id`.
    pub(crate) name_range: Option<Range<u32>>,
}

impl SectionHeader {
    pub(crate) fn payload_range_usize(&self) -> Range<usize> {
        self.payload_range.start as usize..self.payload_range.end as usize
    }
}

pub(crate) fn standard_section_name(id: u8) -> Option<&'static [u8]> {
    Some(match id {
        section_id::TYPE => b"type",
        section_id::IMPORT => b"import",
        section_id::FUNCTION => b"function",
        section_id::TABLE => b"table",
        section_id::MEMORY => b"memory",
        section_id::GLOBAL => b"global",
        section_id::EXPORT => b"export",
        section_id::START => b"start",
        section_id::ELEMENT => b"element",
        section_id::CODE => b"code",
        section_id::DATA => b"data",
        section_id::DATA_COUNT => b"data_count",
        _ => return None,
    })
}

/// A single imported function. `module` / `name` borrow into the source bytes.
#[derive(Debug, Copy, Clone)]
pub(crate) struct WasmFunctionImport<'data> {
    pub(crate) module: &'data str,
    pub(crate) name: &'data str,
    /// Index into the `type` section.
    pub(crate) type_index: u32,
}

/// A single imported global.
#[derive(Debug, Copy, Clone)]
pub(crate) struct WasmGlobalImport<'data> {
    pub(crate) module: &'data str,
    pub(crate) name: &'data str,
    pub(crate) ty: GlobalType,
}

/// A global defined inside the module (not imported).
#[derive(Debug, Clone)]
pub(crate) struct WasmModuleGlobal<'data> {
    pub(crate) ty: GlobalType,
    pub(crate) init_expr: ConstExpr<'data>,
}

/// A single data segment from the `data` section.
#[derive(Debug, Clone)]
pub(crate) struct WasmDataSegment<'data> {
    pub(crate) kind: DataKind<'data>,
    pub(crate) data: &'data [u8],
    /// Byte offset of this segment's encoding within the input data section payload.
    pub(crate) section_offset: u32,
    /// Encoded size of this segment within the input data section payload.
    pub(crate) encoded_size: u32,
}

/// Layout for one data segment within an input object.
#[derive(Debug)]
pub(crate) struct WasmDataSegmentLayout<'data> {
    /// Index of this segment within the object's data section.
    pub(crate) segment_index: u32,
    pub(crate) data: &'data [u8],
    /// Range into the owning object's data-relocation list.
    pub(crate) reloc_range: Range<u32>,
    /// Section-payload offset of the first data byte.
    pub(crate) payload_start: u32,
    /// Output memory index after index remapping.
    pub(crate) output_memory_index: u32,
    /// Byte offset within the output module's linear memory where the payload is placed.
    pub(crate) output_memory_offset: u32,
    /// Encoded size of this segment within the output data section payload.
    pub(crate) encoded_output_size: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct WasmFunctionBody<'data> {
    /// Raw body bytes (locals + operators) without the LEB128 size prefix.
    pub(crate) bytes: Cow<'data, [u8]>,
    /// Byte offset of this body (starting at its size prefix) within the code section payload.
    pub(crate) code_offset: u32,
    /// Range into the owning object's code-relocation list.
    pub(crate) reloc_range: Range<u32>,
    /// Index of the object this body belongs to.
    pub(crate) object_index: usize,
}

pub(crate) fn is_debug_section_name(name: &[u8]) -> bool {
    name.starts_with(b".debug")
}
