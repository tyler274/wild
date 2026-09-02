use super::LINKING_SECTION_NAME;
use super::RELOC_SECTION_PREFIX;
use super::STANDARD_SECTION_LOOKUP_LEN;
use super::TARGET_FEATURES_SECTION_NAME;
use super::WASM_MAGIC;
use super::WASM_VERSION;
use super::Wasm;
use super::abi::*;
use super::gc::*;
use super::output::*;
use super::relocations::*;
use super::section_id;
use super::symbols::*;
use crate::alignment::Alignment;
use crate::bail;
use crate::ensure;
use crate::error::Context as _;
use crate::error::Result;
use crate::platform;
use crate::platform::Args as _;
use crate::symbol::UnversionedSymbolName;
use crate::value_flags::ValueFlags;
use leb128::write::unsigned_len as uleb128_size;
use linker_utils::utils::u32_from_slice;
use std::borrow::Cow;
use std::ops::Range;
use wasmparser::BinaryReader;
use wasmparser::CodeSectionReader;
use wasmparser::ConstExpr;
use wasmparser::DataKind;
use wasmparser::DataSectionReader;
use wasmparser::ExportSectionReader;
use wasmparser::FunctionSectionReader;
use wasmparser::GlobalSectionReader;
use wasmparser::GlobalType;
use wasmparser::ImportSectionReader;
use wasmparser::KnownCustom;
use wasmparser::Linking;
use wasmparser::MemorySectionReader;
use wasmparser::MemoryType;
use wasmparser::Parser;
use wasmparser::Payload;
use wasmparser::RelocationType;
use wasmparser::SymbolFlags;
use wasmparser::SymbolInfo;
use wasmparser::TypeRef;
use wasmparser::TypeSectionReader;

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

impl<'data> File<'data> {
    pub(crate) fn section_is_debug(&self, index: u32) -> bool {
        let Some(header) = self.sections.get(index as usize) else {
            return false;
        };
        let Some(name_range) = &header.name_range else {
            return false;
        };
        let name = self
            .data
            .get(name_range.start as usize..name_range.end as usize)
            .unwrap_or_default();
        is_debug_section_name(name)
    }

    /// Construct a `BinaryReader` over the payload of the standard section with the given id,
    /// or `None` if the input has no such section.
    pub(crate) fn standard_section_reader(&self, id: u8) -> Option<BinaryReader<'data>> {
        let section_index = self.standard_section_index.get(id as usize)?.as_ref()?;
        let header = self.sections.get(*section_index as usize)?;
        let payload = self.data.get(header.payload_range_usize())?;
        Some(BinaryReader::new(
            payload,
            u64::from(header.payload_range.start),
        ))
    }

    pub(crate) fn import_section_reader(&self) -> Result<Option<ImportSectionReader<'data>>> {
        self.standard_section_reader(section_id::IMPORT)
            .map(|r| ImportSectionReader::new(r).map_err(Into::into))
            .transpose()
    }

    pub(crate) fn function_section_reader(&self) -> Result<Option<FunctionSectionReader<'data>>> {
        self.standard_section_reader(section_id::FUNCTION)
            .map(|r| FunctionSectionReader::new(r).map_err(Into::into))
            .transpose()
    }

    pub(crate) fn global_section_reader(&self) -> Result<Option<GlobalSectionReader<'data>>> {
        self.standard_section_reader(section_id::GLOBAL)
            .map(|r| GlobalSectionReader::new(r).map_err(Into::into))
            .transpose()
    }

    pub(crate) fn data_section_reader(&self) -> Result<Option<DataSectionReader<'data>>> {
        self.standard_section_reader(section_id::DATA)
            .map(|r| DataSectionReader::new(r).map_err(Into::into))
            .transpose()
    }

    pub(crate) fn code_section_reader(&self) -> Result<Option<CodeSectionReader<'data>>> {
        self.standard_section_reader(section_id::CODE)
            .map(|r| CodeSectionReader::new(r).map_err(Into::into))
            .transpose()
    }

    pub(crate) fn memory_section_reader(&self) -> Result<Option<MemorySectionReader<'data>>> {
        self.standard_section_reader(section_id::MEMORY)
            .map(|r| MemorySectionReader::new(r).map_err(Into::into))
            .transpose()
    }

    pub(crate) fn export_section_reader(&self) -> Result<Option<ExportSectionReader<'data>>> {
        self.standard_section_reader(section_id::EXPORT)
            .map(|r| ExportSectionReader::new(r).map_err(Into::into))
            .transpose()
    }

    pub(crate) fn type_section_reader(&self) -> Result<Option<TypeSectionReader<'data>>> {
        self.standard_section_reader(section_id::TYPE)
            .map(|r| TypeSectionReader::new(r).map_err(Into::into))
            .transpose()
    }

    /// Type indices of functions defined in this module (excluding imports), in `function`
    /// section order. The function body for each entry lives in the `code` section.
    pub(crate) fn module_functions(&self) -> Result<Vec<u32>> {
        let Some(reader) = self.function_section_reader()? else {
            return Ok(Vec::new());
        };

        reader
            .into_iter()
            .map(|res| res.map_err(Into::into))
            .collect()
    }

    /// Globals defined in this module (excluding imports), in `global` section order.
    pub(crate) fn module_globals(&self) -> Result<Vec<WasmModuleGlobal<'data>>> {
        let Some(reader) = self.global_section_reader()? else {
            return Ok(Vec::new());
        };

        reader
            .into_iter()
            .map(|res| {
                res.map(|g| WasmModuleGlobal {
                    ty: g.ty,
                    init_expr: g.init_expr,
                })
                .map_err(Into::into)
            })
            .collect()
    }

    pub(crate) fn memories(&self) -> Result<Vec<MemoryType>> {
        let Some(reader) = self.memory_section_reader()? else {
            return Ok(Vec::new());
        };
        reader
            .into_iter()
            .map(|res| res.map_err(Into::into))
            .collect()
    }

    /// Function bodies in code-section order. The returned bytes include the body size prefix.
    pub(crate) fn function_bodies(&self) -> Result<Vec<WasmFunctionBody<'data>>> {
        let Some(reader) = self.code_section_reader()? else {
            return Ok(Vec::new());
        };
        let code_payload_start = self.standard_section_index[section_id::CODE as usize]
            .and_then(|i| self.sections.get(i as usize))
            .map_or(0, |h| h.payload_range.start);
        reader
            .into_iter()
            .map(|res| {
                res.map(|body| {
                    let range = body.range();
                    let range = range.start as usize..range.end as usize;
                    WasmFunctionBody {
                        bytes: Cow::Borrowed(&self.data[range.clone()]),
                        code_offset: range.start as u32 - code_payload_start,
                        reloc_range: 0..0,
                        object_index: 0,
                    }
                })
                .map_err(Into::into)
            })
            .collect()
    }

    /// Data segments in declaration order.
    pub(crate) fn data_segments(&self) -> Result<Vec<WasmDataSegment<'data>>> {
        let Some(reader) = self.data_section_reader()? else {
            return Ok(Vec::new());
        };

        let mut segments = Vec::new();
        let mut section_offset = u32::try_from(uleb128_size(u64::from(reader.count())))
            .context("Wasm data count LEB")?;
        for res in reader {
            let d = res?;
            let encoded_size = wasm_data_segment_encoded_size(&d.kind, d.data.len())?;
            segments.push(WasmDataSegment {
                kind: d.kind.clone(),
                data: d.data,
                section_offset,
                encoded_size,
            });
            section_offset = section_offset
                .checked_add(encoded_size)
                .ok_or_else(|| crate::error!("Wasm data section offset overflow"))?;
        }
        Ok(segments)
    }
}

impl<'data> platform::ObjectFile<'data> for File<'data> {
    type Platform = Wasm;

    fn parse_bytes(input: &'data [u8], _is_dynamic: bool) -> crate::error::Result<Self> {
        parse_wasm_module(input).context("failed to parse Wasm object file")
    }

    fn parse(
        input: &crate::input_data::InputBytes<'data>,
        _args: &<Self::Platform as platform::Platform>::Args,
    ) -> crate::error::Result<Self> {
        Self::parse_bytes(input.data, false)
    }

    fn is_dynamic(&self) -> bool {
        // Wasm has no notion of "dynamic objects" in the ELF sense yet.
        false
    }

    fn num_symbols(&self) -> usize {
        self.symbols.len()
    }

    fn symbols_iter(&self) -> impl Iterator<Item = &WasmSymbol> {
        self.symbols.iter()
    }

    fn symbol(
        &self,
        index: object::SymbolIndex,
    ) -> crate::error::Result<&<Self::Platform as platform::Platform>::SymtabEntry> {
        self.symbols
            .get(index.0)
            .ok_or_else(|| crate::error!("wasm symbol index {} out of range", index.0))
    }

    fn section_size(
        &self,
        header: &<Self::Platform as platform::Platform>::SectionHeader,
    ) -> crate::error::Result<u64> {
        Ok(header.payload_range.len() as u64)
    }

    fn symbol_name(
        &self,
        symbol: &<Self::Platform as platform::Platform>::SymtabEntry,
    ) -> crate::error::Result<&'data [u8]> {
        if !symbol.has_name() {
            return Ok(&[]);
        }
        self.data
            .get(symbol.name_range())
            .ok_or_else(|| crate::error!("wasm symbol name range out of bounds"))
    }

    fn symbol_offset_in_section(
        &self,
        symbol: &<Self::Platform as platform::Platform>::SymtabEntry,
        _section_index: object::SectionIndex,
    ) -> crate::error::Result<u64> {
        Ok(match symbol.kind {
            WasmSymbolKind::Data => u64::from(symbol.offset),
            _ => 0,
        })
    }

    fn num_sections(&self) -> usize {
        self.sections.len()
    }

    fn section_iter<'a>(&'a self) -> <Self::Platform as platform::Platform>::SectionIterator<'a> {
        self.sections.iter()
    }

    fn enumerate_sections(
        &self,
    ) -> impl Iterator<
        Item = (
            object::SectionIndex,
            &<Self::Platform as platform::Platform>::SectionHeader,
        ),
    > {
        self.sections
            .iter()
            .enumerate()
            .map(|(i, section)| (object::SectionIndex(i), section))
    }

    fn section(
        &self,
        index: object::SectionIndex,
    ) -> crate::error::Result<&<Self::Platform as platform::Platform>::SectionHeader> {
        self.sections
            .get(index.0)
            .ok_or_else(|| crate::error!("wasm section index {} out of range", index.0))
    }

    fn section_by_name(
        &self,
        name: &str,
    ) -> Option<(
        object::SectionIndex,
        &<Self::Platform as platform::Platform>::SectionHeader,
    )> {
        let needle = name.as_bytes();
        self.sections
            .iter()
            .enumerate()
            .find(|(_, header)| {
                if let Some(name_range) = &header.name_range {
                    self.data
                        .get(name_range.start as usize..name_range.end as usize)
                        == Some(needle)
                } else {
                    standard_section_name(header.id) == Some(needle)
                }
            })
            .map(|(i, header)| (object::SectionIndex(i), header))
    }

    fn symbol_section(
        &self,
        _symbol: &<Self::Platform as platform::Platform>::SymtabEntry,
        _index: object::SymbolIndex,
    ) -> crate::error::Result<Option<object::SectionIndex>> {
        Ok(None)
    }

    fn symbol_versions(&self) -> &[<Self::Platform as platform::Platform>::SymbolVersionIndex] {
        // Wasm doesn't have ELF-style symbol versioning.
        &[]
    }

    fn finalise_sizes_dynamic(
        &self,
        _lib_name: &[u8],
        _state: &mut <Self::Platform as platform::Platform>::DynamicLayoutStateExt<'data>,
        _mem_sizes: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
    ) -> crate::error::Result {
        Ok(())
    }

    fn apply_non_addressable_indexes_dynamic(
        &self,
        _indexes: &mut <Self::Platform as platform::Platform>::NonAddressableIndexes,
        _counts: &mut <Self::Platform as platform::Platform>::NonAddressableCounts,
        _state: &mut <Self::Platform as platform::Platform>::DynamicLayoutStateExt<'data>,
    ) -> crate::error::Result {
        Ok(())
    }

    fn section_name(&self, index: object::SectionIndex) -> crate::error::Result<&'data [u8]> {
        let header = self
            .sections
            .get(index.0)
            .ok_or_else(|| crate::error!("wasm section index {} out of range", index.0))?;
        if let Some(name_range) = &header.name_range {
            Ok(&self.data[name_range.start as usize..name_range.end as usize])
        } else {
            standard_section_name(header.id)
                .ok_or_else(|| crate::error!("unknown wasm section id {}", header.id))
        }
    }

    fn raw_section_data(
        &self,
        section: &<Self::Platform as platform::Platform>::SectionHeader,
    ) -> crate::error::Result<&'data [u8]> {
        Ok(&self.data[section.payload_range_usize()])
    }

    fn section_data(
        &self,
        section: &<Self::Platform as platform::Platform>::SectionHeader,
        _member: &bumpalo_herd::Member<'data>,
        _loaded_metrics: &crate::resolution::LoadedMetrics,
    ) -> crate::error::Result<&'data [u8]> {
        // Wasm sections are never compressed.
        self.raw_section_data(section)
    }

    fn copy_section_data(
        &self,
        section: &<Self::Platform as platform::Platform>::SectionHeader,
        out: &mut [u8],
    ) -> crate::error::Result {
        let bytes = self.raw_section_data(section)?;
        ensure!(
            out.len() == bytes.len(),
            "copy_section_data: output buffer size {} does not match section size {}",
            out.len(),
            bytes.len()
        );
        out.copy_from_slice(bytes);
        Ok(())
    }

    fn section_data_cow(
        &self,
        section: &<Self::Platform as platform::Platform>::SectionHeader,
    ) -> crate::error::Result<std::borrow::Cow<'data, [u8]>> {
        Ok(std::borrow::Cow::Borrowed(self.raw_section_data(section)?))
    }

    fn section_alignment(
        &self,
        _section: &<Self::Platform as platform::Platform>::SectionHeader,
    ) -> crate::error::Result<u64> {
        // Wasm sections themselves don't carry an alignment requirement.
        Ok(1)
    }

    fn relocations(
        &self,
        index: object::SectionIndex,
        _relocations: &<Self::Platform as platform::Platform>::RelocationSections,
    ) -> crate::error::Result<<Self::Platform as platform::Platform>::RelocationList<'data>> {
        let target = u32::try_from(index.0).unwrap_or(u32::MAX);
        let entries = decode_relocs_for(self, Some(target))?;
        Ok(RelocationList {
            entries,
            _phantom: std::marker::PhantomData,
        })
    }

    fn parse_relocations(
        &self,
    ) -> crate::error::Result<<Self::Platform as platform::Platform>::RelocationSections> {
        Ok(())
    }

    fn symbol_version_debug(&self, _symbol_index: object::SymbolIndex) -> Option<String> {
        // Wasm doesn't have ELF-style symbol versioning.
        None
    }

    fn section_display_name(&self, index: object::SectionIndex) -> std::borrow::Cow<'data, str> {
        self.section_name(index).map_or_else(
            |_| format!("<index {}>", index.0).into(),
            String::from_utf8_lossy,
        )
    }

    fn dynamic_tag_values(
        &self,
    ) -> Option<<Self::Platform as platform::Platform>::DynamicTagValues<'data>> {
        None
    }

    fn get_version_names(
        &self,
    ) -> crate::error::Result<<Self::Platform as platform::Platform>::VersionNames<'data>> {
        Ok(())
    }

    fn get_symbol_name_and_version(
        &self,
        symbol: &<Self::Platform as platform::Platform>::SymtabEntry,
        _local_index: usize,
        _version_names: &<Self::Platform as platform::Platform>::VersionNames<'data>,
    ) -> crate::error::Result<<Self::Platform as platform::Platform>::RawSymbolName<'data>> {
        Ok(RawSymbolName {
            name: self.symbol_name(symbol)?,
        })
    }

    fn should_enforce_undefined(
        &self,
        _resources: &crate::layout::GraphResources<'data, '_, Self::Platform>,
    ) -> bool {
        // Wasm has no dynamic objects yet, so this is never reached in practice.
        false
    }

    fn verneed_table(
        &self,
    ) -> crate::error::Result<<Self::Platform as platform::Platform>::VerneedTable<'data>> {
        Ok(VerneedTable { _phantom: &[] })
    }

    fn process_gnu_note_section(
        &self,
        _state: &mut <Self::Platform as platform::Platform>::ObjectLayoutStateExt<'data>,
        _section_index: object::SectionIndex,
    ) -> crate::error::Result {
        // Wasm objects don't carry GNU property notes.
        Ok(())
    }

    fn dynamic_tags(
        &self,
    ) -> crate::error::Result<&'data [<Self::Platform as platform::Platform>::DynamicEntry]> {
        Ok(&[])
    }
}

pub(crate) fn parse_wasm_module<'data>(input: &'data [u8]) -> Result<File<'data>> {
    ensure!(input.len() >= 8, "Wasm module too short");
    ensure!(input[..4] == WASM_MAGIC, "missing Wasm magic header");
    let version = u32_from_slice(&input[4..8]);
    ensure!(
        version == WASM_VERSION,
        "unsupported Wasm version {version}"
    );

    let mut sections: Vec<SectionHeader> = Vec::new();
    let mut symbols: Vec<WasmSymbol> = Vec::new();
    let mut segment_alignments: Vec<Alignment> = Vec::new();
    let mut init_funcs: Vec<WasmInitFunc> = Vec::new();
    let mut reloc_sections: Vec<WasmRelocSection> = Vec::new();
    let mut target_features: Vec<WasmTargetFeature<'data>> = Vec::new();
    let mut standard_section_index = [None; STANDARD_SECTION_LOOKUP_LEN];

    for payload in Parser::new(0).parse_all(input) {
        let payload = payload?;
        let Some((id, range)) = payload.as_section() else {
            continue;
        };

        let mut name_range: Option<Range<u32>> = None;

        if let Payload::CustomSection(reader) = &payload {
            let section_name = reader.name();
            let name_end = reader.data_offset();
            let name_start = name_end as usize - section_name.len();
            name_range = Some(name_start as u32..name_end as u32);

            if section_name == LINKING_SECTION_NAME {
                if let KnownCustom::Linking(linking) = reader.as_known() {
                    parse_linking_subsections(
                        input,
                        &linking,
                        &mut symbols,
                        &mut segment_alignments,
                        &mut init_funcs,
                    )?;
                }
            } else if section_name.starts_with(RELOC_SECTION_PREFIX) {
                if let KnownCustom::Reloc(reloc) = reader.as_known() {
                    reloc_sections.push(WasmRelocSection {
                        target_section_index: reloc.section_index(),
                        payload_range: name_end as u32..range.end as u32,
                    });
                }
            } else if section_name == TARGET_FEATURES_SECTION_NAME {
                target_features.extend(parse_target_features_payload(reader.data())?);
            }
        } else if (section_id::TYPE..=section_id::MAX).contains(&id) {
            standard_section_index[id as usize] = Some(sections.len() as u32);
        }

        sections.push(SectionHeader {
            id,
            payload_range: range.start as u32..range.end as u32,
            name_range,
        });
    }

    // Backfill names for unnamed undefined function/global symbols from the import section.
    // The Wasm linking convention allows symbol entries to omit the name when the symbol is
    // undefined; the canonical name lives in the import entry instead.
    backfill_unnamed_import_symbols(input, &standard_section_index, &sections, &mut symbols)?;

    let (num_function_imports, num_global_imports) =
        count_function_and_global_imports(input, &standard_section_index, &sections)?;
    let num_defined_functions = section_entry_count(
        input,
        &standard_section_index,
        &sections,
        section_id::FUNCTION,
    )?;
    let num_defined_globals = section_entry_count(
        input,
        &standard_section_index,
        &sections,
        section_id::GLOBAL,
    )?;
    let num_data_segments =
        section_entry_count(input, &standard_section_index, &sections, section_id::DATA)?;

    Ok(File {
        data: input,
        sections,
        standard_section_index,
        symbols,
        segment_alignments,
        init_funcs,
        reloc_sections,
        target_features,
        num_function_imports,
        num_global_imports,
        num_defined_functions,
        num_defined_globals,
        num_data_segments,
    })
}

pub(crate) fn count_function_and_global_imports(
    data: &[u8],
    standard_section_index: &[Option<u32>; STANDARD_SECTION_LOOKUP_LEN],
    sections: &[SectionHeader],
) -> Result<(u32, u32)> {
    let Some(section_index) = standard_section_index[section_id::IMPORT as usize] else {
        return Ok((0, 0));
    };
    let header = sections
        .get(section_index as usize)
        .ok_or_else(|| crate::error!("Wasm import section index out of range"))?;
    let payload = data
        .get(header.payload_range_usize())
        .ok_or_else(|| crate::error!("Wasm import section payload out of bounds"))?;
    let reader = ImportSectionReader::new(BinaryReader::new(
        payload,
        u64::from(header.payload_range.start),
    ))?;
    let mut num_function_imports = 0u32;
    let mut num_global_imports = 0u32;
    for import in reader.into_imports() {
        match import?.ty {
            TypeRef::Func(_) | TypeRef::FuncExact(_) => {
                num_function_imports = num_function_imports
                    .checked_add(1)
                    .ok_or_else(|| crate::error!("too many Wasm function imports"))?;
            }
            TypeRef::Global(_) => {
                num_global_imports = num_global_imports
                    .checked_add(1)
                    .ok_or_else(|| crate::error!("too many Wasm global imports"))?;
            }
            _ => {}
        }
    }
    Ok((num_function_imports, num_global_imports))
}

pub(crate) fn section_entry_count(
    data: &[u8],
    standard_section_index: &[Option<u32>; STANDARD_SECTION_LOOKUP_LEN],
    sections: &[SectionHeader],
    section_id: u8,
) -> Result<u32> {
    let Some(section_index) = standard_section_index[section_id as usize] else {
        return Ok(0);
    };
    let header = sections
        .get(section_index as usize)
        .ok_or_else(|| crate::error!("Wasm section index out of range"))?;
    let payload = data
        .get(header.payload_range_usize())
        .ok_or_else(|| crate::error!("Wasm section payload out of bounds"))?;
    let mut reader = BinaryReader::new(payload, u64::from(header.payload_range.start));
    Ok(reader.read_var_u32()?)
}

pub(crate) fn mark_all_wasm_units_live_and_scan_relocs<
    'data,
    'scope,
    A: platform::Arch<Platform = Wasm>,
>(
    object: &mut crate::layout::ObjectLayoutState<'data, Wasm>,
    resources: &'scope crate::layout::GraphResources<'data, 'scope, Wasm>,
    queue: &mut crate::layout::LocalWorkQueue<Wasm>,
    scope: &rayon::Scope<'scope>,
) -> Result {
    object.format_specific.mark_all_units_live();
    object
        .format_specific
        .ensure_relocs_decoded(object.object)?;

    let code_len = object.format_specific.code_relocations.len();
    for i in 0..code_len {
        let reloc = object.format_specific.code_relocations[i];
        note_wasm_reloc_edge::<A>(object, &reloc, resources, queue, scope)?;
    }
    let data_len = object.format_specific.data_relocations.len();
    for i in 0..data_len {
        let reloc = object.format_specific.data_relocations[i];
        note_wasm_reloc_edge::<A>(object, &reloc, resources, queue, scope)?;
    }

    enqueue_wasm_force_export_roots::<A>(object, resources, queue, scope);
    Ok(())
}

pub(crate) fn enqueue_wasm_gc_unit<'data, 'scope, A: platform::Arch<Platform = Wasm>>(
    object: &crate::layout::ObjectLayoutState<'data, Wasm>,
    unit: WasmGcUnit,
    resources: &'scope crate::layout::GraphResources<'data, 'scope, Wasm>,
    queue: &mut crate::layout::LocalWorkQueue<Wasm>,
    scope: &rayon::Scope<'scope>,
) {
    if object.format_specific.is_dead(unit) {
        queue.send_gc_unit_request::<A>(object.file_id, unit, resources, scope);
    }
}

/// Roots: export section, EXPORTED / NO_STRIP linking flags, InitFuncs, `--export`.
/// Entry arrives via `LoadGlobalSymbol` from the prelude path.
pub(crate) fn enqueue_wasm_gc_roots<'data, 'scope, A: platform::Arch<Platform = Wasm>>(
    object: &crate::layout::ObjectLayoutState<'data, Wasm>,
    resources: &'scope crate::layout::GraphResources<'data, 'scope, Wasm>,
    queue: &mut crate::layout::LocalWorkQueue<Wasm>,
    scope: &rayon::Scope<'scope>,
) -> Result {
    let file = object.object;
    let num_function_imports = file.num_function_imports;
    let num_global_imports = file.num_global_imports;

    if let Some(export_section) = file.export_section_reader()? {
        for export in export_section {
            let export = export?;
            let unit = match export.kind {
                wasmparser::ExternalKind::Func | wasmparser::ExternalKind::FuncExact => {
                    if export.index < num_function_imports {
                        Some(WasmGcUnit::FunctionImport(export.index))
                    } else {
                        Some(WasmGcUnit::DefinedFunction(
                            export.index - num_function_imports,
                        ))
                    }
                }
                wasmparser::ExternalKind::Global => {
                    if export.index < num_global_imports {
                        Some(WasmGcUnit::GlobalImport(export.index))
                    } else {
                        Some(WasmGcUnit::DefinedGlobal(export.index - num_global_imports))
                    }
                }
                _ => None,
            };
            if let Some(unit) = unit {
                enqueue_wasm_gc_unit::<A>(object, unit, resources, queue, scope);
            }
        }
    }

    for sym_index in 0..object.object.symbols.len() {
        let sym = &object.object.symbols[sym_index];
        let flags = SymbolFlags::from_bits_truncate(sym.flags);
        if !(flags.contains(SymbolFlags::EXPORTED) || flags.contains(SymbolFlags::NO_STRIP)) {
            continue;
        }
        if let Some(unit) = wasm_gc_unit_for_symbol(object.object, sym) {
            enqueue_wasm_gc_unit::<A>(object, unit, resources, queue, scope);
        }
    }

    for init_index in 0..object.object.init_funcs.len() {
        let init = object.object.init_funcs[init_index];
        let Some(sym) = object.object.symbols.get(init.symbol_index as usize) else {
            bail!("InitFuncs symbol index {} out of range", init.symbol_index);
        };
        if let Some(unit) = wasm_gc_unit_for_symbol(object.object, sym) {
            enqueue_wasm_gc_unit::<A>(object, unit, resources, queue, scope);
        }
        if sym.is_weak() && sym.kind == WasmSymbolKind::Func && !sym.is_undefined() {
            send_wasm_definition_request::<A>(
                object,
                init.symbol_index as usize,
                resources,
                queue,
                scope,
            );
        }
    }

    enqueue_wasm_force_export_roots::<A>(object, resources, queue, scope);

    Ok(())
}

pub(crate) fn enqueue_wasm_force_export_roots<'data, 'scope, A: platform::Arch<Platform = Wasm>>(
    object: &crate::layout::ObjectLayoutState<'data, Wasm>,
    resources: &'scope crate::layout::GraphResources<'data, 'scope, Wasm>,
    queue: &mut crate::layout::LocalWorkQueue<Wasm>,
    scope: &rayon::Scope<'scope>,
) {
    for name in resources.symbol_db.args.force_export_symbol_names() {
        let Some(symbol_id) = resources
            .symbol_db
            .get_unversioned(&UnversionedSymbolName::prehashed(name.as_bytes()))
        else {
            continue;
        };
        let def_id = resources.symbol_db.definition(symbol_id);
        if resources.symbol_db.file_id_for_symbol(def_id) != object.file_id {
            continue;
        }
        let old_flags = resources
            .per_symbol_flags
            .get_atomic(def_id)
            .fetch_or(ValueFlags::DIRECT);
        if !old_flags.has_resolution() {
            queue.send_symbol_request::<A>(def_id, resources, scope);
        }
    }
}

pub(crate) fn walk_wasm_gc_unit_edges<'data, 'scope, A: platform::Arch<Platform = Wasm>>(
    object: &crate::layout::ObjectLayoutState<'data, Wasm>,
    unit: WasmGcUnit,
    resources: &'scope crate::layout::GraphResources<'data, 'scope, Wasm>,
    queue: &mut crate::layout::LocalWorkQueue<Wasm>,
    scope: &rayon::Scope<'scope>,
) -> Result {
    match unit {
        WasmGcUnit::DefinedFunction(ordinal) => {
            let Some(&(start, end)) = object
                .format_specific
                .function_body_spans
                .get(ordinal as usize)
            else {
                bail!("Wasm GC function ordinal {ordinal} out of range");
            };
            let range = reloc_index_range(&object.format_specific.code_relocations, start, end);
            for i in range {
                let reloc = object.format_specific.code_relocations[i];
                note_wasm_reloc_edge::<A>(object, &reloc, resources, queue, scope)?;
            }
        }
        WasmGcUnit::DataSegment(ordinal) => {
            let Some(&(start, end)) = object
                .format_specific
                .data_segment_spans
                .get(ordinal as usize)
            else {
                bail!("Wasm GC data segment ordinal {ordinal} out of range");
            };
            let range = reloc_index_range(&object.format_specific.data_relocations, start, end);
            for i in range {
                let reloc = object.format_specific.data_relocations[i];
                note_wasm_reloc_edge::<A>(object, &reloc, resources, queue, scope)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn note_wasm_reloc_edge<'data, 'scope, A: platform::Arch<Platform = Wasm>>(
    object: &crate::layout::ObjectLayoutState<'data, Wasm>,
    reloc: &WasmRelocation,
    resources: &'scope crate::layout::GraphResources<'data, 'scope, Wasm>,
    queue: &mut crate::layout::LocalWorkQueue<Wasm>,
    scope: &rayon::Scope<'scope>,
) -> Result {
    if reloc.ty == RelocationType::TypeIndexLeb {
        return Ok(());
    }

    let file = object.object;
    let Some(sym) = file.symbols.get(reloc.index as usize).copied() else {
        bail!("Wasm relocation symbol index {} out of range", reloc.index);
    };

    if !sym.is_undefined() {
        if let Some(unit) = wasm_gc_unit_for_symbol(file, &sym) {
            enqueue_wasm_gc_unit::<A>(object, unit, resources, queue, scope);
        }

        if sym.is_weak() && sym.kind == WasmSymbolKind::Func {
            send_wasm_definition_request::<A>(
                object,
                reloc.index as usize,
                resources,
                queue,
                scope,
            );
        }
        return Ok(());
    }

    // Undefined: keep the local import slot live (host / linker-defined / cross-object).
    if let Some(unit) = wasm_gc_unit_for_symbol(file, &sym) {
        enqueue_wasm_gc_unit::<A>(object, unit, resources, queue, scope);
    }

    send_wasm_definition_request::<A>(object, reloc.index as usize, resources, queue, scope);
    Ok(())
}

pub(crate) fn note_wasm_import_unit_definition<
    'data,
    'scope,
    A: platform::Arch<Platform = Wasm>,
>(
    object: &crate::layout::ObjectLayoutState<'data, Wasm>,
    unit: WasmGcUnit,
    resources: &'scope crate::layout::GraphResources<'data, 'scope, Wasm>,
    queue: &mut crate::layout::LocalWorkQueue<Wasm>,
    scope: &rayon::Scope<'scope>,
) {
    let offsets = match unit {
        WasmGcUnit::FunctionImport(index) => object
            .format_specific
            .func_import_symbol_offsets
            .get(index as usize)
            .map_or(&[][..], Vec::as_slice),
        WasmGcUnit::GlobalImport(index) => object
            .format_specific
            .global_import_symbol_offsets
            .get(index as usize)
            .map_or(&[][..], Vec::as_slice),
        _ => return,
    };

    for &sym_offset in offsets {
        send_wasm_definition_request::<A>(object, sym_offset, resources, queue, scope);
    }
}

pub(crate) fn send_wasm_definition_request<'data, 'scope, A: platform::Arch<Platform = Wasm>>(
    object: &crate::layout::ObjectLayoutState<'data, Wasm>,
    local_symbol_offset: usize,
    resources: &'scope crate::layout::GraphResources<'data, 'scope, Wasm>,
    queue: &mut crate::layout::LocalWorkQueue<Wasm>,
    scope: &rayon::Scope<'scope>,
) {
    let local_symbol_id = object.symbol_id_range.offset_to_id(local_symbol_offset);
    let symbol_id = resources.symbol_db.definition(local_symbol_id);
    let previous_flags = resources
        .per_symbol_flags
        .get_atomic(symbol_id)
        .fetch_or(ValueFlags::DIRECT);
    if !previous_flags.has_resolution() {
        queue.send_symbol_request::<A>(symbol_id, resources, scope);
    }
}

/// For unnamed undefined Func/Global symbols, derive the name from the corresponding import
/// section entry. In Wasm relocatable objects, undefined symbols in the linking section may
/// omit their name; the canonical name is carried by the import entry instead.
pub(crate) fn backfill_unnamed_import_symbols(
    data: &[u8],
    standard_section_index: &[Option<u32>; STANDARD_SECTION_LOOKUP_LEN],
    sections: &[SectionHeader],
    symbols: &mut [WasmSymbol],
) -> Result {
    // Collect import names only if there are unnamed undefined symbols that need backfilling.
    let needs_backfill = symbols.iter().any(|s| {
        s.is_undefined()
            && !s.has_name()
            && matches!(s.kind, WasmSymbolKind::Func | WasmSymbolKind::Global)
    });
    if !needs_backfill {
        return Ok(());
    }

    let data_start = data.as_ptr() as usize;

    // Parse the import section to build name lookup tables indexed by function/global import
    // ordinal.
    let Some(import_payload) = standard_section_index
        .get(section_id::IMPORT as usize)
        .and_then(|idx| idx.as_ref())
        .and_then(|&idx| sections.get(idx as usize))
        .and_then(|header| data.get(header.payload_range_usize()))
    else {
        return Ok(());
    };
    let import_reader = ImportSectionReader::new(BinaryReader::new(import_payload, 0))?;

    let mut func_import_names: Vec<(u32, u32)> = Vec::new();
    let mut global_import_names: Vec<(u32, u32)> = Vec::new();
    for import in import_reader.into_imports() {
        let import = import?;
        let name_ptr = import.name.as_ptr() as usize - data_start;
        let name_entry = (name_ptr as u32, import.name.len() as u32);
        match import.ty {
            TypeRef::Func(_) | TypeRef::FuncExact(_) => func_import_names.push(name_entry),
            TypeRef::Global(_) => global_import_names.push(name_entry),
            _ => {}
        }
    }

    for sym in symbols.iter_mut() {
        if !sym.is_undefined() || sym.has_name() {
            continue;
        }
        let (start, len) = match sym.kind {
            WasmSymbolKind::Func => func_import_names
                .get(sym.index as usize)
                .copied()
                .unwrap_or((0, 0)),
            WasmSymbolKind::Global => global_import_names
                .get(sym.index as usize)
                .copied()
                .unwrap_or((0, 0)),
            _ => continue,
        };
        sym.name_start = start;
        sym.name_len = len;
    }

    Ok(())
}

pub(crate) fn parse_linking_subsections<'data>(
    data: &'data [u8],
    linking: &wasmparser::LinkingSectionReader<'data>,
    symbols: &mut Vec<WasmSymbol>,
    segment_alignments: &mut Vec<Alignment>,
    init_funcs: &mut Vec<WasmInitFunc>,
) -> Result {
    let data_start = data.as_ptr() as usize;
    let to_name_range = |s: &str| -> (u32, u32) {
        let start = s.as_ptr() as usize - data_start;
        (start as u32, s.len() as u32)
    };
    for sub in linking.subsections() {
        let sub = sub?;
        match sub {
            Linking::SymbolTable(map) => {
                for sym in map {
                    symbols.push(wasm_symbol_from_info(sym?, to_name_range));
                }
            }
            Linking::SegmentInfo(map) => {
                for seg in map {
                    let seg = seg?;
                    segment_alignments.push(Alignment::from_exponent(seg.alignment)?);
                }
            }
            Linking::InitFuncs(map) => {
                for init in map {
                    let init = init?;
                    init_funcs.push(WasmInitFunc {
                        priority: init.priority,
                        symbol_index: init.symbol_index,
                    });
                }
            }
            // `ComdatInfo` and `Unknown` subsections are not consumed.
            _ => {}
        }
    }

    Ok(())
}

pub(crate) fn wasm_symbol_from_info(
    info: SymbolInfo<'_>,
    to_name_range: impl Fn(&str) -> (u32, u32),
) -> WasmSymbol {
    let mut sym = WasmSymbol::default();
    let mut set_name = |name: Option<&str>| {
        if let Some(n) = name {
            let (start, len) = to_name_range(n);
            sym.name_start = start;
            sym.name_len = len;
        }
    };
    match info {
        SymbolInfo::Func { flags, index, name } => {
            sym.kind = WasmSymbolKind::Func;
            sym.flags = flags.bits();
            sym.index = index;
            set_name(name);
        }
        SymbolInfo::Data {
            flags,
            name,
            symbol,
        } => {
            sym.kind = WasmSymbolKind::Data;
            sym.flags = flags.bits();
            let (start, len) = to_name_range(name);
            sym.name_start = start;
            sym.name_len = len;
            if let Some(def) = symbol {
                sym.index = def.index;
                sym.offset = def.offset;
                sym.size = def.size;
            }
        }
        SymbolInfo::Global { flags, index, name } => {
            sym.kind = WasmSymbolKind::Global;
            sym.flags = flags.bits();
            sym.index = index;
            set_name(name);
        }
        SymbolInfo::Section { flags, section } => {
            sym.kind = WasmSymbolKind::Section;
            sym.flags = flags.bits();
            sym.index = section;
        }
        SymbolInfo::Event { flags, index, name } => {
            sym.kind = WasmSymbolKind::Event;
            sym.flags = flags.bits();
            sym.index = index;
            set_name(name);
        }
        SymbolInfo::Table { flags, index, name } => {
            sym.kind = WasmSymbolKind::Table;
            sym.flags = flags.bits();
            sym.index = index;
            set_name(name);
        }
    }

    sym
}
