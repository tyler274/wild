use super::super::*;
use super::*;
use crate::args::wasm::WasmArgs;
use crate::bail;
use crate::ensure;
use crate::error::Context as _;
use crate::error::Result;
use crate::platform::Args as _;
use crate::symbol::UnversionedSymbolName;
use crate::symbol_db::SymbolDb;
use crate::wasm::Wasm;
use crate::wasm::gc::*;
use crate::wasm::output::*;
use crate::wasm::symbols::*;
use crate::wasm_writer::OutputExport;
use hashbrown::HashMap;
use std::borrow::Cow;
use wasmparser::MemoryType;

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
