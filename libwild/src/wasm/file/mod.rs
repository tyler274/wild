mod parse;
mod scan;
mod types;

use super::Wasm;
use super::abi::*;
use super::gc::*;
use super::output::*;
use super::section_id;
use super::symbols::*;
use crate::ensure;
use crate::error::Context as _;
use crate::error::Result;
use crate::platform;
use leb128::write::unsigned_len as uleb128_size;
#[allow(unused_imports)]
pub(crate) use parse::*;
#[allow(unused_imports)]
pub(crate) use scan::*;
use std::borrow::Cow;
#[allow(unused_imports)]
pub(crate) use types::*;
use wasmparser::BinaryReader;
use wasmparser::CodeSectionReader;
use wasmparser::DataSectionReader;
use wasmparser::ExportSectionReader;
use wasmparser::FunctionSectionReader;
use wasmparser::GlobalSectionReader;
use wasmparser::ImportSectionReader;
use wasmparser::MemorySectionReader;
use wasmparser::MemoryType;
use wasmparser::TypeSectionReader;

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
