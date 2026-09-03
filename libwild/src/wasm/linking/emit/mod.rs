mod encode;
mod memory;

use super::*;
use crate::ensure;
use crate::error::Context as _;
use crate::error::Result;
use crate::layout;
use crate::symbol_db::SymbolDb;
use crate::timing_phase;
use crate::verbose_timing_phase;
use crate::wasm::LINKER_MEMORY_BASE;
use crate::wasm::WASM_DEAD_INDEX;
use crate::wasm::Wasm;
use crate::wasm::file::*;
use crate::wasm::gc::*;
use crate::wasm::output::*;
use crate::wasm::relocations::*;
use crate::wasm::symbols::*;
#[allow(unused_imports)]
pub(crate) use encode::*;
use hashbrown::HashMap;
use hashbrown::HashSet;
#[allow(unused_imports)]
pub(crate) use memory::*;
use rayon::prelude::*;

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
