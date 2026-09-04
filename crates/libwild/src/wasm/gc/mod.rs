mod inputs;
mod units;

use super::linking::*;
use crate::error::Result;
use crate::wasm_writer::OutputImport;
use crate::wasm_writer::OutputImportEntity;
#[allow(unused_imports)]
pub(crate) use inputs::*;
#[allow(unused_imports)]
pub(crate) use units::*;
use wasmparser::GlobalType;

/// Describes how a single import was resolved during cross-object linking.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ImportResolution {
    /// The import was not resolved; keep it in the output import section.
    Unresolved,
    /// The import was resolved to a defined function in `object_index` at local defined-function
    /// position `local_defined_index`.
    ResolvedFunction {
        object_index: usize,
        local_defined_index: u32,
    },
    /// The import was resolved to a defined global in `object_index` at local defined-global
    /// position `local_defined_index`.
    ResolvedGlobal {
        object_index: usize,
        local_defined_index: u32,
    },
    /// Resolved to a linker-synthesized function or global.
    LinkerDefined(WasmLinkerSymbol),
    /// Undefined weak function absorbed into a shared `unreachable` stub.
    WeakUndefStub { stub_index: u32 },
    /// Fixed module global index (GOT.mem / GOT.func entry).
    DirectGlobal { output_index: u32 },
    /// GOT.mem slot pending final module global index.
    GotMemSlot(usize),
    /// GOT.func slot pending final module global index.
    GotFuncSlot(usize),
}

#[derive(Debug, Default)]
pub(crate) struct ObjectImportResolutions {
    pub(crate) function_resolutions: Vec<ImportResolution>,
    pub(crate) global_resolutions: Vec<ImportResolution>,
}

#[derive(Debug, Clone)]
pub(crate) struct SharedFunctionImport<'data> {
    pub(crate) module: &'data str,
    pub(crate) name: &'data str,
    pub(crate) first_object: usize,
    pub(crate) local_type_index: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct SharedGlobalImport<'data> {
    pub(crate) module: &'data str,
    pub(crate) name: &'data str,
    pub(crate) ty: GlobalType,
}

/// Unresolved host imports coalesced by `(module, name)` across objects.
#[derive(Debug, Default)]
pub(crate) struct SharedUnresolvedImports<'data> {
    pub(crate) functions: Vec<SharedFunctionImport<'data>>,
    pub(crate) globals: Vec<SharedGlobalImport<'data>>,
    pub(crate) function_indices: Vec<Vec<Option<u32>>>,
    pub(crate) global_indices: Vec<Vec<Option<u32>>>,
}

impl<'data> SharedUnresolvedImports<'data> {
    pub(crate) fn function_count(&self) -> u32 {
        self.functions.len() as u32
    }

    pub(crate) fn global_count(&self) -> u32 {
        self.globals.len() as u32
    }

    pub(crate) fn function_index(&self, object_index: usize, local_import: usize) -> Option<u32> {
        self.function_indices
            .get(object_index)?
            .get(local_import)
            .copied()
            .flatten()
    }

    pub(crate) fn global_index(&self, object_index: usize, local_import: usize) -> Option<u32> {
        self.global_indices
            .get(object_index)?
            .get(local_import)
            .copied()
            .flatten()
    }

    pub(crate) fn to_output_imports(
        &self,
        index_bases: &[WasmObjectIndexBases],
    ) -> Result<Vec<OutputImport<'data>>> {
        let mut imports = Vec::with_capacity(self.functions.len() + self.globals.len());
        for imp in &self.functions {
            let type_index = index_bases
                .get(imp.first_object)
                .ok_or_else(|| crate::error!("Wasm shared import object index out of range"))?
                .type_index_base
                .checked_add(imp.local_type_index)
                .ok_or_else(|| crate::error!("Wasm type index overflow"))?;
            imports.push(OutputImport {
                module: imp.module,
                name: imp.name,
                entity: OutputImportEntity::Function { type_index },
            });
        }
        for imp in &self.globals {
            imports.push(OutputImport {
                module: imp.module,
                name: imp.name,
                entity: OutputImportEntity::Global(imp.ty),
            });
        }
        Ok(imports)
    }
}
