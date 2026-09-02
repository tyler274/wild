use super::DEFAULT_TABLE_BASE_INIT_EXPR;
use super::EMPTY_FUNCTION_BODY;
use super::LINKER_MEMORY_BASE;
use super::LINKER_MEMORY_BASE_INIT_EXPR;
use super::UNREACHABLE_FUNCTION_BODY;
use super::WASM_DEAD_INDEX;
use super::Wasm;
use super::ZERO_I32_INIT_EXPR;
use super::file::*;
use super::gc::*;
use super::output::*;
use super::relocations::*;
use super::symbols::*;
use crate::args::wasm::WasmArgs;
use crate::bail;
use crate::ensure;
use crate::error::Context as _;
use crate::error::Result;
use crate::input_data::PRELUDE_FILE_ID;
use crate::layout;
use crate::platform::Args as _;
use crate::symbol::UnversionedSymbolName;
use crate::symbol_db::SymbolDb;
use crate::symbol_db::SymbolId;
use crate::timing_phase;
use crate::verbose_timing_phase;
use crate::wasm_writer::OutputExport;
use crate::wasm_writer::OutputGlobal;
use crate::wasm_writer::OutputImportEntity;
use hashbrown::HashMap;
use hashbrown::HashSet;
use rayon::prelude::*;
use std::borrow::Cow;
use wasmparser::FuncType;
use wasmparser::GlobalType;
use wasmparser::MemoryType;
use wasmparser::RelocationType;

pub(crate) fn report_disallowed_unresolved_imports<'data>(
    inputs: &[WasmObjectLayoutInput<'data>],
    resolutions: &[ObjectImportResolutions],
    symbol_db: &SymbolDb<'data, Wasm>,
) -> Result {
    if symbol_db.args.allow_undefined {
        return Ok(());
    }

    let mut errors: Vec<String> = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();

    for (input, res) in inputs.iter().zip(resolutions.iter()) {
        let file_display = symbol_db.file(input.file_id).to_string();
        for (sym_offset, sym) in input.symbols.iter().enumerate() {
            if !sym.is_undefined() || sym.is_weak() || sym.is_explicit_name() {
                continue;
            }

            let is_unresolved = match sym.kind {
                WasmSymbolKind::Func => {
                    let idx = sym.index as usize;
                    input
                        .live_function_imports
                        .get(idx)
                        .copied()
                        .unwrap_or(false)
                        && res
                            .function_resolutions
                            .get(idx)
                            .is_some_and(|r| matches!(r, ImportResolution::Unresolved))
                }
                WasmSymbolKind::Global => {
                    let idx = sym.index as usize;
                    input.live_global_imports.get(idx).copied().unwrap_or(false)
                        && res
                            .global_resolutions
                            .get(idx)
                            .is_some_and(|r| matches!(r, ImportResolution::Unresolved))
                }
                _ => false,
            };
            if !is_unresolved {
                continue;
            }
            let Some(name) = wasm_symbol_name_str(input.data, sym) else {
                bail!(
                    "{file_display}: undefined symbol with no name (linking symbol index {sym_offset})"
                );
            };
            if !seen.insert((file_display.clone(), name.to_owned())) {
                continue;
            }
            errors.push(format!("{file_display}: undefined symbol: {name}"));
        }
    }

    if errors.is_empty() {
        return Ok(());
    }
    bail!("{}", errors.join("\n"));
}

pub(crate) fn collect_shared_unresolved_imports<'data>(
    inputs: &[WasmObjectLayoutInput<'data>],
    resolutions: &[ObjectImportResolutions],
) -> Result<SharedUnresolvedImports<'data>> {
    timing_phase!("Collect shared unresolved Wasm imports");

    let mut functions: Vec<SharedFunctionImport<'data>> = Vec::new();
    let mut globals: Vec<SharedGlobalImport<'data>> = Vec::new();
    let mut func_key_to_idx: HashMap<(&str, &str), u32> = HashMap::new();
    let mut global_key_to_idx: HashMap<(&str, &str), u32> = HashMap::new();
    let mut function_indices = Vec::with_capacity(inputs.len());
    let mut global_indices = Vec::with_capacity(inputs.len());

    for (obj_idx, (input, res)) in inputs.iter().zip(resolutions.iter()).enumerate() {
        let mut func_map = vec![None; input.function_imports.len()];
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
            let local_type_index = import.type_index;
            ensure!(
                (local_type_index as usize) < input.types.len(),
                "Wasm type index {local_type_index} out of range for import `{}`.`{}`",
                import.module,
                import.name
            );
            let key = (import.module, import.name);
            let shared_idx = if let Some(&idx) = func_key_to_idx.get(&key) {
                let existing = &functions[idx as usize];
                let existing_ty =
                    &inputs[existing.first_object].types[existing.local_type_index as usize];
                let this_ty = &input.types[local_type_index as usize];
                ensure!(
                    existing_ty == this_ty,
                    "conflicting types for import `{}`.`{}`",
                    import.module,
                    import.name
                );
                idx
            } else {
                let idx =
                    u32::try_from(functions.len()).context("too many Wasm function imports")?;
                functions.push(SharedFunctionImport {
                    module: import.module,
                    name: import.name,
                    first_object: obj_idx,
                    local_type_index,
                });
                func_key_to_idx.insert(key, idx);
                idx
            };
            func_map[i] = Some(shared_idx);
        }
        function_indices.push(func_map);

        let mut global_map = vec![None; input.global_imports.len()];
        for (i, import) in input.global_imports.iter().enumerate() {
            if !input.live_global_imports.get(i).copied().unwrap_or(false) {
                continue;
            }
            if !matches!(
                res.global_resolutions.get(i),
                Some(ImportResolution::Unresolved)
            ) {
                continue;
            }
            let key = (import.module, import.name);
            let shared_idx = if let Some(&idx) = global_key_to_idx.get(&key) {
                let existing = &globals[idx as usize];
                ensure!(
                    existing.ty == import.ty,
                    "conflicting types for import `{}`.`{}`",
                    import.module,
                    import.name
                );
                idx
            } else {
                let idx = u32::try_from(globals.len()).context("too many Wasm global imports")?;
                globals.push(SharedGlobalImport {
                    module: import.module,
                    name: import.name,
                    ty: import.ty,
                });
                global_key_to_idx.insert(key, idx);
                idx
            };
            global_map[i] = Some(shared_idx);
        }
        global_indices.push(global_map);
    }

    Ok(SharedUnresolvedImports {
        functions,
        globals,
        function_indices,
        global_indices,
    })
}

pub(crate) fn local_defined_function_index(
    input: &WasmObjectLayoutInput<'_>,
    sym: &WasmSymbol,
) -> Result<u32> {
    let original = sym.index - input.function_imports.len() as u32;
    let dense = input
        .defined_function_live_ordinal
        .get(original as usize)
        .copied()
        .unwrap_or(WASM_DEAD_INDEX);
    ensure!(
        dense != WASM_DEAD_INDEX,
        "reference to GC'd Wasm defined function {original}"
    );
    Ok(dense)
}

pub(crate) fn local_defined_global_index(
    input: &WasmObjectLayoutInput<'_>,
    sym: &WasmSymbol,
) -> Result<u32> {
    let original = sym.index - input.global_imports.len() as u32;
    let dense = input
        .defined_global_live_ordinal
        .get(original as usize)
        .copied()
        .unwrap_or(WASM_DEAD_INDEX);
    ensure!(
        dense != WASM_DEAD_INDEX,
        "reference to GC'd Wasm defined global {original}"
    );
    Ok(dense)
}

/// Resolve cross-object imports. For each object's undefined function/global symbol, checks whether
/// `SymbolDb::definition()` points to a defined symbol. Resolutions are keyed by import ordinal
/// (`sym.index`), not symbol-table order.
pub(crate) fn resolve_cross_object_imports<'data>(
    inputs: &[WasmObjectLayoutInput<'data>],
    symbol_db: &crate::symbol_db::SymbolDb<'data, Wasm>,
    file_id_to_index: &HashMap<crate::input_data::FileId, usize>,
) -> Result<Vec<ObjectImportResolutions>> {
    timing_phase!("Resolve Wasm cross-object imports");

    inputs
        .par_iter()
        .map(|input| {
            verbose_timing_phase!("Resolve Wasm object imports");
            let function_resolutions = resolve_import_symbols(
                input.function_imports.len(),
                WasmSymbolKind::Func,
                input,
                inputs,
                symbol_db,
                file_id_to_index,
            )?;
            let global_resolutions = resolve_import_symbols(
                input.global_imports.len(),
                WasmSymbolKind::Global,
                input,
                inputs,
                symbol_db,
                file_id_to_index,
            )?;
            Ok(ObjectImportResolutions {
                function_resolutions,
                global_resolutions,
            })
        })
        .collect()
}

pub(crate) fn resolve_import_symbols<'data>(
    import_count: usize,
    kind: WasmSymbolKind,
    input: &WasmObjectLayoutInput<'data>,
    all_inputs: &[WasmObjectLayoutInput<'data>],
    symbol_db: &crate::symbol_db::SymbolDb<'data, Wasm>,
    file_id_to_index: &HashMap<crate::input_data::FileId, usize>,
) -> Result<Vec<ImportResolution>> {
    ensure!(u32::try_from(import_count).is_ok(), "too many Wasm imports");
    let mut resolutions = vec![ImportResolution::Unresolved; import_count];

    let live_imports = match kind {
        WasmSymbolKind::Func => input.live_function_imports.as_slice(),
        WasmSymbolKind::Global => input.live_global_imports.as_slice(),
        _ => &[],
    };

    for (sym_offset, sym) in input.symbols.iter().enumerate() {
        if !sym.is_undefined() || sym.kind != kind {
            continue;
        }
        let import_idx = sym.index as usize;
        if import_idx >= import_count {
            continue;
        }
        // Dead import slots are not emitted.
        if !live_imports.get(import_idx).copied().unwrap_or(false) {
            continue;
        }
        let resolution = resolve_one_import(
            sym_offset,
            kind,
            input,
            all_inputs,
            symbol_db,
            file_id_to_index,
        )?;
        if matches!(resolutions[import_idx], ImportResolution::Unresolved)
            && !matches!(resolution, ImportResolution::Unresolved)
        {
            resolutions[import_idx] = resolution;
        }
    }

    let import_names: Vec<&str> = match kind {
        WasmSymbolKind::Func => input.function_imports.iter().map(|i| i.name).collect(),
        WasmSymbolKind::Global => input.global_imports.iter().map(|i| i.name).collect(),
        _ => Vec::new(),
    };
    for (import_idx, name) in import_names.iter().enumerate() {
        if !live_imports.get(import_idx).copied().unwrap_or(false) {
            continue;
        }
        if !matches!(resolutions[import_idx], ImportResolution::Unresolved) {
            continue;
        }
        if let Some(resolution) = linker_defined_import_resolution(name, kind, symbol_db) {
            resolutions[import_idx] = resolution;
        }
    }

    Ok(resolutions)
}

/// Try to resolve a single undefined import symbol.
pub(crate) fn resolve_one_import<'data>(
    sym_offset: usize,
    expected_kind: WasmSymbolKind,
    input: &WasmObjectLayoutInput<'data>,
    all_inputs: &[WasmObjectLayoutInput<'data>],
    symbol_db: &crate::symbol_db::SymbolDb<'data, Wasm>,
    file_id_to_index: &HashMap<crate::input_data::FileId, usize>,
) -> Result<ImportResolution> {
    let symbol_id = input.symbol_id_range.offset_to_id(sym_offset);
    let def_id = symbol_db.definition(symbol_id);
    if def_id == symbol_id {
        return Ok(ImportResolution::Unresolved);
    }
    let def_file_id = symbol_db.file_id_for_symbol(def_id);

    if def_file_id == PRELUDE_FILE_ID {
        return Ok(
            linker_defined_from_prelude_def(def_id, expected_kind, symbol_db)
                .unwrap_or(ImportResolution::Unresolved),
        );
    }

    let Some(&def_obj_idx) = file_id_to_index.get(&def_file_id) else {
        return Ok(ImportResolution::Unresolved);
    };
    let def_input = &all_inputs[def_obj_idx];
    let def_sym = &def_input.symbols[def_input.symbol_id_range.id_to_offset(def_id)];
    if def_sym.is_undefined() || def_sym.kind != expected_kind {
        return Ok(ImportResolution::Unresolved);
    }
    match expected_kind {
        WasmSymbolKind::Func => {
            ensure!(
                def_sym.index >= def_input.function_imports.len() as u32,
                "defined Wasm function symbol index {} is within import range",
                def_sym.index
            );
            Ok(ImportResolution::ResolvedFunction {
                object_index: def_obj_idx,
                local_defined_index: local_defined_function_index(def_input, def_sym)?,
            })
        }
        WasmSymbolKind::Global => {
            ensure!(
                def_sym.index >= def_input.global_imports.len() as u32,
                "defined Wasm global symbol index {} is within import range",
                def_sym.index
            );
            Ok(ImportResolution::ResolvedGlobal {
                object_index: def_obj_idx,
                local_defined_index: local_defined_global_index(def_input, def_sym)?,
            })
        }
        _ => Ok(ImportResolution::Unresolved),
    }
}

pub(crate) fn linker_defined_from_prelude_def(
    def_id: crate::symbol_db::SymbolId,
    expected_kind: WasmSymbolKind,
    symbol_db: &SymbolDb<'_, Wasm>,
) -> Option<ImportResolution> {
    let def_info = symbol_db.prelude_symbol_def(def_id)?;
    let crate::parsing::SymbolPlacement::PlatformSpecific(known) = &def_info.placement else {
        return None;
    };
    known
        .matches_import_kind(expected_kind)
        .then_some(ImportResolution::LinkerDefined(*known))
}

/// Resolve an import name to a prelude platform-specific definition, if present in `SymbolDb`.
pub(crate) fn linker_defined_import_resolution(
    import_name: &str,
    expected_kind: WasmSymbolKind,
    symbol_db: &SymbolDb<'_, Wasm>,
) -> Option<ImportResolution> {
    let symbol_id =
        symbol_db.get_unversioned(&UnversionedSymbolName::prehashed(import_name.as_bytes()))?;
    let def_id = symbol_db.definition(symbol_id);
    linker_defined_from_prelude_def(def_id, expected_kind, symbol_db)
}

pub(crate) fn object_needs_linker_memory(input: &WasmObjectLayoutInput<'_>) -> bool {
    !input.memory_imports.is_empty() || !input.memories.is_empty()
}

pub(crate) fn any_object_needs_linker_memory(inputs: &[WasmObjectLayoutInput<'_>]) -> bool {
    inputs.iter().any(object_needs_linker_memory)
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct LinkerImportAbsorption {
    pub(crate) needs_memory_base: bool,
    pub(crate) needs_table_base: bool,
    pub(crate) needs_stack_pointer: bool,
    pub(crate) needs_tls_base: bool,
    pub(crate) needs_ctors: bool,
}

impl LinkerImportAbsorption {
    pub(crate) fn need(&mut self, known: WasmLinkerSymbol) {
        match known {
            WasmLinkerSymbol::CallCtors => self.needs_ctors = true,
            WasmLinkerSymbol::MemoryBase => self.needs_memory_base = true,
            WasmLinkerSymbol::TableBase => self.needs_table_base = true,
            WasmLinkerSymbol::StackPointer => self.needs_stack_pointer = true,
            // Single-threaded. Immutable base (no TLS segment yet).
            WasmLinkerSymbol::TlsBase => self.needs_tls_base = true,
            _ => {}
        }
    }

    pub(crate) fn from_resolutions(
        resolutions: &ObjectImportResolutions,
        live_function_imports: &[bool],
        live_global_imports: &[bool],
    ) -> Self {
        let mut absorption = Self::default();
        for (i, resolution) in resolutions.function_resolutions.iter().enumerate() {
            if !live_function_imports.get(i).copied().unwrap_or(false) {
                continue;
            }
            if let ImportResolution::LinkerDefined(known) = *resolution {
                absorption.need(known);
            }
        }
        for (i, resolution) in resolutions.global_resolutions.iter().enumerate() {
            if !live_global_imports.get(i).copied().unwrap_or(false) {
                continue;
            }
            if let ImportResolution::LinkerDefined(known) = *resolution {
                absorption.need(known);
            }
        }
        absorption
    }
}

/// Synthetic function produced for an unresolved weak function import.
#[derive(Debug, Clone)]
pub(crate) struct WeakUndefFunctionStub {
    pub(crate) ty: wasmparser::FuncType,
    pub(crate) function_index: u32,
}

/// Reserved Wasm index-space slots for linker-defined globals/functions.
#[derive(Debug, Clone, Default)]
pub(crate) struct LinkerDefinedIndices {
    pub(crate) memory_base_global: Option<u32>,
    pub(crate) table_base_global: Option<u32>,
    pub(crate) stack_pointer_global: Option<u32>,
    pub(crate) tls_base_global: Option<u32>,
    /// Index of `__stack_pointer` among the defined globals prepended by
    /// `emit_reserved_linker_definitions` (not the Wasm module global index).
    pub(crate) stack_pointer_defined_slot: Option<u32>,
    pub(crate) call_ctors_func: Option<u32>,
    pub(crate) entry_wrapper_func: Option<u32>,
    pub(crate) weak_undef_stubs: Vec<WeakUndefFunctionStub>,
    /// Linker-defined globals including GOT.mem.
    pub(crate) num_defined_globals: u32,
    pub(crate) num_defined_functions: u32,
    /// Unresolved host global imports.
    pub(crate) global_import_count: u32,
    /// First module global index for GOT.mem entries.
    pub(crate) got_mem_global_base: Option<u32>,
    pub(crate) got_mem_count: u32,
    /// First module global index for GOT.func entries.
    pub(crate) got_func_global_base: Option<u32>,
    pub(crate) got_func_count: u32,
    pub(crate) data_address_globals: Vec<(WasmLinkerSymbol, u32)>,
    // Linker symbols named by `--export` / `--export-if-defined`.
    pub(crate) requested_exports: Vec<WasmLinkerSymbol>,
    // `i32.const` for `__memory_base` when `memory_base_global` is set.
    pub(crate) memory_base_init: u32,
}

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
                undefined_data_errors.push(format!("{}: undefined symbol: {}", key.0, key.1));
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
) -> String {
    let sym_name = layout_inputs.get(entry.object_index).and_then(|input| {
        input
            .symbols
            .get(entry.symbol_offset)
            .and_then(|sym| wasm_symbol_name_str(input.data, sym))
    });
    match sym_name {
        Some(name) => format!("GOT.func.internal.{name}"),
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

/// True if inputs import or export `__wasm_call_ctors`.
pub(crate) fn call_ctors_used_in_objects(inputs: &[WasmObjectLayoutInput<'_>]) -> bool {
    inputs.iter().any(|input| {
        input.function_imports.iter().any(|imp| {
            matches!(
                WasmLinkerSymbol::parse(imp.name),
                Some(WasmLinkerSymbol::CallCtors)
            )
        }) || input.exports.iter().any(|exp| {
            matches!(
                WasmLinkerSymbol::parse(exp.name),
                Some(WasmLinkerSymbol::CallCtors)
            )
        })
    })
}

pub(crate) fn entry_is_defined_function(
    layout_inputs: &[WasmObjectLayoutInput<'_>],
    symbol_db: &SymbolDb<'_, Wasm>,
    file_id_to_index: &HashMap<crate::input_data::FileId, usize>,
) -> bool {
    let Some(entry_name) = symbol_db.entry_symbol_name() else {
        return false;
    };
    let Some(symbol_id) = symbol_db.get_unversioned(&UnversionedSymbolName::prehashed(entry_name))
    else {
        return false;
    };
    let def_id = symbol_db.definition(symbol_id);
    let def_file_id = symbol_db.file_id_for_symbol(def_id);
    let Some(&obj_idx) = file_id_to_index.get(&def_file_id) else {
        return false;
    };
    let input = &layout_inputs[obj_idx];
    let sym = &input.symbols[input.symbol_id_range.id_to_offset(def_id)];
    !sym.is_undefined() && sym.kind == WasmSymbolKind::Func
}

pub(crate) struct LinkerDefinedIndexRequest {
    pub(crate) has_init_funcs: bool,
    // Linker symbols named by `--export` / `--export-if-defined`.
    pub(crate) export_symbols: Vec<WasmLinkerSymbol>,
    pub(crate) has_memory: bool,
    pub(crate) wrap_entry: bool,
    pub(crate) got_mem_count: u32,
    pub(crate) got_func_count: u32,
    pub(crate) needs_memory_base: bool,
    pub(crate) needs_table_base: bool,
}

impl LinkerDefinedIndices {
    pub(crate) fn compute(
        layout_inputs: &[WasmObjectLayoutInput<'_>],
        import_resolutions: &[ObjectImportResolutions],
        function_import_count: u32,
        global_import_count: u32,
        mut weak_undef_stubs: Vec<WeakUndefFunctionStub>,
        request: &LinkerDefinedIndexRequest,
    ) -> Result<Self> {
        let mut needs_memory_base = request.needs_memory_base;
        let mut needs_table_base = request.needs_table_base;
        // wasm-ld always defines `__stack_pointer` for non-PIC executables.
        let mut needs_stack_pointer = true;
        let mut needs_tls_base = false;
        let mut needs_ctors = request.has_init_funcs;
        let mut export_data = Vec::new();
        let mut export_needs = LinkerImportAbsorption::default();
        for &sym in &request.export_symbols {
            if !sym.materialize_on_export() {
                continue;
            }
            if sym.exported_as_data_global(request.has_memory) {
                if !export_data.contains(&sym) {
                    export_data.push(sym);
                }
            } else {
                export_needs.need(sym);
            }
        }
        needs_ctors |= export_needs.needs_ctors;
        needs_table_base |= export_needs.needs_table_base;
        needs_stack_pointer |= export_needs.needs_stack_pointer;
        let export_memory_base = export_needs.needs_memory_base;

        for (input, resolutions) in layout_inputs.iter().zip(import_resolutions.iter()) {
            let absorption = LinkerImportAbsorption::from_resolutions(
                resolutions,
                &input.live_function_imports,
                &input.live_global_imports,
            );
            needs_ctors |= absorption.needs_ctors;
            needs_memory_base |= absorption.needs_memory_base;
            needs_table_base |= absorption.needs_table_base;
            needs_stack_pointer |= absorption.needs_stack_pointer;
            needs_tls_base |= absorption.needs_tls_base;
        }
        let memory_base_init = if needs_memory_base {
            LINKER_MEMORY_BASE
        } else {
            0
        };
        needs_memory_base |= export_memory_base;

        let mut next_global = global_import_count;
        // Defined-global slot before `__stack_pointer` (used for its init expression).
        let stack_pointer_defined_slot = needs_stack_pointer
            .then_some(u32::from(needs_memory_base) + u32::from(needs_table_base));
        let memory_base_global = needs_memory_base.then(|| {
            let idx = next_global;
            next_global += 1;
            idx
        });
        let table_base_global = needs_table_base.then(|| {
            let idx = next_global;
            next_global += 1;
            idx
        });
        let stack_pointer_global = needs_stack_pointer.then(|| {
            let idx = next_global;
            next_global += 1;
            idx
        });
        let tls_base_global = needs_tls_base.then(|| {
            let idx = next_global;
            next_global += 1;
            idx
        });
        let mut data_address_globals = Vec::with_capacity(export_data.len());
        for known in export_data {
            data_address_globals.push((known, next_global));
            next_global = next_global
                .checked_add(1)
                .ok_or_else(|| crate::error!("Wasm global index overflow"))?;
        }
        let got_mem_global_base = if request.got_mem_count > 0 {
            let base = next_global;
            next_global = next_global
                .checked_add(request.got_mem_count)
                .ok_or_else(|| crate::error!("Wasm global index overflow"))?;
            Some(base)
        } else {
            None
        };
        let got_func_global_base = if request.got_func_count > 0 {
            let base = next_global;
            next_global = next_global
                .checked_add(request.got_func_count)
                .ok_or_else(|| crate::error!("Wasm global index overflow"))?;
            Some(base)
        } else {
            None
        };
        let num_defined_globals = next_global - global_import_count;

        let mut next_func = function_import_count;
        let call_ctors_func = needs_ctors.then(|| {
            let idx = next_func;
            next_func += 1;
            idx
        });
        let entry_wrapper_func = request.wrap_entry.then(|| {
            let idx = next_func;
            next_func += 1;
            idx
        });
        for stub in &mut weak_undef_stubs {
            stub.function_index = next_func;
            next_func = next_func
                .checked_add(1)
                .ok_or_else(|| crate::error!("Wasm function index overflow"))?;
        }
        let num_defined_functions = next_func - function_import_count;

        Ok(Self {
            memory_base_global,
            table_base_global,
            stack_pointer_global,
            tls_base_global,
            stack_pointer_defined_slot,
            call_ctors_func,
            entry_wrapper_func,
            weak_undef_stubs,
            num_defined_globals,
            num_defined_functions,
            global_import_count,
            got_mem_global_base,
            got_mem_count: request.got_mem_count,
            got_func_global_base,
            got_func_count: request.got_func_count,
            data_address_globals,
            requested_exports: request.export_symbols.clone(),
            memory_base_init,
        })
    }

    pub(crate) fn global_index(&self, known: WasmLinkerSymbol) -> Option<u32> {
        match known {
            WasmLinkerSymbol::MemoryBase => self.memory_base_global,
            WasmLinkerSymbol::TableBase => self.table_base_global,
            WasmLinkerSymbol::StackPointer => self.stack_pointer_global,
            WasmLinkerSymbol::TlsBase => self.tls_base_global,
            other => self
                .data_address_globals
                .iter()
                .find(|(sym, _)| *sym == other)
                .map(|(_, idx)| *idx),
        }
    }

    pub(crate) fn function_index(&self, known: WasmLinkerSymbol) -> Option<u32> {
        match known {
            WasmLinkerSymbol::CallCtors => self.call_ctors_func,
            _ => None,
        }
    }
}

pub(crate) fn encode_i32_const_body(value: i32) -> Vec<u8> {
    let mut bytes = vec![0x41];
    leb128::write::signed(&mut bytes, i64::from(value)).unwrap();
    bytes
}

/// Encode a linear-memory address as Wasm `i32.const`.
pub(crate) fn encode_i32_const_u32(value: u32) -> Vec<u8> {
    encode_i32_const_body(value as i32)
}

pub(crate) fn ensure_void_void_type(types: &mut Vec<wasmparser::FuncType>) -> u32 {
    ensure_func_type(types, &wasmparser::FuncType::new([], []))
}

pub(crate) fn ensure_func_type(
    types: &mut Vec<wasmparser::FuncType>,
    ty: &wasmparser::FuncType,
) -> u32 {
    if let Some((idx, _)) = types
        .iter()
        .enumerate()
        .find(|(_, existing)| *existing == ty)
    {
        return idx as u32;
    }
    types.push(ty.clone());
    (types.len() - 1) as u32
}

/// Collapse identical function types in the output type section and rewrite every type index that
/// refers into it.
pub(crate) fn deduplicate_output_types(layout: &mut WasmLayout<'_>) {
    if layout.output_types.is_empty() {
        return;
    }

    let mut unique_types = Vec::with_capacity(layout.output_types.len());
    let mut type_to_new_index: HashMap<FuncType, u32> = HashMap::new();
    let mut old_to_new = Vec::with_capacity(layout.output_types.len());

    for ty in std::mem::take(&mut layout.output_types) {
        if let Some(&new_index) = type_to_new_index.get(&ty) {
            old_to_new.push(new_index);
            continue;
        }
        let new_index = u32::try_from(unique_types.len()).expect("too many Wasm types");
        type_to_new_index.insert(ty.clone(), new_index);
        unique_types.push(ty);
        old_to_new.push(new_index);
    }

    layout.output_types = unique_types;

    if old_to_new
        .iter()
        .enumerate()
        .all(|(old, &new)| old as u32 == new)
    {
        // No remapping required.
        return;
    }

    let remap = |index: u32| -> u32 {
        old_to_new
            .get(index as usize)
            .copied()
            .expect("type index out of range during dedup")
    };

    for type_index in &mut layout.function_type_indices {
        *type_index = remap(*type_index);
    }

    for import in &mut layout.imports {
        if let OutputImportEntity::Function { type_index } = &mut import.entity {
            *type_index = remap(*type_index);
        }
    }

    for index_map in &mut layout.object_index_maps {
        for type_index in &mut index_map.type_indices {
            *type_index = remap(*type_index);
        }
    }
}

pub(crate) fn borrowed_linker_function_body(bytes: &'static [u8]) -> WasmFunctionBody<'static> {
    WasmFunctionBody {
        bytes: Cow::Borrowed(bytes),
        code_offset: 0,
        reloc_range: 0..0,
        object_index: 0,
    }
}

pub(crate) fn empty_linker_function_body() -> WasmFunctionBody<'static> {
    borrowed_linker_function_body(EMPTY_FUNCTION_BODY)
}

pub(crate) fn unreachable_linker_function_body() -> WasmFunctionBody<'static> {
    borrowed_linker_function_body(UNREACHABLE_FUNCTION_BODY)
}

pub(crate) fn owned_linker_function_body(bytes: Vec<u8>) -> WasmFunctionBody<'static> {
    WasmFunctionBody {
        bytes: Cow::Owned(bytes),
        code_offset: 0,
        reloc_range: 0..0,
        object_index: 0,
    }
}

/// Encode a body that calls each function in order.
///
/// `calls` is `(function_index, result_count)`. Result values are dropped so that
/// `__wasm_call_ctors` can stay `() -> ()` even when a constructor returns a value.
pub(crate) fn encode_call_sequence_body(calls: &[(u32, usize)]) -> Vec<u8> {
    let mut bytes = vec![0x00]; // 0 locals
    for &(func_index, result_count) in calls {
        bytes.push(0x10); // call
        leb128::write::unsigned(&mut bytes, u64::from(func_index))
            .expect("leb128 write to Vec cannot fail");
        bytes.extend(std::iter::repeat_n(0x1a, result_count)); // drop each result
    }
    bytes.push(0x0b); // end
    bytes
}

pub(crate) fn function_type_for_symbol<'a>(
    input: &'a WasmObjectLayoutInput<'_>,
    sym: &WasmSymbol,
) -> Result<&'a wasmparser::FuncType> {
    let sym_index = sym.index as usize;
    let n_imports = input.function_imports.len();
    let type_index = if sym_index < n_imports {
        input.function_imports[sym_index].type_index
    } else {
        let original = sym_index - n_imports;
        let dense = input
            .defined_function_live_ordinal
            .get(original)
            .copied()
            .unwrap_or(WASM_DEAD_INDEX);
        ensure!(
            dense != WASM_DEAD_INDEX,
            "Wasm init/reference to GC'd function index {}",
            sym.index
        );
        *input.module_functions.get(dense as usize).ok_or_else(|| {
            crate::error!(
                "Wasm function index {} out of range (dense {dense}, live len {})",
                sym.index,
                input.module_functions.len()
            )
        })?
    };
    input
        .types
        .get(type_index as usize)
        .ok_or_else(|| crate::error!("Wasm type index {type_index} out of range"))
}

/// From InitFuncs to `(output function index, result count)`, sorted by ascending priority.
pub(crate) fn collect_sorted_init_function_calls(
    inputs: &[WasmObjectLayoutInput<'_>],
    object_index_maps: &[WasmObjectIndexMap],
) -> Result<Vec<(u32, usize)>> {
    let mut items = Vec::new();
    for (obj_idx, input) in inputs.iter().enumerate() {
        let index_map = &object_index_maps[obj_idx];
        for init in input.init_funcs {
            let sym = &input.symbols[init.symbol_index as usize];
            ensure!(
                sym.kind == WasmSymbolKind::Func && !sym.is_undefined(),
                "Wasm init function must be a defined function symbol"
            );
            let ty = function_type_for_symbol(input, sym)?;
            ensure!(
                ty.params().is_empty(),
                "Wasm constructor must take no parameters (got {} param(s))",
                ty.params().len()
            );
            let output_index = index_map.output_function_index(init.symbol_index as usize, sym)?;
            items.push((init.priority, output_index, ty.results().len()));
        }
    }
    items.sort_by_key(|(priority, _, _)| *priority);
    Ok(items
        .into_iter()
        .map(|(_, index, n_results)| (index, n_results))
        .collect())
}

pub(crate) fn push_i32_global<'data>(
    dst: &mut Vec<OutputGlobal<'data>>,
    mutable: bool,
    init_expr_body: Cow<'data, [u8]>,
) {
    dst.push(OutputGlobal {
        ty: GlobalType {
            content_type: wasmparser::ValType::I32,
            mutable,
            shared: false,
        },
        init_expr_body,
    });
}

pub(crate) fn emit_reserved_linker_definitions(
    layout: &mut WasmLayout<'_>,
    indices: &LinkerDefinedIndices,
    call_ctors_body: Option<Vec<u8>>,
    entry_wrapper_body: Option<Vec<u8>>,
) {
    let mut linker_globals = Vec::with_capacity(indices.num_defined_globals as usize);
    if indices.memory_base_global.is_some() {
        push_i32_global(
            &mut linker_globals,
            false,
            Cow::Owned(encode_i32_const_u32(indices.memory_base_init)),
        );
    }
    if indices.table_base_global.is_some() {
        push_i32_global(
            &mut linker_globals,
            false,
            Cow::Borrowed(DEFAULT_TABLE_BASE_INIT_EXPR),
        );
    }
    if indices.stack_pointer_global.is_some() {
        push_i32_global(
            &mut linker_globals,
            true,
            Cow::Borrowed(LINKER_MEMORY_BASE_INIT_EXPR),
        );
    }
    if indices.tls_base_global.is_some() {
        push_i32_global(
            &mut linker_globals,
            false,
            Cow::Borrowed(ZERO_I32_INIT_EXPR),
        );
    }
    for _ in &indices.data_address_globals {
        push_i32_global(
            &mut linker_globals,
            false,
            Cow::Borrowed(ZERO_I32_INIT_EXPR),
        );
    }
    // GOT.mem placeholders. wasm-ld emits static GOT.data.internal.* as immutable i32 for
    // freestanding executables.
    for _ in 0..indices.got_mem_count {
        push_i32_global(
            &mut linker_globals,
            false,
            Cow::Borrowed(ZERO_I32_INIT_EXPR),
        );
    }
    // GOT.func placeholders. Filled with table indices after the indirect table is finalized.
    for _ in 0..indices.got_func_count {
        push_i32_global(
            &mut linker_globals,
            false,
            Cow::Borrowed(ZERO_I32_INIT_EXPR),
        );
    }
    if !linker_globals.is_empty() {
        let mut rest = std::mem::take(&mut layout.globals);
        linker_globals.append(&mut rest);
        layout.globals = linker_globals;
    }

    if indices.num_defined_functions > 0 {
        let void_ty = ensure_void_void_type(&mut layout.output_types);
        let mut type_indices = Vec::with_capacity(indices.num_defined_functions as usize);
        let mut bodies = Vec::with_capacity(indices.num_defined_functions as usize);
        if indices.call_ctors_func.is_some() {
            type_indices.push(void_ty);
            bodies.push(match call_ctors_body {
                Some(bytes) => owned_linker_function_body(bytes),
                None => empty_linker_function_body(),
            });
        }
        if indices.entry_wrapper_func.is_some() {
            type_indices.push(void_ty);
            bodies.push(match entry_wrapper_body {
                Some(bytes) => owned_linker_function_body(bytes),
                None => empty_linker_function_body(),
            });
        }
        for stub in &indices.weak_undef_stubs {
            type_indices.push(ensure_func_type(&mut layout.output_types, &stub.ty));
            bodies.push(unreachable_linker_function_body());
        }
        type_indices.append(&mut layout.function_type_indices);

        let mut object_bodies = std::mem::take(&mut layout.function_bodies);
        bodies.append(&mut object_bodies);
        layout.function_type_indices = type_indices;
        layout.function_bodies = bodies;
    }
}

pub(crate) const fn wasm_page_size() -> u64 {
    crate::args::wasm::WASM_PAGE_SIZE
}

/// Size of the wasm32 linear-memory address space.
pub(crate) const WASM32_ADDRESS_SPACE_BYTES: u64 = 1u64 << 32;

/// Largest initial memory size.
pub(crate) const WASM32_MAX_INITIAL_MEMORY_BYTES: u64 =
    WASM32_ADDRESS_SPACE_BYTES - wasm_page_size();

pub(crate) fn ensure_memory_covers(
    layout: &mut WasmLayout<'_>,
    stack_size: u32,
    stack_first: bool,
    initial_memory: Option<u64>,
    max_memory: Option<u64>,
    shared_memory: bool,
) -> Result<u64> {
    let page = wasm_page_size();
    let mut bytes_needed = u64::from(layout.data_end.max(layout.memory_base));
    if !stack_first && stack_size > 0 {
        bytes_needed = bytes_needed.max(u64::from(stack_high_after_data(
            layout.data_end,
            stack_size,
        )?));
    }

    if let Some(requested) = initial_memory {
        ensure!(
            requested.is_multiple_of(page),
            "initial memory must be aligned to the page size ({page} bytes)"
        );
        ensure!(
            requested <= WASM32_MAX_INITIAL_MEMORY_BYTES,
            "initial memory too large, cannot be greater than {WASM32_MAX_INITIAL_MEMORY_BYTES}"
        );
        ensure!(
            bytes_needed <= requested,
            "initial memory too small, {bytes_needed} bytes needed"
        );
        bytes_needed = requested;
    }

    if !layout.memories.is_empty() && bytes_needed > 0 {
        let pages_needed = bytes_needed.div_ceil(page).max(1);
        for memory in &mut layout.memories {
            memory.initial = memory.initial.max(pages_needed);
        }
    }

    let initial_pages = layout.memories.iter().map(|m| m.initial).max().unwrap_or(0);
    let initial_bytes = initial_pages.saturating_mul(page);

    if let Some(requested) = max_memory {
        ensure!(
            requested.is_multiple_of(page),
            "maximum memory must be aligned to the page size ({page} bytes)"
        );
        ensure!(
            requested <= WASM32_ADDRESS_SPACE_BYTES,
            "maximum memory too large, cannot be greater than {WASM32_ADDRESS_SPACE_BYTES}"
        );
        ensure!(
            initial_bytes <= requested,
            "maximum memory too small, {initial_bytes} bytes needed"
        );
        let max_pages = requested / page;
        for memory in &mut layout.memories {
            memory.maximum = Some(max_pages);
        }
    }

    if shared_memory {
        for memory in &mut layout.memories {
            memory.shared = true;
            // Shared memories must declare a maximum. Default to the final initial size.
            let max = memory.maximum.unwrap_or(memory.initial).max(memory.initial);
            memory.maximum = Some(max);
        }
    }

    Ok(initial_pages)
}

/// `__heap_end` = end of initial linear memory (`memory.initial * page_size`).
pub(crate) fn heap_end_from_initial_pages(initial_pages: u64) -> Result<u32> {
    u32::try_from(initial_pages.saturating_mul(wasm_page_size()))
        .map_err(|_| crate::error!("Wasm initial memory size overflow"))
}

/// Write stack-pointer init after static data layout.
pub(crate) fn fill_stack_pointer_init(
    layout: &mut WasmLayout<'_>,
    indices: &LinkerDefinedIndices,
    stack_size: u32,
    stack_first: bool,
) -> Result {
    let Some(defined_slot) = indices.stack_pointer_defined_slot else {
        return Ok(());
    };
    let sp = stack_pointer_init(layout.data_end, stack_size, stack_first)?;
    let global = layout
        .globals
        .get_mut(defined_slot as usize)
        .ok_or_else(|| crate::error!("Wasm stack pointer global missing"))?;
    ensure!(
        global.ty.mutable && global.ty.content_type == wasmparser::ValType::I32,
        "Wasm stack pointer global has unexpected type"
    );
    global.init_expr_body = Cow::Owned(encode_i32_const_u32(sp));
    Ok(())
}

pub(crate) fn linker_output_memory_type(
    inputs: &[WasmObjectLayoutInput<'_>],
    shared: bool,
) -> MemoryType {
    let mut initial = 2u64;
    for input in inputs {
        for import in &input.memory_imports {
            initial = initial.max(import.initial);
        }
        for memory in &input.memories {
            initial = initial.max(memory.initial);
        }
    }
    MemoryType {
        memory64: false,
        shared,
        initial,
        maximum: None,
        page_size_log2: None,
    }
}

pub(crate) fn ensure_memory_export<'data>(
    exports: &mut Vec<OutputExport<'data>>,
    name: &'data str,
) {
    exports.retain(|export| !matches!(export.kind, wasmparser::ExternalKind::Memory));
    exports.push(OutputExport {
        name,
        kind: wasmparser::ExternalKind::Memory,
        index: 0,
    });
}

pub(crate) fn export_name_exists(exports: &[OutputExport<'_>], name: &str) -> bool {
    exports.iter().any(|export| export.name == name)
}

pub(crate) fn push_export<'data>(
    exports: &mut Vec<OutputExport<'data>>,
    name: &'data str,
    kind: wasmparser::ExternalKind,
    index: u32,
) {
    if export_name_exists(exports, name) {
        return;
    }
    exports.push(OutputExport { name, kind, index });
}

/// Resolved user entry function, if present among the linked objects.
pub(crate) struct ResolvedEntry<'data> {
    pub(crate) export_name: &'data str,
    pub(crate) function_index: u32,
}

/// Find the defined user entry function (default `_start`) without exporting it yet.
pub(crate) fn resolve_entry_function<'data>(
    layout_inputs: &[WasmObjectLayoutInput<'data>],
    object_index_maps: &[WasmObjectIndexMap],
    symbol_db: &SymbolDb<'data, Wasm>,
    file_id_to_index: &HashMap<crate::input_data::FileId, usize>,
) -> Result<Option<ResolvedEntry<'data>>> {
    let Some(entry_name_bytes) = symbol_db.entry_symbol_name() else {
        return Ok(None);
    };
    let entry_display = String::from_utf8_lossy(entry_name_bytes);
    let not_defined =
        || crate::error!("entry symbol not defined (pass --no-entry to suppress): {entry_display}");

    let Some(symbol_id) =
        symbol_db.get_unversioned(&UnversionedSymbolName::prehashed(entry_name_bytes))
    else {
        return Err(not_defined());
    };
    let def_id = symbol_db.definition(symbol_id);
    let def_file_id = symbol_db.file_id_for_symbol(def_id);

    let Some(&def_obj_idx) = file_id_to_index.get(&def_file_id) else {
        return Err(not_defined());
    };
    let def_input = &layout_inputs[def_obj_idx];
    let def_sym = &def_input.symbols[def_input.symbol_id_range.id_to_offset(def_id)];
    if def_sym.is_undefined() || def_sym.kind != WasmSymbolKind::Func {
        return Err(not_defined());
    }

    let index_map = object_index_maps
        .get(def_obj_idx)
        .context("missing Wasm object index map for entry symbol definition")?;
    let function_index = remap_wasm_index(&index_map.function_indices, def_sym.index, "function")?;
    let export_name = core::str::from_utf8(symbol_db.symbol_name(def_id)?.bytes())
        .context("invalid UTF-8 in Wasm entry symbol name")?;
    Ok(Some(ResolvedEntry {
        export_name,
        function_index,
    }))
}

/// Export the command entry (default `_start`). With a wrapper, retarget any existing export.
pub(crate) fn ensure_entry_export<'data>(
    exports: &mut Vec<OutputExport<'data>>,
    entry: Option<&ResolvedEntry<'data>>,
    entry_wrapper_func: Option<u32>,
) {
    let Some(entry) = entry else {
        return;
    };
    let index = entry_wrapper_func.unwrap_or(entry.function_index);
    if let Some(existing) = exports
        .iter_mut()
        .find(|export| export.name == entry.export_name)
    {
        if entry_wrapper_func.is_some() {
            existing.kind = wasmparser::ExternalKind::Func;
            existing.index = index;
        }
        return;
    }
    exports.push(OutputExport {
        name: entry.export_name,
        kind: wasmparser::ExternalKind::Func,
        index,
    });
}

pub(crate) fn requested_linker_export_symbols(args: &WasmArgs) -> Vec<WasmLinkerSymbol> {
    let mut symbols = Vec::new();
    for name in args.force_export_symbol_names() {
        let Some(sym) = WasmLinkerSymbol::parse(name) else {
            continue;
        };
        if !symbols.contains(&sym) {
            symbols.push(sym);
        }
    }
    symbols
}

pub(crate) fn try_export_linker_defined(
    exports: &mut Vec<OutputExport<'_>>,
    known: WasmLinkerSymbol,
    indices: &LinkerDefinedIndices,
) -> bool {
    let name = <&str>::from(known);
    if let Some(index) = indices.function_index(known) {
        push_export(exports, name, wasmparser::ExternalKind::Func, index);
        return true;
    }
    if let Some(index) = indices.global_index(known) {
        push_export(exports, name, wasmparser::ExternalKind::Global, index);
        return true;
    }
    false
}

pub(crate) fn is_requested_linker_export(indices: &LinkerDefinedIndices, name: &str) -> bool {
    indices
        .requested_exports
        .iter()
        .any(|&sym| <&str>::from(sym) == name)
}

/// Export symbols requested via `--export` and `--export-if-defined`.
pub(crate) fn ensure_force_exports<'data>(
    exports: &mut Vec<OutputExport<'data>>,
    layout_inputs: &[WasmObjectLayoutInput<'data>],
    object_index_maps: &[WasmObjectIndexMap],
    symbol_db: &SymbolDb<'data, Wasm>,
    entry: Option<&ResolvedEntry<'data>>,
    indices: &LinkerDefinedIndices,
    file_id_to_index: &HashMap<crate::input_data::FileId, usize>,
) -> Result<()> {
    for &known in &indices.requested_exports {
        if try_export_linker_defined(exports, known, indices) {
            continue;
        }
        let name = <&str>::from(known);
        if symbol_db
            .args
            .required_export_symbols
            .iter()
            .any(|required| required == name)
        {
            bail!("symbol exported via --export not found: {name}");
        }
    }

    for name in symbol_db.args.force_export_symbol_names() {
        if is_requested_linker_export(indices, name) {
            continue;
        }
        let required = symbol_db.args.required_export_symbols.contains(name);
        let Some(symbol_id) =
            symbol_db.get_unversioned(&UnversionedSymbolName::prehashed(name.as_bytes()))
        else {
            if required {
                bail!("symbol exported via --export not found: {name}");
            }
            continue;
        };
        let def_id = symbol_db.definition(symbol_id);
        let def_file_id = symbol_db.file_id_for_symbol(def_id);

        let Some(&def_obj_idx) = file_id_to_index.get(&def_file_id) else {
            if required {
                bail!("symbol exported via --export not found: {name}");
            }
            continue;
        };
        let def_input = &layout_inputs[def_obj_idx];
        let def_sym = &def_input.symbols[def_input.symbol_id_range.id_to_offset(def_id)];
        if def_sym.is_undefined() {
            if required {
                bail!("symbol exported via --export not found: {name}");
            }
            continue;
        }

        let index_map = object_index_maps
            .get(def_obj_idx)
            .context("missing Wasm object index map for --export symbol definition")?;
        let export_name = core::str::from_utf8(symbol_db.symbol_name(def_id)?.bytes())
            .context("invalid UTF-8 in Wasm --export symbol name")?;

        match def_sym.kind {
            WasmSymbolKind::Func => {
                let mut index =
                    remap_wasm_index(&index_map.function_indices, def_sym.index, "function")?;
                // If this is the entry and we wrap it, export the wrapper.
                if let (Some(entry), Some(wrapper)) = (entry, indices.entry_wrapper_func)
                    && export_name == entry.export_name
                {
                    index = wrapper;
                }
                push_export(exports, export_name, wasmparser::ExternalKind::Func, index);
            }
            WasmSymbolKind::Global => {
                let index = remap_wasm_index(&index_map.global_indices, def_sym.index, "global")?;
                push_export(
                    exports,
                    export_name,
                    wasmparser::ExternalKind::Global,
                    index,
                );
            }
            _ => {
                bail!(
                    "Wasm --export of non-function/non-global symbols is not yet supported: {name}"
                );
            }
        }
    }
    Ok(())
}

pub(crate) fn build_output_module_layout<'data, 'files>(
    groups: &'files mut [layout::GroupState<'data, Wasm>],
    symbol_db: &crate::symbol_db::SymbolDb<'data, Wasm>,
) -> Result<WasmLayout<'data>>
where
    'data: 'files,
{
    timing_phase!("Build Wasm module layout");

    let mut layout_inputs = {
        timing_phase!("Collect Wasm object layout inputs");
        let handed_off: Vec<_> = groups
            .iter_mut()
            .flat_map(|group| group.files.iter_mut())
            .filter_map(|file| match file {
                layout::FileLayoutState::Object(object) => {
                    Some(object.format_specific.take_decoded_code_data())
                }
                _ => None,
            })
            .collect();

        let objects_and_states: Vec<_> = layout::objects_iter(groups)
            .map(|state| (&state.object, &state.format_specific))
            .collect();
        ensure!(
            objects_and_states.len() == handed_off.len(),
            "Wasm layout input count does not match taken code/data count"
        );
        objects_and_states
            .par_iter()
            .zip(handed_off.into_par_iter())
            .map(|((object, state), decoded)| {
                verbose_timing_phase!("Collect Wasm object layout input");
                WasmObjectLayoutInput::from_file(object, state, decoded)
            })
            .collect::<Result<Vec<_>>>()?
    };

    if symbol_db.args.shared_memory {
        validate_shared_memory_features(&layout_inputs, symbol_db)?;
    }

    let file_id_to_index = layout_file_id_to_index(&layout_inputs);
    let mut import_resolutions =
        resolve_cross_object_imports(&layout_inputs, symbol_db, &file_id_to_index)?;
    let has_init_funcs = layout_inputs
        .iter()
        .any(|input| !input.init_funcs.is_empty());
    // Like wasm-ld, wrap only when InitFuncs exist and crt does not already call
    // `__wasm_call_ctors`.
    let wrap_entry = has_init_funcs
        && !call_ctors_used_in_objects(&layout_inputs)
        && entry_is_defined_function(&layout_inputs, symbol_db, &file_id_to_index);

    let (indices, reloc_scan, shared_imports) = setup_got_mem_and_indices(
        &layout_inputs,
        &mut import_resolutions,
        symbol_db,
        &file_id_to_index,
        has_init_funcs,
        wrap_entry,
    )?;
    let got_mem = &reloc_scan.got_mem;
    let got_func = &reloc_scan.got_func;
    let index_bases = allocate_wasm_object_index_bases(&layout_inputs, &shared_imports, &indices)?;
    let object_index_maps = {
        timing_phase!("Build per-object Wasm index maps");
        layout_inputs
            .par_iter_mut()
            .zip(import_resolutions.par_iter())
            .enumerate()
            .map(|(obj_idx, (input, resolutions))| {
                verbose_timing_phase!("Build Wasm object index map");
                let index_map = input.build_object_index_map(
                    obj_idx,
                    index_bases[obj_idx],
                    resolutions,
                    &index_bases,
                    &indices,
                    &shared_imports,
                )?;
                classify_code_relocations(&mut input.function_bodies, &input.code_relocations);
                for body in &mut input.function_bodies {
                    body.object_index = obj_idx;
                }
                Ok(index_map)
            })
            .collect::<Result<Vec<_>>>()?
    };

    let linker_memory = any_object_needs_linker_memory(&layout_inputs);
    let stack_size = symbol_db.args.z_stack_size;
    let stack_first = symbol_db.args.stack_first;
    let initial_memory = symbol_db.args.initial_memory;
    let max_memory = symbol_db.args.max_memory;
    let shared_memory = symbol_db.args.shared_memory;
    if stack_size > 0 {
        ensure_stack_size_aligned(stack_size)?;
    }
    let mut layout = WasmLayout {
        memory_base: if linker_memory || indices.memory_base_init == LINKER_MEMORY_BASE {
            LINKER_MEMORY_BASE
        } else {
            0
        },
        ..WasmLayout::default()
    };
    let data_start = if stack_first {
        stack_size
    } else {
        layout.memory_base
    };
    let mut memory_cursor = data_start;
    {
        timing_phase!("Merge Wasm object layouts");
        let n_objects = object_index_maps.len();
        layout.object_index_maps = object_index_maps;
        layout.per_object_symbols.reserve(n_objects);
        layout.object_data_layouts.reserve(n_objects);
        layout.object_code_relocations.reserve(n_objects);
        layout.object_data_relocations.reserve(n_objects);

        {
            timing_phase!("Merge Wasm section lists");
            layout.imports = shared_imports.to_output_imports(&index_bases)?;
            for (input, index_map) in layout_inputs
                .iter_mut()
                .zip(layout.object_index_maps.iter())
            {
                layout.output_types.extend(input.types.iter().cloned());
                for &local_type_index in &input.module_functions {
                    layout.function_type_indices.push(remap_wasm_index(
                        &index_map.type_indices,
                        local_type_index,
                        "type",
                    )?);
                }
                layout.globals.append(&mut input.globals);
                layout.exports.extend(input.remapped_exports(index_map)?);
                layout.memories.extend(input.memories.iter().copied());
                layout
                    .unsupported_output
                    .append(&mut input.unsupported_output);
            }
        }
        {
            timing_phase!("Merge Wasm function bodies");
            let n_bodies = layout_inputs
                .iter()
                .map(|input| input.function_bodies.len())
                .sum();
            layout.function_bodies.reserve(n_bodies);
            for input in &mut layout_inputs {
                layout.function_bodies.append(&mut input.function_bodies);
            }
        }
        {
            timing_phase!("Apply Wasm GOT index maps");
            apply_got_to_index_maps(
                layout
                    .object_index_maps
                    .iter_mut()
                    .map(|map| &mut map.got_mem_globals),
                got_mem,
            );
            apply_got_to_index_maps(
                layout
                    .object_index_maps
                    .iter_mut()
                    .map(|map| &mut map.got_func_globals),
                got_func,
            );
            fill_function_symbol_redirects(
                &mut layout.object_index_maps,
                &layout_inputs,
                symbol_db,
                &file_id_to_index,
            );
        }
        {
            for input in &layout_inputs {
                layout.per_object_symbols.push(input.symbols);
            }
        }
        {
            timing_phase!("Layout Wasm data segments");
            for (obj_idx, input) in layout_inputs.iter().enumerate() {
                layout.object_data_layouts.push(layout_object_data(
                    input,
                    &layout.object_index_maps[obj_idx],
                    &mut memory_cursor,
                )?);
            }
            for input in &mut layout_inputs {
                layout
                    .object_code_relocations
                    .push(std::mem::take(&mut input.code_relocations));
                layout
                    .object_data_relocations
                    .push(std::mem::take(&mut input.data_relocations));
            }
        }
    }

    let init_function_calls =
        collect_sorted_init_function_calls(&layout_inputs, &layout.object_index_maps)?;
    let call_ctors_body = indices
        .call_ctors_func
        .map(|_| encode_call_sequence_body(&init_function_calls));

    let entry = resolve_entry_function(
        &layout_inputs,
        &layout.object_index_maps,
        symbol_db,
        &file_id_to_index,
    )?;
    let entry_wrapper_body = match (indices.entry_wrapper_func, indices.call_ctors_func, &entry) {
        (Some(_), Some(ctors), Some(entry)) => Some(encode_call_sequence_body(&[
            (ctors, 0),
            (entry.function_index, 0),
        ])),
        _ => None,
    };

    {
        timing_phase!("Wasm linker-defined symbols and data addresses");
        emit_reserved_linker_definitions(
            &mut layout,
            &indices,
            call_ctors_body,
            entry_wrapper_body,
        );
        deduplicate_output_types(&mut layout);

        // wasm-ld always defines a linear memory for executables.
        if layout.memories.is_empty() {
            layout
                .memories
                .push(linker_output_memory_type(&layout_inputs, shared_memory));
        }
        if !layout.memories.is_empty() {
            ensure_memory_export(&mut layout.exports, symbol_db.args.memory_export_name());
        }
        layout.data_end = memory_cursor;
        let initial_pages = ensure_memory_covers(
            &mut layout,
            stack_size,
            stack_first,
            initial_memory,
            max_memory,
            shared_memory,
        )?;
        // wasm-ld only defines `__heap_end` when linear memory exists (end of `memory.initial`).
        let heap_end = if layout.memories.is_empty() {
            None
        } else {
            Some(heap_end_from_initial_pages(initial_pages)?)
        };
        let data_end = layout.data_end;
        compute_data_addresses(
            &mut layout.object_index_maps,
            &layout.per_object_symbols,
            &layout.object_data_layouts,
            &layout_inputs,
            symbol_db,
            &file_id_to_index,
            data_start,
            data_end,
            stack_size,
            heap_end,
            stack_first,
        )?;
        fill_got_mem_inits(
            &mut layout,
            &indices,
            got_mem,
            data_start,
            data_end,
            stack_size,
            heap_end,
            stack_first,
        )?;
        fill_exported_data_global_inits(
            &mut layout,
            &indices,
            data_start,
            data_end,
            stack_size,
            heap_end,
            stack_first,
        )?;
        fill_stack_pointer_init(&mut layout, &indices, stack_size, stack_first)?;
        ensure_entry_export(
            &mut layout.exports,
            entry.as_ref(),
            indices.entry_wrapper_func,
        );
        ensure_force_exports(
            &mut layout.exports,
            &layout_inputs,
            &layout.object_index_maps,
            symbol_db,
            entry.as_ref(),
            &indices,
            &file_id_to_index,
        )?;
    }
    {
        timing_phase!("Finalize Wasm indirect function table");
        let weak_undef_funcs: HashSet<u32> = indices
            .weak_undef_stubs
            .iter()
            .map(|s| s.function_index)
            .collect();
        finalize_indirect_function_table(
            &mut layout,
            &layout_inputs,
            &reloc_scan.table_index_symbol_indices,
            reloc_scan.needs_table,
            &weak_undef_funcs,
        )?;
        // GOT.func inits need table slots assigned above.
        fill_got_func_inits(&mut layout, &indices, got_func, &layout_inputs)?;
    }
    layout.encode_metadata_sections(&layout_inputs, &indices, got_mem, got_func, symbol_db)?;
    Ok(layout)
}

/// Assign indirect-call table slots and synthesize `table` / `element` sections.
pub(crate) fn finalize_indirect_function_table(
    layout: &mut WasmLayout<'_>,
    layout_inputs: &[WasmObjectLayoutInput<'_>],
    table_index_symbol_indices: &[Vec<usize>],
    needs_table: bool,
    weak_undef_funcs: &HashSet<u32>,
) -> Result {
    if !needs_table {
        return Ok(());
    }

    // Collect output function indices that must appear in the table.
    let mut needed: Vec<u32> = Vec::new();
    let mut seen = HashSet::new();

    for (obj_idx, (input, sym_indices)) in layout_inputs
        .iter()
        .zip(table_index_symbol_indices.iter())
        .enumerate()
    {
        let index_map = &layout.object_index_maps[obj_idx];
        for &sym_idx in sym_indices {
            let sym = input.symbols.get(sym_idx).ok_or_else(|| {
                crate::error!("table index relocation symbol {sym_idx} out of range")
            })?;
            ensure!(
                sym.kind == WasmSymbolKind::Func,
                "R_WASM_TABLE_INDEX_* references non-function symbol"
            );
            let func_out = index_map.output_function_index(sym_idx, sym)?;
            if weak_undef_funcs.contains(&func_out) {
                continue;
            }
            if seen.insert(func_out) {
                needed.push(func_out);
            }
        }
    }

    // Slot 0 is unused and first function at slot 1. This matches common clang/wasm-ld layout.
    let max_func = layout
        .object_index_maps
        .iter()
        .flat_map(|m| m.function_indices.iter().copied())
        .filter(|&idx| idx != WASM_DEAD_INDEX)
        .max()
        .unwrap_or(0);
    let mut slots_by_func = vec![u32::MAX; max_func as usize + 1];

    for &func_out in weak_undef_funcs {
        if (func_out as usize) < slots_by_func.len() {
            slots_by_func[func_out as usize] = 0;
        }
    }

    let mut element_functions = Vec::with_capacity(needed.len());
    for (i, &func_out) in needed.iter().enumerate() {
        let slot = i as u32 + 1;
        ensure!(
            (func_out as usize) < slots_by_func.len(),
            "output function index {func_out} out of range for table slots"
        );
        slots_by_func[func_out as usize] = slot;
        element_functions.push(func_out);
    }

    let mut initial = element_functions.len() as u64 + 1;
    for input in layout_inputs {
        for imp in &input.table_imports {
            initial = initial.max(imp.initial);
            ensure!(
                imp.element_type.is_func_ref(),
                "only funcref table imports are supported (got {:?})",
                imp.element_type
            );
        }
    }

    layout.tables = vec![wasmparser::TableType {
        element_type: wasmparser::RefType::FUNCREF,
        initial,
        maximum: Some(initial),
        shared: false,
        table64: false,
    }];
    layout.element_functions = element_functions;
    layout.function_table_slots = slots_by_func;

    Ok(())
}

/// Wasm symbols synthesized by the linker.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter, strum::EnumString, strum::IntoStaticStr,
)]
pub(crate) enum WasmLinkerSymbol {
    // Data
    #[strum(serialize = "__data_end")]
    DataEnd,
    #[strum(serialize = "__global_base")]
    GlobalBase,
    #[strum(serialize = "__heap_base")]
    HeapBase,
    #[strum(serialize = "__heap_end")]
    HeapEnd,
    #[strum(serialize = "__wasm_first_page_end")]
    WasmFirstPageEnd,
    #[strum(serialize = "__dso_handle")]
    DsoHandle,
    // Globals
    #[strum(serialize = "__memory_base")]
    MemoryBase,
    #[strum(serialize = "__table_base")]
    TableBase,
    #[strum(serialize = "__stack_pointer")]
    StackPointer,
    #[strum(serialize = "__tls_base")]
    TlsBase,
    // Functions
    #[strum(serialize = "__wasm_call_ctors")]
    CallCtors,
}

impl WasmLinkerSymbol {
    pub(crate) fn name(self) -> &'static [u8] {
        <&'static str>::from(self).as_bytes()
    }

    pub(crate) fn parse(name: &str) -> Option<Self> {
        name.parse().ok()
    }

    pub(crate) fn materialize_on_export(self) -> bool {
        // `--export` materializes every linker symbol except `__tls_base`.
        !matches!(self, Self::TlsBase)
    }

    pub(crate) fn exported_as_data_global(self, has_memory: bool) -> bool {
        match self {
            Self::DataEnd
            | Self::GlobalBase
            | Self::HeapBase
            | Self::WasmFirstPageEnd
            | Self::DsoHandle => true,
            Self::HeapEnd => has_memory,
            _ => false,
        }
    }

    pub(crate) fn matches_import_kind(self, kind: WasmSymbolKind) -> bool {
        match self {
            Self::CallCtors => kind == WasmSymbolKind::Func,
            Self::MemoryBase | Self::TableBase | Self::StackPointer | Self::TlsBase => {
                kind == WasmSymbolKind::Global
            }
            Self::DataEnd
            | Self::GlobalBase
            | Self::HeapBase
            | Self::HeapEnd
            | Self::WasmFirstPageEnd
            | Self::DsoHandle => false,
        }
    }

    /// Data-symbol address after memory layout. `None` for non-data variants or absent memory.
    pub(crate) fn data_address(
        self,
        data_start: u32,
        data_end: u32,
        stack_size: u32,
        heap_end: Option<u32>,
        stack_first: bool,
    ) -> Result<Option<u32>> {
        Ok(match self {
            Self::DataEnd => Some(data_end),
            Self::GlobalBase | Self::DsoHandle => Some(data_start),
            Self::HeapBase => Some(heap_base_address(data_end, stack_size, stack_first)?),
            Self::WasmFirstPageEnd => Some(u32::try_from(wasm_page_size())?),
            Self::HeapEnd => heap_end,
            Self::MemoryBase
            | Self::TableBase
            | Self::StackPointer
            | Self::TlsBase
            | Self::CallCtors => None,
        })
    }
}

pub(crate) fn compute_data_addresses(
    object_index_maps: &mut [WasmObjectIndexMap],
    per_object_symbols: &[&[WasmSymbol]],
    object_data_layouts: &[Vec<WasmDataSegmentLayout<'_>>],
    layout_inputs: &[WasmObjectLayoutInput<'_>],
    symbol_db: &SymbolDb<'_, Wasm>,
    file_id_to_index: &HashMap<crate::input_data::FileId, usize>,
    data_start: u32,
    data_end: u32,
    stack_size: u32,
    heap_end: Option<u32>,
    stack_first: bool,
) -> Result {
    let segment_offsets_by_object: Vec<Vec<Option<u32>>> = object_data_layouts
        .iter()
        .map(|layout| data_segment_memory_offsets_by_original_index(layout))
        .collect();

    for (obj_idx, (index_map, symbols)) in object_index_maps
        .iter_mut()
        .zip(per_object_symbols.iter())
        .enumerate()
    {
        let mut data_addresses = vec![0u32; symbols.len()];
        for (sym_idx, sym) in symbols.iter().enumerate() {
            if sym.kind != WasmSymbolKind::Data {
                continue;
            }
            let symbol_id = layout_inputs[obj_idx].symbol_id_range.offset_to_id(sym_idx);

            if !sym.is_undefined() {
                if let Some(addr) =
                    try_data_symbol_memory_address(&segment_offsets_by_object[obj_idx], sym)?
                {
                    data_addresses[sym_idx] = addr;
                }
                continue;
            }

            let def_id = symbol_db.definition(symbol_id);
            let def_file_id = symbol_db.file_id_for_symbol(def_id);

            if let Some(&def_obj_idx) = file_id_to_index.get(&def_file_id) {
                let def_input = &layout_inputs[def_obj_idx];
                let def_sym =
                    per_object_symbols[def_obj_idx][def_id.to_offset(def_input.symbol_id_range)];
                if !def_sym.is_undefined() {
                    if let Some(addr) = try_data_symbol_memory_address(
                        &segment_offsets_by_object[def_obj_idx],
                        &def_sym,
                    )? {
                        data_addresses[sym_idx] = addr;
                    }
                    continue;
                }
            }

            if let Some(def_info) = symbol_db.prelude_symbol_def(def_id)
                && let crate::parsing::SymbolPlacement::PlatformSpecific(known) =
                    &def_info.placement
                && let Some(address) =
                    known.data_address(data_start, data_end, stack_size, heap_end, stack_first)?
            {
                data_addresses[sym_idx] = address;
            }
        }
        index_map.data_addresses = data_addresses;
    }

    Ok(())
}

pub(crate) fn allocate_wasm_object_index_bases(
    layout_inputs: &[WasmObjectLayoutInput<'_>],
    shared_imports: &SharedUnresolvedImports<'_>,
    indices: &LinkerDefinedIndices,
) -> Result<Vec<WasmObjectIndexBases>> {
    timing_phase!("Allocate Wasm object index bases");

    let mut index_bases = Vec::with_capacity(layout_inputs.len());
    let mut next_type_index = 0u32;
    let function_import_count = shared_imports.function_count();
    let global_import_count = shared_imports.global_count();

    for input in layout_inputs {
        index_bases.push(WasmObjectIndexBases {
            type_index_base: next_type_index,
            defined_function_base: 0,
            defined_global_base: 0,
        });
        next_type_index = next_type_index
            .checked_add(u32::try_from(input.types.len()).context("too many Wasm types")?)
            .ok_or_else(|| crate::error!("Wasm type index overflow"))?;
    }

    let mut next_defined_function_index = function_import_count
        .checked_add(indices.num_defined_functions)
        .ok_or_else(|| crate::error!("Wasm function index overflow"))?;
    let mut next_defined_global_index = global_import_count
        .checked_add(indices.num_defined_globals)
        .ok_or_else(|| crate::error!("Wasm global index overflow"))?;
    for (input, index_base) in layout_inputs.iter().zip(index_bases.iter_mut()) {
        index_base.defined_function_base = next_defined_function_index;
        index_base.defined_global_base = next_defined_global_index;
        next_defined_function_index = next_defined_function_index
            .checked_add(
                u32::try_from(input.module_functions.len()).context("too many Wasm functions")?,
            )
            .ok_or_else(|| crate::error!("Wasm function index overflow"))?;
        next_defined_global_index = next_defined_global_index
            .checked_add(u32::try_from(input.globals.len()).context("too many Wasm globals")?)
            .ok_or_else(|| crate::error!("Wasm global index overflow"))?;
    }

    Ok(index_bases)
}

/// Assign each body a range into the object's sorted code-relocation list.
pub(crate) fn classify_code_relocations(
    bodies: &mut [WasmFunctionBody<'_>],
    relocs: &[WasmRelocation],
) {
    if relocs.is_empty() {
        return;
    }

    let mut i = 0usize;
    for body in bodies.iter_mut() {
        let body_start = body.code_offset;
        let body_end = body_start + body.bytes.len() as u32;
        while i < relocs.len() && relocs[i].offset < body_start {
            i += 1;
        }
        let lo = i;
        while i < relocs.len() && relocs[i].offset < body_end {
            i += 1;
        }
        body.reloc_range =
            u32::try_from(lo).unwrap_or(u32::MAX)..u32::try_from(i).unwrap_or(u32::MAX);
    }
}

pub(crate) fn remap_wasm_index(indices: &[u32], index: u32, kind: &str) -> Result<u32> {
    let mapped = indices.get(index as usize).copied().ok_or_else(|| {
        crate::error!(
            "Wasm {kind} index {index} out of range (map len {})",
            indices.len()
        )
    })?;
    ensure!(
        mapped != WASM_DEAD_INDEX,
        "Wasm {kind} index {index} was removed by GC"
    );
    Ok(mapped)
}
