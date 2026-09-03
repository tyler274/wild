//! Traverses the graph of symbol references to figure out what sections from the input files are
//! referenced. Determines which sections need to be linked, sums their sizes decides what goes
//! where in the output file then allocates addresses for each symbol.

use crate::FileSystem;
use crate::diagnostics::SymbolInfoPrinter;
use crate::error;
use crate::error::Context;
use crate::error::Result;
use crate::expression_eval::evaluate_const;
use crate::file_writer;
use crate::grouping::Group;
use crate::grouping::SequencedLinkerScript;
use crate::input_data::FileId;
use crate::output_section_id::OutputSections;
use crate::output_section_map::OutputSectionMap;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::part_id::PartId;
use crate::platform::Arch;
use crate::platform::Args as _;
use crate::platform::ObjectFile;
use crate::platform::Platform;
use crate::platform::SectionAttributes as _;
use crate::resolution::ResolvedGroup;
use crate::string_merging::MergedStringStartAddresses;
use crate::symbol_db::SymbolDb;
use crate::timing_phase;
use crate::value_flags::PerSymbolFlags;
use hashbrown::HashMap;
use hashbrown::HashSet;
use itertools::Itertools;
use linker_utils::elf::RelocationKind;
use std::sync::Mutex;

pub(crate) mod addresses;
pub(crate) mod graph;
pub(crate) mod script;
pub(crate) mod sections;
pub(crate) mod sizes;
pub(crate) mod types;

pub(crate) use addresses::*;
pub(crate) use graph::*;
pub(crate) use script::*;
pub(crate) use sections::*;
pub(crate) use sizes::*;
pub(crate) use types::*;

pub fn compute<'data, P: Platform, A: Arch<Platform = P>, F: FileSystem>(
    symbol_db: SymbolDb<'data, A::Platform>,
    mut per_symbol_flags: PerSymbolFlags,
    mut groups: Vec<ResolvedGroup<'data, A::Platform>>,
    mut output_sections: OutputSections<'data, P>,
    output: &mut file_writer::Output<F>,
) -> Result<Layout<'data, A::Platform>> {
    timing_phase!("Layout");

    let layout_resources_ext = <A::Platform as Platform>::layout_resources_ext(&symbol_db.groups);

    let atomic_per_symbol_flags = per_symbol_flags.borrow_atomic();

    let mut symbol_info_printer = SymbolInfoPrinter::new(symbol_db.args, &groups);
    symbol_info_printer.update(&symbol_db, &atomic_per_symbol_flags);

    let string_merge_inputs = crate::string_merging::StringMergeInputs::new(
        &mut groups,
        &symbol_db.section_part_ids,
        &output_sections,
    )?;

    let (merged_strings, gc_outputs) = rayon::join(
        || {
            crate::string_merging::merge_strings(
                &string_merge_inputs,
                &output_sections,
                symbol_db.args,
            )
        },
        || {
            traverse_reference_graph::<A>(
                groups,
                &symbol_db,
                &atomic_per_symbol_flags,
                &output_sections,
                layout_resources_ext,
            )
        },
    );

    let mut merged_strings = merged_strings?;
    let gc_outputs = gc_outputs?;

    let mut group_states = gc_outputs.group_states;
    let thunk_layout_builder = gc_outputs.thunk_layout_builder;

    let epilogue_file_id = FileId::new(group_states.len() as u32, 0);

    let atomic_per_symbol_flags = per_symbol_flags.borrow_atomic();
    P::finalise_copy_relocations(&mut group_states, &symbol_db, &atomic_per_symbol_flags)?;

    let mut dynamic_symbol_definitions =
        merge_dynamic_symbol_definitions(&group_states, &symbol_db)?;

    create_canonical_plt_entries(
        &group_states,
        &symbol_db,
        &atomic_per_symbol_flags,
        &mut dynamic_symbol_definitions,
    )?;

    let mut script_sorted_sections = harvest_and_sort_script_sections(
        &mut group_states,
        &output_sections,
        &symbol_db.section_part_ids,
    );

    group_states.push(GroupState {
        files: vec![FileLayoutState::Epilogue(EpilogueLayoutState::new(
            symbol_db.args,
            symbol_db.output_kind,
            &mut dynamic_symbol_definitions,
            &group_states,
        ))],
        queue: LocalWorkQueue::new(epilogue_file_id.group()),
        common: CommonGroupState::new(&output_sections),
        num_symbols: 0,
        section_group_order: SectionGroupOrder::Epilogue,
    });

    let finalise_sizes_ext =
        P::create_finalise_sizes_ext::<A>(symbol_db.args, &mut group_states, &symbol_db)?;

    let finalise_sizes_resources = FinaliseSizesResources {
        dynamic_symbol_definitions: &dynamic_symbol_definitions,
        symbol_db: &symbol_db,
        merged_strings: &merged_strings,
        format_specific: &finalise_sizes_ext,
        script_sorted_sections: &script_sorted_sections,
    };

    finalise_all_sizes::<P, A>(
        &mut group_states,
        &atomic_per_symbol_flags,
        &finalise_sizes_resources,
    )?;

    // Dropping `symbol_info_printer` will cause it to print. So we'll either print now, or, if we
    // got an error or panic, then we'll have printed at that point.
    symbol_info_printer.update(&symbol_db, &atomic_per_symbol_flags);
    drop(symbol_info_printer);

    let non_addressable_counts = apply_non_addressable_indexes(&mut group_states, &symbol_db)?;

    propagate_section_attributes(&group_states, &mut output_sections);

    let linker_scripts: Vec<&SequencedLinkerScript<P>> = symbol_db
        .groups
        .iter()
        .filter_map(|group| match group {
            Group::LinkerScripts(scripts) => Some(scripts),
            _ => None,
        })
        .flatten()
        .collect();

    let mut location_counters = Vec::new();
    for script in &linker_scripts {
        location_counters.extend(script.parsed.location_counters.iter().cloned());
    }

    let (output_order, program_segments) =
        output_sections.output_order(symbol_db.output_kind, &linker_scripts, &location_counters)?;

    tracing::trace!(
        "Output order:\n{}",
        output_order.display::<A::Platform>(&output_sections, &program_segments)
    );

    let (mut section_part_sizes, gdb_index_data) = compute_total_section_part_sizes(
        &mut group_states,
        &mut output_sections,
        &output_order,
        &program_segments,
        &mut per_symbol_flags,
        gc_outputs.must_keep_sections,
        &finalise_sizes_resources,
    )?;
    drop(finalise_sizes_resources);

    if symbol_db.args.common().incremental {
        for (section_id, _) in output_sections.ids_with_info() {
            if !section_id.is_regular::<P>() || !output_sections.has_data_in_file(section_id) {
                continue;
            }
            // Metadata sections (.comment, .symtab, …) are rewritten on every update; only
            // allocated content needs spare room to grow.
            if !output_sections
                .output_info(section_id)
                .section_attributes
                .is_alloc()
            {
                continue;
            }
            let range = section_id.part_id_range::<P>();
            let mut part_id = PartId::from_usize(range.end.as_usize().saturating_sub(1));
            while part_id.as_usize() >= range.start.as_usize() {
                if section_part_sizes.get(part_id) > 0 {
                    section_part_sizes
                        .increment(part_id, crate::incremental::INCREMENTAL_SECTION_PADDING);
                    break;
                }
                if part_id.as_usize() == range.start.as_usize() {
                    break;
                }
                part_id = PartId::from_usize(part_id.as_usize() - 1);
            }
        }
    }

    let got_relr_n = A::Platform::GOT_RELR_SECTION_ID
        .and_then(A::Platform::single_part_id)
        .map_or(0, |part_id| section_part_sizes.get(part_id) / 8);

    let thunk_blocks = thunk_layout_builder
        .map(|builder| {
            builder.build(
                &mut group_states,
                &symbol_db,
                &per_symbol_flags,
                &output_sections,
                &section_part_sizes,
            )
        })
        .unwrap_or_default();

    allocate_thunk_block_space::<A::Platform>(
        &mut group_states,
        &thunk_blocks,
        &mut section_part_sizes,
        &symbol_db,
    );

    let mut memory_regions = HashMap::new();
    let mut memory_region_order = Vec::new();
    for s in &linker_scripts {
        for region in &s.parsed.memory_regions {
            memory_regions
                .try_insert(
                    region.name,
                    MemoryRegion {
                        origin: evaluate_const(&region.origin)?,
                        length: evaluate_const(&region.length)?,
                        used: 0,
                        used_lma: 0,
                        flags: region.flags,
                    },
                )
                .map_err(|_| {
                    error!(
                        "region '{}' already defined",
                        String::from_utf8_lossy(region.name)
                    )
                })?;
            memory_region_order.push(region.name);
        }
    }

    let sizeof_headers = if let Some(FileLayoutState::Prelude(internal)) =
        group_states.first().and_then(|g| g.files.first())
    {
        P::get_sizeof_headers(internal.header_info.as_ref().unwrap())
    } else {
        unreachable!();
    };

    P::finalise_output_section_alignments(&section_part_sizes, &mut output_sections);

    let (mut section_part_layouts, mut section_layouts, mut resolved_location_counters) =
        compute_and_apply_section_layout::<A::Platform>(
            &mut group_states,
            &section_part_sizes,
            &output_sections,
            &program_segments,
            &output_order,
            &symbol_db,
            &mut memory_regions,
            &memory_region_order,
            sizeof_headers,
        )?;

    if apply_merge_vma_padding::<A::Platform>(
        &mut merged_strings,
        &mut group_states,
        &mut section_part_sizes,
        &section_part_layouts,
    ) {
        (
            section_part_layouts,
            section_layouts,
            resolved_location_counters,
        ) = compute_and_apply_section_layout::<A::Platform>(
            &mut group_states,
            &section_part_sizes,
            &output_sections,
            &program_segments,
            &output_order,
            &symbol_db,
            &mut memory_regions,
            &memory_region_order,
            sizeof_headers,
        )?;
    }

    extend_sections_for_script_output_data(
        &output_sections,
        &mut section_layouts,
        &resolved_location_counters,
    );

    if symbol_db.args.should_relax() && A::supports_size_reduction_relaxations() {
        perform_iterative_relaxation::<A>(
            &mut group_states,
            &mut section_part_sizes,
            &mut section_part_layouts,
            &mut section_layouts,
            &output_sections,
            &program_segments,
            &output_order,
            &symbol_db,
            &per_symbol_flags,
            &mut memory_regions,
            &memory_region_order,
            sizeof_headers,
            &mut resolved_location_counters,
        )?;
    }

    if let Some((last_section_id, _)) = section_layouts
        .iter()
        .max_by_key(|(_, record)| record.file_end())
    {
        let last_part_id =
            PartId::from_usize(last_section_id.part_id_range::<P>().end.as_usize() - 1);

        let extra_file_size = A::Platform::last_part_size_to_extend(
            &section_part_layouts.get(last_part_id),
            last_part_id,
        )?;

        if extra_file_size > 0 {
            section_part_sizes.increment(last_part_id, extra_file_size as u64);

            let part_layout = section_part_layouts.get_mut(last_part_id);
            part_layout.file_size += extra_file_size;
            part_layout.mem_size += extra_file_size as u64;

            let section_layout = section_layouts.get_mut(last_section_id);
            section_layout.file_size += extra_file_size;
            section_layout.mem_size += extra_file_size as u64;
        }
    }

    let merged_section_layouts = merge_secondary_parts(&output_sections, &section_layouts);

    let Some(FileLayoutState::Prelude(internal)) =
        &group_states.first().and_then(|g| g.files.first())
    else {
        unreachable!();
    };
    let header_info = internal.header_info.as_ref().unwrap();
    let segment_layouts = compute_segment_layout::<A::Platform>(
        &section_layouts,
        &output_sections,
        &output_order,
        &program_segments,
        header_info,
        symbol_db.args,
    )?;

    let mem_offsets: OutputSectionPartMap<u64> = starting_memory_offsets(&section_part_layouts);
    let starting_mem_offsets_by_group = compute_start_offsets_by_group(&group_states, mem_offsets);

    let merged_string_start_addresses = MergedStringStartAddresses::compute(
        &output_sections,
        &starting_mem_offsets_by_group,
        &merged_strings,
    );

    assign_addresses_to_sorted_sections(
        &mut group_states,
        &starting_mem_offsets_by_group,
        &mut script_sorted_sections,
    );

    let mut symbol_resolutions = SymbolResolutions {
        resolutions: Vec::with_capacity(symbol_db.num_symbols()),
    };

    let mut res_writer = sharded_vec_writer::VecWriter::new(&mut symbol_resolutions.resolutions);

    let mut per_group_res_writers = group_states
        .iter()
        .map(|group| res_writer.take_shard(group.num_symbols))
        .collect_vec();

    let thunk_block_addresses_out = std::iter::repeat_with(Default::default)
        .take(thunk_blocks.len())
        .collect();

    let resources = FinaliseLayoutResources {
        symbol_db: &symbol_db,
        output_sections: &output_sections,
        output_order: &output_order,
        section_layouts: &section_layouts,
        merged_string_start_addresses: &merged_string_start_addresses,
        merged_strings: &merged_strings,
        per_symbol_flags: &per_symbol_flags,
        dynamic_symbol_definitions: &dynamic_symbol_definitions,
        segment_layouts: &segment_layouts,
        program_segments: &program_segments,
        format_specific: &finalise_sizes_ext,
        thunk_blocks: &thunk_blocks,
        thunk_block_addresses: &thunk_block_addresses_out,
        script_sorted_sections: &script_sorted_sections,
    };

    let group_layouts = compute_symbols_and_layouts(
        group_states,
        starting_mem_offsets_by_group,
        &mut per_group_res_writers,
        &resources,
    )?;

    for shard in per_group_res_writers {
        res_writer
            .try_return_shard(shard)
            .context("Group resolutions not filled")?;
    }

    update_dynamic_symbol_resolutions(
        &resources,
        &group_layouts,
        &mut symbol_resolutions.resolutions,
    );
    update_redirect_resolutions(
        &symbol_db,
        &mut symbol_resolutions.resolutions,
        &output_sections,
        &section_layouts,
        &merged_section_layouts,
        sizeof_headers,
        &memory_regions,
        &resolved_location_counters,
    )?;
    crate::gc_stats::maybe_write_gc_stats(&group_layouts, &symbol_db)?;

    let thunk_block_addresses = thunk_block_addresses_out
        .into_iter()
        .map(|m| m.into_inner().unwrap())
        .collect();

    let relocation_statistics = OutputSectionMap::with_size(section_layouts.len());

    let num_sections = output_sections.num_sections();

    let format_specific = P::create_layout_ext(finalise_sizes_ext, &symbol_resolutions)?;

    let incremental_reverse_relocs = Mutex::new(if symbol_db.args.common().incremental {
        crate::incremental::ReverseRelocIndex::new(symbol_resolutions.resolutions.len())
    } else {
        crate::incremental::ReverseRelocIndex::new(0)
    });

    let mut layout = Layout {
        symbol_db,
        symbol_resolutions,
        got_relr_n,
        segment_layouts,
        section_part_layouts,
        section_layouts,
        merged_section_layouts,
        group_layouts,
        output_sections,
        program_segments,
        output_order,
        non_addressable_counts,
        merged_strings,
        merged_string_start_addresses,
        has_static_tls: gc_outputs.has_static_tls,
        has_variant_pcs: gc_outputs.has_variant_pcs,
        relocation_statistics,
        per_symbol_flags,
        dynamic_symbol_definitions,
        format_specific,
        thunk_block_addresses,
        compressed_debug_sections: OutputSectionMap::with_size(num_sections),
        gdb_index_data,
        script_sorted_sections,
        resolved_location_counters,
        incremental_skip_payloads: HashSet::new(),
        incremental_reverse_relocs,
        incremental_patch: None,
    };

    P::maybe_compress_debug_sections::<A>(&mut layout)?;

    output.set_size(compute_total_file_size(&layout.section_layouts));

    Ok(layout)
}

pub(crate) fn objects_iter<'groups, 'data, P: Platform>(
    group_states: &'groups [GroupState<'data, P>],
) -> impl Iterator<Item = &'groups ObjectLayoutState<'data, P>> + Clone {
    group_states.iter().flat_map(|group| {
        group.files.iter().filter_map(|file| match file {
            FileLayoutState::Object(object) => Some(object),
            _ => None,
        })
    })
}

pub(crate) fn section_debug<P: Platform>(
    object: &P::File<'_>,
    section_index: object::SectionIndex,
) -> impl std::fmt::Display {
    let name = object.section_name(section_index).map_or_else(
        |_| "??".to_owned(),
        |name| String::from_utf8_lossy(name).into_owned(),
    );
    std::fmt::from_fn(move |f| write!(f, "`{name}`"))
}

pub(crate) fn needs_tlsld(relocation_kind: RelocationKind) -> bool {
    matches!(
        relocation_kind,
        RelocationKind::TlsLd | RelocationKind::TlsLdGot | RelocationKind::TlsLdGotBase
    )
}
