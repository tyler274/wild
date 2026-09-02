use super::WASM_DEAD_INDEX;
use super::file::*;
use super::linking::*;
use super::output::*;
use super::relocations::*;
use super::section_id;
use super::symbols::*;
use crate::alignment::Alignment;
use crate::bail;
use crate::ensure;
use crate::error::Context as _;
use crate::error::Result;
use crate::wasm_writer::OutputExport;
use crate::wasm_writer::OutputGlobal;
use crate::wasm_writer::OutputImport;
use crate::wasm_writer::OutputImportEntity;
use std::borrow::Cow;
use std::ops::Range;
use wasmparser::DataKind;
use wasmparser::GlobalType;
use wasmparser::MemoryType;
use wasmparser::TypeRef;

#[derive(Debug, Clone, Copy)]
pub(crate) enum WasmGcUnit {
    DefinedFunction(u32),
    DefinedGlobal(u32),
    DataSegment(u32),
    FunctionImport(u32),
    GlobalImport(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub(crate) enum WasmGcUnitState {
    #[default]
    Dead = 0,
    Live = 1,
}

impl WasmGcUnitState {
    pub(crate) fn is_live(self) -> bool {
        self == Self::Live
    }
}

#[derive(Debug, Default)]
pub(crate) struct WasmObjectLayout<'data> {
    pub(crate) symbol_id_range: crate::symbol_db::SymbolIdRange,
    pub(crate) file_id: crate::input_data::FileId,
    // Set once per-unit GC states have been allocated at object activate.
    pub(crate) gc_states_ready: bool,
    pub(crate) gc_defined_functions: Vec<WasmGcUnitState>,
    pub(crate) gc_defined_globals: Vec<WasmGcUnitState>,
    pub(crate) gc_data_segments: Vec<WasmGcUnitState>,
    pub(crate) gc_function_imports: Vec<WasmGcUnitState>,
    pub(crate) gc_global_imports: Vec<WasmGcUnitState>,
    pub(crate) func_import_symbol_offsets: Vec<Vec<usize>>,
    pub(crate) global_import_symbol_offsets: Vec<Vec<usize>>,
    pub(crate) relocs_ready: bool,
    pub(crate) code_relocations: Vec<WasmRelocation>,
    pub(crate) data_relocations: Vec<WasmRelocation>,
    pub(crate) function_bodies: Vec<WasmFunctionBody<'data>>,
    pub(crate) data_segments: Vec<WasmDataSegment<'data>>,
    pub(crate) function_body_spans: Vec<(u32, u32)>,
    pub(crate) data_segment_spans: Vec<(u32, u32)>,
    pub(crate) defined_function_live_ordinal: Vec<u32>,
    pub(crate) defined_global_live_ordinal: Vec<u32>,
}

pub(crate) struct DecodedCodeData<'data> {
    pub(crate) ready: bool,
    pub(crate) code_relocations: Vec<WasmRelocation>,
    pub(crate) data_relocations: Vec<WasmRelocation>,
    pub(crate) function_bodies: Vec<WasmFunctionBody<'data>>,
    pub(crate) data_segments: Vec<WasmDataSegment<'data>>,
}

impl<'data> WasmObjectLayout<'data> {
    /// Allocate per-unit GC states from the object's unit counts.
    pub(crate) fn ensure_gc_states(&mut self, file: &File<'_>) {
        if self.gc_states_ready {
            return;
        }
        self.gc_defined_functions =
            vec![WasmGcUnitState::Dead; file.num_defined_functions as usize];
        self.gc_defined_globals = vec![WasmGcUnitState::Dead; file.num_defined_globals as usize];
        self.gc_data_segments = vec![WasmGcUnitState::Dead; file.num_data_segments as usize];
        self.gc_function_imports = vec![WasmGcUnitState::Dead; file.num_function_imports as usize];
        self.gc_global_imports = vec![WasmGcUnitState::Dead; file.num_global_imports as usize];

        let mut func_import_symbol_offsets = vec![Vec::new(); file.num_function_imports as usize];
        let mut global_import_symbol_offsets = vec![Vec::new(); file.num_global_imports as usize];
        for (sym_offset, sym) in file.symbols.iter().enumerate() {
            if !sym.is_undefined() {
                continue;
            }
            match sym.kind {
                WasmSymbolKind::Func => {
                    if let Some(slots) = func_import_symbol_offsets.get_mut(sym.index as usize) {
                        slots.push(sym_offset);
                    }
                }
                WasmSymbolKind::Global => {
                    if let Some(slots) = global_import_symbol_offsets.get_mut(sym.index as usize) {
                        slots.push(sym_offset);
                    }
                }
                _ => {}
            }
        }
        self.func_import_symbol_offsets = func_import_symbol_offsets;
        self.global_import_symbol_offsets = global_import_symbol_offsets;
        self.gc_states_ready = true;
    }

    pub(crate) fn state(&self, unit: WasmGcUnit) -> Option<WasmGcUnitState> {
        match unit {
            WasmGcUnit::DefinedFunction(i) => self.gc_defined_functions.get(i as usize).copied(),
            WasmGcUnit::DefinedGlobal(i) => self.gc_defined_globals.get(i as usize).copied(),
            WasmGcUnit::DataSegment(i) => self.gc_data_segments.get(i as usize).copied(),
            WasmGcUnit::FunctionImport(i) => self.gc_function_imports.get(i as usize).copied(),
            WasmGcUnit::GlobalImport(i) => self.gc_global_imports.get(i as usize).copied(),
        }
    }

    pub(crate) fn state_mut(&mut self, unit: WasmGcUnit) -> Option<&mut WasmGcUnitState> {
        match unit {
            WasmGcUnit::DefinedFunction(i) => self.gc_defined_functions.get_mut(i as usize),
            WasmGcUnit::DefinedGlobal(i) => self.gc_defined_globals.get_mut(i as usize),
            WasmGcUnit::DataSegment(i) => self.gc_data_segments.get_mut(i as usize),
            WasmGcUnit::FunctionImport(i) => self.gc_function_imports.get_mut(i as usize),
            WasmGcUnit::GlobalImport(i) => self.gc_global_imports.get_mut(i as usize),
        }
    }

    pub(crate) fn is_dead(&self, unit: WasmGcUnit) -> bool {
        self.state(unit) == Some(WasmGcUnitState::Dead)
    }

    pub(crate) fn mark_live(&mut self, unit: WasmGcUnit) -> bool {
        match self.state_mut(unit) {
            Some(state) if *state == WasmGcUnitState::Dead => {
                *state = WasmGcUnitState::Live;
                true
            }
            _ => false,
        }
    }

    /// Decode code/data reloc sections and keep borrowed bodies/segments once per object.
    pub(crate) fn ensure_relocs_decoded(&mut self, file: &File<'data>) -> Result {
        if self.relocs_ready {
            return Ok(());
        }

        let code_relocations =
            decode_sorted_relocs_for(file, file.standard_section_index[section_id::CODE as usize])?;
        let data_relocations =
            decode_sorted_relocs_for(file, file.standard_section_index[section_id::DATA as usize])?;

        let function_bodies = file.function_bodies()?;
        let function_body_spans = function_body_spans_from_bodies(&function_bodies)?;
        let data_segments = file.data_segments()?;
        let data_segment_spans = data_segment_spans_from_segments(&data_segments)?;

        self.function_bodies = function_bodies;
        self.data_segments = data_segments;
        self.function_body_spans = function_body_spans;
        self.data_segment_spans = data_segment_spans;
        self.code_relocations = code_relocations;
        self.data_relocations = data_relocations;
        self.relocs_ready = true;
        Ok(())
    }

    /// Pack live defined function/global ordinals into dense 0..n maps after the GC walk.
    pub(crate) fn compute_live_ordinals(&mut self) {
        self.defined_function_live_ordinal = pack_live_ordinals(&self.gc_defined_functions);
        self.defined_global_live_ordinal = pack_live_ordinals(&self.gc_defined_globals);
    }

    pub(crate) fn take_decoded_code_data(&mut self) -> DecodedCodeData<'data> {
        let ready = self.relocs_ready;
        self.relocs_ready = false;
        self.function_body_spans.clear();
        self.data_segment_spans.clear();
        DecodedCodeData {
            ready,
            code_relocations: std::mem::take(&mut self.code_relocations),
            data_relocations: std::mem::take(&mut self.data_relocations),
            function_bodies: std::mem::take(&mut self.function_bodies),
            data_segments: std::mem::take(&mut self.data_segments),
        }
    }

    pub(crate) fn is_data_segment_live(&self, index: usize) -> bool {
        gc_index_is_live(&self.gc_data_segments, index)
    }

    pub(crate) fn is_defined_function_live(&self, index: usize) -> bool {
        gc_index_is_live(&self.gc_defined_functions, index)
    }

    pub(crate) fn is_defined_global_live(&self, index: usize) -> bool {
        gc_index_is_live(&self.gc_defined_globals, index)
    }

    pub(crate) fn live_function_import_bits(&self) -> Vec<bool> {
        gc_live_bits(&self.gc_function_imports)
    }

    pub(crate) fn live_global_import_bits(&self) -> Vec<bool> {
        gc_live_bits(&self.gc_global_imports)
    }

    /// True when GC state was never allocated (object not activated) or every unit is live.
    pub(crate) fn all_units_live(&self) -> bool {
        !self.gc_states_ready
            || [
                self.gc_defined_functions.as_slice(),
                self.gc_defined_globals.as_slice(),
                self.gc_data_segments.as_slice(),
                self.gc_function_imports.as_slice(),
                self.gc_global_imports.as_slice(),
            ]
            .into_iter()
            .all(gc_all_live)
    }

    pub(crate) fn all_defined_functions_live(&self) -> bool {
        !self.gc_states_ready || gc_all_live(&self.gc_defined_functions)
    }

    pub(crate) fn all_defined_globals_live(&self) -> bool {
        !self.gc_states_ready || gc_all_live(&self.gc_defined_globals)
    }

    pub(crate) fn all_data_segments_live(&self) -> bool {
        !self.gc_states_ready || gc_all_live(&self.gc_data_segments)
    }

    pub(crate) fn mark_all_units_live(&mut self) {
        for states in [
            &mut self.gc_defined_functions,
            &mut self.gc_defined_globals,
            &mut self.gc_data_segments,
            &mut self.gc_function_imports,
            &mut self.gc_global_imports,
        ] {
            states.fill(WasmGcUnitState::Live);
        }
    }
}

pub(crate) fn gc_index_is_live(states: &[WasmGcUnitState], index: usize) -> bool {
    states.get(index).is_some_and(|s| s.is_live())
}

pub(crate) fn gc_all_live(states: &[WasmGcUnitState]) -> bool {
    states.iter().copied().all(WasmGcUnitState::is_live)
}

pub(crate) fn gc_live_bits(states: &[WasmGcUnitState]) -> Vec<bool> {
    states.iter().map(|s| s.is_live()).collect()
}

pub(crate) fn pack_live_ordinals(states: &[WasmGcUnitState]) -> Vec<u32> {
    let mut next = 0u32;
    states
        .iter()
        .map(|state| {
            if state.is_live() {
                let ordinal = next;
                next += 1;
                ordinal
            } else {
                WASM_DEAD_INDEX
            }
        })
        .collect()
}

pub(crate) fn identity_ordinals(n: usize) -> Vec<u32> {
    (0..n as u32).collect()
}

pub(crate) fn sort_relocations_by_offset(relocs: &mut [WasmRelocation]) {
    if relocs.windows(2).any(|w| w[0].offset > w[1].offset) {
        relocs.sort_unstable_by_key(|r| r.offset);
    }
}

pub(crate) fn decode_relocs_for(
    file: &File<'_>,
    target_section_index: Option<u32>,
) -> Result<Vec<WasmRelocation>> {
    let Some(target) = target_section_index else {
        return Ok(Vec::new());
    };
    file.reloc_sections
        .iter()
        .find(|s| s.target_section_index == target)
        .map(|s| s.decode_entries(file.data))
        .transpose()
        .map(|opt| opt.unwrap_or_default())
}

pub(crate) fn decode_sorted_relocs_for(
    file: &File<'_>,
    target_section_index: Option<u32>,
) -> Result<Vec<WasmRelocation>> {
    let mut relocs = decode_relocs_for(file, target_section_index)?;
    sort_relocations_by_offset(&mut relocs);
    Ok(relocs)
}

pub(crate) fn reloc_index_range(relocs: &[WasmRelocation], start: u32, end: u32) -> Range<usize> {
    let lo = relocs.partition_point(|r| r.offset < start);
    let hi = relocs.partition_point(|r| r.offset < end);
    lo..hi
}

pub(crate) fn relocs_in_offset_range(
    relocs: &[WasmRelocation],
    start: u32,
    end: u32,
) -> &[WasmRelocation] {
    &relocs[reloc_index_range(relocs, start, end)]
}

pub(crate) fn function_body_spans_from_bodies(
    bodies: &[WasmFunctionBody<'_>],
) -> Result<Vec<(u32, u32)>> {
    bodies.iter().map(function_body_span).collect()
}

pub(crate) fn function_body_span(body: &WasmFunctionBody<'_>) -> Result<(u32, u32)> {
    let start = body.code_offset;
    let len = u32::try_from(body.bytes.len()).context("Wasm function body too large")?;
    let end = start
        .checked_add(len)
        .ok_or_else(|| crate::error!("Wasm function body span overflow"))?;
    Ok((start, end))
}

pub(crate) fn data_segment_spans_from_segments(
    segments: &[WasmDataSegment<'_>],
) -> Result<Vec<(u32, u32)>> {
    segments.iter().map(data_segment_span).collect()
}

pub(crate) fn data_segment_span(segment: &WasmDataSegment<'_>) -> Result<(u32, u32)> {
    let start = segment.section_offset;
    let end = start
        .checked_add(segment.encoded_size)
        .ok_or_else(|| crate::error!("Wasm data segment span overflow"))?;
    Ok((start, end))
}

/// Map a linking symbol to its file-local GC unit, if any.
pub(crate) fn wasm_gc_unit_for_symbol(file: &File<'_>, symbol: &WasmSymbol) -> Option<WasmGcUnit> {
    match symbol.kind {
        WasmSymbolKind::Func if symbol.is_undefined() => {
            Some(WasmGcUnit::FunctionImport(symbol.index))
        }
        WasmSymbolKind::Func => symbol
            .index
            .checked_sub(file.num_function_imports)
            .map(WasmGcUnit::DefinedFunction),
        WasmSymbolKind::Global if symbol.is_undefined() => {
            Some(WasmGcUnit::GlobalImport(symbol.index))
        }
        WasmSymbolKind::Global => symbol
            .index
            .checked_sub(file.num_global_imports)
            .map(WasmGcUnit::DefinedGlobal),
        WasmSymbolKind::Data if !symbol.is_undefined() => {
            Some(WasmGcUnit::DataSegment(symbol.index))
        }
        _ => None,
    }
}

#[derive(Debug)]
pub(crate) struct WasmObjectLayoutInput<'data> {
    /// Input module bytes.
    pub(crate) data: &'data [u8],
    pub(crate) types: Vec<wasmparser::FuncType>,
    pub(crate) function_imports: Vec<WasmFunctionImport<'data>>,
    pub(crate) global_imports: Vec<WasmGlobalImport<'data>>,
    pub(crate) live_function_imports: Vec<bool>,
    pub(crate) live_global_imports: Vec<bool>,
    pub(crate) memory_imports: Vec<MemoryType>,
    pub(crate) table_imports: Vec<wasmparser::TableType>,
    pub(crate) module_functions: Vec<u32>,
    pub(crate) globals: Vec<OutputGlobal<'data>>,
    pub(crate) exports: Vec<OutputExport<'data>>,
    pub(crate) function_bodies: Vec<WasmFunctionBody<'data>>,
    pub(crate) memories: Vec<MemoryType>,
    pub(crate) unsupported_output: Vec<&'static str>,
    pub(crate) code_relocations: Vec<WasmRelocation>,
    pub(crate) data_segments: Vec<WasmDataSegment<'data>>,
    pub(crate) data_segment_original_indices: Vec<u32>,
    pub(crate) segment_alignments: &'data [Alignment],
    pub(crate) data_relocations: Vec<WasmRelocation>,
    pub(crate) symbols: &'data [WasmSymbol],
    pub(crate) init_funcs: &'data [WasmInitFunc],
    pub(crate) target_features: &'data [WasmTargetFeature<'data>],
    pub(crate) symbol_id_range: crate::symbol_db::SymbolIdRange,
    pub(crate) file_id: crate::input_data::FileId,
    pub(crate) defined_function_live_ordinal: Vec<u32>,
    pub(crate) defined_global_live_ordinal: Vec<u32>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct WasmObjectIndexBases {
    pub(crate) type_index_base: u32,
    pub(crate) defined_function_base: u32,
    pub(crate) defined_global_base: u32,
}

impl<'data> WasmObjectLayoutInput<'data> {
    pub(crate) fn from_file(
        file: &'data File<'data>,
        layout: &WasmObjectLayout<'data>,
        decoded: DecodedCodeData<'data>,
    ) -> Result<Self> {
        let symbol_id_range = layout.symbol_id_range;
        let file_id = layout.file_id;

        let all_live = layout.all_units_live();
        let keep_all_functions = layout.all_defined_functions_live();
        let keep_all_globals = layout.all_defined_globals_live();
        let keep_all_data_segments = layout.all_data_segments_live();

        let mut types = Vec::new();
        if let Some(type_section) = file.type_section_reader()? {
            for group in type_section {
                for ty in group?.into_types() {
                    let wasmparser::CompositeInnerType::Func(func) = ty.composite_type.inner else {
                        bail!("Wasm non-function types are not emitted")
                    };
                    types.push(func);
                }
            }
        }

        let mut function_imports = Vec::new();
        let mut global_imports = Vec::new();
        let mut memory_imports = Vec::new();
        let mut table_imports = Vec::new();
        if let Some(imports) = file.import_section_reader()? {
            for import in imports.into_imports() {
                let import = import?;
                match import.ty {
                    TypeRef::Func(type_index) | TypeRef::FuncExact(type_index) => {
                        function_imports.push(WasmFunctionImport {
                            module: import.module,
                            name: import.name,
                            type_index,
                        });
                    }
                    TypeRef::Global(ty) => {
                        global_imports.push(WasmGlobalImport {
                            module: import.module,
                            name: import.name,
                            ty,
                        });
                    }
                    TypeRef::Table(ty) => {
                        table_imports.push(ty);
                    }
                    TypeRef::Memory(memory) => {
                        memory_imports.push(memory);
                    }
                    TypeRef::Tag(_) => bail!("Wasm tag imports are not emitted"),
                }
            }
        }

        let live_function_imports = if all_live {
            vec![true; function_imports.len()]
        } else {
            layout.live_function_import_bits()
        };
        let live_global_imports = if all_live {
            vec![true; global_imports.len()]
        } else {
            layout.live_global_import_bits()
        };

        let code_section_index = file.standard_section_index[section_id::CODE as usize];
        let data_section_index = file.standard_section_index[section_id::DATA as usize];

        let (code_relocations_all, data_relocations_all) = if decoded.ready {
            (decoded.code_relocations, decoded.data_relocations)
        } else {
            (
                decode_sorted_relocs_for(file, code_section_index)?,
                decode_sorted_relocs_for(file, data_section_index)?,
            )
        };

        // TODO(wasm): Currently relocs targeting `.debug*` are ignored (not applied, not emitted).
        let has_unsupported_non_code_relocs = file.reloc_sections.iter().any(|s| {
            let target = Some(s.target_section_index);
            if target == code_section_index || target == data_section_index {
                return false;
            }
            !file.section_is_debug(s.target_section_index)
        });

        let mut unsupported_output = Vec::new();
        if has_unsupported_non_code_relocs {
            unsupported_output.push("non-code relocation");
        }
        if !data_relocations_all.is_empty()
            && !data_relocations_are_supported(&data_relocations_all)
        {
            unsupported_output.push("data relocation");
        }
        if file.standard_section_index[section_id::TABLE as usize].is_some() {
            unsupported_output.push("table definition");
        }
        if file.standard_section_index[section_id::START as usize].is_some() {
            unsupported_output.push("start");
        }
        let all_data_segments = if decoded.ready {
            decoded.data_segments
        } else {
            file.data_segments()?
        };
        for segment in &all_data_segments {
            if let DataKind::Passive = segment.kind {
                unsupported_output.push("passive data segment");
                break;
            }
        }

        let all_module_functions = file.module_functions()?;
        let all_function_bodies = if decoded.ready {
            decoded.function_bodies
        } else {
            file.function_bodies()?
        };
        ensure!(
            all_module_functions.len() == all_function_bodies.len(),
            "Wasm function and code section counts differ"
        );
        let memories = file.memories()?;

        let all_globals = file
            .module_globals()?
            .into_iter()
            .map(|global| {
                let init_expr_body = crate::wasm_writer::const_expr_body(&global.init_expr)
                    .ok_or_else(|| {
                        crate::error!("Wasm global initializer is missing end opcode")
                    })?;
                Ok(OutputGlobal {
                    ty: global.ty,
                    init_expr_body: Cow::Borrowed(init_expr_body),
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let defined_function_live_ordinal = if keep_all_functions {
            identity_ordinals(all_module_functions.len())
        } else {
            layout.defined_function_live_ordinal.clone()
        };
        let defined_global_live_ordinal = if keep_all_globals {
            identity_ordinals(all_globals.len())
        } else {
            layout.defined_global_live_ordinal.clone()
        };

        let (module_functions, function_bodies, code_relocations) = if keep_all_functions {
            // All defined functions live.
            (
                all_module_functions,
                all_function_bodies,
                code_relocations_all,
            )
        } else {
            let mut module_functions = Vec::new();
            let mut function_bodies = Vec::new();
            let mut code_relocations = Vec::new();
            for (i, (ty, body)) in all_module_functions
                .into_iter()
                .zip(all_function_bodies)
                .enumerate()
            {
                if !layout.is_defined_function_live(i) {
                    continue;
                }
                let body_start = body.code_offset;
                let body_end = body_start + body.bytes.len() as u32;
                code_relocations.extend_from_slice(relocs_in_offset_range(
                    &code_relocations_all,
                    body_start,
                    body_end,
                ));
                module_functions.push(ty);
                function_bodies.push(body);
            }
            sort_relocations_by_offset(&mut code_relocations);
            (module_functions, function_bodies, code_relocations)
        };

        let globals = if keep_all_globals {
            all_globals
        } else {
            all_globals
                .into_iter()
                .enumerate()
                .filter_map(|(i, global)| layout.is_defined_global_live(i).then_some(global))
                .collect()
        };

        let (data_segments, data_segment_original_indices, data_relocations) =
            if keep_all_data_segments {
                let n = all_data_segments.len();
                let original_indices = (0..n as u32).collect();
                (all_data_segments, original_indices, data_relocations_all)
            } else {
                let mut data_segments = Vec::new();
                let mut data_segment_original_indices = Vec::new();
                let mut data_relocations = Vec::new();
                for (i, segment) in all_data_segments.into_iter().enumerate() {
                    if !layout.is_data_segment_live(i) {
                        continue;
                    }
                    let (start, end) = data_segment_span(&segment)?;
                    data_relocations.extend_from_slice(relocs_in_offset_range(
                        &data_relocations_all,
                        start,
                        end,
                    ));
                    data_segment_original_indices
                        .push(u32::try_from(i).context("too many data segments")?);
                    data_segments.push(segment);
                }
                sort_relocations_by_offset(&mut data_relocations);
                (
                    data_segments,
                    data_segment_original_indices,
                    data_relocations,
                )
            };

        let mut exports = Vec::new();
        if let Some(export_section) = file.export_section_reader()? {
            for export in export_section {
                let export = export?;
                exports.push(OutputExport {
                    name: export.name,
                    kind: export.kind,
                    index: export.index,
                });
            }
        }

        Ok(Self {
            data: file.data,
            types,
            function_imports,
            global_imports,
            live_function_imports,
            live_global_imports,
            memory_imports,
            table_imports,
            module_functions,
            globals,
            exports,
            function_bodies,
            memories,
            unsupported_output,
            code_relocations,
            data_segments,
            data_segment_original_indices,
            segment_alignments: file.segment_alignments.as_slice(),
            data_relocations,
            symbols: file.symbols.as_slice(),
            init_funcs: file.init_funcs.as_slice(),
            target_features: file.target_features.as_slice(),
            symbol_id_range,
            file_id,
            defined_function_live_ordinal,
            defined_global_live_ordinal,
        })
    }

    pub(crate) fn build_object_index_map(
        &self,
        object_index: usize,
        index_bases: WasmObjectIndexBases,
        resolutions: &ObjectImportResolutions,
        all_index_bases: &[WasmObjectIndexBases],
        indices: &LinkerDefinedIndices,
        shared_imports: &SharedUnresolvedImports<'data>,
    ) -> Result<WasmObjectIndexMap> {
        ensure!(
            resolutions.function_resolutions.len() == self.function_imports.len(),
            "Wasm function import resolution count mismatch"
        );
        ensure!(
            resolutions.global_resolutions.len() == self.global_imports.len(),
            "Wasm global import resolution count mismatch"
        );

        let mut type_indices = Vec::with_capacity(self.types.len());
        for local_ty in 0..self.types.len() {
            let output_type_index = index_bases
                .type_index_base
                .checked_add(u32::try_from(local_ty).context("too many Wasm types")?)
                .ok_or_else(|| crate::error!("Wasm type index overflow"))?;
            type_indices.push(output_type_index);
        }

        let mut index_map = WasmObjectIndexMap {
            type_indices,
            function_indices: Vec::with_capacity(
                self.function_imports.len() + self.defined_function_live_ordinal.len(),
            ),
            global_indices: Vec::with_capacity(
                self.global_imports.len() + self.defined_global_live_ordinal.len(),
            ),
            memory_indices: Vec::with_capacity(self.memory_imports.len() + self.memories.len()),
            table_indices: vec![0; self.table_imports.len()],
            data_addresses: Vec::new(),
            got_mem_globals: Vec::new(),
            got_func_globals: Vec::new(),
            function_symbol_redirects: Vec::new(),
        };

        for (i, resolution) in resolutions.function_resolutions.iter().enumerate() {
            if !self.live_function_imports.get(i).copied().unwrap_or(false) {
                index_map.function_indices.push(WASM_DEAD_INDEX);
                continue;
            }
            match *resolution {
                ImportResolution::Unresolved => {
                    let output_function_index = shared_imports
                        .function_index(object_index, i)
                        .ok_or_else(|| {
                            crate::error!(
                                "missing shared function import index for object {object_index} \
                                 import {i}"
                            )
                        })?;
                    index_map.function_indices.push(output_function_index);
                }
                ImportResolution::LinkerDefined(known) => {
                    let index = indices.function_index(known).ok_or_else(|| {
                        crate::error!("missing reserved Wasm function for {known:?}")
                    })?;
                    index_map.function_indices.push(index);
                }
                ImportResolution::WeakUndefStub { stub_index } => {
                    let index = indices
                        .weak_undef_stubs
                        .get(stub_index as usize)
                        .map(|s| s.function_index)
                        .ok_or_else(|| {
                            crate::error!("Wasm weak-undef stub index {stub_index} out of range")
                        })?;
                    index_map.function_indices.push(index);
                }
                ImportResolution::ResolvedFunction {
                    object_index: def_object_index,
                    local_defined_index,
                } => {
                    ensure!(
                        def_object_index < all_index_bases.len(),
                        "Wasm function import resolution references object index \
                         {def_object_index} out of range"
                    );
                    ensure!(
                        local_defined_index != WASM_DEAD_INDEX,
                        "Wasm function import resolved to a GC'd definition"
                    );
                    let target_bases = &all_index_bases[def_object_index];
                    let output_function_index = target_bases
                        .defined_function_base
                        .checked_add(local_defined_index)
                        .ok_or_else(|| crate::error!("Wasm function index overflow"))?;
                    index_map.function_indices.push(output_function_index);
                }
                ImportResolution::ResolvedGlobal { .. }
                | ImportResolution::DirectGlobal { .. }
                | ImportResolution::GotMemSlot(_)
                | ImportResolution::GotFuncSlot(_) => {
                    bail!("function import resolved as global");
                }
            }
        }

        for (i, resolution) in resolutions.global_resolutions.iter().enumerate() {
            if !self.live_global_imports.get(i).copied().unwrap_or(false) {
                index_map.global_indices.push(WASM_DEAD_INDEX);
                continue;
            }
            match *resolution {
                ImportResolution::Unresolved => {
                    let output_global_index = shared_imports
                        .global_index(object_index, i)
                        .ok_or_else(|| {
                            crate::error!(
                                "missing shared global import index for object {object_index} \
                                 import {i}"
                            )
                        })?;
                    index_map.global_indices.push(output_global_index);
                }
                ImportResolution::LinkerDefined(known) => {
                    let index = indices.global_index(known).ok_or_else(|| {
                        crate::error!("missing reserved Wasm global for {known:?}")
                    })?;
                    index_map.global_indices.push(index);
                }
                ImportResolution::DirectGlobal { output_index } => {
                    index_map.global_indices.push(output_index);
                }
                ImportResolution::GotMemSlot(_) => {
                    bail!("GOT.mem slot was not converted to a module global index");
                }
                ImportResolution::GotFuncSlot(_) => {
                    bail!("GOT.func slot was not converted to a module global index");
                }
                ImportResolution::ResolvedGlobal {
                    object_index,
                    local_defined_index,
                } => {
                    ensure!(
                        object_index < all_index_bases.len(),
                        "Wasm global import resolution references object index {object_index} out \
                         of range"
                    );
                    ensure!(
                        local_defined_index != WASM_DEAD_INDEX,
                        "Wasm global import resolved to a GC'd definition"
                    );
                    let target_bases = &all_index_bases[object_index];
                    let output_global_index = target_bases
                        .defined_global_base
                        .checked_add(local_defined_index)
                        .ok_or_else(|| crate::error!("Wasm global index overflow"))?;
                    index_map.global_indices.push(output_global_index);
                }
                ImportResolution::ResolvedFunction { .. }
                | ImportResolution::WeakUndefStub { .. } => {
                    bail!("global import resolved as function");
                }
            }
        }

        // Full function index space: imports (above) + original defined ordinals.
        for &dense_or_dead in &self.defined_function_live_ordinal {
            if dense_or_dead == WASM_DEAD_INDEX {
                index_map.function_indices.push(WASM_DEAD_INDEX);
            } else {
                let output_function_index = index_bases
                    .defined_function_base
                    .checked_add(dense_or_dead)
                    .ok_or_else(|| crate::error!("Wasm function index overflow"))?;
                index_map.function_indices.push(output_function_index);
            }
        }

        for &dense_or_dead in &self.defined_global_live_ordinal {
            if dense_or_dead == WASM_DEAD_INDEX {
                index_map.global_indices.push(WASM_DEAD_INDEX);
            } else {
                let output_global_index = index_bases
                    .defined_global_base
                    .checked_add(dense_or_dead)
                    .ok_or_else(|| crate::error!("Wasm global index overflow"))?;
                index_map.global_indices.push(output_global_index);
            }
        }

        // Imported and defined memories are merged into a single output memory.
        let memory_slot_count = self.memory_imports.len() + self.memories.len();
        index_map.memory_indices = vec![0; memory_slot_count];

        Ok(index_map)
    }

    pub(crate) fn remapped_exports(
        &self,
        index_map: &WasmObjectIndexMap,
    ) -> Result<Vec<OutputExport<'data>>> {
        self.exports
            .iter()
            .map(|export| {
                let index = match export.kind {
                    wasmparser::ExternalKind::Func | wasmparser::ExternalKind::FuncExact => {
                        remap_wasm_index(&index_map.function_indices, export.index, "function")?
                    }
                    wasmparser::ExternalKind::Global => {
                        remap_wasm_index(&index_map.global_indices, export.index, "global")?
                    }
                    wasmparser::ExternalKind::Memory => {
                        remap_wasm_index(&index_map.memory_indices, export.index, "memory")?
                    }
                    wasmparser::ExternalKind::Table => 0, // single output table
                    wasmparser::ExternalKind::Tag => bail!("Wasm tag exports are not emitted"),
                };
                Ok(OutputExport { index, ..*export })
            })
            .collect()
    }
}

/// Describes how a single import was resolved during cross-object linking.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ImportResolution {
    /// The import was not resolved; keep it in the output import section.
    Unresolved,
    /// The import was resolved to a defined function in `object_index` at local defined-function
    /// position `local_defined_index`.
    ResolvedFunction {
        object_index: usize,
        local_defined_index: u32,
    },
    /// The import was resolved to a defined global in `object_index` at local defined-global
    /// position `local_defined_index`.
    ResolvedGlobal {
        object_index: usize,
        local_defined_index: u32,
    },
    /// Resolved to a linker-synthesized function or global.
    LinkerDefined(WasmLinkerSymbol),
    /// Undefined weak function absorbed into a shared `unreachable` stub.
    WeakUndefStub { stub_index: u32 },
    /// Fixed module global index (GOT.mem / GOT.func entry).
    DirectGlobal { output_index: u32 },
    /// GOT.mem slot pending final module global index.
    GotMemSlot(usize),
    /// GOT.func slot pending final module global index.
    GotFuncSlot(usize),
}

#[derive(Debug, Default)]
pub(crate) struct ObjectImportResolutions {
    pub(crate) function_resolutions: Vec<ImportResolution>,
    pub(crate) global_resolutions: Vec<ImportResolution>,
}

#[derive(Debug, Clone)]
pub(crate) struct SharedFunctionImport<'data> {
    pub(crate) module: &'data str,
    pub(crate) name: &'data str,
    pub(crate) first_object: usize,
    pub(crate) local_type_index: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct SharedGlobalImport<'data> {
    pub(crate) module: &'data str,
    pub(crate) name: &'data str,
    pub(crate) ty: GlobalType,
}

/// Unresolved host imports coalesced by `(module, name)` across objects.
#[derive(Debug, Default)]
pub(crate) struct SharedUnresolvedImports<'data> {
    pub(crate) functions: Vec<SharedFunctionImport<'data>>,
    pub(crate) globals: Vec<SharedGlobalImport<'data>>,
    pub(crate) function_indices: Vec<Vec<Option<u32>>>,
    pub(crate) global_indices: Vec<Vec<Option<u32>>>,
}

impl<'data> SharedUnresolvedImports<'data> {
    pub(crate) fn function_count(&self) -> u32 {
        self.functions.len() as u32
    }

    pub(crate) fn global_count(&self) -> u32 {
        self.globals.len() as u32
    }

    pub(crate) fn function_index(&self, object_index: usize, local_import: usize) -> Option<u32> {
        self.function_indices
            .get(object_index)?
            .get(local_import)
            .copied()
            .flatten()
    }

    pub(crate) fn global_index(&self, object_index: usize, local_import: usize) -> Option<u32> {
        self.global_indices
            .get(object_index)?
            .get(local_import)
            .copied()
            .flatten()
    }

    pub(crate) fn to_output_imports(
        &self,
        index_bases: &[WasmObjectIndexBases],
    ) -> Result<Vec<OutputImport<'data>>> {
        let mut imports = Vec::with_capacity(self.functions.len() + self.globals.len());
        for imp in &self.functions {
            let type_index = index_bases
                .get(imp.first_object)
                .ok_or_else(|| crate::error!("Wasm shared import object index out of range"))?
                .type_index_base
                .checked_add(imp.local_type_index)
                .ok_or_else(|| crate::error!("Wasm type index overflow"))?;
            imports.push(OutputImport {
                module: imp.module,
                name: imp.name,
                entity: OutputImportEntity::Function { type_index },
            });
        }
        for imp in &self.globals {
            imports.push(OutputImport {
                module: imp.module,
                name: imp.name,
                entity: OutputImportEntity::Global(imp.ty),
            });
        }
        Ok(imports)
    }
}
