use super::types::{
    FileLayoutState, FinaliseLayoutResources, GroupState, InputSectionPosition,
    InputSectionPositions, MAX_RELAXATION_ITERATIONS, MemoryRegion, ObjectLayoutState,
    OutputRecordLayout, RescanCandidates, RescanSections, ResolutionWriter,
    SYMBOL_ADDRESS_UNRESOLVED, SymbolOutputInfos,
};
use crate::expression_eval::ResolvedLocationCounter;
use crate::output_section_id::{OutputOrder, OutputSections};
use crate::output_section_part_map::OutputSectionPartMap;
use crate::resolution::SectionSlot;
use crate::symbol_db::{SymbolDb, SymbolId, SymbolIdRange};
use crate::{
    EnginePlatform, advance_section_offset, compute_and_apply_section_layout,
    compute_start_offsets_by_group, starting_memory_offsets, timing_phase, verbose_timing_phase,
};
use hashbrown::HashMap;
use linker_utils::relaxation::{SectionRelaxDeltas, opt_input_to_output};
use object::SectionIndex;
use rayon::iter::{
    IndexedParallelIterator, IntoParallelIterator, IntoParallelRefIterator,
    IntoParallelRefMutIterator, ParallelIterator,
};
use smallvec::SmallVec;
use wild_error::bail;
use wild_error::error::Result;
use wild_platform::output_section_map::OutputSectionMap;
use wild_platform::program_segments::ProgramSegments;
use wild_platform::value_flags::{PerSymbolFlags, ValueFlags};
use wild_platform::{Arch, ObjectFile, Platform, RelaxSymbolInfo, SectionHeader as _, Symbol as _};

pub fn default_create_resolutions<'data, P: EnginePlatform>(
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

pub fn compute_object_section_positions<'data, P: EnginePlatform>(
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

pub fn compute_input_section_positions<'data, P: EnginePlatform>(
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
pub fn compute_section_and_symbol_addresses<'data, P: EnginePlatform>(
    group_states: &[GroupState<'data, P>],
    section_part_layouts: &OutputSectionPartMap<OutputRecordLayout>,
    symbol_db: &SymbolDb<'data, P>,
    output_sections: &OutputSections<'data, P>,
    address_buf: &mut Vec<u64>,
) -> (InputSectionPositions, SymbolOutputInfos) {
    timing_phase!("Compute section and symbol addresses");
    let mem_offsets: OutputSectionPartMap<u64> = starting_memory_offsets(section_part_layouts);
    let starting_offsets = compute_start_offsets_by_group(group_states, mem_offsets);

    let mut addresses = std::mem::take(address_buf);
    addresses.clear();
    addresses.resize(symbol_db.num_symbols(), SYMBOL_ADDRESS_UNRESOLVED);

    let section_positions = {
        let shards = split_symbol_addresses_by_group(&mut addresses, symbol_db, group_states.len());

        group_states
            .par_iter()
            .zip(shards.into_par_iter())
            .enumerate()
            .map(|(group_idx, (group, (range_start, shard)))| {
                verbose_timing_phase!("Compute addresses for group");
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
                                let sym_id = obj
                                    .symbol_id_range
                                    .input_to_id(object::SymbolIndex(sym_offset));
                                if symbol_db.definition(sym_id) != sym_id {
                                    continue;
                                }
                                if let Some(addr) =
                                    canonical_symbol_output_address(obj, sym_id, &positions)
                                {
                                    shard[sym_id.as_usize() - range_start] = addr;
                                }
                            }

                            positions
                        }
                        _ => vec![],
                    })
                    .collect()
            })
            .collect()
    };

    (section_positions, SymbolOutputInfos { addresses })
}

fn canonical_symbol_output_address<P: EnginePlatform>(
    obj: &ObjectLayoutState<P>,
    canonical_id: SymbolId,
    positions: &[Option<InputSectionPosition>],
) -> Option<u64> {
    let local = canonical_id.to_input(obj.symbol_id_range);
    let sym = obj.object.symbol(local).ok()?;
    match obj.object.symbol_section(sym, local) {
        Ok(Some(section)) => {
            let sec_addr = positions.get(section.0).copied().flatten()?;
            let input_offset = obj.object.symbol_offset_in_section(sym, section).ok()?;
            let output_offset =
                opt_input_to_output(obj.section_relax_deltas.get(section.0), input_offset);
            Some(sec_addr.address + output_offset)
        }
        Ok(None) if sym.is_absolute() => Some(sym.value()),
        _ => None,
    }
}

fn collect_needed_relaxation_symbols<A: Arch>(
    group_states: &[GroupState<A::Platform>],
    rescan: &RescanSections,
    symbol_db: &SymbolDb<A::Platform>,
) -> Vec<Vec<SymbolId>>
where
    A::Platform: EnginePlatform,
{
    let referenced: Vec<SymbolId> = group_states
        .par_iter()
        .enumerate()
        .flat_map(|(group_idx, group)| {
            let mut ids = Vec::new();
            for (file_idx, file) in group.files.iter().enumerate() {
                let FileLayoutState::Object(obj) = file else {
                    continue;
                };
                let Some(sections) = rescan.get(group_idx).and_then(|files| files.get(file_idx))
                else {
                    continue;
                };
                for &sec_idx in sections {
                    let Ok(relocs) = obj
                        .object
                        .relocations(SectionIndex(sec_idx), &obj.relocations)
                    else {
                        continue;
                    };
                    for sym_idx in A::collect_relaxation_referenced_symbols(
                        relocs,
                        obj.section_relax_deltas.get(sec_idx),
                    ) {
                        let local_id = obj.symbol_id_range.input_to_id(sym_idx);
                        ids.push(symbol_db.definition(local_id));
                    }
                }
            }
            ids
        })
        .collect();

    // Bucket by the defining group so each shard is written only by its owner.
    let mut by_group = vec![Vec::new(); group_states.len()];
    for id in referenced {
        let group_idx = symbol_db.file_id_for_symbol(id).group();
        if let Some(bucket) = by_group.get_mut(group_idx) {
            bucket.push(id);
        }
    }
    by_group
}

fn compute_selected_symbol_addresses<'data, P: EnginePlatform>(
    group_states: &[GroupState<'data, P>],
    section_part_layouts: &OutputSectionPartMap<OutputRecordLayout>,
    symbol_db: &SymbolDb<'data, P>,
    output_sections: &OutputSections<'data, P>,
    address_buf: &mut Vec<u64>,
    needed_by_group: &[Vec<SymbolId>],
) -> (InputSectionPositions, SymbolOutputInfos) {
    timing_phase!("Compute section and symbol addresses");
    let mem_offsets: OutputSectionPartMap<u64> = starting_memory_offsets(section_part_layouts);
    let section_positions =
        compute_input_section_positions(group_states, mem_offsets, symbol_db, output_sections);

    let mut addresses = std::mem::take(address_buf);
    addresses.clear();
    addresses.resize(symbol_db.num_symbols(), SYMBOL_ADDRESS_UNRESOLVED);

    let shards = split_symbol_addresses_by_group(&mut addresses, symbol_db, group_states.len());
    group_states
        .par_iter()
        .zip(shards.into_par_iter())
        .zip(needed_by_group.par_iter())
        .for_each(|((group, (range_start, shard)), needed)| {
            verbose_timing_phase!("Compute addresses for group");
            for &sym_id in needed {
                let file_id = symbol_db.file_id_for_symbol(sym_id);
                let Some(FileLayoutState::Object(obj)) = group.files.get(file_id.file()) else {
                    continue;
                };
                let Some(positions) = section_positions
                    .get(file_id.group())
                    .and_then(|files| files.get(file_id.file()))
                else {
                    continue;
                };
                if let Some(addr) = canonical_symbol_output_address(obj, sym_id, positions) {
                    debug_assert!(sym_id.as_usize() >= range_start);
                    let idx = sym_id.as_usize() - range_start;
                    shard[idx] = addr;
                }
            }
        });

    (section_positions, SymbolOutputInfos { addresses })
}

fn group_symbol_id_range<P: EnginePlatform>(
    group_idx: usize,
    symbol_db: &SymbolDb<P>,
) -> std::ops::Range<usize> {
    symbol_db.groups.get(group_idx).map_or(0..0, |group| {
        let range = group.symbol_id_range();
        let start = range.start().as_usize();
        start..start + range.len()
    })
}

fn split_symbol_addresses_by_group<'a, P: EnginePlatform>(
    addresses: &'a mut [u64],
    symbol_db: &SymbolDb<P>,
    num_groups: usize,
) -> Vec<(usize, &'a mut [u64])> {
    let ranges: Vec<std::ops::Range<usize>> = (0..num_groups)
        .map(|i| group_symbol_id_range(i, symbol_db))
        .collect();
    split_ordered_symbol_ranges(addresses, &ranges)
}

/// Splits `addresses` according to disjoint, increasing `ranges`. Empty ranges get an empty shard
/// and do not consume the slice.
fn split_ordered_symbol_ranges<'a>(
    mut rest: &'a mut [u64],
    ranges: &[std::ops::Range<usize>],
) -> Vec<(usize, &'a mut [u64])> {
    debug_assert!(
        ranges
            .iter()
            .filter(|range| !range.is_empty())
            .collect::<Vec<_>>()
            .windows(2)
            .all(|window| window[0].end <= window[1].start),
        "group symbol ranges must be disjoint and ordered"
    );

    let mut cursor = 0;
    let mut shards = Vec::with_capacity(ranges.len());
    for range in ranges {
        if range.is_empty() {
            shards.push((0, &mut [][..]));
            continue;
        }
        debug_assert!(
            range.start >= cursor,
            "group symbol ranges must be in increasing order"
        );
        let (_gap, after_gap) = rest.split_at_mut(range.start - cursor);
        let (shard, after) = after_gap.split_at_mut(range.len());
        shards.push((range.start, shard));
        rest = after;
        cursor = range.end;
    }
    shards
}

pub fn resolve_early_object_symbol<'data, P: EnginePlatform>(
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
pub fn relaxation_scan_pass<'data, A: Arch>(
    group_states: &mut [GroupState<'data, A::Platform>],
    section_part_layouts: &OutputSectionPartMap<OutputRecordLayout>,
    symbol_db: &SymbolDb<'data, A::Platform>,
    per_symbol_flags: &PerSymbolFlags,
    section_part_sizes: &mut OutputSectionPartMap<u64>,
    prev_rescan: Option<&RescanSections>,
    output_sections: &OutputSections<'data, A::Platform>,
    address_buf: &mut Vec<u64>,
) -> (u64, RescanCandidates)
where
    A::Platform: EnginePlatform,
{
    timing_phase!("Relaxation scan pass");

    let (section_addresses, symbol_infos) = if let Some(rescan) = prev_rescan {
        let needed = collect_needed_relaxation_symbols::<A>(group_states, rescan, symbol_db);
        compute_selected_symbol_addresses(
            group_states,
            section_part_layouts,
            symbol_db,
            output_sections,
            address_buf,
            &needed,
        )
    } else {
        compute_section_and_symbol_addresses(
            group_states,
            section_part_layouts,
            symbol_db,
            output_sections,
            address_buf,
        )
    };

    // Scan each group.
    #[expect(clippy::type_complexity)]
    let group_results: Vec<(OutputSectionPartMap<u64>, Vec<SmallVec<[(usize, u64); 16]>>)> =
        group_states
            .par_iter_mut()
            .enumerate()
            .map(|(group_idx, group)| {
                verbose_timing_phase!("Relaxation scan for group");
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

    // Give the allocation back so the next relaxation pass can reuse it.
    *address_buf = symbol_infos.addresses;

    (total_deleted, next_rescan_candidates)
}

pub fn perform_iterative_relaxation<'data, A: Arch>(
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
    let mut address_buf = Vec::new();

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
            &mut address_buf,
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
