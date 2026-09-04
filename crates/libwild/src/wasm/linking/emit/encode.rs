use super::super::*;
use crate::ensure;
use crate::error::Result;
use crate::wasm::DEFAULT_TABLE_BASE_INIT_EXPR;
use crate::wasm::EMPTY_FUNCTION_BODY;
use crate::wasm::LINKER_MEMORY_BASE_INIT_EXPR;
use crate::wasm::UNREACHABLE_FUNCTION_BODY;
use crate::wasm::WASM_DEAD_INDEX;
use crate::wasm::ZERO_I32_INIT_EXPR;
use crate::wasm::file::*;
use crate::wasm::gc::*;
use crate::wasm::output::*;
use crate::wasm::symbols::*;
use crate::wasm_writer::OutputGlobal;
use crate::wasm_writer::OutputImportEntity;
use hashbrown::HashMap;
use std::borrow::Cow;
use wasmparser::FuncType;
use wasmparser::GlobalType;

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
