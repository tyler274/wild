mod compute;
mod input_order;

use super::types::*;
use crate::alignment;
use crate::alignment::Alignment;
use crate::ensure;
use crate::error::Context;
use crate::error::Result;
use crate::layout::EnginePlatform;
use crate::layout_rules::SectionKind;
use crate::output_section_id;
use crate::output_section_id::OutputOrder;
use crate::output_section_id::OutputSections;
use crate::output_section_map::OutputSectionMap;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::platform::Args as _;
use crate::platform::SectionAttributes as _;
use crate::platform::SectionFlags as _;
use crate::program_segments::ProgramSegmentId;
use crate::program_segments::ProgramSegments;
use crate::timing_phase;
use crate::verbose_timing_phase;
#[allow(unused_imports)]
pub(crate) use compute::*;
#[allow(unused_imports)]
pub(crate) use input_order::*;
use itertools::Itertools;
use rayon::iter::IndexedParallelIterator;
use rayon::iter::IntoParallelIterator;
use rayon::iter::ParallelIterator;
use std::mem::take;

pub(crate) fn layout_section_from_part_layouts<'data, P: EnginePlatform>(
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

pub(crate) fn merge_secondary_parts<P: EnginePlatform>(
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

pub(crate) fn compute_start_offsets_by_group<P: EnginePlatform>(
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

pub(crate) fn compute_symbols_and_layouts<'data, P: EnginePlatform>(
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

pub(crate) fn compute_segment_layout<'data, P: EnginePlatform>(
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

/// Performs layout of sections and segments then makes sure that the loadable segments don't
/// overlap and that sections don't overlap.
#[test]
fn test_no_disallowed_overlaps() {
    use crate::OsFileSystem;
    use crate::elf::Elf64;
    use crate::output_section_id::OrderEvent;
    use crate::output_section_id::OutputSectionId;
    use hashbrown::HashMap;

    let output_kind =
        crate::output_kind::OutputKind::StaticExecutable(crate::args::RelocationModel::Fixed);
    let mut output_sections = OutputSections::<Elf64>::with_base_address(0x1000, output_kind);
    let (output_order, program_segments) =
        output_sections.output_order(output_kind, &[], &[]).unwrap();
    let mut args = crate::args::elf::ElfArgs::default();
    if args.architecture() == crate::arch::Architecture::Unsupported {
        args.set_architecture(crate::arch::Architecture::X86_64);
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
