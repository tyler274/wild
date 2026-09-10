use super::*;
use crate::alignment;
use crate::alignment::Alignment;
use crate::error::Result;
use crate::expression_eval::ResolvedLocationCounter;
use crate::layout::EnginePlatform;
use crate::layout::types::*;
use crate::output_section_id::OutputOrder;
use crate::output_section_id::OutputSections;
use crate::output_section_map::OutputSectionMap;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::part_id::PartId;
use crate::program_segments::ProgramSegments;
use crate::resolution::SectionSlot;
use crate::string_merging::MergedStringsSection;
use crate::symbol_db::SymbolDb;
use hashbrown::HashMap;
use object::SectionIndex;

pub(crate) fn advance_section_offset<P: EnginePlatform>(
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
pub(crate) fn input_order_affix_sizes<P: EnginePlatform>(
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

pub(crate) fn collect_input_order_contributions<P: EnginePlatform>(
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

pub(crate) fn redistribute_input_order_sizes<P: EnginePlatform>(
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

pub(crate) fn apply_input_order_section_alignments<P: EnginePlatform>(
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

pub(crate) fn apply_merge_vma_padding<P: EnginePlatform>(
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

pub(crate) fn compute_and_apply_section_layout<'data, P: EnginePlatform>(
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
