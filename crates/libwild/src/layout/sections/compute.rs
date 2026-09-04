use super::*;
use crate::alignment::Alignment;
use crate::bail;
use crate::error;
use crate::error::Context;
use crate::error::Result;
use crate::expression_eval::ResolvedLocationCounter;
use crate::expression_eval::evaluate_early_expression;
use crate::layout::EnginePlatform;
use crate::layout::script::*;
use crate::layout::types::*;
use crate::linker_script::Expression;
use crate::output_section_id::OrderEvent;
use crate::output_section_id::OutputOrder;
use crate::output_section_id::OutputSectionId;
use crate::output_section_id::OutputSections;
use crate::output_section_map::OutputSectionMap;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::parsing::SymbolLoc;
use crate::part_id::PartId;
use crate::platform::Args as _;
use crate::platform::SectionAttributes as _;
use crate::platform::SectionFlags as _;
use crate::program_segments::ProgramSegments;
use crate::symbol_db::SymbolDb;
use crate::timing_phase;
use hashbrown::HashMap;
use hashbrown::HashSet;
use std::cell::OnceCell;

pub(crate) fn compute_layout_sections<'data, P: EnginePlatform>(
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

    let events: Vec<OrderEvent<'data>> = output_order.into_iter().collect();

    let expression_eval = |expr: &Expression<'data>,
                           loc: &SymbolLoc,
                           memory_regions: &HashMap<&[u8], MemoryRegion>,
                           section_layouts: &OutputSectionMap<OutputRecordLayout>,
                           resolved_lc: &[ResolvedLocationCounter],
                           laid_out_mem_offsets: &OutputSectionPartMap<Option<u64>>,
                           rest: &[OrderEvent<'data>]| {
        let bound;
        let expr = if expr.contains_next_section() {
            let (align, size) = next_allocated_section_metrics::<P>(
                rest,
                sizes,
                output_sections,
                output_order.script_section_order(),
                &input_order_max_align,
            );
            bound = expr.rewrite_next_section(align, size);
            &bound
        } else {
            expr
        };
        let mut visited_nodes = HashSet::new();
        evaluate_early_expression(
            expr,
            loc,
            memory_regions,
            section_layouts,
            resolved_lc,
            laid_out_mem_offsets,
            group_states,
            sizes,
            output_sections,
            symbol_db,
            sizeof_headers,
            &section_positions,
            &mut visited_nodes,
            &const_script_symbols,
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
        &events,
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

    for (i, event) in events.iter().cloned().enumerate() {
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
                    &events[i + 1..],
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
                    &events[i + 1..],
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
                // GNU: `.data ALIGN(0x2000) :` is ALIGN of the current VMA.
                // `.` is a pending `. = expr` if one is in flight
                // (`. = 0x400004; .text ALIGN(0) :`), otherwise the VMA after
                // the previous section.
                let current_vma = pending_location.unwrap_or(mem_offset);
                resolved_lc.push(ResolvedLocationCounter {
                    value: current_vma,
                    section_offset: None,
                });
                let loc = SymbolLoc::LocationCounter(resolved_lc.len() - 1, None);
                let result = expression_eval(
                    &expr,
                    &loc,
                    memory_regions,
                    &section_layouts,
                    &resolved_lc,
                    &laid_out_mem_offsets,
                    &events[i + 1..],
                );
                resolved_lc.pop();
                pending_location = Some(result?);
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
                            &events[i + 1..],
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

/// GNU `ALIGNOF(NEXT_SECTION)` / `SIZEOF(NEXT_SECTION)`: the next output section in script
/// order that has a non-zero allocation, or (0, 0) if there is none.
fn next_allocated_section_metrics<'data, P: EnginePlatform>(
    rest: &[OrderEvent<'data>],
    sizes: &OutputSectionPartMap<u64>,
    output_sections: &OutputSections<'data, P>,
    script_section_order: &[OutputSectionId],
    input_order_max_align: &HashMap<OutputSectionId, Alignment>,
) -> (u64, u64) {
    let rest_ids;
    let scan = if script_section_order.is_empty() {
        rest_ids = rest
            .iter()
            .filter_map(|event| match event {
                OrderEvent::Section(section_id) => Some(*section_id),
                _ => None,
            })
            .collect::<Vec<_>>();
        rest_ids.as_slice()
    } else {
        let start_pos = rest
            .iter()
            .find_map(|event| match event {
                OrderEvent::Section(section_id) => Some(*section_id),
                _ => None,
            })
            .and_then(|id| {
                script_section_order
                    .iter()
                    .position(|&section_id| section_id == id)
            })
            .unwrap_or(script_section_order.len());
        &script_section_order[start_pos..]
    };
    for &section_id in scan {
        let section_id = output_sections.primary_output_section(section_id);
        let range = section_id.part_id_range::<P>();
        let size: u64 = sizes.values_in_range(range.clone()).copied().sum();
        if size == 0 {
            continue;
        }
        let mut align = sizes.max_alignment(range, output_sections);
        if let Some(&max_input_align) = input_order_max_align.get(&section_id) {
            align = align.max(max_input_align);
        }
        return (align.value(), size);
    }
    (0, 0)
}

/// Checks if we've allocated space to any sections which aren't listed in our output ordering.
/// Without this check, we'll fail in the write phase, but the failure message there is less
/// helpful. No-op if debug assertions are off.
pub(crate) fn validate_all_non_empty_sections_emitted<P: EnginePlatform>(
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
