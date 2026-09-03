use super::types::*;
use crate::OutputKind;
use crate::alignment;
use crate::error::Context;
use crate::error::Result;
use crate::expression_eval::ResolvedLocationCounter;
use crate::expression_eval::SymbolValue;
use crate::grouping::Group;
use crate::input_data::InputRef;
use crate::output_section_id::GnuBuildIdPlacement;
use crate::output_section_id::OutputOrder;
use crate::output_section_id::OutputSections;
use crate::output_section_map::OutputSectionMap;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::parsing::InternalSymDefInfo;
use crate::parsing::SymbolLoc;
use crate::parsing::SymbolPlacement;
use crate::platform::Arch;
use crate::platform::Args as _;
use crate::platform::NonAddressableIndexes as _;
use crate::platform::ObjectFile;
use crate::platform::Platform;
use crate::platform::SectionAttributes as _;
use crate::program_segments::ProgramSegments;
use crate::symbol::UnversionedSymbolName;
use crate::symbol_db::SymbolDb;
use crate::timing_phase;
use crate::value_flags::AtomicPerSymbolFlags;
use crate::value_flags::PerSymbolFlags;
use crate::value_flags::ValueFlags;
use crate::verbose_timing_phase;
use hashbrown::HashMap;
use rayon::iter::IntoParallelRefMutIterator;
use rayon::iter::ParallelIterator;
use std::num::NonZeroU32;

/// Update resolutions for symbol redirects.
pub(crate) fn update_redirect_resolutions<'data, P: Platform>(
    symbol_db: &SymbolDb<'data, P>,
    resolutions: &mut [Option<Resolution<P>>],
    output_sections: &OutputSections<'data, P>,
    section_layouts: &OutputSectionMap<OutputRecordLayout>,
    merged_section_layouts: &OutputSectionMap<OutputRecordLayout>,
    sizeof_headers: u64,
    memory_regions: &HashMap<&[u8], MemoryRegion>,
    resolved_location_counters: &[ResolvedLocationCounter],
) -> Result {
    verbose_timing_phase!("Update symdef resolutions");

    for group in &symbol_db.groups {
        match group {
            Group::Prelude(prelude) => {
                for def_info in &prelude.symbol_definitions {
                    update_defsym_symbol_resolution(
                        None,
                        def_info,
                        symbol_db,
                        resolutions,
                        output_sections,
                        merged_section_layouts,
                        memory_regions,
                        sizeof_headers,
                        &[],
                    )?;
                }
            }
            Group::LinkerScripts(scripts) => {
                for script in scripts {
                    for def_info in &script.parsed.symbol_defs {
                        let SymbolPlacement::Redirect(redirect) = &def_info.placement else {
                            continue;
                        };
                        let section_layouts = if matches!(
                            redirect.loc,
                            SymbolLoc::SectionStartRelative(_)
                                | SymbolLoc::SectionEndRelative(_)
                                | SymbolLoc::LocationCounter(..)
                        ) {
                            section_layouts
                        } else {
                            merged_section_layouts
                        };
                        update_defsym_symbol_resolution(
                            Some(&script.parsed.input),
                            def_info,
                            symbol_db,
                            resolutions,
                            output_sections,
                            section_layouts,
                            memory_regions,
                            sizeof_headers,
                            resolved_location_counters,
                        )?;
                    }
                }
            }
            Group::Objects(_) | Group::StubLibraries(_) | Group::SyntheticSymbols(_) => {}
            #[cfg(all(feature = "plugins", unix))]
            Group::LtoInputs(_) => {}
        }
    }

    Ok(())
}

pub(crate) fn update_defsym_symbol_resolution<'data, P: Platform>(
    input_ref: Option<&InputRef<'data>>,
    def_info: &InternalSymDefInfo<'data, P>,
    symbol_db: &SymbolDb<'data, P>,
    resolutions: &mut [Option<Resolution<P>>],
    output_sections: &OutputSections<'data, P>,
    section_layouts: &OutputSectionMap<OutputRecordLayout>,
    memory_regions: &HashMap<&[u8], MemoryRegion>,
    sizeof_headers: u64,
    resolved_location_counters: &[ResolvedLocationCounter],
) -> Result {
    if let SymbolPlacement::Redirect(redirect) = &def_info.placement {
        // GNU ld ignores unused PROVIDE, including when the right-hand side is undefined
        // (kernel `PROVIDE(foo = __pi_foo)` when the startup object is not built).
        if def_info.is_provide {
            let mut missing_rhs = false;
            redirect.expression.visit_expressions(&mut |e| {
                if let crate::linker_script::Expression::Symbol(name) = e
                    && symbol_db
                        .get_unversioned(&UnversionedSymbolName::prehashed(name))
                        .is_none()
                {
                    missing_rhs = true;
                }
                true
            });
            if missing_rhs {
                return Ok(());
            }
        }

        let value = crate::expression_eval::evaluate_expression(
            &redirect.expression,
            &redirect.loc,
            input_ref,
            section_layouts,
            output_sections,
            memory_regions,
            symbol_db,
            sizeof_headers,
            resolved_location_counters,
            // During late evaluation of linker scripts, we don't have any part relative symbols.
            &OutputSectionPartMap::default(),
            &mut |name| {
                let Some(target_symbol_id) =
                    symbol_db.get_unversioned(&UnversionedSymbolName::prehashed(name))
                else {
                    return Err(redirect.missing_target(name));
                };

                let canonical_target_id = symbol_db.definition(target_symbol_id);

                let resolution = resolutions[canonical_target_id.as_usize()]
                    .as_ref()
                    .ok_or_else(|| redirect.missing_resolution(name))?;

                let symbol_value = match symbol_db.output_section_id(canonical_target_id) {
                    Some(section_id) => SymbolValue::SectionRelative {
                        section_id,
                        address: resolution.raw_value,
                    },
                    None => SymbolValue::Absolute(resolution.raw_value),
                };
                Ok(symbol_value)
            },
        )?;

        if def_info.name.is_empty() {
            return Ok(());
        }

        let canonical_symbol_id = symbol_db
            .get_unversioned(&UnversionedSymbolName::prehashed(def_info.name))
            .map(|id| symbol_db.definition(id))
            .ok_or_else(|| redirect.missing_target(def_info.name))?;

        let resolution = resolutions[canonical_symbol_id.as_usize()]
            .as_mut()
            .ok_or_else(|| redirect.missing_resolution(def_info.name))?;

        resolution.raw_value = value;
    }

    Ok(())
}

/// Update resolutions for all dynamic symbols that our output file defines.
pub(crate) fn update_dynamic_symbol_resolutions<'data, P: Platform>(
    resources: &FinaliseLayoutResources<'_, 'data, P>,
    layouts: &[GroupLayout<'data, P>],
    resolutions: &mut [Option<Resolution<P>>],
) {
    if P::DYNSYM_SECTION_ID.is_none() {
        return;
    }

    timing_phase!("Update dynamic symbol resolutions");

    let Some(FileLayout::Epilogue(epilogue)) = layouts.last().and_then(|g| g.files.last()) else {
        panic!("Epilogue should be the last file");
    };

    for (index, sym) in resources.dynamic_symbol_definitions.iter().enumerate() {
        let dynamic_symbol_index = NonZeroU32::try_from(epilogue.dynsym_start_index + index as u32)
            .expect("Dynamic symbol definitions should start > 0");
        if let Some(res) = &mut resolutions[sym.symbol_id.as_usize()] {
            res.dynamic_symbol_index = Some(dynamic_symbol_index);
        }
    }
}

pub(crate) fn finalise_all_sizes<'data, P: Platform, A: Arch<Platform = P>>(
    group_states: &mut [GroupState<'data, P>],
    per_symbol_flags: &AtomicPerSymbolFlags,
    resources: &FinaliseSizesResources<'data, '_, P>,
) -> Result {
    timing_phase!("Finalise per-object sizes");

    group_states.par_iter_mut().try_for_each(|state| {
        verbose_timing_phase!("Finalise sizes for group");
        state.finalise_sizes::<A>(per_symbol_flags, resources)
    })
}

pub(crate) fn merge_dynamic_symbol_definitions<'data, P: Platform>(
    group_states: &[GroupState<'data, P>],
    symbol_db: &SymbolDb<'data, P>,
) -> Result<Vec<DynamicSymbolDefinition<'data, P>>> {
    timing_phase!("Merge dynamic symbol definitions");

    let mut dynamic_symbol_definitions = Vec::new();
    for group in group_states {
        dynamic_symbol_definitions.extend(group.common.dynamic_symbol_definitions.iter().copied());
    }

    append_prelude_defsym_dynamic_symbols(
        group_states,
        symbol_db,
        &mut dynamic_symbol_definitions,
    )?;

    Ok(dynamic_symbol_definitions)
}

pub(crate) fn create_canonical_plt_entries<'data, P: Platform>(
    group_states: &[GroupState<'data, P>],
    symbol_db: &SymbolDb<'data, P>,
    per_symbol_flags: &AtomicPerSymbolFlags<'_>,
    dynamic_symbol_definitions: &mut Vec<DynamicSymbolDefinition<'data, P>>,
) -> Result {
    timing_phase!("Create canonical PLT entries");

    for group in group_states {
        for file in &group.files {
            let FileLayoutState::Dynamic(dynamic) = file else {
                continue;
            };

            for symbol_id in dynamic.symbol_id_range {
                if symbol_db.is_canonical(symbol_id)
                    && per_symbol_flags
                        .get_atomic(symbol_id)
                        .get()
                        .needs_canonical_plt()
                {
                    dynamic_symbol_definitions
                        .push(P::create_dynamic_symbol_definition(symbol_db, symbol_id)?);
                }
            }
        }
    }

    Ok(())
}

pub(crate) fn append_prelude_defsym_dynamic_symbols<'data, P: Platform>(
    group_states: &[GroupState<'data, P>],
    symbol_db: &SymbolDb<'data, P>,
    dynamic_symbol_definitions: &mut Vec<DynamicSymbolDefinition<'data, P>>,
) -> Result {
    if symbol_db.output_kind.needs_dynsym()
        && let Some(first_group) = group_states.first()
        && let Some(FileLayoutState::Prelude(prelude)) = first_group.files.first()
    {
        let symbol_id_range = prelude.symbol_id_range;
        for (index, def_info) in prelude
            .internal_symbols
            .symbol_definitions
            .iter()
            .enumerate()
        {
            if !matches!(def_info.placement, SymbolPlacement::Redirect(_)) {
                continue;
            }

            let symbol_id = symbol_id_range.offset_to_id(index);
            if !symbol_db.is_canonical(symbol_id)
                || dynamic_symbol_definitions
                    .iter()
                    .any(|def| def.symbol_id == symbol_id)
            {
                continue;
            }

            dynamic_symbol_definitions
                .push(P::create_dynamic_symbol_definition(symbol_db, symbol_id)?);
        }
    }

    Ok(())
}

pub(crate) fn compute_total_file_size(
    section_layouts: &OutputSectionMap<OutputRecordLayout>,
) -> u64 {
    let mut file_size = 0;
    section_layouts.for_each(|_, s| file_size = file_size.max(s.file_offset + s.file_size));
    file_size as u64
}

/// Computes how much to allocate for a particular resolution. This is intended for debug assertions
/// when we're writing, to make sure that we would have allocated memory before we write.
pub(crate) fn compute_allocations<P: Platform>(
    resolution: &Resolution<P>,
    output_kind: OutputKind,
    args: &P::Args,
) -> OutputSectionPartMap<u64> {
    let mut sizes =
        OutputSectionPartMap::with_dense_size(crate::part_id::regular_part_base::<P>().as_usize());
    P::allocate_resolution(resolution.flags, &mut sizes, output_kind, args);
    sizes
}

pub(crate) fn compute_total_section_part_sizes<'data, P: Platform>(
    group_states: &mut [GroupState<'data, P>],
    output_sections: &mut OutputSections<P>,
    output_order: &OutputOrder<'data>,
    program_segments: &ProgramSegments<P::ProgramSegmentDef>,
    per_symbol_flags: &mut PerSymbolFlags,
    must_keep_sections: OutputSectionMap<bool>,
    resources: &FinaliseSizesResources<'data, '_, P>,
) -> Result<(
    OutputSectionPartMap<u64>,
    Option<P::GdbIndexScanResult<'data>>,
)> {
    timing_phase!("Compute total section sizes");

    let mut total_sizes: OutputSectionPartMap<u64> = output_sections.new_part_map();
    for group_state in group_states.iter() {
        total_sizes.merge(&group_state.common.mem_sizes);
    }

    // Compute and allocate the .gdb_index section size if --gdb-index is enabled.
    let (gdb_index_size, gdb_index_data) = if resources.symbol_db.args.should_write_gdb_index() {
        P::compute_gdb_index_size(group_states)?
    } else {
        (0, None)
    };
    if gdb_index_size > 0 {
        let gdb_index = P::GDB_INDEX_SECTION_ID
            .expect("platform produced a GDB index without a GDB-index section");
        let first_group = group_states.first_mut().unwrap();
        first_group
            .common
            .mem_sizes
            .increment(gdb_index.base_part_id::<P>(), gdb_index_size);
        total_sizes.increment(gdb_index.base_part_id::<P>(), gdb_index_size);
    }

    // We need to apply late-stage adjustments for the epilogue before we do so for the prelude,
    // since the prelude needs to know if the .hash section will be written, which is decided by the
    // epilogue.
    let last_group = group_states.last_mut().unwrap();
    let Some(FileLayoutState::Epilogue(epilogue)) = last_group.files.last_mut() else {
        unreachable!();
    };

    epilogue.apply_late_size_adjustments(&mut last_group.common, &mut total_sizes, resources)?;
    relocate_gnu_build_id_allocation(
        output_sections,
        &mut total_sizes,
        &mut last_group.common.mem_sizes,
    );

    let first_group = group_states.first_mut().unwrap();
    let Some(FileLayoutState::Prelude(prelude)) = first_group.files.first_mut() else {
        unreachable!();
    };

    prelude.apply_late_size_adjustments(
        &mut first_group.common,
        &mut total_sizes,
        must_keep_sections,
        output_sections,
        output_order,
        program_segments,
        per_symbol_flags,
        resources,
    )?;

    let num_sections = prelude
        .header_info
        .as_ref()
        .expect("we should have computed header info by now")
        .num_output_sections_with_content;

    if P::requires_symtab_shndx(num_sections as usize) {
        for s in group_states.iter_mut() {
            P::compute_symtab_shndx_section_size(&mut s.common.mem_sizes, &mut total_sizes);
        }
    }

    Ok((total_sizes, gdb_index_data))
}

/// Move the generated GNU build-id note into the script section that matches
/// `.note.gnu.build-id`, or drop it when that name is discarded.
pub(crate) fn relocate_gnu_build_id_allocation<P: Platform>(
    output_sections: &mut OutputSections<P>,
    total_sizes: &mut OutputSectionPartMap<u64>,
    epilogue_sizes: &mut OutputSectionPartMap<u64>,
) {
    let Some(builtin) = P::NOTE_GNU_BUILD_ID_SECTION_ID else {
        return;
    };
    let Some(builtin_part) = P::single_part_id(builtin) else {
        return;
    };
    let size = total_sizes.get(builtin_part);
    if size == 0 {
        return;
    }

    match output_sections.gnu_build_id_placement {
        GnuBuildIdPlacement::Builtin => {}
        GnuBuildIdPlacement::Discard => {
            total_sizes.decrement(builtin_part, size);
            epilogue_sizes.decrement(builtin_part, size);
            output_sections.gnu_build_id_allocated = size;
        }
        GnuBuildIdPlacement::Merge(target) => {
            if target == builtin {
                return;
            }
            total_sizes.decrement(builtin_part, size);
            epilogue_sizes.decrement(builtin_part, size);
            output_sections.bump_min_alignment(target, alignment::NOTE_GNU_BUILD_ID);
            let dest = target.part_id_with_alignment::<P>(alignment::NOTE_GNU_BUILD_ID);
            total_sizes.increment(dest, size);
            epilogue_sizes.increment(dest, size);
            output_sections.gnu_build_id_allocated = size;
            let attr = output_sections
                .section_infos
                .get(builtin)
                .section_attributes;
            attr.apply(output_sections, target);
        }
    }
}

/// The epilogue still advances the builtin build-id cursor; redirect it after a merge or discard.
pub(crate) fn relocate_gnu_build_id_layout_offset<P: Platform>(
    memory_offsets: &mut OutputSectionPartMap<u64>,
    output_sections: &OutputSections<P>,
) {
    let size = output_sections.gnu_build_id_allocated;
    if size == 0 {
        return;
    }
    let Some(builtin) = P::NOTE_GNU_BUILD_ID_SECTION_ID else {
        return;
    };
    let Some(builtin_part) = P::single_part_id(builtin) else {
        return;
    };
    memory_offsets.decrement(builtin_part, size);
    if let Some(dest) = output_sections.gnu_build_id_dest_part() {
        memory_offsets.increment(dest, size);
    }
}

/// Allocates space for thunk blocks in each object that owns one.
pub(crate) fn allocate_thunk_block_space<P: Platform>(
    group_states: &mut [GroupState<P>],
    thunk_blocks: &[crate::thunks::ThunkBlock],
    total_sizes: &mut OutputSectionPartMap<u64>,
    symbol_db: &SymbolDb<P>,
) {
    if thunk_blocks.is_empty() {
        return;
    }

    verbose_timing_phase!("Apply thunk block sizes");

    let emit_symbols = !symbol_db.args.should_strip_all();

    for group_state in group_states.iter_mut() {
        let mut extra_thunk_sizes: OutputSectionPartMap<u64> = total_sizes.new_empty_like();
        for file in &group_state.files {
            if let FileLayoutState::Object(obj) = file
                && let Some(config) = P::file_thunk_config(obj.object)
                && obj.owns_thunk_block
            {
                let block = thunk_blocks.get(obj.thunk_block_id.as_usize());
                let count = block.map_or(0, |b| b.symbols.len());
                let size = count as u64 * config.thunk_size;
                extra_thunk_sizes.increment(config.primary_function_part_id, size);
                if emit_symbols && let Some(block) = block {
                    P::allocate_thunk_symbol_sizes(
                        &mut extra_thunk_sizes,
                        &block.symbols,
                        symbol_db,
                    );
                }
            }
        }
        group_state.common.mem_sizes.merge(&extra_thunk_sizes);
        total_sizes.merge(&extra_thunk_sizes);
    }
}

/// Propagates attributes from input sections to the output sections into which they were placed.
pub(crate) fn propagate_section_attributes<'data, P: Platform>(
    group_states: &[GroupState<'data, P>],
    output_sections: &mut OutputSections<P>,
) {
    timing_phase!("Propagate section attributes");

    for group_state in group_states {
        group_state
            .common
            .section_attributes
            .iter()
            .for_each(|(&section_id, attributes)| {
                attributes.apply(output_sections, section_id);
            });
    }
}

/// This is similar to computing start addresses, but is used for things that aren't addressable,
/// but which need to be unique. It's non parallel. It could potentially be run in parallel with
/// some of the stages that run after it, that don't need access to the file states.
pub(crate) fn apply_non_addressable_indexes<'data, P: Platform>(
    group_states: &mut [GroupState<'data, P>],
    symbol_db: &SymbolDb<'data, P>,
) -> Result<P::NonAddressableCounts> {
    timing_phase!("Apply non-addressable indexes");

    let mut indexes = P::NonAddressableIndexes::new(symbol_db);

    let mut counts = P::NonAddressableCounts::default();

    for g in group_states.iter_mut() {
        for s in &mut g.files {
            match s {
                FileLayoutState::Dynamic(s) => {
                    s.object.apply_non_addressable_indexes_dynamic(
                        &mut indexes,
                        &mut counts,
                        &mut s.format_specific,
                    )?;
                }
                FileLayoutState::Epilogue(s) => {
                    P::apply_non_addressable_indexes_epilogue(&mut counts, &mut s.format_specific);
                }
                _ => {}
            }
        }
    }

    P::apply_non_addressable_indexes(
        symbol_db,
        &counts,
        group_states.iter_mut().map(|g| &mut g.common.mem_sizes),
    );

    Ok(counts)
}

/// Returns the starting memory address for each alignment within each segment.
pub(crate) fn starting_memory_offsets(
    section_layouts: &OutputSectionPartMap<OutputRecordLayout>,
) -> OutputSectionPartMap<u64> {
    timing_phase!("Compute per-alignment offsets");

    section_layouts.map(|_, rec| rec.mem_offset)
}

pub(crate) fn compute_file_sizes<P: Platform>(
    mem_sizes: &OutputSectionPartMap<u64>,
    output_sections: &OutputSections<'_, P>,
) -> OutputSectionPartMap<usize> {
    mem_sizes.map(|part_id, size| {
        if output_sections.has_data_in_file(part_id.output_section_id::<P>()) {
            *size as usize
        } else {
            0
        }
    })
}

/// Verifies that we allocate and use consistent amounts of various output sections for the supplied
/// combination of flags and output kind. If this function returns an error, then we would have
/// failed during writing anyway. By failing now, we can report the particular combination of inputs
/// that caused the failure.
pub(crate) fn verify_consistent_allocation_handling<P: Platform, A: Arch<Platform = P>>(
    flags: ValueFlags,
    output_kind: OutputKind,
    args: &P::Args,
) -> Result {
    let output_sections = OutputSections::with_base_address(0, output_kind);
    let (output_order, _program_segments) = output_sections.output_order(output_kind, &[], &[])?;
    let mut mem_sizes = output_sections.new_part_map();
    P::allocate_resolution(flags, &mut mem_sizes, output_kind, args);
    let mut memory_offsets = output_sections.new_part_map();
    if let Some(section_id) = P::GOT_SECTION_ID {
        *memory_offsets.get_mut(section_id.base_part_id::<P>()) = 0x10;
    }
    if let Some(section_id) = P::GOT_RELR_SECTION_ID {
        *memory_offsets.get_mut(section_id.base_part_id::<P>()) = 0x10;
    }
    if let Some(section_id) = P::PLT_GOT_SECTION_ID {
        *memory_offsets.get_mut(section_id.base_part_id::<P>()) = 0x10;
    }
    let has_dynamic_symbol =
        flags.is_dynamic() || (flags.needs_export_dynamic() && flags.is_interposable());
    let dynamic_symbol_index = has_dynamic_symbol.then(|| NonZeroU32::new(1).unwrap());

    let resolution = P::create_resolution(
        flags,
        0,
        dynamic_symbol_index,
        &mut memory_offsets,
        args,
        output_kind,
    );

    P::verify_resolution_allocation::<A>(
        &output_sections,
        &output_order,
        output_kind,
        &mem_sizes,
        &resolution,
        args,
    )
    .with_context(|| {
        format!(
            "Inconsistent allocation detected. \
             output_kind={output_kind:?} \
             flags={flags} \
             has_dynamic_symbol={has_dynamic_symbol:?}"
        )
    })?;

    Ok(())
}
