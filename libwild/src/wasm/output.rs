use super::DEFAULT_TABLE_BASE;
use super::TARGET_FEATURE_PREFIX_DISALLOWED;
use super::TARGET_FEATURE_PREFIX_USED;
use super::TARGET_FEATURES_SECTION_NAME;
use super::WASM_DEAD_INDEX;
use super::Wasm;
use super::file::*;
use super::gc::*;
use super::linking::*;
use super::part_id;
use super::relocations::*;
use super::symbols::*;
use crate::bail;
use crate::ensure;
use crate::error::Context as _;
use crate::error::Result;
use crate::part_id::PartId;
use crate::symbol_db::SymbolDb;
use crate::timing_phase;
use crate::verbose_timing_phase;
use crate::wasm_writer::OutputExport;
use crate::wasm_writer::OutputGlobal;
use crate::wasm_writer::OutputImport;
use hashbrown::HashMap;
use hashbrown::HashSet;
use leb128::write::signed_len as sleb128_size;
use leb128::write::unsigned_len as uleb128_size;
use rayon::prelude::*;
use std::borrow::Cow;
use std::ops::Range;
use wasm_encoder::NameMap;
use wasm_encoder::NameSection;
use wasmparser::BinaryReader;
use wasmparser::ConstExpr;
use wasmparser::DataKind;
use wasmparser::MemoryType;
use wasmparser::RelocationType;

#[derive(Debug, Default)]
pub(crate) struct WasmLayout<'data> {
    pub(crate) output_types: Vec<wasmparser::FuncType>,
    pub(crate) imports: Vec<OutputImport<'data>>,
    pub(crate) function_type_indices: Vec<u32>,
    pub(crate) globals: Vec<OutputGlobal<'data>>,
    pub(crate) exports: Vec<OutputExport<'data>>,
    pub(crate) function_bodies: Vec<WasmFunctionBody<'data>>,
    pub(crate) memories: Vec<MemoryType>,
    pub(crate) tables: Vec<wasmparser::TableType>,
    pub(crate) element_functions: Vec<u32>,
    pub(crate) function_table_slots: Vec<u32>,
    pub(crate) memory_base: u32,
    pub(crate) data_end: u32,
    pub(crate) unsupported_output: Vec<&'static str>,
    pub(crate) object_index_maps: Vec<WasmObjectIndexMap>,
    pub(crate) object_data_layouts: Vec<Vec<WasmDataSegmentLayout<'data>>>,
    pub(crate) object_code_relocations: Vec<Vec<WasmRelocation>>,
    pub(crate) object_data_relocations: Vec<Vec<WasmRelocation>>,
    pub(crate) per_object_symbols: Vec<&'data [WasmSymbol]>,
    pub(crate) encoded_sections: WasmEncodedSections,
    pub(crate) code_section_size: u64,
    pub(crate) data_section_size: u64,
}

#[derive(Debug, Default)]
pub(crate) struct WasmEncodedSections {
    pub(crate) ty: Option<Vec<u8>>,
    pub(crate) import: Option<Vec<u8>>,
    pub(crate) function: Option<Vec<u8>>,
    pub(crate) global: Option<Vec<u8>>,
    pub(crate) export: Option<Vec<u8>>,
    pub(crate) memory: Option<Vec<u8>>,
    pub(crate) table: Option<Vec<u8>>,
    pub(crate) element: Option<Vec<u8>>,
    // Custom `name` section.
    pub(crate) name: Option<Vec<u8>>,
    // Custom `target_features` section.
    pub(crate) target_features: Option<Vec<u8>>,
}

impl WasmEncodedSections {
    pub(crate) fn add_sizes_to(
        &self,
        sizes: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
    ) {
        add_encoded_section_size(sizes, part_id::WASM_TYPE, self.ty.as_ref());
        add_encoded_section_size(sizes, part_id::WASM_IMPORT, self.import.as_ref());
        add_encoded_section_size(sizes, part_id::WASM_FUNCTION, self.function.as_ref());
        add_encoded_section_size(sizes, part_id::WASM_TABLE, self.table.as_ref());
        add_encoded_section_size(sizes, part_id::WASM_MEMORY, self.memory.as_ref());
        add_encoded_section_size(sizes, part_id::WASM_GLOBAL, self.global.as_ref());
        add_encoded_section_size(sizes, part_id::WASM_EXPORT, self.export.as_ref());
        add_encoded_section_size(sizes, part_id::WASM_ELEMENT, self.element.as_ref());
        add_encoded_section_size(sizes, part_id::WASM_NAME, self.name.as_ref());
        add_encoded_section_size(
            sizes,
            part_id::WASM_TARGET_FEATURES,
            self.target_features.as_ref(),
        );
    }
}

/// Per-object name entries.
#[derive(Default)]
pub(crate) struct ObjectNameEntries<'a> {
    pub(crate) functions: Vec<(u32, &'a str)>,
    pub(crate) globals: Vec<(u32, &'a str)>,
}

pub(crate) fn add_encoded_section_size(
    sizes: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
    part_id: PartId,
    section: Option<&Vec<u8>>,
) {
    if let Some(bytes) = section {
        sizes.increment(part_id, bytes.len() as u64);
    }
}

pub(crate) fn encode_wasm_section(section: &impl wasm_encoder::Section) -> Vec<u8> {
    let mut bytes = Vec::new();
    section.append_to(&mut bytes);
    bytes
}

pub(crate) fn demangle_symbol_name(name: &str, demangle: bool) -> Cow<'_, str> {
    if demangle {
        symbolic_demangle::demangle(name)
    } else {
        Cow::Borrowed(name)
    }
}

pub(crate) fn build_name_section<'data>(
    layout: &WasmLayout<'data>,
    layout_inputs: &[WasmObjectLayoutInput<'data>],
    indices: &LinkerDefinedIndices,
    got_mem: &GotMem,
    got_func: &GotFunc,
    demangle: bool,
) -> Option<wasm_encoder::NameSection> {
    let (n_func_imports, n_global_imports) = count_output_imports(layout);
    let n_funcs = n_func_imports + layout.function_type_indices.len();
    let n_globals = n_global_imports + layout.globals.len();
    let mut function_names: Vec<Option<&str>> = vec![None; n_funcs];
    let mut global_names: Vec<Option<&str>> = vec![None; n_globals];
    let mut got_mem_names: Vec<String> = Vec::new();
    let mut got_func_names: Vec<String> = Vec::new();

    // Host / remaining imports.
    let mut next_func_import = 0u32;
    let mut next_global_import = 0u32;
    for import in &layout.imports {
        match import.entity {
            crate::wasm_writer::OutputImportEntity::Function { .. } => {
                set_name_first_wins(&mut function_names, next_func_import, import.name);
                next_func_import += 1;
            }
            crate::wasm_writer::OutputImportEntity::Global(_) => {
                set_name_first_wins(&mut global_names, next_global_import, import.name);
                next_global_import += 1;
            }
        }
    }

    // Linker-synthesised functions / globals.
    if let Some(idx) = indices.memory_base_global {
        set_name_first_wins(&mut global_names, idx, "__memory_base");
    }
    if let Some(idx) = indices.table_base_global {
        set_name_first_wins(&mut global_names, idx, "__table_base");
    }
    if let Some(idx) = indices.stack_pointer_global {
        set_name_first_wins(&mut global_names, idx, "__stack_pointer");
    }
    if let Some(idx) = indices.tls_base_global {
        set_name_first_wins(&mut global_names, idx, "__tls_base");
    }
    for &(known, idx) in &indices.data_address_globals {
        set_name_first_wins(&mut global_names, idx, <&str>::from(known));
    }
    if let Some(got_base) = indices.got_mem_global_base {
        got_mem_names.reserve(got_mem.entries.len());
        for (i, entry) in got_mem.entries.iter().enumerate() {
            let name = match entry.def {
                GotMemDef::Object {
                    object_index,
                    symbol_offset,
                } => layout_inputs
                    .get(object_index)
                    .and_then(|input| {
                        input
                            .symbols
                            .get(symbol_offset)
                            .and_then(|sym| wasm_symbol_name_str(input.data, sym))
                    })
                    .map_or_else(
                        || format!("GOT.data.internal.{i}"),
                        |sym| format!("GOT.data.internal.{}", demangle_symbol_name(sym, demangle)),
                    ),
                GotMemDef::LinkerDefined(known) => {
                    let sym = std::str::from_utf8(known.name()).unwrap_or("?");
                    format!("GOT.data.internal.{sym}")
                }
            };
            got_mem_names.push(name);
        }
        for (i, name) in got_mem_names.iter().enumerate() {
            set_name_first_wins(&mut global_names, got_base + i as u32, name.as_str());
        }
    }
    if let Some(got_base) = indices.got_func_global_base {
        got_func_names.reserve(got_func.entries.len());
        for (i, entry) in got_func.entries.iter().enumerate() {
            got_func_names.push(got_func_debug_name(layout_inputs, entry, i, demangle));
        }
        for (i, name) in got_func_names.iter().enumerate() {
            set_name_first_wins(&mut global_names, got_base + i as u32, name.as_str());
        }
    }
    if let Some(idx) = indices.call_ctors_func {
        set_name_first_wins(&mut function_names, idx, "__wasm_call_ctors");
    }

    let per_object_names: Vec<ObjectNameEntries<'_>> = layout_inputs
        .par_iter()
        .zip(layout.object_index_maps.par_iter())
        .map(|(input, index_map)| {
            verbose_timing_phase!("Collect Wasm object name entries");
            let mut entries = ObjectNameEntries::default();
            for sym in input.symbols {
                let Some(name) = wasm_symbol_name_str(input.data, sym) else {
                    continue;
                };
                match sym.kind {
                    WasmSymbolKind::Func
                        if let Some(&out_idx) =
                            index_map.function_indices.get(sym.index as usize)
                            && out_idx != WASM_DEAD_INDEX =>
                    {
                        entries.functions.push((out_idx, name));
                    }
                    WasmSymbolKind::Global
                        if let Some(&out_idx) =
                            index_map.global_indices.get(sym.index as usize)
                            && out_idx != WASM_DEAD_INDEX =>
                    {
                        entries.globals.push((out_idx, name));
                    }
                    _ => {}
                }
            }
            entries
        })
        .collect();
    for entries in per_object_names {
        for (out_idx, name) in entries.functions {
            set_name_first_wins(&mut function_names, out_idx, name);
        }
        for (out_idx, name) in entries.globals {
            set_name_first_wins(&mut global_names, out_idx, name);
        }
    }

    for export in &layout.exports {
        match export.kind {
            wasmparser::ExternalKind::Func => {
                set_name_first_wins(&mut function_names, export.index, export.name);
            }
            wasmparser::ExternalKind::Global => {
                set_name_first_wins(&mut global_names, export.index, export.name);
            }
            _ => {}
        }
    }

    let function_map = name_map_from_dense(&function_names, demangle);
    let global_map = name_map_from_dense(&global_names, demangle);
    if function_map.is_none() && global_map.is_none() {
        return None;
    }

    let mut section = NameSection::new();
    if let Some(map) = function_map {
        section.functions(&map);
    }
    if let Some(map) = global_map {
        section.globals(&map);
    }
    Some(section)
}

pub(crate) fn count_output_imports(layout: &WasmLayout<'_>) -> (usize, usize) {
    let mut functions = 0usize;
    let mut globals = 0usize;
    for import in &layout.imports {
        match import.entity {
            crate::wasm_writer::OutputImportEntity::Function { .. } => functions += 1,
            crate::wasm_writer::OutputImportEntity::Global(_) => globals += 1,
        }
    }
    (functions, globals)
}

pub(crate) fn set_name_first_wins<'a>(names: &mut Vec<Option<&'a str>>, index: u32, name: &'a str) {
    let i = index as usize;
    if i >= names.len() {
        names.resize(i + 1, None);
    }
    if names[i].is_none() {
        names[i] = Some(name);
    }
}

pub(crate) fn name_map_from_dense(names: &[Option<&str>], demangle: bool) -> Option<NameMap> {
    if names.iter().all(Option::is_none) {
        return None;
    }
    let mut map = NameMap::new();
    for (idx, name) in names.iter().enumerate() {
        if let Some(name) = name {
            let name = demangle_symbol_name(name, demangle);
            map.append(idx as u32, &name);
        }
    }
    Some(map)
}

pub(crate) fn wasm_symbol_name_str<'data>(
    data: &'data [u8],
    sym: &WasmSymbol,
) -> Option<&'data str> {
    if !sym.has_name() {
        return None;
    }
    let bytes = data.get(sym.name_range())?;
    core::str::from_utf8(bytes).ok()
}

/// Collect used / disallowed `target_features` entries from all objects.
pub(crate) fn collect_target_feature_sets<'data>(
    layout_inputs: &[WasmObjectLayoutInput<'data>],
) -> Result<(
    HashSet<&'data str>,
    HashMap<&'data str, crate::input_data::FileId>,
)> {
    let mut used: HashSet<&'data str> = HashSet::new();
    // First file that disallowed each feature.
    let mut disallowed: HashMap<&'data str, crate::input_data::FileId> = HashMap::new();

    for input in layout_inputs {
        for feature in input.target_features {
            match feature.prefix {
                TARGET_FEATURE_PREFIX_USED => {
                    used.insert(feature.name);
                }
                TARGET_FEATURE_PREFIX_DISALLOWED => {
                    disallowed.entry(feature.name).or_insert(input.file_id);
                }
                other => {
                    bail!(
                        "unrecognized target_features prefix 0x{other:02x} for feature `{}`",
                        feature.name
                    );
                }
            }
        }
    }

    Ok((used, disallowed))
}

/// Shared memory requires `atomics` and `bulk-memory`.
pub(crate) fn validate_shared_memory_features(
    layout_inputs: &[WasmObjectLayoutInput<'_>],
    symbol_db: &SymbolDb<'_, Wasm>,
) -> Result {
    let (mut used, disallowed) = collect_target_feature_sets(layout_inputs)?;
    if let Some(&file_id) = disallowed.get("shared-mem") {
        bail!(
            "--shared-memory is disallowed by {} because it was not compiled with 'atomics' or 'bulk-memory' features.",
            symbol_db.file(file_id)
        );
    }

    used.extend(symbol_db.args.extra_features.iter().map(|s| s.as_str()));

    for feature in ["atomics", "bulk-memory"] {
        if !used.contains(feature) {
            bail!("'{feature}' feature must be used in order to use shared memory");
        }
    }
    Ok(())
}

/// Merge `target_features` from linked objects and encode the output custom section.
pub(crate) fn build_target_features_section<'data>(
    layout_inputs: &[WasmObjectLayoutInput<'data>],
    extra_features: &'data [String],
) -> Result<Option<wasm_encoder::CustomSection<'static>>> {
    let (mut used, disallowed) = collect_target_feature_sets(layout_inputs)?;

    for name in &used {
        if let Some(&file_id) = disallowed.get(name) {
            bail!(
                "target feature `{name}` is used by linked objects but disallowed by input file \
                 {file_id}"
            );
        }
    }

    used.extend(extra_features.iter().map(|s| s.as_str()));

    if used.is_empty() {
        return Ok(None);
    }

    let mut names: Vec<&'data str> = used.into_iter().collect();
    names.sort_unstable();

    let mut payload = Vec::new();
    leb128::write::unsigned(&mut payload, names.len() as u64).unwrap();
    for name in names {
        payload.push(TARGET_FEATURE_PREFIX_USED);
        let name_bytes = name.as_bytes();
        leb128::write::unsigned(&mut payload, name_bytes.len() as u64).unwrap();
        payload.extend_from_slice(name_bytes);
    }

    Ok(Some(wasm_encoder::CustomSection {
        name: Cow::Borrowed(TARGET_FEATURES_SECTION_NAME),
        data: Cow::Owned(payload),
    }))
}

pub(crate) fn parse_target_features_payload<'data>(
    data: &'data [u8],
) -> Result<Vec<WasmTargetFeature<'data>>> {
    let mut reader = BinaryReader::new(data, 0);
    let count = reader
        .read_var_u32()
        .context("invalid target_features feature count")?;
    let mut features = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let prefix = reader
            .read_u8()
            .context("truncated target_features feature prefix")?;
        let name = reader
            .read_string()
            .context("invalid target_features feature name")?;
        features.push(WasmTargetFeature { prefix, name });
    }
    ensure!(
        reader.eof(),
        "trailing bytes in target_features section after {} features",
        features.len()
    );
    Ok(features)
}

impl<'data> WasmLayout<'data> {
    pub(crate) fn encode_metadata_sections(
        &mut self,
        layout_inputs: &[WasmObjectLayoutInput<'data>],
        indices: &LinkerDefinedIndices,
        got_mem: &GotMem,
        got_func: &GotFunc,
        symbol_db: &SymbolDb<'data, Wasm>,
    ) -> Result {
        timing_phase!("Encode Wasm metadata sections");
        let demangle = symbol_db.args.common.demangle;

        {
            timing_phase!("Encode Wasm type section");
            let type_section = crate::wasm_writer::build_type_section(&self.output_types)?;
            if !type_section.is_empty() {
                self.encoded_sections.ty = Some(encode_wasm_section(&type_section));
            }
        }

        {
            timing_phase!("Encode Wasm import section");
            let import_section = crate::wasm_writer::build_import_section(&self.imports)?;
            if !import_section.is_empty() {
                self.encoded_sections.import = Some(encode_wasm_section(&import_section));
            }
        }

        {
            timing_phase!("Encode Wasm function section");
            let function_section =
                crate::wasm_writer::build_function_section(&self.function_type_indices);
            if !function_section.is_empty() {
                self.encoded_sections.function = Some(encode_wasm_section(&function_section));
            }
        }

        {
            timing_phase!("Encode Wasm global section");
            let global_section = crate::wasm_writer::build_global_section(&self.globals)?;
            if !global_section.is_empty() {
                self.encoded_sections.global = Some(encode_wasm_section(&global_section));
            }
        }

        {
            timing_phase!("Encode Wasm export section");
            let export_section = crate::wasm_writer::build_export_section(&self.exports);
            if !export_section.is_empty() {
                self.encoded_sections.export = Some(encode_wasm_section(&export_section));
            }
        }

        {
            timing_phase!("Encode Wasm memory section");
            let memory_section = crate::wasm_writer::build_memory_section(&self.memories);
            if !memory_section.is_empty() {
                self.encoded_sections.memory = Some(encode_wasm_section(&memory_section));
            }
        }

        if !self.tables.is_empty() {
            timing_phase!("Encode Wasm table section");
            let table_section = crate::wasm_writer::build_table_section(&self.tables)?;
            self.encoded_sections.table = Some(encode_wasm_section(&table_section));
        }

        if !self.element_functions.is_empty() {
            timing_phase!("Encode Wasm element section");
            let element_section =
                crate::wasm_writer::build_element_section(&self.element_functions);
            self.encoded_sections.element = Some(encode_wasm_section(&element_section));
        }

        {
            timing_phase!("Encode Wasm name section");
            if let Some(name_section) =
                build_name_section(self, layout_inputs, indices, got_mem, got_func, demangle)
            {
                self.encoded_sections.name = Some(encode_wasm_section(&name_section));
            }
        }

        {
            timing_phase!("Encode Wasm target_features section");
            if let Some(target_features) =
                build_target_features_section(layout_inputs, &symbol_db.args.extra_features)?
            {
                self.encoded_sections.target_features = Some(encode_wasm_section(&target_features));
            }
        }

        {
            timing_phase!("Compute Wasm code/data section sizes");
            self.code_section_size = compute_code_section_size(&self.function_bodies);
            self.data_section_size = compute_data_section_size(&self.object_data_layouts);
        }

        Ok(())
    }

    pub(crate) fn add_code_section_size(
        &self,
        sizes: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
    ) {
        if self.code_section_size > 0 {
            sizes.increment(part_id::WASM_CODE, self.code_section_size);
        }
    }

    pub(crate) fn add_data_section_size(
        &self,
        sizes: &mut crate::output_section_part_map::OutputSectionPartMap<u64>,
    ) {
        if self.data_section_size > 0 {
            sizes.increment(part_id::WASM_DATA, self.data_section_size);
        }
    }
}

pub(crate) fn const_expr_encoded_size(expr: &ConstExpr<'_>) -> Result<u32> {
    let body = crate::wasm_writer::const_expr_body(expr)
        .ok_or_else(|| crate::error!("Wasm const expression is missing end opcode"))?;
    // instruction bytes plus the trailing `end` (0x0B) opcode
    u32::try_from(body.len() + 1).context("Wasm const expression too large")
}

/// Encoded size of one segment in the data section payload. See `data` in
/// <https://webassembly.github.io/spec/core/binary/modules.html#data-section>.
pub(crate) fn wasm_data_segment_encoded_size(kind: &DataKind<'_>, data_len: usize) -> Result<u32> {
    let data_len = u32::try_from(data_len).context("Wasm data segment too large")?;
    let payload_len = uleb128_size(u64::from(data_len)) as u32 + data_len;
    match kind {
        DataKind::Passive => Ok(1 + payload_len),
        DataKind::Active {
            memory_index,
            offset_expr,
        } => {
            let init_len = const_expr_encoded_size(offset_expr)?;
            let header = if *memory_index == 0 {
                1
            } else {
                1 + uleb128_size(u64::from(*memory_index)) as u32
            };
            Ok(header
                .checked_add(init_len)
                .and_then(|n| n.checked_add(payload_len))
                .ok_or_else(|| crate::error!("Wasm data segment size overflow"))?)
        }
    }
}

/// Byte length of the offset `expr` we emit (`i32.const` + SLEB + `end`).
pub(crate) fn output_i32_const_init_expr_size(offset: u32) -> u32 {
    1 + sleb128_size(i64::from(offset)) as u32 + 1
}

pub(crate) fn output_data_segment_encoded_size(
    kind: &DataKind<'_>,
    data_len: usize,
    output_memory_offset: u32,
    output_memory_index: u32,
) -> Result<u32> {
    let data_len = u32::try_from(data_len).context("Wasm data segment too large")?;
    let payload_len = uleb128_size(u64::from(data_len)) as u32 + data_len;
    match kind {
        DataKind::Passive => bail!("passive data segments are not emitted"),
        DataKind::Active { .. } => {
            let init_len = output_i32_const_init_expr_size(output_memory_offset);
            let header = if output_memory_index == 0 {
                1
            } else {
                1 + uleb128_size(u64::from(output_memory_index)) as u32
            };
            Ok(header
                .checked_add(init_len)
                .and_then(|n| n.checked_add(payload_len))
                .ok_or_else(|| crate::error!("Wasm data segment size overflow"))?)
        }
    }
}

/// Map data-section relocations onto owning segments as ranges into `relocs`.
pub(crate) fn classify_data_reloc_ranges(
    segments: &[WasmDataSegment<'_>],
    relocs: &[WasmRelocation],
) -> Vec<(Range<u32>, u32)> {
    let payload_start_of = |segment: &WasmDataSegment<'_>| -> u32 {
        let data_len = u32::try_from(segment.data.len()).unwrap_or(u32::MAX);
        segment
            .section_offset
            .saturating_add(segment.encoded_size.saturating_sub(data_len))
    };

    if relocs.is_empty() {
        return segments
            .iter()
            .map(|segment| (0..0, payload_start_of(segment)))
            .collect();
    }

    let mut out = Vec::with_capacity(segments.len());
    let mut i = 0usize;
    for segment in segments {
        let payload_start = payload_start_of(segment);
        let end = segment.section_offset.saturating_add(segment.encoded_size);
        while i < relocs.len() && relocs[i].offset < payload_start {
            i += 1;
        }
        let lo = i;
        while i < relocs.len() && relocs[i].offset < end {
            i += 1;
        }
        out.push((
            u32::try_from(lo).unwrap_or(u32::MAX)..u32::try_from(i).unwrap_or(u32::MAX),
            payload_start,
        ));
    }
    out
}

/// Align `data_end` to [`STACK_ALIGNMENT`], then add the stack size.
pub(crate) fn stack_high_after_data(data_end: u32, stack_size: u32) -> Result<u32> {
    let stack_base = u32::try_from(crate::alignment::STACK_ALIGNMENT.align_up(u64::from(data_end)))
        .map_err(|_| crate::error!("Wasm stack base overflow"))?;
    stack_base
        .checked_add(stack_size)
        .ok_or_else(|| crate::error!("Wasm stack pointer overflow"))
}

/// Align the end of static data for `__heap_base`.
pub(crate) fn heap_base_after_data(data_end: u32) -> Result<u32> {
    u32::try_from(crate::alignment::STACK_ALIGNMENT.align_up(u64::from(data_end)))
        .map_err(|_| crate::error!("Wasm heap base overflow"))
}

/// Initial `__stack_pointer` value for the chosen stack layout.
pub(crate) fn stack_pointer_init(data_end: u32, stack_size: u32, stack_first: bool) -> Result<u32> {
    ensure_stack_size_aligned(stack_size)?;
    if stack_first {
        Ok(stack_size)
    } else {
        stack_high_after_data(data_end, stack_size)
    }
}

pub(crate) fn heap_base_address(data_end: u32, stack_size: u32, stack_first: bool) -> Result<u32> {
    if stack_first {
        heap_base_after_data(data_end)
    } else {
        stack_high_after_data(data_end, stack_size)
    }
}

pub(crate) fn ensure_stack_size_aligned(stack_size: u32) -> Result {
    let align = crate::alignment::STACK_ALIGNMENT.value();
    ensure!(
        u64::from(stack_size).is_multiple_of(align),
        "stack size must be {align}-byte aligned"
    );
    Ok(())
}

pub(crate) fn layout_object_data<'data>(
    input: &WasmObjectLayoutInput<'data>,
    index_map: &WasmObjectIndexMap,
    memory_cursor: &mut u32,
) -> Result<Vec<WasmDataSegmentLayout<'data>>> {
    let segment_reloc_ranges =
        classify_data_reloc_ranges(&input.data_segments, &input.data_relocations);
    let mut segments = Vec::with_capacity(input.data_segments.len());
    for (filtered_idx, segment) in input.data_segments.iter().enumerate() {
        let DataKind::Active { memory_index, .. } = segment.kind else {
            bail!("passive data segments are not emitted");
        };
        let output_memory_index =
            remap_wasm_index(&index_map.memory_indices, memory_index, "memory")?;
        let original_index = input
            .data_segment_original_indices
            .get(filtered_idx)
            .copied()
            .unwrap_or(filtered_idx as u32);
        // Linking `SegmentInfo.alignment` is a power-of-two exponent.
        let align = input
            .segment_alignments
            .get(original_index as usize)
            .copied()
            .unwrap_or(crate::alignment::MIN);
        *memory_cursor = u32::try_from(align.align_up(u64::from(*memory_cursor)))
            .map_err(|_| crate::error!("Wasm data segment alignment overflow"))?;
        let output_memory_offset = *memory_cursor;
        let encoded_output_size = output_data_segment_encoded_size(
            &segment.kind,
            segment.data.len(),
            output_memory_offset,
            output_memory_index,
        )?;
        *memory_cursor = memory_cursor
            .checked_add(u32::try_from(segment.data.len()).context("Wasm data segment too large")?)
            .ok_or_else(|| crate::error!("Wasm output memory offset overflow"))?;
        let (reloc_range, payload_start) = segment_reloc_ranges
            .get(filtered_idx)
            .cloned()
            .unwrap_or((0..0, 0));
        segments.push(WasmDataSegmentLayout {
            segment_index: original_index,
            data: segment.data,
            reloc_range,
            payload_start,
            output_memory_index,
            output_memory_offset,
            encoded_output_size,
        });
    }
    Ok(segments)
}

pub(crate) fn compute_data_section_size(
    object_data_layouts: &[Vec<WasmDataSegmentLayout<'_>>],
) -> u64 {
    let segment_count: u32 = object_data_layouts
        .iter()
        .map(|obj| u32::try_from(obj.len()).unwrap_or(u32::MAX))
        .sum();
    if segment_count == 0 {
        return 0;
    }
    let count_leb_size = uleb128_size(u64::from(segment_count)) as u64;
    let segments_total: u64 = object_data_layouts
        .iter()
        .flatten()
        .map(|segment| u64::from(segment.encoded_output_size))
        .sum();
    let payload_size = count_leb_size + segments_total;
    let payload_size_leb_size = uleb128_size(payload_size) as u64;

    // `section` envelope. See <https://webassembly.github.io/spec/core/binary/modules.html#binary-section>
    1 + payload_size_leb_size + payload_size
}

pub(crate) fn compute_code_section_size(bodies: &[WasmFunctionBody<'_>]) -> u64 {
    if bodies.is_empty() {
        return 0;
    }
    let count = bodies.len() as u32;
    let count_leb_size = uleb128_size(u64::from(count)) as u64;
    let bodies_with_prefix_total: u64 = bodies
        .iter()
        .map(|b| {
            let body_len = b.bytes.len() as u64;
            uleb128_size(body_len) as u64 + body_len
        })
        .sum();
    let payload_size = count_leb_size + bodies_with_prefix_total;
    let payload_size_leb_size = uleb128_size(payload_size) as u64;

    // section id (1 byte) + payload size LEB + payload
    1 + payload_size_leb_size + payload_size
}

#[derive(Debug, Default)]
pub(crate) struct WasmObjectIndexMap {
    /// Maps this object's local type index to the final output type index.
    pub(crate) type_indices: Vec<u32>,
    pub(crate) function_indices: Vec<u32>,
    pub(crate) global_indices: Vec<u32>,
    pub(crate) memory_indices: Vec<u32>,
    pub(crate) table_indices: Vec<u32>,
    pub(crate) data_addresses: Vec<u32>,
    pub(crate) got_mem_globals: Vec<Option<u32>>,
    pub(crate) got_func_globals: Vec<Option<u32>>,
    pub(crate) function_symbol_redirects: Vec<Option<u32>>,
}

impl WasmObjectIndexMap {
    /// Resolve a code/data relocation to its output value using the symbol table from the same
    /// object.
    pub(crate) fn resolve_reloc(
        &self,
        reloc: &WasmRelocation,
        symbols: &[WasmSymbol],
        function_table_slots: &[u32],
        memory_base: u32,
    ) -> Result<u32> {
        if reloc.ty == RelocationType::TypeIndexLeb {
            return remap_wasm_index(&self.type_indices, reloc.index, "type");
        }

        let sym = symbols
            .get(reloc.index as usize)
            .ok_or_else(|| crate::error!("relocation symbol index {} out of range", reloc.index))?;

        match reloc.ty {
            RelocationType::FunctionIndexLeb | RelocationType::FunctionIndexI32 => {
                ensure!(
                    sym.kind == WasmSymbolKind::Func,
                    "R_WASM_FUNCTION_INDEX_* references non-function symbol"
                );
                self.output_function_index(reloc.index as usize, sym)
            }
            RelocationType::GlobalIndexLeb | RelocationType::GlobalIndexI32 => match sym.kind {
                WasmSymbolKind::Global => {
                    remap_wasm_index(&self.global_indices, sym.index, "global")
                }
                WasmSymbolKind::Data => self
                    .got_mem_globals
                    .get(reloc.index as usize)
                    .copied()
                    .flatten()
                    .ok_or_else(|| {
                        crate::error!(
                            "missing GOT.mem global for data symbol index {}",
                            reloc.index
                        )
                    }),
                WasmSymbolKind::Func => self
                    .got_func_globals
                    .get(reloc.index as usize)
                    .copied()
                    .flatten()
                    .ok_or_else(|| {
                        crate::error!(
                            "missing GOT.func global for function symbol index {}",
                            reloc.index
                        )
                    }),
                other => {
                    bail!("R_WASM_GLOBAL_INDEX_* references unsupported symbol kind {other:?}")
                }
            },
            RelocationType::TableNumberLeb => {
                ensure!(
                    sym.kind == WasmSymbolKind::Table,
                    "R_WASM_TABLE_NUMBER_LEB references non-table symbol"
                );
                remap_wasm_index(&self.table_indices, sym.index, "table")
            }
            RelocationType::MemoryAddrLeb
            | RelocationType::MemoryAddrSleb
            | RelocationType::MemoryAddrI32
            | RelocationType::MemoryAddrRelSleb => {
                ensure!(
                    sym.kind == WasmSymbolKind::Data,
                    "R_WASM_MEMORY_ADDR_* references non-data symbol"
                );
                let addr = self
                    .data_addresses
                    .get(reloc.index as usize)
                    .copied()
                    .ok_or_else(|| {
                        crate::error!("data address for symbol index {} out of range", reloc.index)
                    })?;
                if reloc.ty == RelocationType::MemoryAddrRelSleb {
                    let relative = i64::from(addr) - i64::from(memory_base) + reloc.addend;
                    let relative = i32::try_from(relative)
                        .map_err(|_| crate::error!("Wasm REL_SLEB relocation out of range"))?;
                    Ok(relative as u32)
                } else {
                    Ok(addr)
                }
            }
            RelocationType::TableIndexSleb
            | RelocationType::TableIndexI32
            | RelocationType::TableIndexRelSleb => {
                ensure!(
                    sym.kind == WasmSymbolKind::Func,
                    "R_WASM_TABLE_INDEX_* references non-function symbol"
                );
                let func_out = self.output_function_index(reloc.index as usize, sym)?;
                let slot = function_table_slots
                    .get(func_out as usize)
                    .copied()
                    .unwrap_or(u32::MAX);
                ensure!(
                    slot != u32::MAX,
                    "function {func_out} has no indirect table slot"
                );
                if reloc.ty == RelocationType::TableIndexRelSleb {
                    if slot == 0 {
                        return Ok(0);
                    }
                    let relative = slot.checked_sub(DEFAULT_TABLE_BASE).ok_or_else(|| {
                        crate::error!("Wasm TABLE_INDEX_REL_SLEB relocation out of range")
                    })?;
                    Ok(relative)
                } else {
                    Ok(slot)
                }
            }
            RelocationType::EventIndexLeb => {
                bail!("event index relocations are not supported yet");
            }
            RelocationType::FunctionOffsetI32 => {
                bail!("function offset relocations are not supported yet");
            }
            RelocationType::SectionOffsetI32 => {
                bail!("section offset relocations are not supported yet");
            }
            other => bail!(
                "unsupported Wasm relocation type {}",
                relocation_type_to_string(other)
            ),
        }
    }

    /// Output function index for a linking-section symbol.
    pub(crate) fn output_function_index(
        &self,
        symbol_offset: usize,
        sym: &WasmSymbol,
    ) -> Result<u32> {
        if let Some(out) = self
            .function_symbol_redirects
            .get(symbol_offset)
            .copied()
            .flatten()
        {
            return Ok(out);
        }
        remap_wasm_index(&self.function_indices, sym.index, "function")
    }
}
