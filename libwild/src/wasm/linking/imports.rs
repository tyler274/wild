use super::*;
use crate::bail;
use crate::ensure;
use crate::error::Context as _;
use crate::error::Result;
use crate::input_data::PRELUDE_FILE_ID;
use crate::symbol::UnversionedSymbolName;
use crate::symbol_db::SymbolDb;
use crate::timing_phase;
use crate::verbose_timing_phase;
use crate::wasm::WASM_DEAD_INDEX;
use crate::wasm::Wasm;
use crate::wasm::gc::*;
use crate::wasm::output::*;
use crate::wasm::symbols::*;
use hashbrown::HashMap;
use hashbrown::HashSet;
use rayon::prelude::*;

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
