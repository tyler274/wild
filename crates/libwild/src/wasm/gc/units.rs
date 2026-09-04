use super::super::WASM_DEAD_INDEX;
use super::super::file::*;
use super::super::relocations::*;
use super::super::section_id;
use super::super::symbols::*;
use crate::error::Context as _;
use crate::error::Result;
use std::ops::Range;

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
