use super::super::STANDARD_SECTION_LOOKUP_LEN;
use super::super::Wasm;
use super::super::gc::*;
use super::super::relocations::*;
use super::super::section_id;
use super::super::symbols::*;
use super::*;
use crate::bail;
use crate::error::Result;
use crate::platform;
use crate::platform::Args as _;
use crate::symbol::UnversionedSymbolName;
use crate::value_flags::ValueFlags;
use wasmparser::BinaryReader;
use wasmparser::ImportSectionReader;
use wasmparser::RelocationType;
use wasmparser::SymbolFlags;
use wasmparser::TypeRef;

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
