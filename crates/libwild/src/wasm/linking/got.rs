use super::*;
use crate::bail;
use crate::ensure;
use crate::error::Context as _;
use crate::error::Result;
use crate::platform::Args as _;
use crate::symbol::UnversionedSymbolName;
use crate::symbol_db::SymbolDb;
use crate::symbol_db::SymbolId;
use crate::timing_phase;
use crate::verbose_timing_phase;
use crate::wasm::WASM_DEAD_INDEX;
use crate::wasm::Wasm;
use crate::wasm::gc::*;
use crate::wasm::output::*;
use crate::wasm::relocations::*;
use crate::wasm::symbols::*;
use hashbrown::HashMap;
use hashbrown::HashSet;
use rayon::prelude::*;
use std::borrow::Cow;
use wasmparser::RelocationType;

/// Synthetic function produced for an unresolved weak function import.
#[derive(Debug, Clone)]
pub(crate) struct WeakUndefFunctionStub {
    pub(crate) ty: wasmparser::FuncType,
    pub(crate) function_index: u32,
}

/// Reserved Wasm index-space slots for linker-defined globals/functions.

/// Where a GOT.mem slot's final linear-memory address comes from.
#[derive(Debug, Clone, Copy)]
pub(crate) enum GotMemDef {
    Object {
        object_index: usize,
        symbol_offset: usize,
    },
    LinkerDefined(WasmLinkerSymbol),
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct GotMemEntry {
    pub(crate) def_symbol_id: SymbolId,
    pub(crate) def: GotMemDef,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct GotFuncEntry {
    pub(crate) def_symbol_id: SymbolId,
    object_index: usize,
    symbol_offset: usize,
}

#[derive(Debug, Default)]
pub(crate) struct GotSlots<E> {
    pub(crate) entries: Vec<E>,
    pub(crate) per_object_global_indices: Vec<Vec<Option<u32>>>,
}

impl<E> GotSlots<E> {
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn len(&self) -> u32 {
        self.entries.len() as u32
    }
}

pub(crate) type GotMem = GotSlots<GotMemEntry>;
pub(crate) type GotFunc = GotSlots<GotFuncEntry>;

pub(crate) fn layout_file_id_to_index(
    layout_inputs: &[WasmObjectLayoutInput<'_>],
) -> HashMap<crate::input_data::FileId, usize> {
    layout_inputs
        .iter()
        .enumerate()
        .map(|(i, input)| (input.file_id, i))
        .collect()
}

/// Per-object map of linking symbol index to provisional GOT slot.
pub(crate) type GotSlotMap = Vec<Vec<Option<usize>>>;

pub(crate) struct LayoutRelocScan {
    pub(crate) got_mem: GotMem,
    pub(crate) got_func: GotFunc,
    pub(crate) per_object_got_mem_slots: GotSlotMap,
    pub(crate) per_object_got_func_slots: GotSlotMap,
    pub(crate) needs_memory_base: bool,
    pub(crate) needs_table_base: bool,
    pub(crate) needs_table: bool,
    pub(crate) table_index_symbol_indices: Vec<Vec<usize>>,
}

/// Scan relocations, absorb GOT.mem / GOT.func / weak-undef imports, and reserve indices.
pub(crate) fn setup_got_mem_and_indices<'data>(
    layout_inputs: &[WasmObjectLayoutInput<'data>],
    resolutions: &mut [ObjectImportResolutions],
    symbol_db: &SymbolDb<'data, Wasm>,
    file_id_to_index: &HashMap<crate::input_data::FileId, usize>,
    has_init_funcs: bool,
    wrap_entry: bool,
) -> Result<(
    LinkerDefinedIndices,
    LayoutRelocScan,
    SharedUnresolvedImports<'data>,
)> {
    timing_phase!("Setup Wasm GOT and indices");

    let mut scan = scan_layout_relocations(layout_inputs, symbol_db, file_id_to_index)?;

    let weak_undef_stubs = {
        timing_phase!("Absorb Wasm GOT and weak-undef imports");
        absorb_got_imports(
            &scan.got_mem,
            "GOT.mem",
            |e| e.def_symbol_id,
            ImportResolution::GotMemSlot,
            layout_inputs,
            resolutions,
            symbol_db,
        )?;
        let weak_undef_stubs = absorb_weak_undef_function_imports(layout_inputs, resolutions)?;
        absorb_got_imports(
            &scan.got_func,
            "GOT.func",
            |e| e.def_symbol_id,
            ImportResolution::GotFuncSlot,
            layout_inputs,
            resolutions,
            symbol_db,
        )?;
        weak_undef_stubs
    };
    report_disallowed_unresolved_imports(layout_inputs, resolutions, symbol_db)?;
    let shared_imports = collect_shared_unresolved_imports(layout_inputs, resolutions)?;

    let indices =
        {
            timing_phase!("Reserve linker-defined Wasm indices");
            let indices = LinkerDefinedIndices::compute(
                layout_inputs,
                resolutions,
                shared_imports.function_count(),
                shared_imports.global_count(),
                weak_undef_stubs,
                &LinkerDefinedIndexRequest {
                    has_init_funcs,
                    export_symbols: requested_linker_export_symbols(symbol_db.args),
                    // Executables always get a defined linear memory.
                    has_memory: true,
                    wrap_entry,
                    got_mem_count: scan.got_mem.len(),
                    got_func_count: scan.got_func.len(),
                    needs_memory_base: scan.needs_memory_base,
                    needs_table_base: scan.needs_table_base,
                },
            )?;

            if !scan.got_mem.is_empty() {
                let first_got = indices.got_mem_global_base.ok_or_else(|| {
                    crate::error!("GOT.mem entries present but no global base reserved")
                })?;
                scan.got_mem.per_object_global_indices =
                    assign_got_slot_global_indices(&scan.per_object_got_mem_slots, first_got)?;
                finalize_got_import_resolutions(resolutions, first_got, |resolution| {
                    match resolution {
                        ImportResolution::GotMemSlot(slot) => Some(slot),
                        _ => None,
                    }
                })?;
            }

            if !scan.got_func.is_empty() {
                let first_got = indices.got_func_global_base.ok_or_else(|| {
                    crate::error!("GOT.func entries present but no global base reserved")
                })?;
                scan.got_func.per_object_global_indices =
                    assign_got_slot_global_indices(&scan.per_object_got_func_slots, first_got)?;
                finalize_got_import_resolutions(resolutions, first_got, |resolution| {
                    match resolution {
                        ImportResolution::GotFuncSlot(slot) => Some(slot),
                        _ => None,
                    }
                })?;
            }

            indices
        };

    Ok((indices, scan, shared_imports))
}

/// True when every undefined Func symbol for that ordinal is weak.
pub(crate) fn pure_weak_function_import_flags(input: &WasmObjectLayoutInput<'_>) -> Vec<bool> {
    let n = input.function_imports.len();
    let mut saw_weak = vec![false; n];
    let mut saw_non_weak = vec![false; n];
    for sym in input.symbols {
        if sym.kind != WasmSymbolKind::Func || !sym.is_undefined() {
            continue;
        }
        let idx = sym.index as usize;
        if idx >= n {
            continue;
        }
        if sym.is_weak() {
            saw_weak[idx] = true;
        } else {
            saw_non_weak[idx] = true;
        }
    }
    saw_weak
        .into_iter()
        .zip(saw_non_weak)
        .map(|(weak, non_weak)| weak && !non_weak)
        .collect()
}

/// Absorb pure undefined-weak function imports into shared `unreachable` stubs.
pub(crate) fn absorb_weak_undef_function_imports<'data>(
    inputs: &[WasmObjectLayoutInput<'data>],
    resolutions: &mut [ObjectImportResolutions],
) -> Result<Vec<WeakUndefFunctionStub>> {
    let pure_weak_flags: Vec<Vec<bool>> =
        inputs.iter().map(pure_weak_function_import_flags).collect();

    let mut non_weak_names = HashSet::new();
    for (input, (res, flags)) in inputs
        .iter()
        .zip(resolutions.iter().zip(pure_weak_flags.iter()))
    {
        for (i, import) in input.function_imports.iter().enumerate() {
            if !input.live_function_imports.get(i).copied().unwrap_or(false) {
                continue;
            }
            if !matches!(
                res.function_resolutions.get(i),
                Some(ImportResolution::Unresolved)
            ) {
                continue;
            }
            if !flags.get(i).copied().unwrap_or(false) {
                non_weak_names.insert(import.name);
            }
        }
    }

    let mut stubs: Vec<WeakUndefFunctionStub> = Vec::new();
    let mut name_to_stub: HashMap<&str, u32> = HashMap::new();

    for (input, (res, flags)) in inputs
        .iter()
        .zip(resolutions.iter_mut().zip(pure_weak_flags.iter()))
    {
        for (i, import) in input.function_imports.iter().enumerate() {
            if !input.live_function_imports.get(i).copied().unwrap_or(false) {
                continue;
            }
            if !matches!(
                res.function_resolutions.get(i),
                Some(ImportResolution::Unresolved)
            ) {
                continue;
            }
            if !flags.get(i).copied().unwrap_or(false) {
                continue;
            }
            if non_weak_names.contains(import.name) {
                continue;
            }
            let ty = input
                .types
                .get(import.type_index as usize)
                .ok_or_else(|| {
                    crate::error!(
                        "Wasm type index {} out of range for weak import `{}`",
                        import.type_index,
                        import.name
                    )
                })?
                .clone();
            let stub_index = if let Some(&idx) = name_to_stub.get(import.name) {
                ensure!(
                    stubs[idx as usize].ty == ty,
                    "conflicting types for undefined weak function `{}`",
                    import.name
                );
                idx
            } else {
                let idx = u32::try_from(stubs.len()).context("too many Wasm weak-undef stubs")?;
                name_to_stub.insert(import.name, idx);
                stubs.push(WeakUndefFunctionStub {
                    ty,
                    function_index: 0,
                });
                idx
            };
            res.function_resolutions[i] = ImportResolution::WeakUndefStub { stub_index };
        }
    }

    Ok(stubs)
}

pub(crate) fn resolve_got_mem_def(
    def_id: SymbolId,
    layout_inputs: &[WasmObjectLayoutInput<'_>],
    symbol_db: &SymbolDb<'_, Wasm>,
    file_id_to_index: &HashMap<crate::input_data::FileId, usize>,
) -> Result<GotMemDef> {
    let def_file_id = symbol_db.file_id_for_symbol(def_id);
    if let Some(&def_obj_idx) = file_id_to_index.get(&def_file_id) {
        let def_input = &layout_inputs[def_obj_idx];
        let def_off = def_id.to_offset(def_input.symbol_id_range);
        let def_ok = def_input
            .symbols
            .get(def_off)
            .is_some_and(|s| s.kind == WasmSymbolKind::Data);
        ensure!(
            def_ok,
            "GOT.mem for `{}` requires a data symbol in the link",
            symbol_db.symbol_name_for_display(def_id)
        );
        return Ok(GotMemDef::Object {
            object_index: def_obj_idx,
            symbol_offset: def_off,
        });
    }

    // Linker-defined data live on the prelude file, not in `layout_inputs`.
    if let Some(def_info) = symbol_db.prelude_symbol_def(def_id)
        && let crate::parsing::SymbolPlacement::PlatformSpecific(known) = def_info.placement
        && matches!(
            known,
            WasmLinkerSymbol::DataEnd
                | WasmLinkerSymbol::GlobalBase
                | WasmLinkerSymbol::HeapBase
                | WasmLinkerSymbol::HeapEnd
                | WasmLinkerSymbol::WasmFirstPageEnd
                | WasmLinkerSymbol::DsoHandle
        )
    {
        return Ok(GotMemDef::LinkerDefined(known));
    }

    bail!(
        "GOT.mem for `{}` requires a defined data symbol in the link",
        symbol_db.symbol_name_for_display(def_id)
    )
}

pub(crate) fn note_undefined_data_from_reloc(
    input: &WasmObjectLayoutInput<'_>,
    symbol_db: &SymbolDb<'_, Wasm>,
    reloc: &WasmRelocation,
    seen: &mut HashSet<(String, String)>,
    errors: &mut Vec<(String, String)>,
) -> Result {
    if reloc.ty == RelocationType::TypeIndexLeb {
        return Ok(());
    }
    let Some(sym) = input.symbols.get(reloc.index as usize) else {
        return Ok(());
    };
    if sym.kind != WasmSymbolKind::Data
        || !sym.is_undefined()
        || sym.is_weak()
        || sym.is_explicit_name()
    {
        return Ok(());
    }
    let symbol_id = input.symbol_id_range.offset_to_id(reloc.index as usize);
    if !symbol_db.is_undefined(symbol_db.definition(symbol_id)) {
        return Ok(());
    }

    let file_display = symbol_db.file(input.file_id).to_string();
    let Some(name) = wasm_symbol_name_str(input.data, sym) else {
        bail!(
            "{file_display}: undefined symbol with no name (linking symbol index {})",
            reloc.index
        );
    };
    if seen.insert((file_display.clone(), name.to_owned())) {
        errors.push((file_display, name.to_owned()));
    }
    Ok(())
}

/// Per-object scan of layout relocations.
pub(crate) struct ObjectRelocScan {
    pub(crate) got_mem_first: Vec<SymbolId>,
    pub(crate) got_mem_hits: Vec<(usize, SymbolId)>,
    pub(crate) got_func_first: Vec<(SymbolId, usize)>,
    pub(crate) got_func_hits: Vec<(usize, SymbolId)>,
    pub(crate) table_syms: Vec<usize>,
    pub(crate) needs_memory_base: bool,
    pub(crate) needs_table_base: bool,
    pub(crate) needs_table: bool,
    pub(crate) undefined_data_errors: Vec<(String, String)>,
}

pub(crate) fn scan_object_layout_relocations(
    input: &WasmObjectLayoutInput<'_>,
    symbol_db: &SymbolDb<'_, Wasm>,
    check_undefined_data: bool,
) -> Result<ObjectRelocScan> {
    verbose_timing_phase!("Scan Wasm object layout relocations");

    let mut got_mem_first = Vec::new();
    let mut got_mem_seen = HashSet::new();
    let mut got_mem_hits = Vec::new();
    let mut got_func_first = Vec::new();
    let mut got_func_seen = HashSet::new();
    let mut got_func_hits = Vec::new();
    let mut table_syms = Vec::new();
    let mut table_sym_seen = HashSet::new();
    let mut needs_memory_base = false;
    let mut needs_table_base = false;
    let mut needs_table = !input.table_imports.is_empty();
    let mut undefined_data_errors = Vec::new();
    let mut seen_undefined_data = HashSet::new();

    for reloc in input
        .code_relocations
        .iter()
        .chain(input.data_relocations.iter())
    {
        if check_undefined_data {
            note_undefined_data_from_reloc(
                input,
                symbol_db,
                reloc,
                &mut seen_undefined_data,
                &mut undefined_data_errors,
            )?;
        }
        match reloc.ty {
            RelocationType::MemoryAddrRelSleb => {
                needs_memory_base = true;
            }
            RelocationType::TableNumberLeb => {
                needs_table = true;
            }
            RelocationType::TableIndexSleb
            | RelocationType::TableIndexI32
            | RelocationType::TableIndexRelSleb => {
                if reloc.ty == RelocationType::TableIndexRelSleb {
                    needs_table_base = true;
                }
                needs_table = true;
                let sym_idx = reloc.index as usize;
                let Some(sym) = input.symbols.get(sym_idx) else {
                    bail!("table index relocation symbol {} out of range", reloc.index);
                };
                ensure!(
                    sym.kind == WasmSymbolKind::Func,
                    "R_WASM_TABLE_INDEX_* references non-function symbol"
                );
                if table_sym_seen.insert(sym_idx) {
                    table_syms.push(sym_idx);
                }
            }
            RelocationType::GlobalIndexLeb | RelocationType::GlobalIndexI32 => {
                let sym_idx = reloc.index as usize;
                let Some(sym) = input.symbols.get(sym_idx) else {
                    bail!(
                        "GLOBAL_INDEX relocation symbol index {} out of range",
                        reloc.index
                    );
                };
                match sym.kind {
                    WasmSymbolKind::Data => {
                        let symbol_id = input.symbol_id_range.offset_to_id(sym_idx);
                        let def_id = symbol_db.definition(symbol_id);
                        if got_mem_seen.insert(def_id) {
                            got_mem_first.push(def_id);
                        }
                        got_mem_hits.push((sym_idx, def_id));
                    }
                    WasmSymbolKind::Func => {
                        let symbol_id = input.symbol_id_range.offset_to_id(sym_idx);
                        let def_id = symbol_db.definition(symbol_id);
                        if got_func_seen.insert(def_id) {
                            got_func_first.push((def_id, sym_idx));
                        }
                        got_func_hits.push((sym_idx, def_id));
                        // Ensure the function appears in the indirect table (null weak stubs
                        // are skipped later when assigning slots).
                        needs_table = true;
                        if table_sym_seen.insert(sym_idx) {
                            table_syms.push(sym_idx);
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    Ok(ObjectRelocScan {
        got_mem_first,
        got_mem_hits,
        got_func_first,
        got_func_hits,
        table_syms,
        needs_memory_base,
        needs_table_base,
        needs_table,
        undefined_data_errors,
    })
}

pub(crate) fn scan_layout_relocations(
    layout_inputs: &[WasmObjectLayoutInput<'_>],
    symbol_db: &SymbolDb<'_, Wasm>,
    file_id_to_index: &HashMap<crate::input_data::FileId, usize>,
) -> Result<LayoutRelocScan> {
    timing_phase!("Scan Wasm layout relocations");

    let check_undefined_data = !symbol_db.args.allow_undefined;
    let object_scans: Vec<Result<ObjectRelocScan>> = layout_inputs
        .par_iter()
        .map(|input| scan_object_layout_relocations(input, symbol_db, check_undefined_data))
        .collect();

    let mut mem_def_to_slot: HashMap<SymbolId, usize> = HashMap::new();
    let mut mem_entries = Vec::new();
    let mut per_object_got_mem_slots = vec![Vec::new(); layout_inputs.len()];
    let mut func_def_to_slot: HashMap<SymbolId, usize> = HashMap::new();
    let mut func_entries = Vec::new();
    let mut per_object_got_func_slots = vec![Vec::new(); layout_inputs.len()];
    let mut needs_memory_base = false;
    let mut needs_table_base = false;
    let mut needs_table = false;
    let mut table_index_symbol_indices = vec![Vec::new(); layout_inputs.len()];
    let mut undefined_data_errors: Vec<String> = Vec::new();
    let mut seen_undefined_data: HashSet<(String, String)> = HashSet::new();

    for (obj_idx, scan) in object_scans.into_iter().enumerate() {
        let scan = scan?;
        needs_memory_base |= scan.needs_memory_base;
        needs_table_base |= scan.needs_table_base;
        needs_table |= scan.needs_table;
        for key in scan.undefined_data_errors {
            if seen_undefined_data.insert(key.clone()) {
                let name = demangle_symbol_name(&key.1, symbol_db.args.demangle());
                undefined_data_errors.push(format!("{}: undefined symbol: {name}", key.0));
            }
        }
        table_index_symbol_indices[obj_idx] = scan.table_syms;

        for def_id in scan.got_mem_first {
            if let hashbrown::hash_map::Entry::Vacant(entry) = mem_def_to_slot.entry(def_id) {
                let def = resolve_got_mem_def(def_id, layout_inputs, symbol_db, file_id_to_index)?;
                let slot = mem_entries.len();
                entry.insert(slot);
                mem_entries.push(GotMemEntry {
                    def_symbol_id: def_id,
                    def,
                });
            }
        }
        for (def_id, symbol_offset) in scan.got_func_first {
            if let hashbrown::hash_map::Entry::Vacant(entry) = func_def_to_slot.entry(def_id) {
                let slot = func_entries.len();
                entry.insert(slot);
                func_entries.push(GotFuncEntry {
                    def_symbol_id: def_id,
                    object_index: obj_idx,
                    symbol_offset,
                });
            }
        }

        let got_mem_hits = scan
            .got_mem_hits
            .into_iter()
            .map(|(sym_idx, def_id)| (sym_idx, mem_def_to_slot[&def_id]))
            .collect();
        let got_func_hits = scan
            .got_func_hits
            .into_iter()
            .map(|(sym_idx, def_id)| (sym_idx, func_def_to_slot[&def_id]))
            .collect();
        assign_got_hits(
            &mut per_object_got_mem_slots,
            obj_idx,
            layout_inputs[obj_idx].symbols.len(),
            got_mem_hits,
        );
        assign_got_hits(
            &mut per_object_got_func_slots,
            obj_idx,
            layout_inputs[obj_idx].symbols.len(),
            got_func_hits,
        );
    }

    if !undefined_data_errors.is_empty() {
        bail!("{}", undefined_data_errors.join("\n"));
    }

    Ok(LayoutRelocScan {
        got_mem: GotMem {
            entries: mem_entries,
            per_object_global_indices: Vec::new(),
        },
        got_func: GotFunc {
            entries: func_entries,
            per_object_global_indices: Vec::new(),
        },
        per_object_got_mem_slots,
        per_object_got_func_slots,
        needs_memory_base,
        needs_table_base,
        needs_table,
        table_index_symbol_indices,
    })
}

pub(crate) fn assign_got_slot_global_indices(
    per_object_slots: &GotSlotMap,
    first_global_index: u32,
) -> Result<Vec<Vec<Option<u32>>>> {
    let mut per_object = Vec::with_capacity(per_object_slots.len());
    for obj_map in per_object_slots {
        if obj_map.is_empty() {
            per_object.push(Vec::new());
            continue;
        }
        let mut out = Vec::with_capacity(obj_map.len());
        for slot in obj_map {
            out.push(match slot {
                Some(s) => Some(
                    first_global_index
                        .checked_add(*s as u32)
                        .ok_or_else(|| crate::error!("Wasm global index overflow"))?,
                ),
                None => None,
            });
        }
        per_object.push(out);
    }
    Ok(per_object)
}

pub(crate) fn assign_got_hits(
    dst: &mut [Vec<Option<usize>>],
    obj_idx: usize,
    symbol_count: usize,
    hits: Vec<(usize, usize)>,
) {
    if hits.is_empty() {
        return;
    }
    let mut obj_map = vec![None; symbol_count];
    for (sym_idx, slot) in hits {
        obj_map[sym_idx] = Some(slot);
    }
    dst[obj_idx] = obj_map;
}

pub(crate) fn apply_got_to_index_maps<'a, E>(
    dsts: impl Iterator<Item = &'a mut Vec<Option<u32>>>,
    got: &GotSlots<E>,
) {
    if got.is_empty() {
        return;
    }
    for (dst, src) in dsts.zip(got.per_object_global_indices.iter()) {
        if !src.is_empty() {
            *dst = src.clone();
        }
    }
}

/// Map defined weak functions to the winning definition's output index.
pub(crate) fn fill_function_symbol_redirects(
    object_index_maps: &mut [WasmObjectIndexMap],
    layout_inputs: &[WasmObjectLayoutInput<'_>],
    symbol_db: &SymbolDb<'_, Wasm>,
    file_id_to_index: &HashMap<crate::input_data::FileId, usize>,
) {
    for (obj_idx, input) in layout_inputs.iter().enumerate() {
        let mut redirects = vec![None; input.symbols.len()];
        for (sym_off, sym) in input.symbols.iter().enumerate() {
            if sym.kind != WasmSymbolKind::Func || !sym.is_weak() || sym.is_undefined() {
                continue;
            }
            let local_id = input.symbol_id_range.offset_to_id(sym_off);
            let def_id = symbol_db.definition(local_id);
            if def_id == local_id {
                continue;
            }
            let Some(&def_obj_idx) = file_id_to_index.get(&symbol_db.file_id_for_symbol(def_id))
            else {
                continue;
            };
            let def_input = &layout_inputs[def_obj_idx];
            let def_sym = &def_input.symbols[def_input.symbol_id_range.id_to_offset(def_id)];
            if def_sym.kind != WasmSymbolKind::Func || def_sym.is_undefined() {
                continue;
            }
            let Some(&out) = object_index_maps[def_obj_idx]
                .function_indices
                .get(def_sym.index as usize)
            else {
                continue;
            };
            if out != WASM_DEAD_INDEX {
                redirects[sym_off] = Some(out);
            }
        }
        object_index_maps[obj_idx].function_symbol_redirects = redirects;
    }
}

pub(crate) fn got_func_debug_name(
    layout_inputs: &[WasmObjectLayoutInput<'_>],
    entry: &GotFuncEntry,
    index: usize,
    demangle: bool,
) -> String {
    let sym_name = layout_inputs.get(entry.object_index).and_then(|input| {
        input
            .symbols
            .get(entry.symbol_offset)
            .and_then(|sym| wasm_symbol_name_str(input.data, sym))
    });
    match sym_name {
        Some(name) => format!("GOT.func.internal.{}", demangle_symbol_name(name, demangle)),
        None => format!("GOT.func.internal.{index}"),
    }
}

pub(crate) fn absorb_got_imports<E>(
    got: &GotSlots<E>,
    module: &str,
    def_symbol_id: impl Fn(&E) -> SymbolId,
    to_resolution: impl Fn(usize) -> ImportResolution,
    layout_inputs: &[WasmObjectLayoutInput<'_>],
    resolutions: &mut [ObjectImportResolutions],
    symbol_db: &SymbolDb<'_, Wasm>,
) -> Result {
    if got.is_empty() {
        return Ok(());
    }

    let names: Vec<UnversionedSymbolName<'_>> = got
        .entries
        .iter()
        .map(|entry| {
            let id = def_symbol_id(entry);
            symbol_db.symbol_name(id).with_context(|| {
                format!(
                    "{module} entry missing symbol name for `{}`",
                    symbol_db.symbol_name_for_display(id)
                )
            })
        })
        .collect::<Result<_>>()?;

    let mut name_to_slot: HashMap<&[u8], usize> = HashMap::new();
    for (slot, name) in names.iter().enumerate() {
        name_to_slot.entry(name.bytes()).or_insert(slot);
    }

    for (input, res) in layout_inputs.iter().zip(resolutions.iter_mut()) {
        for (i, import) in input.global_imports.iter().enumerate() {
            if import.module != module {
                continue;
            }
            if !matches!(res.global_resolutions[i], ImportResolution::Unresolved) {
                continue;
            }
            let Some(&slot) = name_to_slot.get(import.name.as_bytes()) else {
                continue;
            };
            res.global_resolutions[i] = to_resolution(slot);
        }
    }
    Ok(())
}

pub(crate) fn finalize_got_import_resolutions(
    resolutions: &mut [ObjectImportResolutions],
    first_got: u32,
    take_slot: impl Fn(ImportResolution) -> Option<usize>,
) -> Result {
    for res in resolutions.iter_mut() {
        for resolution in &mut res.global_resolutions {
            let Some(slot) = take_slot(*resolution) else {
                continue;
            };
            let output_index = first_got
                .checked_add(slot as u32)
                .ok_or_else(|| crate::error!("Wasm global index overflow"))?;
            *resolution = ImportResolution::DirectGlobal { output_index };
        }
    }
    Ok(())
}

pub(crate) fn fill_got_mem_inits(
    layout: &mut WasmLayout<'_>,
    indices: &LinkerDefinedIndices,
    got_mem: &GotMem,
    data_start: u32,
    data_end: u32,
    stack_size: u32,
    heap_end: Option<u32>,
    stack_first: bool,
) -> Result {
    let Some(got_base) = indices.got_mem_global_base else {
        return Ok(());
    };
    let defined_slot = (got_base - indices.global_import_count) as usize;

    for (i, entry) in got_mem.entries.iter().enumerate() {
        let addr = match entry.def {
            GotMemDef::Object {
                object_index,
                symbol_offset,
            } => layout.object_index_maps[object_index]
                .data_addresses
                .get(symbol_offset)
                .copied()
                .ok_or_else(|| crate::error!("GOT.mem missing data address for definition"))?,
            GotMemDef::LinkerDefined(known) => known
                .data_address(data_start, data_end, stack_size, heap_end, stack_first)?
                .ok_or_else(|| {
                    crate::error!(
                        "GOT.mem linker-defined symbol `{}` has no data address",
                        std::str::from_utf8(known.name()).unwrap_or("?")
                    )
                })?,
        };
        let global_slot = defined_slot + i;
        let global = layout
            .globals
            .get_mut(global_slot)
            .ok_or_else(|| crate::error!("GOT.mem global slot {global_slot} out of range"))?;
        global.init_expr_body = Cow::Owned(encode_i32_const_u32(addr));
    }
    Ok(())
}

pub(crate) fn fill_exported_data_global_inits(
    layout: &mut WasmLayout<'_>,
    indices: &LinkerDefinedIndices,
    data_start: u32,
    data_end: u32,
    stack_size: u32,
    heap_end: Option<u32>,
    stack_first: bool,
) -> Result {
    for &(known, global_index) in &indices.data_address_globals {
        let addr = known
            .data_address(data_start, data_end, stack_size, heap_end, stack_first)?
            .ok_or_else(|| {
                crate::error!(
                    "linker-defined symbol `{}` has no address to export",
                    std::str::from_utf8(known.name()).unwrap_or("?")
                )
            })?;
        let defined_slot = (global_index - indices.global_import_count) as usize;
        let global = layout.globals.get_mut(defined_slot).ok_or_else(|| {
            crate::error!("exported data global slot {defined_slot} out of range")
        })?;
        global.init_expr_body = Cow::Owned(encode_i32_const_u32(addr));
    }
    Ok(())
}

/// Fill GOT.func globals with table indices. Requires the indirect function table first. Undefined
/// weak targets resolve through `function_indices` to unreachable stubs.
pub(crate) fn fill_got_func_inits(
    layout: &mut WasmLayout<'_>,
    indices: &LinkerDefinedIndices,
    got_func: &GotFunc,
    layout_inputs: &[WasmObjectLayoutInput<'_>],
) -> Result {
    let Some(got_base) = indices.got_func_global_base else {
        return Ok(());
    };
    let defined_slot = (got_base - indices.global_import_count) as usize;

    for (i, entry) in got_func.entries.iter().enumerate() {
        let input = layout_inputs.get(entry.object_index).ok_or_else(|| {
            crate::error!("GOT.func object index {} out of range", entry.object_index)
        })?;
        let sym = input.symbols.get(entry.symbol_offset).ok_or_else(|| {
            crate::error!(
                "GOT.func symbol offset {} out of range",
                entry.symbol_offset
            )
        })?;
        ensure!(
            sym.kind == WasmSymbolKind::Func,
            "GOT.func symbol is not a function"
        );
        let func_out = layout.object_index_maps[entry.object_index]
            .output_function_index(entry.symbol_offset, sym)?;
        let slot = layout
            .function_table_slots
            .get(func_out as usize)
            .copied()
            .unwrap_or(u32::MAX);
        ensure!(
            slot != u32::MAX,
            "GOT.func function {func_out} has no indirect table slot"
        );

        let global_slot = defined_slot + i;
        let global = layout
            .globals
            .get_mut(global_slot)
            .ok_or_else(|| crate::error!("GOT.func global slot {global_slot} out of range"))?;
        let table_i32 = i32::try_from(slot)
            .map_err(|_| crate::error!("GOT.func table index out of i32 range"))?;
        global.init_expr_body = Cow::Owned(encode_i32_const_body(table_i32));
    }
    Ok(())
}
