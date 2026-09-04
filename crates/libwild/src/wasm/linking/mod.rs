mod emit;
mod got;
mod imports;

use super::LINKER_MEMORY_BASE;
use super::Wasm;
use super::gc::*;
use super::output::*;
use super::symbols::*;
use crate::error::Result;
use crate::symbol::UnversionedSymbolName;
use crate::symbol_db::SymbolDb;
#[allow(unused_imports)]
pub(crate) use emit::*;
#[allow(unused_imports)]
pub(crate) use got::*;
use hashbrown::HashMap;
#[allow(unused_imports)]
pub(crate) use imports::*;

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
