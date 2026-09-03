use super::types::*;
use crate::LayoutRules;
use crate::alignment::Alignment;
use crate::error::Result;
use crate::layout_rules::SectionOutputInfo;
use crate::layout_rules::SectionRuleOutcome;
use crate::layout_rules::SectionRules;
use crate::output_section_id::CustomSectionDetails;
use crate::output_section_id::InitFiniSectionDetail;
use crate::output_section_id::OutputSections;
use crate::output_section_id::SectionName;
use crate::part_id;
use crate::part_id::PartId;
use crate::platform::Args as _;
use crate::platform::ObjectFile;
use crate::platform::Platform;
use crate::platform::SectionHeader as _;
use crate::string_merging::StringMergeSectionExtra;
use crate::string_merging::StringMergeSectionSlot;
use crate::symbol_db::SymbolDb;
use crate::timing_phase;
use crate::verbose_timing_phase;
use object::SectionIndex;
use rayon::iter::IndexedParallelIterator;
use rayon::iter::IntoParallelRefMutIterator;
use rayon::iter::ParallelIterator;

pub(super) fn resolve_sections<'data, P: Platform>(
    groups: &mut [ResolvedGroup<'data, P>],
    symbol_db: &mut SymbolDb<'data, P>,
    layout_rules: &LayoutRules<'data>,
    output_sections: &OutputSections<'data, P>,
) -> Result {
    timing_phase!("Resolve sections");

    let loaded_metrics: LoadedMetrics = Default::default();
    let herd = symbol_db.herd;

    let group_section_counts: Vec<usize> = groups
        .iter()
        .map(|group| {
            group
                .files
                .iter()
                .map(|f| match f {
                    ResolvedFile::Object(obj) => obj.section_id_range.len(),
                    ResolvedFile::NotLoaded(n) => n.section_id_range.len(),
                    _ => 0,
                })
                .sum()
        })
        .collect();

    let mut section_part_ids = Vec::with_capacity(symbol_db.next_input_section_id.as_usize());

    let mut section_part_ids_writer = sharded_vec_writer::VecWriter::new(&mut section_part_ids);
    let mut per_group_section_writers =
        section_part_ids_writer.take_shards(group_section_counts.into_iter());

    groups
        .par_iter_mut()
        .zip(per_group_section_writers.par_iter_mut())
        .try_for_each_init(
            || herd.get(),
            |allocator,
             (group, shard): (
                &mut ResolvedGroup<'data, P>,
                &mut sharded_vec_writer::Shard<PartId>,
            )|
             -> Result {
                verbose_timing_phase!("Resolve group sections");

                for file in &mut group.files {
                    match file {
                        ResolvedFile::Object(obj) => {
                            obj.relocations = obj.common.object.parse_relocations()?;
                            let (sections, part_ids) = resolve_sections_for_object(
                                &mut *obj,
                                symbol_db.args,
                                allocator,
                                &loaded_metrics,
                                &layout_rules.section_rules,
                                output_sections,
                            )?;
                            obj.sections = sections;
                            for part_id in part_ids {
                                shard.push(part_id);
                            }
                        }
                        ResolvedFile::NotLoaded(n) => {
                            for _ in 0..n.section_id_range.len() {
                                shard.push(crate::part_id::UNMAPPED);
                            }
                        }
                        _ => {}
                    }
                }
                Ok(())
            },
        )?;

    for shard in per_group_section_writers {
        section_part_ids_writer.return_shard(shard);
    }

    symbol_db.section_part_ids = section_part_ids;

    loaded_metrics.log();

    Ok(())
}

pub(super) fn assign_section_ids<'data, P: Platform>(
    resolved: &mut [ResolvedGroup<'data, P>],
    section_part_ids: &mut [PartId],
    output_sections: &mut OutputSections<'data, P>,
    args: &P::Args,
) {
    timing_phase!("Assign section IDs");

    for group in resolved {
        for file in &mut group.files {
            if let ResolvedFile::Object(s) = file {
                let obj_part_ids = &mut section_part_ids[s.section_id_range.as_usize()];
                output_sections.add_sections(&s.custom_sections, obj_part_ids, args);
                apply_init_fini_secondaries(
                    &s.init_fini_sections,
                    s.sections.as_slice(),
                    obj_part_ids,
                    output_sections,
                );
            }
        }
    }
}
fn apply_init_fini_secondaries<'data, P: Platform>(
    details: &[InitFiniSectionDetail],
    sections: &[SectionSlot],
    section_part_ids: &mut [PartId],
    output_sections: &mut OutputSections<'data, P>,
) {
    for d in details {
        let Some(slot) = sections.get(d.index as usize) else {
            continue;
        };

        match slot {
            SectionSlot::Unloaded(_) | SectionSlot::MustLoad(_) => {}
            _ => continue,
        }

        let sid =
            output_sections.get_or_create_init_fini_secondary(d.primary, d.priority, d.alignment);
        section_part_ids[d.index as usize] = sid.part_id_with_alignment::<P>(d.alignment);
    }
}
fn resolve_sections_for_object<'data, P: Platform>(
    obj: &mut ResolvedObject<'data, P>,
    args: &P::Args,
    allocator: &bumpalo_herd::Member<'data>,
    loaded_metrics: &LoadedMetrics,
    rules: &SectionRules,
    output_sections: &OutputSections<'data, P>,
) -> Result<(Vec<SectionSlot>, Vec<PartId>)> {
    // Note, we build up the collection with push rather than collect because at the time of
    // writing, object's `SectionTable::enumerate` isn't an exact-size iterator, so using collect
    // would result in resizing.
    let mut sections = Vec::with_capacity(obj.common.object.num_sections());
    let mut section_part_ids = Vec::with_capacity(obj.common.object.num_sections());
    let mut executable_bytes: u64 = 0;
    for (input_section_index, input_section) in obj.common.object.enumerate_sections() {
        let section_size = obj.common.object.section_size(input_section).unwrap_or(0);
        if input_section.is_executable() {
            executable_bytes += section_size;
        }
        let (slot, part_id) = resolve_section(
            input_section_index,
            input_section,
            obj,
            args,
            allocator,
            loaded_metrics,
            rules,
            output_sections,
        )?;
        sections.push(slot);
        section_part_ids.push(part_id);
    }
    obj.executable_bytes = executable_bytes;
    Ok((sections, section_part_ids))
}

fn part_id_for_output<P: Platform>(
    output_info: &SectionOutputInfo,
    alignment: Alignment,
) -> PartId {
    if output_info.input_order {
        output_info
            .section_id
            .part_id_with_alignment::<P>(crate::alignment::MIN)
    } else if output_info.section_id.is_regular::<P>() {
        output_info
            .section_id
            .part_id_with_alignment::<P>(alignment)
    } else {
        output_info.section_id.base_part_id::<P>()
    }
}

#[inline(always)]
fn resolve_section<'data, P: Platform>(
    input_section_index: SectionIndex,
    input_section: &'data P::SectionHeader,
    obj: &mut ResolvedObject<'data, P>,
    args: &P::Args,
    allocator: &bumpalo_herd::Member<'data>,
    loaded_metrics: &LoadedMetrics,
    rules: &SectionRules,
    output_sections: &OutputSections<'data, P>,
) -> Result<(SectionSlot, PartId)> {
    let section_name = obj
        .common
        .object
        .section_name(input_section_index)
        .unwrap_or_default();

    P::verify_allowed_input_section_name(section_name)?;

    let raw_alignment = obj.common.object.section_alignment(input_section)?;
    let alignment = Alignment::new(raw_alignment.max(1))?;
    let should_merge_sections = part_id::should_merge_sections(input_section, raw_alignment, args)
        && !obj
            .common
            .object
            .section_has_relocations(input_section_index, &obj.relocations);

    let mut unloaded_section;
    let mut is_debug_info = false;
    let mut must_load = input_section.should_retain() || input_section.is_note();
    let part_id: PartId;

    let file_name = if let Some(entry) = &obj.common.input.entry {
        // For archive members, match against the member name (e.g., "app.o"),
        // not the archive filename (e.g., "libfoo.a").
        Some(entry.identifier.as_slice())
    } else {
        obj.common
            .input
            .file
            .filename
            .file_name()
            .map(|n| n.as_encoded_bytes())
    };

    let emit_relocs_name = if args.emit_relocs()
        && !args.should_output_partial_object()
        && input_section.is_reloc_section()
    {
        emit_relocs_section_name::<P>(
            input_section,
            section_name,
            obj.common.object,
            file_name,
            rules,
            output_sections,
            allocator,
        )
    } else {
        None
    };

    let rule_outcome = if args.should_output_partial_object() {
        P::lookup_for_partial_link(section_name, input_section, args)
    } else {
        let outcome = rules.lookup::<P>(section_name, file_name, input_section);
        if matches!(outcome, SectionRuleOutcome::Discard) && emit_relocs_name.is_some() {
            SectionRuleOutcome::Custom
        } else {
            outcome
        }
    };

    match rule_outcome {
        SectionRuleOutcome::Section(output_info) => {
            part_id = part_id_for_output::<P>(&output_info, alignment);

            must_load |= output_info.must_keep;

            unloaded_section = UnloadedSection::new();
            unloaded_section.needs_sorting = output_info.sorted || args.sort_sections_by_name();
            unloaded_section.sort_by_init_priority = output_info.sort_by_init_priority;
            unloaded_section.sort_by_alignment = output_info.sort_by_alignment;
        }
        SectionRuleOutcome::SortedSection(output_info) => {
            part_id = part_id_for_output::<P>(&output_info, alignment);
            if let Some(priority) = P::init_section_priority(section_name) {
                obj.init_fini_sections.push(InitFiniSectionDetail {
                    index: input_section_index.0 as u32,
                    primary: output_info.section_id,
                    priority,
                    alignment,
                });
            }

            must_load |= output_info.must_keep;

            unloaded_section = UnloadedSection::new();
        }
        SectionRuleOutcome::Discard => {
            return Ok((SectionSlot::Discard, crate::part_id::UNMAPPED));
        }
        SectionRuleOutcome::NoteGnuStack => {
            P::validate_stack_section(input_section, obj, args)?;
            return Ok((SectionSlot::Discard, crate::part_id::UNMAPPED));
        }
        SectionRuleOutcome::EhFrame => {
            return Ok((
                SectionSlot::FrameData(input_section_index),
                crate::part_id::UNMAPPED,
            ));
        }
        SectionRuleOutcome::NoteGnuProperty => {
            return Ok((
                SectionSlot::NoteGnuProperty(input_section_index),
                crate::part_id::UNMAPPED,
            ));
        }
        SectionRuleOutcome::Debug => {
            if args.should_strip_debug() && !input_section.is_alloc() {
                return Ok((SectionSlot::Discard, crate::part_id::UNMAPPED));
            }

            is_debug_info = !input_section.is_alloc();

            part_id = PartId::CUSTOM_PLACEHOLDER;
            unloaded_section = UnloadedSection::new();
        }
        SectionRuleOutcome::DebugIndex => {
            P::handle_debug_index_section(
                obj,
                input_section_index,
                input_section,
                allocator,
                loaded_metrics,
            )?;
            return Ok((SectionSlot::Discard, crate::part_id::UNMAPPED));
        }
        SectionRuleOutcome::Custom => {
            part_id = PartId::CUSTOM_PLACEHOLDER;
            unloaded_section = UnloadedSection::new();
            unloaded_section.start_stop_eligible = !section_name.starts_with(b".");
        }
        SectionRuleOutcome::RiscVAttribute => {
            return Ok((
                SectionSlot::RiscvVAttributes(input_section_index),
                crate::part_id::UNMAPPED,
            ));
        }
    }

    if part_id == PartId::CUSTOM_PLACEHOLDER {
        let identity_name = emit_relocs_name.unwrap_or(section_name);
        let custom_section = CustomSectionDetails {
            identity: P::section_identity(SectionName(identity_name), input_section),
            alignment,
            index: input_section_index,
        };

        obj.custom_sections.push(custom_section);
    }

    let slot = if should_merge_sections {
        let section_data =
            obj.common
                .object
                .section_data(input_section, allocator, loaded_metrics)?;

        if section_data.is_empty() {
            return Ok((SectionSlot::Discard, crate::part_id::UNMAPPED));
        }

        obj.string_merge_extras.push(StringMergeSectionExtra {
            index: input_section_index,
            section_data,
            is_strings: input_section.is_strings(),
            alignment,
            entsize: input_section.merge_entsize(),
        });

        SectionSlot::MergeStrings(StringMergeSectionSlot::new())
    } else if is_debug_info {
        SectionSlot::UnloadedDebugInfo
    } else if must_load {
        SectionSlot::MustLoad(unloaded_section)
    } else {
        SectionSlot::Unloaded(unloaded_section)
    };

    Ok((slot, part_id))
}

/// GNU `--emit-relocs` names copied reloc sections after the *output* section that
/// contains the target (`.data.rel.local` in `.data` → `.rela.data`), not the
/// input reloc name (`.rela.data.rel.local`).
fn emit_relocs_section_name<'data, P: Platform>(
    input_section: &P::SectionHeader,
    input_section_name: &'data [u8],
    object: &P::File<'data>,
    file_name: Option<&[u8]>,
    rules: &SectionRules,
    output_sections: &OutputSections<'data, P>,
    allocator: &bumpalo_herd::Member<'data>,
) -> Option<&'data [u8]> {
    let prefix = input_section.reloc_output_name_prefix()?;
    let target_idx = input_section.reloc_target_section_index()?;
    let target_name = object.section_name(target_idx).ok()?;
    let target_header = object.section(target_idx).ok()?;
    let target_output_name = match rules.lookup::<P>(target_name, file_name, target_header) {
        SectionRuleOutcome::Section(info) | SectionRuleOutcome::SortedSection(info) => {
            let primary = output_sections.primary_output_section(info.section_id);
            output_sections.name(primary)?.0
        }
        SectionRuleOutcome::Custom | SectionRuleOutcome::Debug => target_name,
        SectionRuleOutcome::EhFrame => {
            let id = P::EH_FRAME_SECTION_ID?;
            output_sections.name(id)?.0
        }
        _ => return None,
    };
    if input_section_name.len() == prefix.len() + target_output_name.len()
        && input_section_name.starts_with(prefix)
        && &input_section_name[prefix.len()..] == target_output_name
    {
        return Some(input_section_name);
    }
    let mut name = Vec::with_capacity(prefix.len() + target_output_name.len());
    name.extend_from_slice(prefix);
    name.extend_from_slice(target_output_name);
    Some(allocator.alloc_slice_copy(&name))
}

pub(super) fn populate_start_stop_sections<'data, P: Platform>(
    resolved: &[ResolvedGroup<'data, P>],
    section_part_ids: &[PartId],
    output_sections: &OutputSections<'data, P>,
    args: &P::Args,
    syn: &mut ResolvedSyntheticSymbols<'data, P>,
) {
    if !P::NEEDS_START_STOP_SECTION_GC || !args.should_gc_sections() {
        return;
    }

    let mut referenced_sections = output_sections.new_section_map::<bool>();
    let mut has_referenced_sections = false;

    for definition in &syn.symbol_definitions {
        if let Some(section_id) = definition.section_id() {
            *referenced_sections.get_mut(section_id) = true;
            has_referenced_sections = true;
        }
    }

    if !has_referenced_sections {
        return;
    }

    let start_stop_sections = syn.start_stop_sections.as_mut().unwrap();
    for group in resolved {
        for file in &group.files {
            let ResolvedFile::Object(s) = file else {
                continue;
            };

            let obj_part_ids = &section_part_ids[s.section_id_range.as_usize()];

            for custom_section in &s.custom_sections {
                let section_index = custom_section.index;

                let SectionSlot::Unloaded(unloaded) = s.sections[section_index.0] else {
                    continue;
                };

                if !unloaded.start_stop_eligible {
                    continue;
                }

                let section_id = obj_part_ids[section_index.0].output_section_id::<P>();
                if !*referenced_sections.get(section_id) {
                    continue;
                }

                let gc_unit = P::gc_unit_for_section(section_index);

                start_stop_sections
                    .get_mut(section_id)
                    .push(StartStopCandidate {
                        file_id: s.common.file_id,
                        gc_unit,
                    });
            }
        }
    }
}
