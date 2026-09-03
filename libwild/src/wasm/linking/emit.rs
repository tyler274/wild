use super::*;
use crate::args::wasm::WasmArgs;
use crate::bail;
use crate::ensure;
use crate::error::Context as _;
use crate::error::Result;
use crate::layout;
use crate::platform::Args as _;
use crate::symbol::UnversionedSymbolName;
use crate::symbol_db::SymbolDb;
use crate::timing_phase;
use crate::verbose_timing_phase;
use crate::wasm::DEFAULT_TABLE_BASE_INIT_EXPR;
use crate::wasm::EMPTY_FUNCTION_BODY;
use crate::wasm::LINKER_MEMORY_BASE;
use crate::wasm::LINKER_MEMORY_BASE_INIT_EXPR;
use crate::wasm::UNREACHABLE_FUNCTION_BODY;
use crate::wasm::WASM_DEAD_INDEX;
use crate::wasm::Wasm;
use crate::wasm::ZERO_I32_INIT_EXPR;
use crate::wasm::file::*;
use crate::wasm::gc::*;
use crate::wasm::output::*;
use crate::wasm::relocations::*;
use crate::wasm::symbols::*;
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
