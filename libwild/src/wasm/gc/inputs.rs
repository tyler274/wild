use super::super::WASM_DEAD_INDEX;
use super::super::file::*;
use super::super::linking::*;
use super::super::output::*;
use super::super::relocations::*;
use super::super::section_id;
use super::super::symbols::*;
use super::*;
use crate::alignment::Alignment;
use crate::bail;
use crate::ensure;
use crate::error::Context as _;
use crate::error::Result;
use crate::wasm_writer::OutputExport;
use crate::wasm_writer::OutputGlobal;
use std::borrow::Cow;
use wasmparser::DataKind;
use wasmparser::MemoryType;
use wasmparser::TypeRef;

#[derive(Debug)]
pub(crate) struct WasmObjectLayoutInput<'data> {
    /// Input module bytes.
    pub(crate) data: &'data [u8],
    pub(crate) types: Vec<wasmparser::FuncType>,
    pub(crate) function_imports: Vec<WasmFunctionImport<'data>>,
    pub(crate) global_imports: Vec<WasmGlobalImport<'data>>,
    pub(crate) live_function_imports: Vec<bool>,
    pub(crate) live_global_imports: Vec<bool>,
    pub(crate) memory_imports: Vec<MemoryType>,
    pub(crate) table_imports: Vec<wasmparser::TableType>,
    pub(crate) module_functions: Vec<u32>,
    pub(crate) globals: Vec<OutputGlobal<'data>>,
    pub(crate) exports: Vec<OutputExport<'data>>,
    pub(crate) function_bodies: Vec<WasmFunctionBody<'data>>,
    pub(crate) memories: Vec<MemoryType>,
    pub(crate) unsupported_output: Vec<&'static str>,
    pub(crate) code_relocations: Vec<WasmRelocation>,
    pub(crate) data_segments: Vec<WasmDataSegment<'data>>,
    pub(crate) data_segment_original_indices: Vec<u32>,
    pub(crate) segment_alignments: &'data [Alignment],
    pub(crate) data_relocations: Vec<WasmRelocation>,
    pub(crate) symbols: &'data [WasmSymbol],
    pub(crate) init_funcs: &'data [WasmInitFunc],
    pub(crate) target_features: &'data [WasmTargetFeature<'data>],
    pub(crate) symbol_id_range: crate::symbol_db::SymbolIdRange,
    pub(crate) file_id: crate::input_data::FileId,
    pub(crate) defined_function_live_ordinal: Vec<u32>,
    pub(crate) defined_global_live_ordinal: Vec<u32>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct WasmObjectIndexBases {
    pub(crate) type_index_base: u32,
    pub(crate) defined_function_base: u32,
    pub(crate) defined_global_base: u32,
}

impl<'data> WasmObjectLayoutInput<'data> {
    pub(crate) fn from_file(
        file: &'data File<'data>,
        layout: &WasmObjectLayout<'data>,
        decoded: DecodedCodeData<'data>,
    ) -> Result<Self> {
        let symbol_id_range = layout.symbol_id_range;
        let file_id = layout.file_id;

        let all_live = layout.all_units_live();
        let keep_all_functions = layout.all_defined_functions_live();
        let keep_all_globals = layout.all_defined_globals_live();
        let keep_all_data_segments = layout.all_data_segments_live();

        let mut types = Vec::new();
        if let Some(type_section) = file.type_section_reader()? {
            for group in type_section {
                for ty in group?.into_types() {
                    let wasmparser::CompositeInnerType::Func(func) = ty.composite_type.inner else {
                        bail!("Wasm non-function types are not emitted")
                    };
                    types.push(func);
                }
            }
        }

        let mut function_imports = Vec::new();
        let mut global_imports = Vec::new();
        let mut memory_imports = Vec::new();
        let mut table_imports = Vec::new();
        if let Some(imports) = file.import_section_reader()? {
            for import in imports.into_imports() {
                let import = import?;
                match import.ty {
                    TypeRef::Func(type_index) | TypeRef::FuncExact(type_index) => {
                        function_imports.push(WasmFunctionImport {
                            module: import.module,
                            name: import.name,
                            type_index,
                        });
                    }
                    TypeRef::Global(ty) => {
                        global_imports.push(WasmGlobalImport {
                            module: import.module,
                            name: import.name,
                            ty,
                        });
                    }
                    TypeRef::Table(ty) => {
                        table_imports.push(ty);
                    }
                    TypeRef::Memory(memory) => {
                        memory_imports.push(memory);
                    }
                    TypeRef::Tag(_) => bail!("Wasm tag imports are not emitted"),
                }
            }
        }

        let live_function_imports = if all_live {
            vec![true; function_imports.len()]
        } else {
            layout.live_function_import_bits()
        };
        let live_global_imports = if all_live {
            vec![true; global_imports.len()]
        } else {
            layout.live_global_import_bits()
        };

        let code_section_index = file.standard_section_index[section_id::CODE as usize];
        let data_section_index = file.standard_section_index[section_id::DATA as usize];

        let (code_relocations_all, data_relocations_all) = if decoded.ready {
            (decoded.code_relocations, decoded.data_relocations)
        } else {
            (
                decode_sorted_relocs_for(file, code_section_index)?,
                decode_sorted_relocs_for(file, data_section_index)?,
            )
        };

        // TODO(wasm): Currently relocs targeting `.debug*` are ignored (not applied, not emitted).
        let has_unsupported_non_code_relocs = file.reloc_sections.iter().any(|s| {
            let target = Some(s.target_section_index);
            if target == code_section_index || target == data_section_index {
                return false;
            }
            !file.section_is_debug(s.target_section_index)
        });

        let mut unsupported_output = Vec::new();
        if has_unsupported_non_code_relocs {
            unsupported_output.push("non-code relocation");
        }
        if !data_relocations_all.is_empty()
            && !data_relocations_are_supported(&data_relocations_all)
        {
            unsupported_output.push("data relocation");
        }
        if file.standard_section_index[section_id::TABLE as usize].is_some() {
            unsupported_output.push("table definition");
        }
        if file.standard_section_index[section_id::START as usize].is_some() {
            unsupported_output.push("start");
        }
        let all_data_segments = if decoded.ready {
            decoded.data_segments
        } else {
            file.data_segments()?
        };
        for segment in &all_data_segments {
            if let DataKind::Passive = segment.kind {
                unsupported_output.push("passive data segment");
                break;
            }
        }

        let all_module_functions = file.module_functions()?;
        let all_function_bodies = if decoded.ready {
            decoded.function_bodies
        } else {
            file.function_bodies()?
        };
        ensure!(
            all_module_functions.len() == all_function_bodies.len(),
            "Wasm function and code section counts differ"
        );
        let memories = file.memories()?;

        let all_globals = file
            .module_globals()?
            .into_iter()
            .map(|global| {
                let init_expr_body = crate::wasm_writer::const_expr_body(&global.init_expr)
                    .ok_or_else(|| {
                        crate::error!("Wasm global initializer is missing end opcode")
                    })?;
                Ok(OutputGlobal {
                    ty: global.ty,
                    init_expr_body: Cow::Borrowed(init_expr_body),
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let defined_function_live_ordinal = if keep_all_functions {
            identity_ordinals(all_module_functions.len())
        } else {
            layout.defined_function_live_ordinal.clone()
        };
        let defined_global_live_ordinal = if keep_all_globals {
            identity_ordinals(all_globals.len())
        } else {
            layout.defined_global_live_ordinal.clone()
        };

        let (module_functions, function_bodies, code_relocations) = if keep_all_functions {
            // All defined functions live.
            (
                all_module_functions,
                all_function_bodies,
                code_relocations_all,
            )
        } else {
            let mut module_functions = Vec::new();
            let mut function_bodies = Vec::new();
            let mut code_relocations = Vec::new();
            for (i, (ty, body)) in all_module_functions
                .into_iter()
                .zip(all_function_bodies)
                .enumerate()
            {
                if !layout.is_defined_function_live(i) {
                    continue;
                }
                let body_start = body.code_offset;
                let body_end = body_start + body.bytes.len() as u32;
                code_relocations.extend_from_slice(relocs_in_offset_range(
                    &code_relocations_all,
                    body_start,
                    body_end,
                ));
                module_functions.push(ty);
                function_bodies.push(body);
            }
            sort_relocations_by_offset(&mut code_relocations);
            (module_functions, function_bodies, code_relocations)
        };

        let globals = if keep_all_globals {
            all_globals
        } else {
            all_globals
                .into_iter()
                .enumerate()
                .filter_map(|(i, global)| layout.is_defined_global_live(i).then_some(global))
                .collect()
        };

        let (data_segments, data_segment_original_indices, data_relocations) =
            if keep_all_data_segments {
                let n = all_data_segments.len();
                let original_indices = (0..n as u32).collect();
                (all_data_segments, original_indices, data_relocations_all)
            } else {
                let mut data_segments = Vec::new();
                let mut data_segment_original_indices = Vec::new();
                let mut data_relocations = Vec::new();
                for (i, segment) in all_data_segments.into_iter().enumerate() {
                    if !layout.is_data_segment_live(i) {
                        continue;
                    }
                    let (start, end) = data_segment_span(&segment)?;
                    data_relocations.extend_from_slice(relocs_in_offset_range(
                        &data_relocations_all,
                        start,
                        end,
                    ));
                    data_segment_original_indices
                        .push(u32::try_from(i).context("too many data segments")?);
                    data_segments.push(segment);
                }
                sort_relocations_by_offset(&mut data_relocations);
                (
                    data_segments,
                    data_segment_original_indices,
                    data_relocations,
                )
            };

        let mut exports = Vec::new();
        if let Some(export_section) = file.export_section_reader()? {
            for export in export_section {
                let export = export?;
                exports.push(OutputExport {
                    name: export.name,
                    kind: export.kind,
                    index: export.index,
                });
            }
        }

        Ok(Self {
            data: file.data,
            types,
            function_imports,
            global_imports,
            live_function_imports,
            live_global_imports,
            memory_imports,
            table_imports,
            module_functions,
            globals,
            exports,
            function_bodies,
            memories,
            unsupported_output,
            code_relocations,
            data_segments,
            data_segment_original_indices,
            segment_alignments: file.segment_alignments.as_slice(),
            data_relocations,
            symbols: file.symbols.as_slice(),
            init_funcs: file.init_funcs.as_slice(),
            target_features: file.target_features.as_slice(),
            symbol_id_range,
            file_id,
            defined_function_live_ordinal,
            defined_global_live_ordinal,
        })
    }

    pub(crate) fn build_object_index_map(
        &self,
        object_index: usize,
        index_bases: WasmObjectIndexBases,
        resolutions: &ObjectImportResolutions,
        all_index_bases: &[WasmObjectIndexBases],
        indices: &LinkerDefinedIndices,
        shared_imports: &SharedUnresolvedImports<'data>,
    ) -> Result<WasmObjectIndexMap> {
        ensure!(
            resolutions.function_resolutions.len() == self.function_imports.len(),
            "Wasm function import resolution count mismatch"
        );
        ensure!(
            resolutions.global_resolutions.len() == self.global_imports.len(),
            "Wasm global import resolution count mismatch"
        );

        let mut type_indices = Vec::with_capacity(self.types.len());
        for local_ty in 0..self.types.len() {
            let output_type_index = index_bases
                .type_index_base
                .checked_add(u32::try_from(local_ty).context("too many Wasm types")?)
                .ok_or_else(|| crate::error!("Wasm type index overflow"))?;
            type_indices.push(output_type_index);
        }

        let mut index_map = WasmObjectIndexMap {
            type_indices,
            function_indices: Vec::with_capacity(
                self.function_imports.len() + self.defined_function_live_ordinal.len(),
            ),
            global_indices: Vec::with_capacity(
                self.global_imports.len() + self.defined_global_live_ordinal.len(),
            ),
            memory_indices: Vec::with_capacity(self.memory_imports.len() + self.memories.len()),
            table_indices: vec![0; self.table_imports.len()],
            data_addresses: Vec::new(),
            got_mem_globals: Vec::new(),
            got_func_globals: Vec::new(),
            function_symbol_redirects: Vec::new(),
        };

        for (i, resolution) in resolutions.function_resolutions.iter().enumerate() {
            if !self.live_function_imports.get(i).copied().unwrap_or(false) {
                index_map.function_indices.push(WASM_DEAD_INDEX);
                continue;
            }
            match *resolution {
                ImportResolution::Unresolved => {
                    let output_function_index = shared_imports
                        .function_index(object_index, i)
                        .ok_or_else(|| {
                            crate::error!(
                                "missing shared function import index for object {object_index} \
                                 import {i}"
                            )
                        })?;
                    index_map.function_indices.push(output_function_index);
                }
                ImportResolution::LinkerDefined(known) => {
                    let index = indices.function_index(known).ok_or_else(|| {
                        crate::error!("missing reserved Wasm function for {known:?}")
                    })?;
                    index_map.function_indices.push(index);
                }
                ImportResolution::WeakUndefStub { stub_index } => {
                    let index = indices
                        .weak_undef_stubs
                        .get(stub_index as usize)
                        .map(|s| s.function_index)
                        .ok_or_else(|| {
                            crate::error!("Wasm weak-undef stub index {stub_index} out of range")
                        })?;
                    index_map.function_indices.push(index);
                }
                ImportResolution::ResolvedFunction {
                    object_index: def_object_index,
                    local_defined_index,
                } => {
                    ensure!(
                        def_object_index < all_index_bases.len(),
                        "Wasm function import resolution references object index \
                         {def_object_index} out of range"
                    );
                    ensure!(
                        local_defined_index != WASM_DEAD_INDEX,
                        "Wasm function import resolved to a GC'd definition"
                    );
                    let target_bases = &all_index_bases[def_object_index];
                    let output_function_index = target_bases
                        .defined_function_base
                        .checked_add(local_defined_index)
                        .ok_or_else(|| crate::error!("Wasm function index overflow"))?;
                    index_map.function_indices.push(output_function_index);
                }
                ImportResolution::ResolvedGlobal { .. }
                | ImportResolution::DirectGlobal { .. }
                | ImportResolution::GotMemSlot(_)
                | ImportResolution::GotFuncSlot(_) => {
                    bail!("function import resolved as global");
                }
            }
        }

        for (i, resolution) in resolutions.global_resolutions.iter().enumerate() {
            if !self.live_global_imports.get(i).copied().unwrap_or(false) {
                index_map.global_indices.push(WASM_DEAD_INDEX);
                continue;
            }
            match *resolution {
                ImportResolution::Unresolved => {
                    let output_global_index = shared_imports
                        .global_index(object_index, i)
                        .ok_or_else(|| {
                            crate::error!(
                                "missing shared global import index for object {object_index} \
                                 import {i}"
                            )
                        })?;
                    index_map.global_indices.push(output_global_index);
                }
                ImportResolution::LinkerDefined(known) => {
                    let index = indices.global_index(known).ok_or_else(|| {
                        crate::error!("missing reserved Wasm global for {known:?}")
                    })?;
                    index_map.global_indices.push(index);
                }
                ImportResolution::DirectGlobal { output_index } => {
                    index_map.global_indices.push(output_index);
                }
                ImportResolution::GotMemSlot(_) => {
                    bail!("GOT.mem slot was not converted to a module global index");
                }
                ImportResolution::GotFuncSlot(_) => {
                    bail!("GOT.func slot was not converted to a module global index");
                }
                ImportResolution::ResolvedGlobal {
                    object_index,
                    local_defined_index,
                } => {
                    ensure!(
                        object_index < all_index_bases.len(),
                        "Wasm global import resolution references object index {object_index} out \
                         of range"
                    );
                    ensure!(
                        local_defined_index != WASM_DEAD_INDEX,
                        "Wasm global import resolved to a GC'd definition"
                    );
                    let target_bases = &all_index_bases[object_index];
                    let output_global_index = target_bases
                        .defined_global_base
                        .checked_add(local_defined_index)
                        .ok_or_else(|| crate::error!("Wasm global index overflow"))?;
                    index_map.global_indices.push(output_global_index);
                }
                ImportResolution::ResolvedFunction { .. }
                | ImportResolution::WeakUndefStub { .. } => {
                    bail!("global import resolved as function");
                }
            }
        }

        // Full function index space: imports (above) + original defined ordinals.
        for &dense_or_dead in &self.defined_function_live_ordinal {
            if dense_or_dead == WASM_DEAD_INDEX {
                index_map.function_indices.push(WASM_DEAD_INDEX);
            } else {
                let output_function_index = index_bases
                    .defined_function_base
                    .checked_add(dense_or_dead)
                    .ok_or_else(|| crate::error!("Wasm function index overflow"))?;
                index_map.function_indices.push(output_function_index);
            }
        }

        for &dense_or_dead in &self.defined_global_live_ordinal {
            if dense_or_dead == WASM_DEAD_INDEX {
                index_map.global_indices.push(WASM_DEAD_INDEX);
            } else {
                let output_global_index = index_bases
                    .defined_global_base
                    .checked_add(dense_or_dead)
                    .ok_or_else(|| crate::error!("Wasm global index overflow"))?;
                index_map.global_indices.push(output_global_index);
            }
        }

        // Imported and defined memories are merged into a single output memory.
        let memory_slot_count = self.memory_imports.len() + self.memories.len();
        index_map.memory_indices = vec![0; memory_slot_count];

        Ok(index_map)
    }

    pub(crate) fn remapped_exports(
        &self,
        index_map: &WasmObjectIndexMap,
    ) -> Result<Vec<OutputExport<'data>>> {
        self.exports
            .iter()
            .map(|export| {
                let index = match export.kind {
                    wasmparser::ExternalKind::Func | wasmparser::ExternalKind::FuncExact => {
                        remap_wasm_index(&index_map.function_indices, export.index, "function")?
                    }
                    wasmparser::ExternalKind::Global => {
                        remap_wasm_index(&index_map.global_indices, export.index, "global")?
                    }
                    wasmparser::ExternalKind::Memory => {
                        remap_wasm_index(&index_map.memory_indices, export.index, "memory")?
                    }
                    wasmparser::ExternalKind::Table => 0, // single output table
                    wasmparser::ExternalKind::Tag => bail!("Wasm tag exports are not emitted"),
                };
                Ok(OutputExport { index, ..*export })
            })
            .collect()
    }
}
