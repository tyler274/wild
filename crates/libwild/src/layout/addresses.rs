use super::sections::*;
use super::sizes::*;
use super::types::*;
use crate::bail;
use crate::error::Result;
use crate::expression_eval::ResolvedLocationCounter;
use crate::layout::EnginePlatform;
use crate::output_section_id::OutputOrder;
use crate::output_section_id::OutputSections;
use crate::output_section_map::OutputSectionMap;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::platform::Arch;
use crate::platform::ObjectFile;
use crate::platform::Platform;
use crate::platform::RelaxSymbolInfo;
use crate::platform::SectionHeader as _;
use crate::platform::Symbol as _;
use crate::program_segments::ProgramSegments;
use crate::resolution::SectionSlot;
use crate::symbol_db::SymbolDb;
use crate::symbol_db::SymbolId;
use crate::symbol_db::SymbolIdRange;
use crate::timing_phase;
use crate::value_flags::PerSymbolFlags;
use crate::value_flags::ValueFlags;
use hashbrown::HashMap;
use linker_utils::relaxation::SectionRelaxDeltas;
use linker_utils::relaxation::opt_input_to_output;
use object::SectionIndex;
use rayon::iter::IndexedParallelIterator;
use rayon::iter::IntoParallelRefIterator;
use rayon::iter::IntoParallelRefMutIterator;
use rayon::iter::ParallelIterator;
use smallvec::SmallVec;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;

pub(crate) fn default_create_resolutions<'data, P: EnginePlatform>(
    memory_offsets: &mut OutputSectionPartMap<u64>,
    resolutions_out: &mut ResolutionWriter<'_, '_, P>,
    resources: &FinaliseLayoutResources<'_, 'data, P>,
    symbol_id_range: SymbolIdRange,
) -> Result {
    for symbol_id in symbol_id_range {
        let flags: ValueFlags = resources
            .symbol_db
            .flags_for_symbol(resources.per_symbol_flags, symbol_id);
        if flags.has_resolution() && resources.symbol_db.is_canonical(symbol_id) {
            resolutions_out.write(Some(P::create_resolution(
                flags,
                0,
                None,
                memory_offsets,
                resources.symbol_db.args,
                resources.symbol_db.output_kind,
            )))?;
        } else {
            resolutions_out.write(None)?;
        }
    }

    Ok(())
}

pub(crate) fn compute_object_section_positions<'data, P: EnginePlatform>(
    obj: &ObjectLayoutState<'data, P>,
    offsets: &mut OutputSectionPartMap<u64>,
    symbol_db: &SymbolDb<'data, P>,
    output_sections: &OutputSections<P>,
) -> Vec<Option<InputSectionPosition>> {
    let mut positions = vec![None; obj.sections.len()];
    for (sec_idx, slot) in obj
        .sections
        .iter()
        .enumerate()
        .map(|(idx, slot)| (object::SectionIndex(idx), slot))
    {
        match slot {
            SectionSlot::Loaded(sec) => {
                let part_id = obj.section_part_id(sec_idx, &symbol_db.section_part_ids);
                let mut offset = offsets.get(part_id);
                let address = advance_section_offset(&mut offset, *sec, part_id, output_sections);
                *offsets.get_mut(part_id) = offset;
                positions[sec_idx.0] = Some(InputSectionPosition { part_id, address });
            }
            SectionSlot::LoadedDebugInfo(sec) => {
                // Advance offsets so subsequent sections are placed correctly, but we don't need
                // the address for relaxation.
                let part_id = obj.section_part_id(sec_idx, &symbol_db.section_part_ids);
                let mut offset = offsets.get(part_id);
                advance_section_offset(&mut offset, *sec, part_id, output_sections);
                *offsets.get_mut(part_id) = offset;
            }
            _ => {}
        }
    }

    P::compute_object_addresses(obj, offsets);

    positions
}

pub(crate) fn compute_input_section_positions<'data, P: EnginePlatform>(
    group_states: &[GroupState<'data, P>],
    mem_offsets: OutputSectionPartMap<u64>,
    symbol_db: &SymbolDb<'data, P>,
    output_sections: &OutputSections<P>,
) -> InputSectionPositions {
    let starting_offsets = compute_start_offsets_by_group(group_states, mem_offsets);

    group_states
        .par_iter()
        .enumerate()
        .map(|(group_idx, group)| {
            let mut offsets = starting_offsets[group_idx].clone();

            group
                .files
                .iter()
                .map(|file| match file {
                    FileLayoutState::Object(obj) => compute_object_section_positions(
                        obj,
                        &mut offsets,
                        symbol_db,
                        output_sections,
                    ),
                    _ => vec![],
                })
                .collect()
        })
        .collect()
}

/// Compute the output address of every loaded input section and every symbol in a single parallel
/// pass over groups.
pub(crate) fn compute_section_and_symbol_addresses<'data, P: EnginePlatform>(
    group_states: &[GroupState<'data, P>],
    section_part_layouts: &OutputSectionPartMap<OutputRecordLayout>,
    symbol_db: &SymbolDb<'data, P>,
    output_sections: &OutputSections<'data, P>,
) -> (InputSectionPositions, SymbolOutputInfos) {
    timing_phase!("Compute section and symbol addresses");
    let mem_offsets: OutputSectionPartMap<u64> = starting_memory_offsets(section_part_layouts);
    let starting_offsets = compute_start_offsets_by_group(group_states, mem_offsets);

    let symbol_addresses: Vec<AtomicU64> = (0..symbol_db.num_symbols())
        .map(|_| AtomicU64::new(SYMBOL_ADDRESS_UNRESOLVED))
        .collect();

    let section_positions = group_states
        .par_iter()
        .enumerate()
        .map(|(group_idx, group)| {
            let mut offsets = starting_offsets[group_idx].clone();

            group
                .files
                .iter()
                .map(|file| match file {
                    FileLayoutState::Object(obj) => {
                        let positions = compute_object_section_positions(
                            obj,
                            &mut offsets,
                            symbol_db,
                            output_sections,
                        );

                        // While we have the section addresses, also resolve symbol
                        // output addresses for this file's canonical definitions.
                        for sym_offset in 0..obj.symbol_id_range.len() {
                            let sym_input_idx = object::SymbolIndex(sym_offset);
                            let Ok(sym) = obj.object.symbol(sym_input_idx) else {
                                continue;
                            };
                            let sym_id = obj.symbol_id_range.input_to_id(sym_input_idx);
                            let def_id = symbol_db.definition(sym_id);
                            // Only record the address for the canonical definition.
                            if def_id != sym_id {
                                continue;
                            }

                            match obj.object.symbol_section(sym, sym_input_idx) {
                                Ok(Some(section)) => {
                                    let Some(sec_addr) =
                                        positions.get(section.0).copied().flatten()
                                    else {
                                        continue;
                                    };
                                    let Ok(input_offset) =
                                        obj.object.symbol_offset_in_section(sym, section)
                                    else {
                                        continue;
                                    };
                                    let output_offset = opt_input_to_output(
                                        obj.section_relax_deltas.get(section.0),
                                        input_offset,
                                    );
                                    symbol_addresses[sym_id.as_usize()]
                                        .store(sec_addr.address + output_offset, Relaxed);
                                }
                                Ok(None) if sym.is_absolute() => {
                                    symbol_addresses[sym_id.as_usize()].store(sym.value(), Relaxed);
                                }
                                _ => {}
                            }
                        }

                        positions
                    }
                    _ => vec![],
                })
                .collect()
        })
        .collect();

    let addresses = symbol_addresses
        .into_iter()
        .map(|a| a.into_inner())
        .collect();

    (section_positions, SymbolOutputInfos { addresses })
}

pub(crate) fn resolve_early_object_symbol<'data, P: EnginePlatform>(
    canonical_id: SymbolId,
    obj: &ObjectLayoutState<'data, P>,
    section_positions: &InputSectionPositions,
    symbol_db: &SymbolDb<'data, P>,
) -> Result<crate::expression_eval::SymbolValue> {
    let file_id = symbol_db.file_id_for_symbol(canonical_id);
    let local_index = canonical_id.to_input(obj.symbol_id_range);
    let symbol = obj.object.symbol(local_index)?;
    let Some(section_index) = obj.object.symbol_section(symbol, local_index)? else {
        if symbol.is_absolute() {
            return Ok(crate::expression_eval::SymbolValue::Absolute(
                symbol.value(),
            ));
        }
        bail!(
            "cannot resolve address of symbol '{}'",
            symbol_db.symbol_name_for_display(canonical_id)
        );
    };

    let section_position = section_positions
        .get(file_id.group())
        .and_then(|group| group.get(file_id.file()))
        .and_then(|file| file.get(section_index.0))
        .copied()
        .flatten();

    let Some(section_position) = section_position else {
        if matches!(
            obj.sections.get(section_index.0),
            Some(SectionSlot::Sorted(_))
        ) {
            bail!(
                "Early evaluation of sorted section {} is not supported",
                obj.object.section_display_name(section_index)
            );
        }
        bail!(
            "cannot resolve address of symbol '{}' because its section does not have an early layout",
            symbol_db.symbol_name_for_display(canonical_id)
        );
    };

    let input_offset = obj.object.symbol_offset_in_section(symbol, section_index)?;
    let output_offset =
        opt_input_to_output(obj.section_relax_deltas.get(section_index.0), input_offset);
    Ok(crate::expression_eval::SymbolValue::PartRelative {
        part_id: section_position.part_id,
        offset: section_position.address + output_offset,
    })
}

/// Run one pass of the relaxation scan across all groups/objects.  Returns the total number of
/// bytes newly deleted in this pass together with the set of sections that should be rescanned on
/// the next iteration.
pub(crate) fn relaxation_scan_pass<'data, A: Arch>(
    group_states: &mut [GroupState<'data, A::Platform>],
    section_part_layouts: &OutputSectionPartMap<OutputRecordLayout>,
    symbol_db: &SymbolDb<'data, A::Platform>,
    per_symbol_flags: &PerSymbolFlags,
    section_part_sizes: &mut OutputSectionPartMap<u64>,
    prev_rescan: Option<&RescanSections>,
    output_sections: &OutputSections<'data, A::Platform>,
) -> (u64, RescanCandidates)
where
    A::Platform: EnginePlatform,
{
    timing_phase!("Relaxation scan pass");

    let (section_addresses, symbol_infos) = compute_section_and_symbol_addresses(
        group_states,
        section_part_layouts,
        symbol_db,
        output_sections,
    );

    // Scan each group.
    #[expect(clippy::type_complexity)]
    let group_results: Vec<(OutputSectionPartMap<u64>, Vec<SmallVec<[(usize, u64); 16]>>)> =
        group_states
            .par_iter_mut()
            .enumerate()
            .map(|(group_idx, group)| {
                let mut reductions = section_part_sizes.new_empty_like();
                let mut file_rescans: Vec<SmallVec<[(usize, u64); 16]>> =
                    Vec::with_capacity(group.files.len());

                for (file_idx, file) in group.files.iter_mut().enumerate() {
                    let FileLayoutState::Object(obj) = file else {
                        file_rescans.push(SmallVec::new());
                        continue;
                    };

                    let file_section_addrs = &section_addresses[group_idx][file_idx];

                    let sections_to_scan: SmallVec<[usize; 16]> = match prev_rescan {
                        Some(rescan) => rescan[group_idx][file_idx].clone(),
                        None => obj
                            .sections
                            .iter()
                            .enumerate()
                            .filter_map(|(i, slot)| {
                                if let SectionSlot::Loaded(_) = slot
                                    && let Ok(header) = obj.object.section(SectionIndex(i))
                                    && header.is_executable()
                                {
                                    Some(i)
                                } else {
                                    None
                                }
                            })
                            .collect(),
                    };

                    let mut next_rescan: SmallVec<[(usize, u64); 16]> = SmallVec::new();

                    for sec_idx in &sections_to_scan {
                        let sec_idx = *sec_idx;
                        let section_index = SectionIndex(sec_idx);
                        let Ok(relocs) = obj.object.relocations(section_index, &obj.relocations)
                        else {
                            continue;
                        };

                        let Some(sec_output_addr) = file_section_addrs
                            .get(sec_idx)
                            .copied()
                            .flatten()
                            .map(|section| section.address)
                        else {
                            continue;
                        };

                        let existing_deltas = obj.section_relax_deltas.get(sec_idx);

                        // Symbol resolver: look up the canonical definition's output
                        // address via the precomputed table.
                        let mut resolve_symbol =
                            |sym_idx: object::SymbolIndex| -> Option<RelaxSymbolInfo> {
                                let local_id = obj.symbol_id_range.input_to_id(sym_idx);
                                let def_id = symbol_db.definition(local_id);
                                symbol_infos.resolve(def_id, per_symbol_flags)
                            };

                        let Ok(section_header) = obj.object.section(section_index) else {
                            continue;
                        };
                        let Ok(section_bytes) = obj.object.raw_section_data(section_header) else {
                            continue;
                        };

                        let (raw_deltas, min_margin) = A::collect_relaxation_deltas(
                            sec_output_addr,
                            section_bytes,
                            relocs,
                            existing_deltas,
                            &mut resolve_symbol,
                        );

                        if let Some(margin) = min_margin {
                            next_rescan.push((sec_idx, margin));
                        }

                        if raw_deltas.is_empty() {
                            continue;
                        }

                        let new_total_deleted: u64 =
                            raw_deltas.iter().map(|(_, b)| u64::from(*b)).sum();

                        if let SectionSlot::Loaded(sec) = &mut obj.sections[sec_idx] {
                            let part_id = symbol_db.section_part_ids
                                [obj.section_id_range.start().as_usize() + sec_idx];
                            let old_capacity = sec.capacity(part_id, output_sections);
                            sec.size -= new_total_deleted;
                            let new_capacity = sec.capacity(part_id, output_sections);
                            debug_assert!(old_capacity >= new_capacity);
                            let capacity_reduction = old_capacity - new_capacity;
                            if capacity_reduction > 0 {
                                group
                                    .common
                                    .mem_sizes
                                    .decrement(part_id, capacity_reduction);
                                *reductions.get_mut(part_id) += capacity_reduction;
                            }
                        }

                        if let Some(existing) = obj.section_relax_deltas.get_mut(sec_idx) {
                            existing.merge_additional(raw_deltas);
                        } else {
                            obj.section_relax_deltas
                                .insert_sorted(sec_idx, SectionRelaxDeltas::new(raw_deltas));
                        }
                    }

                    file_rescans.push(next_rescan);
                }

                (reductions, file_rescans)
            })
            .collect();

    let mut total_deleted = 0u64;
    let mut next_rescan_candidates: RescanCandidates = Vec::with_capacity(group_results.len());
    for (reduction, file_rescans) in group_results {
        for (part_id, &amount) in reduction.iter() {
            if amount > 0 {
                section_part_sizes.decrement(part_id, amount);
                total_deleted += amount;
            }
        }
        next_rescan_candidates.push(file_rescans);
    }

    (total_deleted, next_rescan_candidates)
}

pub(crate) fn perform_iterative_relaxation<'data, A: Arch>(
    group_states: &mut [GroupState<'data, A::Platform>],
    section_part_sizes: &mut OutputSectionPartMap<u64>,
    section_part_layouts: &mut OutputSectionPartMap<OutputRecordLayout>,
    section_layouts: &mut OutputSectionMap<OutputRecordLayout>,
    output_sections: &OutputSections<'data, A::Platform>,
    program_segments: &ProgramSegments<<A::Platform as Platform>::ProgramSegmentDef>,
    output_order: &OutputOrder<'data>,
    symbol_db: &SymbolDb<'data, A::Platform>,
    per_symbol_flags: &PerSymbolFlags,
    memory_regions: &mut HashMap<&[u8], MemoryRegion>,
    memory_region_order: &[&[u8]],
    sizeof_headers: u64,
    resolved_location_counters: &mut Vec<ResolvedLocationCounter>,
) -> Result
where
    A::Platform: EnginePlatform,
{
    timing_phase!("Iterative relaxation");

    let mut rescan_sections: Option<RescanSections> = None;

    for _iteration in 0..MAX_RELAXATION_ITERATIONS {
        if let Some(ref rescan) = rescan_sections
            && rescan
                .iter()
                .all(|files| files.iter().all(|secs| secs.is_empty()))
        {
            break;
        }

        let (deleted, next_candidates) = relaxation_scan_pass::<A>(
            group_states,
            section_part_layouts,
            symbol_db,
            per_symbol_flags,
            section_part_sizes,
            rescan_sections.as_ref(),
            output_sections,
        );

        if deleted == 0 {
            break;
        }

        // Filter the rescan candidates: only keep sections whose closest
        // unrelaxed candidate is within `deleted` bytes of the relaxation
        // boundary.  Candidates further away cannot possibly succeed because
        // addresses shift by at most `deleted` bytes per iteration.
        rescan_sections = Some(
            next_candidates
                .into_iter()
                .map(|files| {
                    files
                        .into_iter()
                        .map(|secs| {
                            secs.into_iter()
                                .filter(|&(_, margin)| margin <= deleted)
                                .map(|(idx, _)| idx)
                                .collect()
                        })
                        .collect()
                })
                .collect(),
        );

        (
            *section_part_layouts,
            *section_layouts,
            *resolved_location_counters,
        ) = compute_and_apply_section_layout::<A::Platform>(
            group_states,
            section_part_sizes,
            output_sections,
            program_segments,
            output_order,
            symbol_db,
            memory_regions,
            memory_region_order,
            sizeof_headers,
        )?;
    }
    Ok(())
}
