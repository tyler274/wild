use super::addresses::*;
use super::script::*;
use super::types::*;
use crate::alignment;
use crate::alignment::Alignment;
use crate::bail;
use crate::ensure;
use crate::error;
use crate::error::Context;
use crate::error::Result;
use crate::expression_eval::ResolvedLocationCounter;
use crate::expression_eval::ResolvedSymbolValue;
use crate::layout_rules::SectionKind;
use crate::linker_script::Expression;
use crate::output_section_id;
use crate::output_section_id::OrderEvent;
use crate::output_section_id::OutputOrder;
use crate::output_section_id::OutputSectionId;
use crate::output_section_id::OutputSections;
use crate::output_section_map::OutputSectionMap;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::parsing::SymbolLoc;
use crate::part_id::PartId;
use crate::platform::Args as _;
use crate::platform::Platform;
use crate::platform::SectionAttributes as _;
use crate::platform::SectionFlags as _;
use crate::program_segments::ProgramSegmentId;
use crate::program_segments::ProgramSegments;
use crate::resolution::SectionSlot;
use crate::string_merging::MergedStringsSection;
use crate::symbol::UnversionedSymbolName;
use crate::symbol_db::SymbolDb;
use crate::timing_phase;
use crate::verbose_timing_phase;
use hashbrown::HashMap;
use itertools::Itertools;
use object::SectionIndex;
use rayon::iter::IndexedParallelIterator;
use rayon::iter::IntoParallelIterator;
use rayon::iter::ParallelIterator;
use std::cell::OnceCell;
use std::mem::take;

pub(crate) fn layout_section_from_part_layouts<'data, P: Platform>(
    part: &OutputRecordLayout,
    section_layout: &mut OutputRecordLayout,
    section_info: &output_section_id::SectionOutputInfo<'data, P>,
    is_first_part: bool,
) {
    if is_first_part {
        *section_layout = *part;
        section_layout.alignment = section_info.min_alignment;
        if part.mem_size > 0 {
            section_layout.alignment = section_layout.alignment.max(part.alignment);
        }
        return;
    }

    let file_offset = section_layout.file_offset.min(part.file_offset);
    let mem_offset = section_layout.mem_offset.min(part.mem_offset);
    let lma_offset = section_layout.lma_offset.min(part.lma_offset);

    let file_size = section_layout.file_end().max(part.file_end()) - file_offset;
    let mem_size = section_layout.mem_end().max(part.mem_end()) - mem_offset;

    let alignment = if part.mem_size > 0 {
        section_layout.alignment.max(part.alignment)
    } else {
        section_layout.alignment
    };

    *section_layout = OutputRecordLayout {
        file_size,
        mem_size,
        alignment,
        file_offset,
        mem_offset,
        lma_offset,
    };
}

pub(crate) fn merge_secondary_parts<P: Platform>(
    output_sections: &OutputSections<P>,
    section_layouts: &OutputSectionMap<OutputRecordLayout>,
) -> OutputSectionMap<OutputRecordLayout> {
    verbose_timing_phase!("Merge secondary parts");

    let mut merged = section_layouts.clone();

    for (id, info) in output_sections.ids_with_info() {
        if let SectionKind::Secondary(primary_id) = info.kind {
            let secondary_layout = take(merged.get_mut(id));
            let primary = merged.get_mut(primary_id);
            let has_location_counters = info
                .location_info
                .as_ref()
                .is_some_and(|li| li.location_counters.0 < li.location_counters.1);
            if has_location_counters {
                // An empty primary is an artifact of splitting on `. = ALIGN(...)`
                // before the first matcher (kernel `.data_nosave`). Keep its VMA
                // but take the file offset of the first file-backed secondary so
                // `sh_offset` matches `p_offset ≡ p_vaddr` (GNU ld). Do not adopt
                // a trailing empty ALIGN secondary, which sits at the hole's end.
                if primary.file_size == 0
                    && primary.mem_size == 0
                    && secondary_layout.file_size > 0
                    && secondary_layout.file_offset > primary.file_offset
                {
                    primary.file_offset = secondary_layout.file_offset;
                    primary.lma_offset = secondary_layout.lma_offset;
                }
                let mem_end = secondary_layout.mem_offset + secondary_layout.mem_size;
                if mem_end > primary.mem_offset + primary.mem_size {
                    primary.mem_size = mem_end - primary.mem_offset;
                }
                let file_end = secondary_layout.file_offset + secondary_layout.file_size;
                if file_end > primary.file_offset + primary.file_size {
                    primary.file_size = file_end - primary.file_offset;
                }
                if secondary_layout.mem_size > 0 {
                    primary.alignment = primary.alignment.max(secondary_layout.alignment);
                }
            } else {
                primary.merge(&secondary_layout);
            }
        }
    }

    merged
}

pub(crate) fn compute_start_offsets_by_group<P: Platform>(
    group_states: &[GroupState<P>],
    mut mem_offsets: OutputSectionPartMap<u64>,
) -> Vec<OutputSectionPartMap<u64>> {
    timing_phase!("Compute per-group start offsets");

    let mut indices: Vec<usize> = (0..group_states.len()).collect();
    indices.sort_by_key(|&i| (group_states[i].section_group_order, i));

    let mut starts = Vec::new();
    starts.resize_with(group_states.len(), || mem_offsets.new_empty_like());
    for i in indices {
        starts[i] = mem_offsets.merge_and_return_start_offsets(&group_states[i].common.mem_sizes);
    }
    starts
}

pub(crate) fn compute_symbols_and_layouts<'data, P: Platform>(
    group_states: Vec<GroupState<'data, P>>,
    starting_mem_offsets_by_group: Vec<OutputSectionPartMap<u64>>,
    per_group_res_writers: &mut [sharded_vec_writer::Shard<Option<Resolution<P>>>],
    resources: &FinaliseLayoutResources<'_, 'data, P>,
) -> Result<Vec<GroupLayout<'data, P>>> {
    timing_phase!("Assign symbol addresses");

    group_states
        .into_par_iter()
        .zip(starting_mem_offsets_by_group)
        .zip(per_group_res_writers)
        .map(|((state, mut memory_offsets), symbols_out)| {
            verbose_timing_phase!("Assign addresses for group");

            if cfg!(debug_assertions) {
                let offset_verifier = crate::verification::OffsetVerifier::new::<P>(
                    &memory_offsets,
                    &state.common.mem_sizes,
                );

                // Make sure that ignored offsets really aren't used by `finalise_layout` by setting
                // them to an arbitrary value. If they are used, we'll quickly notice.
                crate::verification::clear_ignored::<P>(&mut memory_offsets);

                let layout = state.finalise_layout(&mut memory_offsets, symbols_out, resources)?;

                offset_verifier.verify(
                    &memory_offsets,
                    resources.output_sections,
                    resources.output_order,
                    &layout.files,
                )?;
                Ok(layout)
            } else {
                state.finalise_layout(&mut memory_offsets, symbols_out, resources)
            }
        })
        .collect()
}

pub(crate) fn compute_segment_layout<'data, P: Platform>(
    section_layouts: &OutputSectionMap<OutputRecordLayout>,
    output_sections: &OutputSections<P>,
    output_order: &OutputOrder<'data>,
    program_segments: &ProgramSegments<P::ProgramSegmentDef>,
    header_info: &HeaderInfo,
    args: &P::Args,
) -> Result<SegmentLayouts> {
    #[derive(Clone)]
    struct Record {
        segment_id: ProgramSegmentId,
        file_start: usize,
        file_end: usize,
        mem_start: u64,
        mem_end: u64,
        lma_start: u64,
        lma_end: u64,
        alignment: Alignment,
    }

    timing_phase!("Compute segment layouts");

    use output_section_id::OrderEvent;
    let mut complete = Vec::with_capacity(program_segments.len());
    let mut active_segments = vec![None; program_segments.len()];

    if args.should_output_partial_object() {
        return Ok(SegmentLayouts::default());
    }

    for event in output_order {
        match event {
            OrderEvent::SegmentStart(segment_id) => {
                if program_segments.is_stack_segment(segment_id) {
                    // STACK segment is special as it does not contain any section.
                    active_segments[segment_id.as_usize()] = Some(Record {
                        segment_id,
                        file_start: 0,
                        file_end: 0,
                        mem_start: 0,
                        mem_end: args.stack_size_override().map_or(0, |size| size.get()),
                        lma_start: 0,
                        lma_end: args.stack_size_override().map_or(0, |size| size.get()),
                        alignment: alignment::MIN,
                    });
                } else {
                    active_segments[segment_id.as_usize()] = Some(Record {
                        segment_id,
                        file_start: usize::MAX,
                        file_end: 0,
                        mem_start: u64::MAX,
                        mem_end: 0,
                        lma_start: u64::MAX,
                        lma_end: 0,
                        alignment: alignment::MIN,
                    });
                }
            }
            OrderEvent::SegmentEnd(segment_id) => {
                let record = active_segments[segment_id.as_usize()]
                    .take()
                    .context("SegmentEnd without matching SegmentStart")?;

                complete.push(record);
            }
            OrderEvent::Section(section_id) => {
                let section_layout = section_layouts.get(section_id);
                let merge_target = output_sections.primary_output_section(section_id);

                // Skip all ignored sections that will not end up in the final file.
                if section_layout.file_size == 0
                    && section_layout.mem_size == 0
                    && output_sections.output_section_indexes[merge_target.as_usize()].is_none()
                {
                    continue;
                }
                let section_flags = output_sections.section_flags(merge_target);
                let section_info = output_sections.output_info(section_id);

                if active_segments.iter().all(|s| s.is_none()) {
                    if output_order.has_custom_phdrs() {
                        continue;
                    }
                    ensure!(
                        section_layout.mem_offset == 0,
                        "Expected zero address for section {} not present in any program segment.",
                        output_sections.section_debug(section_id)
                    );
                    ensure!(
                        !section_flags.is_alloc(),
                        "Alloc section {} not present in any program segment.",
                        output_sections.section_debug(section_id)
                    );
                } else {
                    P::validate_section(
                        section_info,
                        section_flags,
                        section_layout,
                        merge_target,
                        output_sections,
                        section_id,
                    )?;

                    for opt_rec in &mut active_segments {
                        let Some(rec) = opt_rec.as_mut() else {
                            continue;
                        };

                        // Sections that occupy only TLS address space should not contribute to the
                        // extent of non-TLS LOAD or RELRO segments.
                        if section_info
                            .section_attributes
                            .occupies_only_tls_address_space()
                            && !program_segments.is_tls_segment(rec.segment_id)
                        {
                            continue;
                        }

                        // GNU ld keeps non-ALLOC sections (`.comment 0 :`, empty script
                        // markers that never received SHF_ALLOC) out of PT_LOAD bounds.
                        if !section_flags.is_alloc()
                            && program_segments.is_load_segment(rec.segment_id)
                        {
                            continue;
                        }

                        rec.file_start = rec.file_start.min(section_layout.file_offset);
                        rec.mem_start = rec.mem_start.min(section_layout.mem_offset);
                        rec.lma_start = rec.lma_start.min(section_layout.lma_offset);

                        rec.file_end = rec
                            .file_end
                            .max(section_layout.file_offset + section_layout.file_size);
                        rec.mem_end = rec
                            .mem_end
                            .max(section_layout.mem_offset + section_layout.mem_size);
                        rec.lma_end = rec
                            .lma_end
                            .max(section_layout.lma_offset + section_layout.mem_size);
                        rec.alignment = rec.alignment.max(section_layout.alignment);
                    }
                }
            }
            OrderEvent::SetLocation(..)
            | OrderEvent::SetLocationRelative(..)
            | OrderEvent::SetSectionAddress(_) => {}
        }
    }

    complete.sort_by_key(|r| r.segment_id);

    assert_eq!(complete.len(), program_segments.len());
    let mut tls_layout = None;

    let mut segments = header_info
        .active_segment_ids
        .iter()
        .map(|&id| {
            let r = &complete[id.as_usize()];

            let sizes = if r.file_start <= r.file_end {
                OutputRecordLayout {
                    file_size: r.file_end - r.file_start,
                    mem_size: r.mem_end - r.mem_start,
                    alignment: r.alignment,
                    file_offset: r.file_start,
                    mem_offset: r.mem_start,
                    lma_offset: r.lma_start,
                }
            } else {
                OutputRecordLayout {
                    file_size: 0,
                    mem_size: 0,
                    alignment: r.alignment,
                    file_offset: 0,
                    mem_offset: 0,
                    lma_offset: 0,
                }
            };

            if program_segments.is_tls_segment(id) {
                tls_layout = Some(sizes);
            }

            SegmentLayout { id, sizes }
        })
        .collect_vec();

    if output_order.has_custom_phdrs() {
        segments.sort_by_key(|s| s.id);
    } else {
        segments.sort_by_key(|s| program_segments.order_key(s.id, s.sizes.mem_offset));
    }

    Ok(SegmentLayouts {
        segments,
        tls_layout,
    })
}

pub(crate) fn advance_section_offset<P: Platform>(
    offset: &mut u64,
    sec: Section,
    part_id: PartId,
    output_sections: &OutputSections<P>,
) -> u64 {
    if output_sections.uses_input_order(part_id.output_section_id::<P>()) {
        let (address, end) = sec.place(*offset);
        *offset = end;
        address
    } else {
        let address = *offset;
        *offset += sec.capacity(part_id, output_sections);
        address
    }
}

pub(crate) fn packed_span(start: u64, inputs: &[(Alignment, u64)]) -> u64 {
    let mut offset = start;
    for &(alignment, size) in inputs {
        offset = alignment.align_up(offset) + size;
    }
    offset - start
}

/// Sizes of non-object groups that sit before/after input-order object contributions
/// (prelude merged strings, epilogue). Those groups must keep their `mem_sizes`; packing
/// only replaces the object groups.
pub(crate) fn input_order_affix_sizes<P: Platform>(
    group_states: &[GroupState<P>],
    ordered: &[InputOrderItem],
) -> HashMap<PartId, (u64, u64)> {
    let mut object_range: HashMap<PartId, (usize, usize)> = HashMap::new();
    for item in ordered {
        let range = object_range
            .entry(item.part_id)
            .or_insert((item.group_idx, item.group_idx));
        range.0 = range.0.min(item.group_idx);
        range.1 = range.1.max(item.group_idx);
    }

    let mut affixes = HashMap::new();
    for (part_id, &(first_obj, last_obj)) in &object_range {
        let mut prefix = 0u64;
        let mut suffix = 0u64;
        for (idx, group) in group_states.iter().enumerate() {
            let size = group.common.mem_sizes.get(*part_id);
            if idx < first_obj {
                prefix += size;
            } else if idx > last_obj {
                suffix += size;
            }
        }
        if prefix > 0 || suffix > 0 {
            affixes.insert(*part_id, (prefix, suffix));
        }
    }
    affixes
}

pub(crate) fn collect_input_order_contributions<P: Platform>(
    group_states: &[GroupState<P>],
    output_sections: &OutputSections<P>,
    section_part_ids: &[PartId],
) -> (HashMap<PartId, Vec<(Alignment, u64)>>, Vec<InputOrderItem>) {
    let mut ordered = Vec::new();

    for (group_idx, group) in group_states.iter().enumerate() {
        for file in &group.files {
            let FileLayoutState::Object(obj) = file else {
                continue;
            };
            for (sec_idx, slot) in obj.sections.iter().enumerate() {
                let (SectionSlot::Loaded(sec) | SectionSlot::LoadedDebugInfo(sec)) = slot else {
                    continue;
                };
                let part_id = obj.section_part_id(SectionIndex(sec_idx), section_part_ids);
                if !output_sections.uses_input_order(part_id.output_section_id::<P>()) {
                    continue;
                }
                ordered.push(InputOrderItem {
                    part_id,
                    group_idx,
                    link_order: obj.link_order,
                    alignment: sec.alignment,
                    size: sec.size,
                });
            }
        }
    }

    ordered.sort_by_key(|item| (item.link_order, item.group_idx));
    let mut by_part: HashMap<PartId, Vec<(Alignment, u64)>> = HashMap::new();
    for item in &ordered {
        by_part
            .entry(item.part_id)
            .or_default()
            .push((item.alignment, item.size));
    }

    (by_part, ordered)
}

pub(crate) fn redistribute_input_order_sizes<P: Platform>(
    group_states: &mut [GroupState<P>],
    ordered: &[InputOrderItem],
    section_part_layouts: &OutputSectionPartMap<OutputRecordLayout>,
    affixes: &HashMap<PartId, (u64, u64)>,
) {
    let mut items_by_part: HashMap<PartId, Vec<&InputOrderItem>> = HashMap::new();
    for item in ordered {
        items_by_part.entry(item.part_id).or_default().push(item);
    }
    for items in items_by_part.values_mut() {
        items.sort_by_key(|item| (item.link_order, item.group_idx));
    }

    for (part_id, items) in items_by_part {
        let mut is_object_group = vec![false; group_states.len()];
        for item in &items {
            is_object_group[item.group_idx] = true;
        }
        for (group_idx, group) in group_states.iter_mut().enumerate() {
            if is_object_group[group_idx] {
                *group.common.mem_sizes.get_mut(part_id) = 0;
            }
        }

        let prefix = affixes.get(&part_id).map(|(p, _)| *p).unwrap_or(0);
        let mut offset = section_part_layouts.get(part_id).mem_offset + prefix;
        let mut current_group = None;
        let mut group_start = offset;
        for item in items {
            if current_group != Some(item.group_idx) {
                if let Some(group_idx) = current_group {
                    *group_states[group_idx].common.mem_sizes.get_mut(part_id) =
                        offset - group_start;
                }
                current_group = Some(item.group_idx);
                group_start = offset;
            }
            offset = item.alignment.align_up(offset) + item.size;
        }
        if let Some(group_idx) = current_group {
            *group_states[group_idx].common.mem_sizes.get_mut(part_id) = offset - group_start;
        }
    }
}

pub(crate) fn apply_input_order_section_alignments<P: Platform>(
    section_layouts: &mut OutputSectionMap<OutputRecordLayout>,
    by_part: &HashMap<PartId, Vec<(Alignment, u64)>>,
) {
    for (part_id, inputs) in by_part {
        let Some(max_align) = inputs.iter().map(|(alignment, _)| *alignment).max() else {
            continue;
        };
        let layout = section_layouts.get_mut(part_id.output_section_id::<P>());
        layout.alignment = layout.alignment.max(max_align);
    }
}

pub(crate) fn apply_merge_vma_padding<P: Platform>(
    merged_strings: &mut OutputSectionMap<MergedStringsSection>,
    group_states: &mut [GroupState<P>],
    section_part_sizes: &mut OutputSectionPartMap<u64>,
    section_part_layouts: &OutputSectionPartMap<OutputRecordLayout>,
) -> bool {
    let prelude_sizes = group_states.iter_mut().find_map(|g| {
        if g.files
            .iter()
            .any(|f| matches!(f, FileLayoutState::Prelude(_)))
        {
            Some(&mut g.common.mem_sizes)
        } else {
            None
        }
    });
    let Some(prelude_sizes) = prelude_sizes else {
        return false;
    };

    let mut changed = false;
    merged_strings.for_each_mut(|section_id, merged| {
        if merged.len() == 0 {
            return;
        }
        let part_id = section_id.part_id_with_alignment::<P>(alignment::MIN);
        let start_vma = section_part_layouts.get(part_id).mem_offset;
        let delta = merged.repad_to_vma(start_vma);
        if delta == 0 {
            return;
        }
        changed = true;
        if delta > 0 {
            let mag = delta as u64;
            section_part_sizes.increment(part_id, mag);
            prelude_sizes.increment(part_id, mag);
        } else {
            let mag = (-delta) as u64;
            section_part_sizes.decrement(part_id, mag);
            prelude_sizes.decrement(part_id, mag);
        }
    });
    changed
}

pub(crate) fn compute_and_apply_section_layout<'data, P: Platform>(
    group_states: &mut [GroupState<'data, P>],
    sizes: &OutputSectionPartMap<u64>,
    output_sections: &OutputSections<'data, P>,
    program_segments: &ProgramSegments<P::ProgramSegmentDef>,
    output_order: &OutputOrder<'data>,
    symbol_db: &SymbolDb<'data, P>,
    memory_regions: &mut HashMap<&[u8], MemoryRegion>,
    memory_region_order: &[&[u8]],
    sizeof_headers: u64,
) -> Result<(
    OutputSectionPartMap<OutputRecordLayout>,
    OutputSectionMap<OutputRecordLayout>,
    Vec<ResolvedLocationCounter>,
)> {
    let (by_part, ordered) = collect_input_order_contributions(
        group_states,
        output_sections,
        &symbol_db.section_part_ids,
    );
    let affixes = input_order_affix_sizes(group_states, &ordered);
    let mut layout_inputs = by_part.clone();
    for (part_id, (prefix, suffix)) in &affixes {
        let inputs = layout_inputs.entry(*part_id).or_default();
        if *prefix > 0 {
            inputs.insert(0, (alignment::MIN, *prefix));
        }
        if *suffix > 0 {
            inputs.push((alignment::MIN, *suffix));
        }
    }
    let (section_part_layouts, mut section_layouts, resolved_location_counters) =
        compute_layout_sections::<P>(
            group_states,
            sizes,
            output_sections,
            program_segments,
            output_order,
            symbol_db,
            memory_regions,
            memory_region_order,
            sizeof_headers,
            &layout_inputs,
        )?;
    redistribute_input_order_sizes(group_states, &ordered, &section_part_layouts, &affixes);
    apply_input_order_section_alignments::<P>(&mut section_layouts, &by_part);
    Ok((
        section_part_layouts,
        section_layouts,
        resolved_location_counters,
    ))
}

pub(crate) fn compute_layout_sections<'data, P: Platform>(
    group_states: &[GroupState<'data, P>],
    sizes: &OutputSectionPartMap<u64>,
    output_sections: &OutputSections<'data, P>,
    program_segments: &ProgramSegments<P::ProgramSegmentDef>,
    output_order: &OutputOrder<'data>,
    symbol_db: &SymbolDb<'data, P>,
    memory_regions: &mut HashMap<&[u8], MemoryRegion>,
    memory_region_order: &[&[u8]],
    sizeof_headers: u64,
    input_order_sizes: &HashMap<PartId, Vec<(Alignment, u64)>>,
) -> Result<(
    OutputSectionPartMap<OutputRecordLayout>,
    OutputSectionMap<OutputRecordLayout>,
    Vec<ResolvedLocationCounter>,
)> {
    let args = symbol_db.args;
    let segment_alignments = compute_segment_alignments::<P>(
        sizes,
        program_segments,
        output_order,
        args,
        output_sections,
    );

    timing_phase!("Layout sections");

    let mut section_layouts = OutputSectionMap::with_size(output_sections.num_sections());
    let section_positions = OnceCell::new();

    let const_script_symbols = collect_const_script_symbols(symbol_db);

    let mut overlay_vma: HashMap<u32, u64> = HashMap::new();
    let mut overlay_lma_end: HashMap<u32, u64> = HashMap::new();
    let mut overlay_max_size: HashMap<u32, u64> = HashMap::new();
    let mut input_order_max_align: HashMap<OutputSectionId, Alignment> = HashMap::new();
    for (part_id, inputs) in input_order_sizes {
        let Some(max_align) = inputs.iter().map(|(alignment, _)| *alignment).max() else {
            continue;
        };
        let section_id = output_sections.primary_output_section(part_id.output_section_id::<P>());
        input_order_max_align
            .entry(section_id)
            .and_modify(|existing| *existing = (*existing).max(max_align))
            .or_insert(max_align);
    }

    let expression_eval =
        |expr: &Expression<'data>,
         loc: &SymbolLoc,
         memory_regions: &HashMap<&[u8], MemoryRegion>,
         section_layouts: &OutputSectionMap<OutputRecordLayout>,
         resolved_lc: &[ResolvedLocationCounter],
         laid_out_mem_offsets: &OutputSectionPartMap<Option<u64>>| {
            crate::expression_eval::evaluate_expression(
                expr,
                loc,
                None,
                section_layouts,
                output_sections,
                memory_regions,
                symbol_db,
                sizeof_headers,
                resolved_lc,
                &|name| {
                    if let Some(value) = const_script_symbols.get(name) {
                        return Ok(ResolvedSymbolValue::Absolute(*value));
                    }
                    let Some(symbol_id) =
                        symbol_db.get_unversioned(&UnversionedSymbolName::prehashed(name))
                    else {
                        bail!(
                            "undefined symbol '{}' in linker script expression",
                            String::from_utf8_lossy(name)
                        );
                    };

                    let canonical_id = symbol_db.definition(symbol_id);
                    let file_id = symbol_db.file_id_for_symbol(canonical_id);
                    let is_object = matches!(
                        group_states
                            .get(file_id.group())
                            .and_then(|group| group.files.get(file_id.file())),
                        Some(FileLayoutState::Object(_))
                    );
                    if !is_object {
                        return Ok(ResolvedSymbolValue::Absolute(layout_time_symbol_value(
                            name,
                            symbol_db,
                            section_layouts,
                            output_sections,
                            memory_regions,
                            loc,
                            sizeof_headers,
                            resolved_lc,
                            &const_script_symbols,
                            0,
                        )?));
                    }

                    let symbol_value = match resolve_early_object_symbol(
                        symbol_id,
                        group_states,
                        section_positions.get_or_init(|| {
                            compute_input_section_positions(
                                group_states,
                                sizes.new_empty_like(),
                                symbol_db,
                                output_sections,
                            )
                        }),
                        symbol_db,
                    )? {
                        EarlyObjectSymbolValue::Absolute(value) => {
                            ResolvedSymbolValue::Absolute(value)
                        }
                        EarlyObjectSymbolValue::PartRelative { part_id, offset } => {
                            let Some(part_address) = laid_out_mem_offsets.get(part_id) else {
                                bail!(
                                    "cannot resolve address of symbol '{}' because its output section part has not been laid out yet",
                                    String::from_utf8_lossy(name)
                                );
                            };
                            let address = part_address + offset;
                            let symbol_section = output_sections
                                .primary_output_section(part_id.output_section_id::<P>());
                            let current_section = match loc {
                                SymbolLoc::SectionStartRelative(id)
                                | SymbolLoc::SectionEndRelative(id) => {
                                    Some(output_sections.primary_output_section(*id))
                                }
                                SymbolLoc::LocationCounter(idx, Some(id))
                                    if resolved_lc
                                        .get(*idx)
                                        .is_some_and(|entry| entry.section_offset.is_some()) =>
                                {
                                    Some(output_sections.primary_output_section(*id))
                                }
                                _ => None,
                            };
                            if current_section == Some(symbol_section) {
                                let section_base = section_layouts.get(symbol_section).mem_offset;
                                ResolvedSymbolValue::SectionRelative(
                                    address.checked_sub(section_base).with_context(|| {
                                        format!(
                                            "address of symbol '{}' is before its output section",
                                            String::from_utf8_lossy(name)
                                        )
                                    })?,
                                )
                            } else {
                                ResolvedSymbolValue::Absolute(address)
                            }
                        }
                    };
                    Ok(symbol_value)
                },
            )
        };

    // Memory offsets of the output-section parts that have been laid out so far. Used to resolve
    // object symbols referenced from location-counter expressions.
    let mut laid_out_mem_offsets = output_sections.new_part_map::<Option<u64>>();
    let mut file_offset = 0;
    let mut mem_offset = expression_eval(
        &output_sections.base_address,
        &SymbolLoc::None,
        memory_regions,
        &section_layouts,
        &[],
        &laid_out_mem_offsets,
    )?;
    let mut lma_offset = mem_offset;
    let mut nonalloc_mem_offsets: OutputSectionMap<u64> =
        OutputSectionMap::with_size(output_sections.num_sections());
    let mut reloc_alloc_mem_offsets: OutputSectionMap<u64> =
        OutputSectionMap::with_size(output_sections.num_sections());

    let mut pending_location = None;
    let mut resolved_lc = vec![Default::default(); output_order.num_location_counters()];
    if !resolved_lc.is_empty() {
        resolved_lc[0] = ResolvedLocationCounter {
            value: mem_offset,
            section_offset: None,
        };
    }

    let mut records_out = output_sections.new_part_map();

    // TLS sections without data (like .tbss) overlap normal sections in memory.
    // This is possible because every thread copies the TLS segments (see TLS PHDR)
    // to construct thread local data. However, uninitialized TLS data is assumed to be zero
    // and therefore no copy happens. It would be wasteful to reserve that address in the TLS
    // template, so we don't do it.
    let mut tls_memsave: Option<u64> = None;

    // ALLOC sections not covered by a PT_LOAD (typical: ELF file/program/section headers when
    // the linker script's PHDRS omit FILEHDR) occupy file space only. GNU ld does not put them
    // in the process VMA, so the first loadable section keeps the script address. When those
    // file-only headers precede the first LOAD, pad the file offset so `p_offset ≡ p_vaddr`.
    let mut load_segment_depth = 0u32;
    let mut pad_file_at_next_load = false;
    let mut load_origins: Vec<(u64, u64, usize)> = Vec::new();

    for event in output_order {
        match event {
            OrderEvent::SetLocation(expr, mut loc, idx) => {
                if matches!(loc, SymbolLoc::SectionEnd(_)) {
                    resolved_lc[idx] = ResolvedLocationCounter {
                        value: mem_offset,
                        section_offset: None,
                    };
                    loc = SymbolLoc::LocationCounter(idx, None);
                }
                let value = expression_eval(
                    &expr,
                    &loc,
                    memory_regions,
                    &section_layouts,
                    &resolved_lc,
                    &laid_out_mem_offsets,
                )?;
                pending_location = Some(value);
                resolved_lc[idx] = ResolvedLocationCounter {
                    value,
                    section_offset: None,
                };
            }
            OrderEvent::SetLocationRelative(expr, section_id, loc, idx) => {
                let primary_id = output_sections.primary_output_section(section_id);
                let section_base = section_layouts.get(primary_id).mem_offset;
                let value = expression_eval(
                    &expr,
                    &loc,
                    memory_regions,
                    &section_layouts,
                    &resolved_lc,
                    &laid_out_mem_offsets,
                )?;
                // `. += N` is `. = . + N`. Inside a section, `.` is a section offset, so
                // `evaluate_expression` returns `section_base + N` when the RHS is relative.
                // If the RHS mentions a symbol (`text_size`), the expression is treated as
                // absolute and we get just `N`. Convert that back into an absolute VMA.
                let value = if value >= section_base {
                    value
                } else {
                    section_base.wrapping_add(value)
                };
                let offset = value - section_base;
                pending_location = Some(value);
                resolved_lc[idx] = ResolvedLocationCounter {
                    value,
                    section_offset: Some(offset),
                };
            }
            OrderEvent::SetSectionAddress(expr) => {
                let value = expression_eval(
                    &expr,
                    &SymbolLoc::None,
                    memory_regions,
                    &section_layouts,
                    &resolved_lc,
                    &laid_out_mem_offsets,
                )?;
                pending_location = Some(value);
            }
            OrderEvent::SegmentStart(segment_id) => {
                if program_segments.is_load_segment(segment_id) {
                    let segment_alignment = segment_alignments
                        .get(&segment_id)
                        .copied()
                        .unwrap_or_else(|| args.loadable_segment_alignment());
                    if let Some(addr) = pending_location {
                        // The OrderEvent::SetLocation is ELF-specific only.
                        mem_offset = addr;
                        lma_offset = mem_offset;
                        file_offset =
                            segment_alignment.align_modulo(mem_offset, file_offset as u64) as usize;
                        pad_file_at_next_load = false;
                    } else if pad_file_at_next_load {
                        // Keep the script VMA; pad the file so p_offset ≡ p_vaddr.
                        file_offset =
                            segment_alignment.align_modulo(mem_offset, file_offset as u64) as usize;
                        lma_offset = mem_offset;
                        pad_file_at_next_load = false;
                    } else {
                        let segment_def = *program_segments.segment_def(segment_id);
                        P::align_load_segment_start(
                            segment_def,
                            segment_alignment,
                            &mut file_offset,
                            &mut mem_offset,
                        );
                        P::align_load_segment_start(
                            segment_def,
                            segment_alignment,
                            &mut file_offset,
                            &mut lma_offset,
                        );
                    }
                    load_origins.push((mem_offset, lma_offset, file_offset));
                    load_segment_depth += 1;
                }
            }
            OrderEvent::SegmentEnd(segment_id) => {
                if program_segments.is_load_segment(segment_id) {
                    load_origins.pop();
                    load_segment_depth = load_segment_depth.saturating_sub(1);
                }
            }
            OrderEvent::Section(section_id) => {
                let section_info = output_sections.output_info(section_id);
                let merge_target = output_sections.primary_output_section(section_id);
                let primary_info = output_sections.output_info(merge_target);
                // `.comment 0 :` / `.symtab 0 :` have an explicit VMA of 0 and are not ALLOC.
                // They must not consume a pending `. = ALIGN(...)` or follow the location
                // counter into a PT_LOAD. Only custom script sections (empty markers like
                // `.init.begin`) follow `.` when they inherit a LOAD without SHF_ALLOC.
                let has_explicit_nonalloc_vma = !primary_info.section_attributes.is_alloc()
                    && primary_info
                        .location_info
                        .as_ref()
                        .and_then(|info| info.location.as_ref())
                        .is_some();
                let section_offset = if has_explicit_nonalloc_vma {
                    None
                } else {
                    pending_location.take()
                };
                let mut follow_location_counter = load_segment_depth > 0
                    && !has_explicit_nonalloc_vma
                    && merge_target.is_custom::<P>();

                if section_info
                    .section_attributes
                    .occupies_only_tls_address_space()
                {
                    // Save our current mem_offset as we enter our first nobits TLS section
                    if tls_memsave.is_none() {
                        tls_memsave = Some(mem_offset);
                    }
                } else if let Some(tls_memsave) = tls_memsave.take() {
                    // Restore offsets when exiting nobits TLS sections
                    mem_offset = tls_memsave;
                }

                let part_id_range = section_id.part_id_range::<P>();
                let max_alignment = sizes.max_alignment(part_id_range.clone(), output_sections);
                let overlay = section_info
                    .location_info
                    .as_ref()
                    .and_then(|info| info.overlay);

                let region_name = section_info.region_name.or_else(|| {
                    // Only auto-pick MEMORY regions for sections that appear in the linker script.
                    // Linker-generated sections keep the default layout.
                    section_info.location_info.as_ref().and_then(|_| {
                        pick_compatible_memory_region(
                            memory_regions,
                            memory_region_order,
                            section_info.section_attributes.is_alloc(),
                            section_info.section_attributes.is_writable(),
                            section_info.section_attributes.is_executable(),
                        )
                    })
                });
                let region = region_name
                    .map(|region_name| {
                        memory_regions.get(region_name).with_context(|| {
                            format!(
                                "Memory region '{}' not declared",
                                String::from_utf8_lossy(region_name),
                            )
                        })
                    })
                    .transpose()?;
                if let Some(region) = region {
                    mem_offset = region.origin + region.used;
                }

                let at_region = section_info
                    .location_info
                    .as_ref()
                    .and_then(|info| info.at_region)
                    .map(|region_name| {
                        memory_regions.get(region_name).with_context(|| {
                            format!(
                                "Memory region '{}' not declared",
                                String::from_utf8_lossy(region_name),
                            )
                        })
                    })
                    .transpose()?;
                if let Some(region) = at_region {
                    lma_offset = region.origin + region.used_lma;
                }

                if let Some(ov) = overlay {
                    if ov.member == 0 {
                        overlay_vma.insert(ov.group, mem_offset);
                    } else if let Some(&vma) = overlay_vma.get(&ov.group) {
                        mem_offset = vma;
                        if let Some(&lma) = overlay_lma_end.get(&ov.group) {
                            lma_offset = lma;
                        }
                    }
                }

                if let Some(offset) = section_offset {
                    let merge_target = output_sections.primary_output_section(section_id);
                    let is_top_level = section_info
                        .location_info
                        .as_ref()
                        .is_some_and(|info| info.is_top_level);
                    if is_top_level
                        && let Some(region) = region
                        && (offset < mem_offset || offset > mem_offset + region.length)
                    {
                        bail!(
                            "address 0x{offset:x} of section '{}' is not within region `{}'",
                            output_sections.display_name(section_id),
                            String::from_utf8_lossy(section_info.region_name.unwrap()),
                        );
                    }
                    if offset >= mem_offset {
                        if load_segment_depth > 0
                            && (section_id == merge_target || !is_top_level)
                            && output_sections.has_data_in_file(merge_target)
                        {
                            file_offset += (offset - mem_offset) as usize;
                        }
                        if load_segment_depth > 0 {
                            mem_offset = offset;
                        }
                    } else {
                        // Explicit VMA behind the location counter (`.comment 0 :`).
                        follow_location_counter = false;
                    }
                }
                if at_region.is_none()
                    && overlay.is_none_or(|ov| ov.member == 0)
                    && section_info
                        .location_info
                        .as_ref()
                        .and_then(|info| info.at_location.as_ref())
                        .is_none()
                {
                    lma_offset = mem_offset;
                }

                let is_top_level = section_info
                    .location_info
                    .as_ref()
                    .is_some_and(|info| info.is_top_level);
                let has_explicit_section_addr = section_info
                    .location_info
                    .as_ref()
                    .and_then(|info| info.location.as_ref())
                    .is_some();
                // GNU ld aligns a script output section to the max input sh_addralign
                // even after `. = ALIGN(n)` (kernel `. = ALIGN(8); .exit.text` with
                // 16-byte inputs). An explicit section address (`.foo 0x1000 :`)
                // still wins.
                if is_top_level
                    && !has_explicit_section_addr
                    && let Some(&max_input_align) = input_order_max_align.get(&section_id)
                {
                    mem_offset = max_input_align.align_up(mem_offset);
                    lma_offset = max_input_align.align_up(lma_offset);
                    if output_sections.has_data_in_file(merge_target) {
                        file_offset = max_input_align.align_up_usize(file_offset);
                    }
                }

                let mut is_first_part = true;

                let merge_target = output_sections.primary_output_section(section_id);
                let section_flags = output_sections.section_flags(merge_target);

                let mut part_sizes = sizes
                    .in_range(part_id_range.clone())
                    .map(|(id, &size)| (id, size))
                    .peekable();

                // For sections with only empty parts, make sure we run the loop below at least once
                // so as to properly initialise the section's layout.
                let empty_part = part_sizes
                    .peek()
                    .is_none()
                    .then_some((part_id_range.start, 0));

                for (part_id, part_size) in part_sizes.chain(empty_part) {
                    let part_layout = records_out.get_mut(part_id);
                    let alignment = if is_first_part {
                        max_alignment
                    } else {
                        part_id.alignment(output_sections).min(max_alignment)
                    };
                    let aligned_mem_offset = alignment.align_up(mem_offset);
                    let mem_size = if Some(section_id) == P::RELRO_PADDING_SECTION_ID {
                        let page_alignment = args.loadable_segment_alignment();
                        let aligned_offset = page_alignment.align_up(mem_offset);
                        aligned_offset - mem_offset
                    } else if let Some(inputs) = input_order_sizes.get(&part_id) {
                        packed_span(aligned_mem_offset, inputs)
                    } else {
                        part_size
                    };

                    // Note, we align up even if our size is zero, otherwise our section will
                    // start at an unaligned address. We don't however align up for NOBITS
                    // sections.
                    if output_sections.has_data_in_file(merge_target) {
                        file_offset = alignment.align_up_usize(file_offset);
                    }

                    if section_flags.is_alloc() && args.should_output_partial_object() {
                        let file_size = if output_sections.has_data_in_file(merge_target) {
                            mem_size as usize
                        } else {
                            0
                        };

                        let section_id = part_id.output_section_id::<P>();
                        let part_mem_offset =
                            alignment.align_up(*reloc_alloc_mem_offsets.get(section_id));
                        *reloc_alloc_mem_offsets.get_mut(section_id) = part_mem_offset + mem_size;

                        *part_layout = OutputRecordLayout {
                            file_size,
                            mem_size,
                            alignment,
                            file_offset,
                            mem_offset: part_mem_offset,
                            lma_offset: part_mem_offset,
                        };

                        file_offset += file_size;
                    } else if section_flags.is_alloc() && load_segment_depth == 0 {
                        // Headers (and any other ALLOC content) outside PT_LOAD occupy the
                        // file only, matching GNU ld PHDRS without FILEHDR.
                        pad_file_at_next_load = true;
                        let file_size = if output_sections.has_data_in_file(merge_target) {
                            mem_size as usize
                        } else {
                            0
                        };
                        *part_layout = OutputRecordLayout {
                            file_size,
                            mem_size,
                            alignment,
                            file_offset,
                            mem_offset: 0,
                            lma_offset: 0,
                        };
                        file_offset += file_size;
                    } else if section_flags.is_alloc() || follow_location_counter {
                        // ALLOC sections in a PT_LOAD, and empty/non-ALLOC custom sections that
                        // inherit that LOAD, follow the location-counter VMA (GNU ld).
                        mem_offset = alignment.align_up(mem_offset);
                        lma_offset = alignment.align_up(lma_offset);

                        let file_size = if output_sections.has_data_in_file(merge_target) {
                            mem_size as usize
                        } else {
                            0
                        };

                        // Skip file space for NOBITS in this LOAD (`p_offset ≡ p_vaddr`). Never
                        // rewind: disjoint MEMORY VMAs in one LOAD stay packed in the file.
                        if file_size > 0
                            && let Some(&(load_vma, load_lma, load_file)) = load_origins.last()
                        {
                            let delta = if overlay.is_some_and(|ov| ov.member > 0) {
                                lma_offset.checked_sub(load_lma)
                            } else {
                                mem_offset.checked_sub(load_vma)
                            };
                            if let Some(delta) = delta {
                                let candidate = load_file.saturating_add(delta as usize);
                                if candidate > file_offset {
                                    file_offset = candidate;
                                }
                            }
                        }

                        *part_layout = OutputRecordLayout {
                            file_size,
                            mem_size,
                            alignment,
                            file_offset,
                            mem_offset,
                            lma_offset,
                        };

                        file_offset += file_size;
                        mem_offset += mem_size;
                        lma_offset += mem_size;
                    } else {
                        let section_id = part_id.output_section_id::<P>();
                        let mem_offset = alignment.align_up(*nonalloc_mem_offsets.get(section_id));

                        *nonalloc_mem_offsets.get_mut(section_id) += mem_size;

                        *part_layout = OutputRecordLayout {
                            file_size: mem_size as usize,
                            mem_size,
                            alignment,
                            file_offset,
                            mem_offset,
                            lma_offset: mem_offset,
                        };
                        file_offset += mem_size as usize;
                    }

                    *laid_out_mem_offsets.get_mut(part_id) = Some(part_layout.mem_offset);

                    layout_section_from_part_layouts(
                        part_layout,
                        section_layouts.get_mut(section_id),
                        section_info,
                        is_first_part,
                    );

                    if let Some(expr) = section_info
                        .location_info
                        .as_ref()
                        .and_then(|info| info.at_location.as_ref())
                        && overlay.is_none_or(|ov| ov.member == 0)
                    {
                        lma_offset = expression_eval(
                            expr,
                            &SymbolLoc::None,
                            memory_regions,
                            &section_layouts,
                            &resolved_lc,
                            &laid_out_mem_offsets,
                        )?;
                        part_layout.lma_offset = lma_offset;
                        let section_layout = section_layouts.get_mut(section_id);
                        section_layout.lma_offset = lma_offset;
                    }

                    is_first_part = false;
                }

                if let Some(region_name) = region_name
                    && let Some(region) = memory_regions.get_mut(region_name)
                {
                    let max_offset = region.origin + region.length;
                    if mem_offset > max_offset {
                        bail!(
                            "region '{}' overflowed by {} bytes",
                            String::from_utf8_lossy(region_name),
                            mem_offset - max_offset
                        )
                    }
                    region.used = mem_offset - region.origin;
                }
                if let Some(region_name) = section_info
                    .location_info
                    .as_ref()
                    .and_then(|info| info.at_region)
                    && let Some(region) = memory_regions.get_mut(region_name)
                {
                    region.used_lma = lma_offset.saturating_sub(region.origin);
                }

                if let Some(ov) = overlay {
                    overlay_lma_end.insert(ov.group, lma_offset);
                    let size = section_layouts.get(section_id).mem_size;
                    overlay_max_size
                        .entry(ov.group)
                        .and_modify(|m| *m = (*m).max(size))
                        .or_insert(size);
                    if ov.is_last
                        && let Some(&vma) = overlay_vma.get(&ov.group)
                        && let Some(&max_size) = overlay_max_size.get(&ov.group)
                    {
                        mem_offset = vma + max_size;
                    } else if overlay.is_some()
                        && let Some(&vma) = overlay_vma.get(&ov.group)
                    {
                        mem_offset = vma;
                    }
                }
            }
        }
    }

    validate_all_non_empty_sections_emitted(sizes, output_sections, output_order)?;

    Ok((records_out, section_layouts, resolved_lc))
}

pub(crate) fn pick_compatible_memory_region<'data>(
    memory_regions: &HashMap<&[u8], MemoryRegion>,
    memory_region_order: &[&'data [u8]],
    alloc: bool,
    writable: bool,
    executable: bool,
) -> Option<&'data [u8]> {
    if !alloc || memory_region_order.is_empty() {
        return None;
    }
    for &name in memory_region_order {
        let Some(region) = memory_regions.get(name) else {
            continue;
        };
        if memory_flags_match(region.flags, writable, executable) {
            return Some(name);
        }
    }
    None
}

pub(crate) fn memory_flags_match(
    flags: Option<crate::linker_script::MemoryFlags>,
    writable: bool,
    executable: bool,
) -> bool {
    let Some(flags) = flags else {
        return true;
    };
    if writable && !flags.write {
        return false;
    }
    if executable && !flags.exec {
        return false;
    }
    true
}

/// Checks if we've allocated space to any sections which aren't listed in our output ordering.
/// Without this check, we'll fail in the write phase, but the failure message there is less
/// helpful. No-op if debug assertions are off.
pub(crate) fn validate_all_non_empty_sections_emitted<P: Platform>(
    sizes: &OutputSectionPartMap<u64>,
    output_sections: &OutputSections<P>,
    output_order: &OutputOrder,
) -> Result {
    if !cfg!(debug_assertions) {
        return Ok(());
    }

    let mut emitted_sections: OutputSectionMap<bool> =
        OutputSectionMap::with_size(output_sections.num_sections());

    for event in output_order {
        if let OrderEvent::Section(output_section_id) = event {
            *emitted_sections.get_mut(output_section_id) = true;
        }
    }

    let mut error = None;
    sizes.map(|part_id, &size| {
        if size > 0 && !emitted_sections.get(part_id.output_section_id::<P>()) {
            error = Some(error!(
                "Internal error: Section {section} has non-zero allocation, \
                but isn't in output order",
                section = output_sections.section_debug(part_id.output_section_id::<P>()),
            ));
        }
    });
    if let Some(error) = error {
        return Err(error);
    }
    Ok(())
}

/// Performs layout of sections and segments then makes sure that the loadable segments don't
/// overlap and that sections don't overlap.
#[test]
fn test_no_disallowed_overlaps() {
    use crate::OsFileSystem;
    use crate::elf::Elf64;
    use crate::output_section_id::OrderEvent;

    let output_kind =
        crate::output_kind::OutputKind::StaticExecutable(crate::args::RelocationModel::Fixed);
    let mut output_sections = OutputSections::<Elf64>::with_base_address(0x1000, output_kind);
    let (output_order, program_segments) =
        output_sections.output_order(output_kind, &[], &[]).unwrap();
    let mut args = crate::args::elf::ElfArgs::default();
    if args.arch == crate::arch::Architecture::Unsupported {
        args.arch = crate::arch::Architecture::X86_64;
    }

    let sections_to_output: hashbrown::HashSet<OutputSectionId> = output_order
        .into_iter()
        .filter_map(|event| {
            if let OrderEvent::Section(output_section_id) = event {
                Some(output_section_id)
            } else {
                None
            }
        })
        .collect();

    let section_part_sizes = output_sections.new_part_map::<u64>().map(|part_id, _| {
        if sections_to_output.contains(&part_id.output_section_id::<Elf64>()) {
            7
        } else {
            0
        }
    });

    let output_kind =
        crate::output_kind::OutputKind::StaticExecutable(crate::args::RelocationModel::Fixed);
    let arena = colosseum::sync::Arena::new();
    let auxiliary = crate::input_data::AuxiliaryFiles::new(&args, &arena, &OsFileSystem).unwrap();
    let herd = Default::default();
    let symbol_db =
        crate::symbol_db::SymbolDb::<Elf64>::new(&args, output_kind, &auxiliary, &herd).unwrap();

    let (_, section_layouts, _) = compute_layout_sections::<Elf64>(
        &[],
        &section_part_sizes,
        &output_sections,
        &program_segments,
        &output_order,
        &symbol_db,
        &mut HashMap::new(),
        &[],
        0,
        &HashMap::new(),
    )
    .unwrap();

    // Make sure no alloc sections overlap
    let mut last_file_start = 0;
    let mut last_mem_start = 0;
    let mut last_file_end = 0;
    let mut last_mem_end = 0;
    let mut last_section_id = crate::output_section_id::FILE_HEADER;

    for event in &output_order {
        let OrderEvent::Section(section_id) = event else {
            continue;
        };

        let section_flags = output_sections.section_flags(section_id);
        if !section_flags.is_alloc() {
            return;
        }

        let section = section_layouts.get(section_id);
        let mem_offset = section.mem_offset;
        let mem_end = mem_offset + section.mem_size;
        assert!(
            mem_offset >= last_mem_end,
            "Memory sections: {last_section_id} @{last_mem_start:x}..{last_mem_end:x} overlaps {section_id} @{mem_offset:x}..{mem_end:x}",
        );
        let file_offset = section.file_offset;
        let file_end = file_offset + section.file_size;
        assert!(
            file_offset >= last_file_end,
            "File sections {last_section_id} @{last_file_start:x}..{last_file_end} {section_id} @{file_offset:x}..{file_end:x}",
        );
        last_mem_start = mem_offset;
        last_file_start = file_offset;
        last_mem_end = mem_end;
        last_file_end = file_end;
        last_section_id = section_id;
    }

    let header_info = HeaderInfo {
        num_output_sections_with_content: 0,
        active_segment_ids: (0..program_segments.len())
            .map(ProgramSegmentId::new)
            .collect(),
    };

    let mut section_index = 0;
    output_sections.section_infos.for_each(|_, info| {
        if info.section_attributes.is_alloc() {
            output_sections
                .output_section_indexes
                .push(Some(section_index));
            section_index += 1;
        } else {
            output_sections.output_section_indexes.push(None);
        }
    });

    let segment_layouts = compute_segment_layout::<Elf64>(
        &section_layouts,
        &output_sections,
        &output_order,
        &program_segments,
        &header_info,
        &args,
    )
    .unwrap();

    // Make sure loadable segments don't overlap in memory or in the file.
    let mut last_file = 0;
    let mut last_mem = 0;
    for seg_layout in &segment_layouts.segments {
        let seg_id = seg_layout.id;
        if program_segments.is_load_segment(seg_id) {
            continue;
        }
        assert!(
            seg_layout.sizes.mem_offset >= last_mem,
            "Overlapping memory segment: {} < {}",
            last_mem,
            seg_layout.sizes.mem_offset,
        );
        assert!(
            seg_layout.sizes.file_offset >= last_file,
            "Overlapping file segment {} < {}",
            last_file,
            seg_layout.sizes.file_offset,
        );
        last_mem = seg_layout.sizes.mem_offset + seg_layout.sizes.mem_size;
        last_file = seg_layout.sizes.file_offset + seg_layout.sizes.file_size;
    }
}
