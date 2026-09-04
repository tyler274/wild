use super::super::LINKING_SECTION_NAME;
use super::super::RELOC_SECTION_PREFIX;
use super::super::STANDARD_SECTION_LOOKUP_LEN;
use super::super::TARGET_FEATURES_SECTION_NAME;
use super::super::WASM_MAGIC;
use super::super::WASM_VERSION;
use super::super::output::*;
use super::super::relocations::*;
use super::super::section_id;
use super::super::symbols::*;
use super::*;
use crate::alignment::Alignment;
use crate::ensure;
use crate::error::Result;
use linker_utils::utils::u32_from_slice;
use std::ops::Range;
use wasmparser::BinaryReader;
use wasmparser::ImportSectionReader;
use wasmparser::KnownCustom;
use wasmparser::Linking;
use wasmparser::Parser;
use wasmparser::Payload;
use wasmparser::SymbolInfo;
use wasmparser::TypeRef;

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
