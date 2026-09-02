use crate::OutputFileData;
use crate::OutputKind;
use crate::alignment;
use crate::args::elf::BuildIdOption;
use crate::args::elf::ElfArgs;
use crate::bail;
use crate::elf;
use crate::elf::ElfClass;
use crate::elf::ElfWord as _;
use crate::elf::GNU_NOTE_NAME;
use crate::elf::NoteProperty;
use crate::elf::RiscVAttribute;
use crate::elf::output_section_id;
use crate::elf::part_id;
use crate::ensure;
use crate::error;
use crate::error::Context as _;
use crate::error::Result;
use crate::file_writer::SizedOutput;
use crate::file_writer::insufficient_allocation;
use crate::file_writer::split_buffers_by_alignment;
use crate::file_writer::split_output_by_group;
use crate::file_writer::split_output_into_sections;
use crate::layout::EpilogueLayout;
use crate::layout::FileLayout;
use crate::layout::InternalSymbols;
use crate::layout::Layout;
use crate::layout::LinkerScriptLayoutState;
use crate::layout::ObjectLayout;
use crate::layout::PreludeLayout;
use crate::layout::Resolution;
use crate::layout::Section;
use crate::layout::SyntheticSymbolsLayout;
use crate::output_section_id::OrderEvent;
use crate::output_section_id::OutputOrder;
use crate::output_section_id::OutputSectionId;
use crate::output_section_id::OutputSections;
use crate::output_section_id::SectionOutputInfo;
use crate::output_section_map::OutputSectionMap;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::output_trace::TraceOutput;
use crate::parsing::SymbolLoc;
use crate::part_id::PartId;
use crate::platform::Arch;
use crate::platform::Args as _;
use crate::platform::ObjectFile;
use crate::platform::Platform;
use crate::platform::SectionAttributes as _;
use crate::platform::SectionType as _;
use crate::resolution::SectionSlot;
use crate::sframe;
use crate::sharding::ShardKey;
use crate::timing_phase;
use crate::value_flags::ValueFlags;
use crate::verbose_timing_phase;
use crate::writable_elf::WritableNoteHeader as _;
use crate::writable_elf::WritableSymbol as _;
use linker_utils::elf::RISCV_ATTRIBUTE_VENDOR_NAME;
use linker_utils::elf::RelocationKind;
use linker_utils::elf::riscvattr::TAG_RISCV_ARCH;
use linker_utils::elf::riscvattr::TAG_RISCV_PRIV_SPEC;
use linker_utils::elf::riscvattr::TAG_RISCV_PRIV_SPEC_MINOR;
use linker_utils::elf::riscvattr::TAG_RISCV_PRIV_SPEC_REVISION;
use linker_utils::elf::riscvattr::TAG_RISCV_STACK_ALIGN;
use linker_utils::elf::riscvattr::TAG_RISCV_UNALIGNED_ACCESS;
use linker_utils::elf::riscvattr::TAG_RISCV_WHOLE_FILE;
use linker_utils::elf::secnames;
use linker_utils::elf::secnames::NOTE_GNU_BUILD_ID_SECTION_NAME_STR;
use object::LittleEndian;
use object::elf::NT_GNU_BUILD_ID;
use object::elf::NT_GNU_PROPERTY_TYPE_0;
use object::from_bytes_mut;
use object::read::elf::Crel;
use rayon::iter::IndexedParallelIterator;
use rayon::iter::IntoParallelIterator as _;
use rayon::iter::IntoParallelRefMutIterator as _;
use rayon::iter::ParallelBridge as _;
use rayon::iter::ParallelIterator as _;
use rayon::slice::ParallelSliceMut as _;
use std::collections::BTreeMap;
use std::io::Cursor;
use std::io::Write;
use std::sync::atomic::Ordering::Relaxed;
use tracing::debug_span;
use uuid::Uuid;
use zerocopy::FromBytes;
use zerocopy::transmute_mut;

pub(crate) mod dynamic;
pub(crate) mod headers;
pub(crate) mod relocations;
pub(crate) mod symbols;
pub(crate) mod types;

pub(crate) use dynamic::*;
pub(crate) use headers::*;
pub(crate) use relocations::*;
pub(crate) use symbols::*;
pub(crate) use types::*;

pub(crate) fn write<'data, C: ElfClass, A: Arch<Platform = elf::Elf<C>>>(
    sized_output: &mut SizedOutput<impl OutputFileData>,
    layout: &ElfLayout<'data, C>,
) -> Result {
    write_file_contents::<C, A>(sized_output, layout)?;
    apply_incremental_reloc_patches::<C, A>(sized_output, layout)?;
    if layout.args().common().validate_output {
        crate::validation::validate_bytes(layout, &sized_output.out)?;
    }

    let mut section_buffers = split_output_into_sections(layout, &mut sized_output.out).0;

    if layout.args().should_write_eh_frame_hdr
        && layout
            .section_layouts
            .get(output_section_id::EH_FRAME_HDR)
            .mem_size
            > 0
    {
        sort_eh_frame_hdr_entries(section_buffers.get_mut(output_section_id::EH_FRAME_HDR));
    }

    write_sframe_section(section_buffers.get_mut(output_section_id::SFRAME), layout)?;

    write_gnu_build_id_note(sized_output, &layout.args().build_id, layout)?;
    Ok(())
}

pub(crate) fn apply_incremental_reloc_patches<C: ElfClass, A: Arch<Platform = elf::Elf<C>>>(
    sized_output: &mut SizedOutput<impl OutputFileData>,
    layout: &ElfLayout<C>,
) -> Result {
    let Some(job) = &layout.incremental_patch else {
        return Ok(());
    };
    if layout.incremental_skip_payloads.is_empty() {
        return Ok(());
    }

    let new_res: Vec<u64> = layout.symbol_resolutions.raw_values().collect();
    let out = &mut sized_output.out;
    let n = job
        .old_resolutions
        .len()
        .min(new_res.len())
        .min(job.reverse_relocs.heads.len());
    let mut patched = 0u64;
    for sym_id in 0..n {
        let old = job.old_resolutions[sym_id];
        let new = new_res[sym_id];
        if old == new {
            continue;
        }
        let mut idx = job.reverse_relocs.heads[sym_id];
        while idx != u32::MAX {
            let node = &job.reverse_relocs.nodes[idx as usize];
            idx = node.next;
            if !layout
                .incremental_skip_payloads
                .contains(&crate::input_data::FileId::from_encoded(node.file_id))
            {
                continue;
            }
            patch_skipped_reloc_site::<C, A>(out, node, new)?;
            patched += 1;
        }
    }
    if patched > 0 {
        tracing::debug!(patched, "incremental reverse-reloc patches");
    }
    Ok(())
}

pub(crate) fn patch_skipped_reloc_site<C: ElfClass, A: Arch<Platform = elf::Elf<C>>>(
    out: &mut [u8],
    node: &crate::incremental::ReverseRelocNode,
    new_s: u64,
) -> Result {
    let r_type = object::elf::RelocationType(node.r_type);
    let Ok(rel_info) = A::relocation_from_raw(r_type) else {
        return Ok(());
    };
    let value = match rel_info.kind {
        RelocationKind::Absolute | RelocationKind::AbsoluteSet => {
            new_s.wrapping_add(node.addend as u64)
        }
        RelocationKind::Relative => new_s
            .wrapping_add(node.addend as u64)
            .wrapping_sub(node.place),
        _ => return Ok(()),
    };
    let start = usize::try_from(node.file_offset).context("reloc file offset overflow")?;
    if start >= out.len() {
        return Ok(());
    }
    rel_info.write_to_buffer(value, &mut out[start..])?;
    Ok(())
}

pub(crate) fn write_gnu_build_id_note<C: ElfClass>(
    sized_output: &mut SizedOutput<impl OutputFileData>,
    build_id_option: &BuildIdOption,
    layout: &ElfLayout<C>,
) -> Result {
    let hash_placeholder;
    let uuid_placeholder;
    let build_id = match build_id_option {
        BuildIdOption::Fast => {
            hash_placeholder = compute_hash(sized_output);
            hash_placeholder.as_bytes()
        }
        BuildIdOption::Hex(hex) => hex.as_slice(),
        BuildIdOption::Uuid => {
            uuid_placeholder = Uuid::new_v4();
            uuid_placeholder.as_bytes()
        }
        BuildIdOption::None => return Ok(()),
    };

    let dest_part = match layout.output_sections.gnu_build_id_dest_part() {
        Some(part) => part,
        None => return Ok(()),
    };
    let section_id = dest_part.output_section_id::<elf::Elf<C>>();
    let mut buffers = split_output_into_sections(layout, &mut sized_output.out).0;
    let section_buf = buffers.get_mut(section_id);
    let part_layout = layout.section_part_layouts.get(dest_part);
    let section_layout = layout.section_layouts.get(section_id);
    if part_layout.file_size == 0 {
        return Ok(());
    }
    let part_start = part_layout
        .file_offset
        .saturating_sub(section_layout.file_offset);
    let part_end = part_start + part_layout.file_size;
    let part_buf = section_buf
        .get_mut(part_start..part_end)
        .ok_or_else(|| insufficient_allocation(NOTE_GNU_BUILD_ID_SECTION_NAME_STR))?;
    let note_size = C::NOTE_HEADER_SIZE as usize + GNU_NOTE_NAME.len() + build_id.len();
    let start = part_buf
        .len()
        .checked_sub(note_size)
        .ok_or_else(|| insufficient_allocation(NOTE_GNU_BUILD_ID_SECTION_NAME_STR))?;
    let (note_header, mut rest) = from_bytes_mut::<elf::NoteHeader<C>>(&mut part_buf[start..])
        .map_err(|_| insufficient_allocation(NOTE_GNU_BUILD_ID_SECTION_NAME_STR))?;
    note_header.set_name_size(GNU_NOTE_NAME.len() as u32);
    note_header.set_descriptor_size(build_id.len() as u32);
    note_header.set_type(NT_GNU_BUILD_ID);

    let name_out = rest.split_off_mut(..GNU_NOTE_NAME.len()).unwrap();
    name_out.copy_from_slice(GNU_NOTE_NAME);

    rest.copy_from_slice(build_id);

    Ok(())
}

pub(crate) fn compute_hash(sized_output: &SizedOutput<impl OutputFileData>) -> blake3::Hash {
    timing_phase!("Compute build ID");
    blake3::Hasher::new()
        .update_rayon(&sized_output.out)
        .finalize()
}

pub(crate) fn write_file_contents<'data, C: ElfClass, A: Arch<Platform = elf::Elf<C>>>(
    sized_output: &mut SizedOutput<impl OutputFileData>,
    layout: &ElfLayout<'data, C>,
) -> Result {
    timing_phase!("Write data to file");
    let (mut section_buffers, padding) = split_output_into_sections(layout, &mut sized_output.out);

    fill_padding_for_sections::<C, A>(layout, padding);
    prefill_script_fill::<C, A>(layout, &mut section_buffers);
    write_script_output_data(layout, &mut section_buffers)?;

    let sym_index_map = if layout.args().should_copy_input_relocs() {
        build_sym_index_map(layout)
    } else {
        Vec::new()
    };

    let mut writable_buckets = split_buffers_by_alignment(&mut section_buffers, layout);
    prefill_script_fill_parts::<C, A>(layout, &mut writable_buckets);
    let groups_and_buffers = split_output_by_group(layout, &mut writable_buckets);
    groups_and_buffers
        .into_par_iter()
        .with_max_len(1)
        .try_for_each(|(group, mut buffers)| -> Result {
            verbose_timing_phase!("Write group");

            let mut table_writer = TableWriter::from_layout(
                layout,
                group.dynstr_start_offset,
                group.strtab_start_offset,
                &mut buffers,
                group.format_specific.eh_frame_start_address,
            );

            for file in &group.files {
                write_file::<C, A>(
                    file,
                    &mut buffers,
                    &mut table_writer,
                    layout,
                    &sized_output.trace,
                    &sym_index_map,
                )
                .with_context(|| format!("Failed copying from {file} to output file"))?;
            }
            table_writer
                .validate_empty(&group.mem_sizes)
                .with_context(|| format!("validate_empty failed for {group}"))?;
            Ok(())
        })?;

    for (output_section_id, _) in layout.output_sections.ids_with_info() {
        let relocations = layout
            .relocation_statistics
            .get(output_section_id)
            .load(Relaxed);

        if relocations > 0 {
            tracing::debug!(
                target: "metrics",
                section = layout.output_sections.display_name(output_section_id),
                relocations, "resolved relocations");
        }
    }

    fill_padding::<C, A>(layout, section_buffers);

    Ok(())
}

pub(crate) fn fill_padding_for_sections<C: ElfClass, A: Arch<Platform = elf::Elf<C>>>(
    layout: &Layout<'_, elf::Elf<C>>,
    padding: crate::file_writer::PaddingSlices<'_>,
) {
    timing_phase!("Fill padding for sections");

    for pslice in padding.slices {
        if pslice.slice.is_empty() {
            continue;
        }
        // Gaps between secondaries of the same primary already carry that section. Trailing
        // `. = . + N` after the last input sits between that primary and the next output
        // section, so look up the merged file range that still owns those bytes.
        let section_id = pslice
            .parent_section_id
            .or_else(|| section_covering_file_offset(layout, pslice.file_offset));
        if let Some(section_id) = section_id {
            let section_info = layout.output_sections.output_info(section_id);
            fill_section_padding::<C, A>(pslice.slice, section_info);
        } else {
            pslice.slice.fill(0);
        }
    }
}

pub(crate) fn section_covering_file_offset<C: ElfClass>(
    layout: &Layout<'_, elf::Elf<C>>,
    file_offset: usize,
) -> Option<crate::output_section_id::OutputSectionId> {
    let mut found = None;
    layout.merged_section_layouts.for_each(|id, rec| {
        if rec.file_size == 0 {
            return;
        }
        if file_offset >= rec.file_offset && file_offset < rec.file_end() {
            let info = layout.output_sections.output_info(id);
            if info.fill.is_some() {
                found = Some(id);
            }
        }
    });
    found
}

pub(crate) fn prefill_script_fill<C: ElfClass, A: Arch<Platform = elf::Elf<C>>>(
    layout: &Layout<'_, elf::Elf<C>>,
    section_buffers: &mut OutputSectionMap<&mut [u8]>,
) {
    for (section_id, info) in layout.output_sections.ids_with_info() {
        if info.fill.is_some() {
            fill_section_padding::<C, A>(section_buffers.get_mut(section_id), info);
        }
    }
}

pub(crate) fn prefill_script_fill_parts<C: ElfClass, A: Arch<Platform = elf::Elf<C>>>(
    layout: &Layout<'_, elf::Elf<C>>,
    writable_buckets: &mut OutputSectionPartMap<&mut [u8]>,
) {
    for event in &layout.output_order {
        let OrderEvent::Section(section_id) = event else {
            continue;
        };
        let info = layout.output_sections.output_info(section_id);
        if info.fill.is_none() {
            continue;
        }
        for part_id in section_id.parts::<elf::Elf<C>>() {
            let buf = writable_buckets.get_mut(part_id);
            if !buf.is_empty() {
                fill_section_padding::<C, A>(buf, info);
            }
        }
    }
}

pub(crate) fn fill_padding<C: ElfClass, A: Arch<Platform = elf::Elf<C>>>(
    layout: &Layout<'_, elf::Elf<C>>,
    mut section_buffers: OutputSectionMap<&mut [u8]>,
) {
    section_buffers.for_each_mut(|section_id, out| {
        if out.is_empty() {
            return;
        }
        fill_section_padding::<C, A>(out, layout.output_sections.output_info(section_id));
    });
}

pub(crate) fn write_sframe_section<C: ElfClass>(
    sframe_buffer: &mut [u8],
    layout: &ElfLayout<C>,
) -> Result {
    if layout.args().discard_sframe || sframe_buffer.is_empty() {
        return Ok(());
    }

    timing_phase!("Write .sframe");

    let sframe_start_address = layout.mem_address_of_built_in(output_section_id::SFRAME);
    let sframe_ranges: Vec<_> = layout
        .group_layouts
        .iter()
        .flat_map(|group| group.files.iter())
        .filter_map(|file| {
            if let FileLayout::Object(object) = file {
                Some(object.sframe_ranges.iter().cloned())
            } else {
                None
            }
        })
        .flatten()
        .collect();

    sframe::sort_sframe_section(
        sframe_buffer,
        sframe_start_address,
        &sframe_ranges,
        layout.symbol_db.args,
    )
}

pub(crate) fn sort_eh_frame_hdr_entries(eh_frame_hdr: &mut [u8]) {
    timing_phase!("Sort .eh_frame_hdr");
    let entry_bytes = &mut eh_frame_hdr[size_of::<elf::EhFrameHdr>()..];
    let entries = <[elf::EhFrameHdrEntry]>::mut_from_bytes(entry_bytes).unwrap();
    entries.par_sort_by_key(|e| e.frame_ptr);
}

pub(crate) fn write_file<'data, C: ElfClass, A: Arch<Platform = elf::Elf<C>>>(
    file: &FileLayout<'data, elf::Elf<C>>,
    buffers: &mut OutputSectionPartMap<&mut [u8]>,
    table_writer: &mut TableWriter<'_, '_, C>,
    layout: &ElfLayout<'data, C>,
    trace: &TraceOutput,
    sym_index_map: &[Option<u32>],
) -> Result {
    match file {
        FileLayout::Object(s) => {
            write_object::<C, A>(s, buffers, table_writer, layout, trace, sym_index_map)?;
        }
        FileLayout::Prelude(s) => write_prelude::<C, A>(s, buffers, table_writer, layout)?,
        FileLayout::Epilogue(s) => write_epilogue::<C, A>(s, buffers, table_writer, layout, trace)?,
        FileLayout::SyntheticSymbols(s) => {
            write_synthetic_symbols::<C, A>(s, table_writer, layout)?;
        }
        FileLayout::LinkerScript(s) => write_linker_script_state::<C, A>(s, table_writer, layout)?,
        FileLayout::NotLoaded | FileLayout::StubLibrary(_) => {}
        FileLayout::Dynamic(s) => write_dynamic_file::<C, A>(s, table_writer, layout)?,
    }
    Ok(())
}

pub(crate) fn write_object<'data, C: ElfClass, A: Arch<Platform = elf::Elf<C>>>(
    object: &ObjectLayout<'data, elf::Elf<C>>,
    buffers: &mut OutputSectionPartMap<&mut [u8]>,
    table_writer: &mut TableWriter<'_, '_, C>,
    layout: &ElfLayout<'data, C>,
    trace: &TraceOutput,
    sym_index_map: &[Option<u32>],
) -> Result {
    verbose_timing_phase!("Write object", file_id = object.file_id.as_u32());

    let _span = debug_span!("write_file", filename = %object.input).entered();
    let _file_span = layout.args().common().trace_span_for_file(object.file_id);

    for (i, sec) in object.sections.iter().enumerate() {
        let section_index = object::SectionIndex(i);

        match sec {
            SectionSlot::Loaded(sec) => {
                table_writer.reset_relr_run();
                write_object_section::<C, A>(
                    object,
                    layout,
                    *sec,
                    section_index,
                    buffers,
                    table_writer,
                    trace,
                )?;
            }
            SectionSlot::LoadedDebugInfo(sec) => {
                write_debug_section::<C, A>(object, layout, *sec, section_index, buffers)?;
            }
            SectionSlot::FrameData(section_index) => {
                write_eh_frame_data::<C, A>(object, *section_index, layout, table_writer, trace)?;
            }
            _ => (),
        }
    }
    for (symbol_id, resolution) in layout.resolutions_in_range(object.symbol_id_range) {
        let _span = tracing::trace_span!("Symbol", %symbol_id).entered();
        if let Some(res) = resolution {
            table_writer
                .process_resolution::<A>(Some(layout), layout.args(), res)
                .with_context(|| {
                    format!(
                        "Failed to process `{}` with resolution {res:?}",
                        layout.symbol_debug(symbol_id)
                    )
                })?;

            // Dynamic symbols that we define are handled by the epilogue so that they can be
            // written in the correct order. Here, we only need to handle weak symbols that we
            // reference that aren't defined by any shared objects we're linking against.
            if res.flags.is_dynamic() {
                let symbol = object
                    .object
                    .symbol(object.symbol_id_range.id_to_input(symbol_id))?;
                let name = object.object.symbol_name(symbol)?;
                table_writer.dynsym_writer.copy_symbol_shndx(
                    symbol,
                    name,
                    0,
                    0,
                    ValueFlags::empty(),
                )?;
                if layout.gnu_version_enabled() {
                    table_writer
                        .version_writer
                        .set_next_symbol_version(object::elf::VER_NDX_GLOBAL)?;
                }
            }
        }
    }

    if layout.args().should_output_partial_object() {
        write_symbols(object, &mut table_writer.debug_symbol_writer, layout)?;
        write_rela_sections(object, buffers, layout, sym_index_map)?;
    } else if layout.args().emit_relocs() {
        if !layout.args().should_strip_all() {
            write_symbols(object, &mut table_writer.debug_symbol_writer, layout)?;
        }
        write_rela_sections(object, buffers, layout, sym_index_map)?;
    } else if !layout.args().should_strip_all() {
        write_symbols(object, &mut table_writer.debug_symbol_writer, layout)?;
    }
    if object.owns_thunk_block
        && let Some(addresses) = layout
            .thunk_block_addresses
            .get(object.thunk_block_id.as_usize())
    {
        write_thunks::<C, A>(
            addresses,
            buffers,
            layout,
            &mut table_writer.debug_symbol_writer,
        )?;
    }
    Ok(())
}

/// Write thunk instructions for a set of (SymbolId -> thunk_address) mappings.
///
/// Thunks are sorted by SymbolId for determinism and written consecutively into the primary
/// function part buffer. Space must already have been reserved during `finalise_sizes`.
pub(crate) fn write_thunks<'data, C: ElfClass, A: Arch<Platform = elf::Elf<C>>>(
    thunk_addresses: &BTreeMap<crate::symbol_db::SymbolId, u64>,
    buffers: &mut OutputSectionPartMap<&mut [u8]>,
    layout: &ElfLayout<'data, C>,
    symbol_writer: &mut SymbolTableWriter<'_, '_, C>,
) -> Result {
    if thunk_addresses.is_empty() {
        return Ok(());
    }

    let config = A::thunk_config().expect("write_thunks called without thunk config");
    let thunk_size = config.thunk_size as usize;
    let primary_part_id = config.primary_function_part_id;
    let emit_symbols = !layout.args().should_strip_all();

    let text_section_id = primary_part_id.output_section_id::<elf::Elf<C>>();
    let text_shndx = layout
        .output_sections
        .output_index_of_section(text_section_id)
        .unwrap_or(0);

    for (symbol_id, &thunk_address) in thunk_addresses {
        debug_assert_ne!(thunk_address, 0, "Thunk address should have been assigned");

        let res = layout
            .merged_symbol_resolution(*symbol_id)
            .with_context(|| {
                format!(
                    "No resolution for symbol {} needed by thunk",
                    layout.symbol_db.symbol_name_for_display(*symbol_id)
                )
            })?;

        let target_address = res.plt_address().unwrap_or(res.raw_value);

        let buf = buffers.get_mut(primary_part_id);
        let thunk_buf = buf
            .split_off_mut(..thunk_size)
            .ok_or_else(|| crate::file_writer::insufficient_allocation("thunk space in .text"))?;

        A::write_thunk(thunk_address, target_address, thunk_buf);

        if emit_symbols {
            let orig_name = layout
                .symbol_db
                .symbol_name(*symbol_id)
                .map(|n| n.bytes().to_vec())
                .unwrap_or_default();
            let mut thunk_name = crate::elf::THUNK_SYMBOL_PREFIX.as_bytes().to_vec();
            thunk_name.extend_from_slice(&orig_name);
            let entry = symbol_writer.define_symbol(
                true,
                SymbolSection::Index(text_shndx),
                thunk_address,
                thunk_size as u64,
                Some(&thunk_name),
            )?;
            entry.set_binding_and_type(object::elf::STB_LOCAL, object::elf::STT_FUNC);
        }
    }

    Ok(())
}

pub(crate) fn write_object_section<'data, C: ElfClass, A: Arch<Platform = elf::Elf<C>>>(
    object: &ObjectLayout<'data, elf::Elf<C>>,
    layout: &ElfLayout<'data, C>,
    section: Section,
    section_index: object::SectionIndex,
    buffers: &mut OutputSectionPartMap<&mut [u8]>,
    table_writer: &mut TableWriter<'_, '_, C>,
    trace: &TraceOutput,
) -> Result {
    let part_id = object.section_part_id(section_index, &layout.symbol_db.section_part_ids);
    if layout.args().should_copy_input_relocs() {
        let section_type = layout
            .output_sections
            .output_info(part_id.output_section_id::<elf::Elf<C>>())
            .section_attributes
            .ty();
        if section_type.is_rela() || section_type.is_rel() {
            return Ok(());
        }
    }
    let skip_payload = layout.skip_incremental_payload(object.file_id);
    let out = write_section_raw::<C, A>(
        object,
        layout,
        section,
        section_index,
        buffers,
        !skip_payload,
    )?;
    if skip_payload {
        return Ok(());
    }

    // We need to reverse the contents and adjust relocations because .ctors/.dtors are executed in
    // reverse order while .init_array/.fini_array are executed in forward order.
    if should_reverse_contents(
        section_index,
        part_id,
        object.object,
        &layout.output_sections,
    ) {
        return write_section_reversed::<C, A>(
            object,
            layout,
            section_index,
            table_writer,
            trace,
            out,
        );
    }

    if layout.args().should_output_partial_object() {
        return Ok(());
    }

    let relocations = object.relocations(section_index)?;
    let result = match relocations {
        elf::RelocationList::Rela(rela) => apply_relocations::<C, A, elf::ElfRela<C>, _>(
            object,
            out,
            section_index,
            rela.iter().map(|rela| Ok(elf::ElfRela::new(*rela))),
            layout,
            table_writer,
            trace,
        ),
        elf::RelocationList::Crel(crel_iter) => apply_relocations::<C, A, elf::ElfCrel<C>, _>(
            object,
            out,
            section_index,
            crel_iter.map(|r| r.map(elf::ElfCrel::new)),
            layout,
            table_writer,
            trace,
        ),
    };
    result.with_context(|| {
        format!(
            "Failed to apply relocations in section `{}` of {}",
            object.object.section_display_name(section_index),
            object.input
        )
    })?;
    Ok(())
}

pub(crate) fn write_section_reversed<'data, C: ElfClass, A: Arch<Platform = elf::Elf<C>>>(
    object: &ObjectLayout<'data, elf::Elf<C>>,
    layout: &ElfLayout<'data, C>,
    section_index: object::SectionIndex,
    table_writer: &mut TableWriter<'_, '_, C>,
    trace: &TraceOutput,
    out: &mut [u8],
) -> Result {
    let word_size = C::ADDRESS_SIZE as usize;

    if !out.is_empty() {
        ensure!(
            out.len().is_multiple_of(word_size),
            "Section size is not a multiple of word size"
        );

        let pointers: &mut [elf::Word<C>] = <[elf::Word<C>]>::mut_from_bytes(out).unwrap();
        pointers.reverse();
    }

    // For reversed sections, we need to adjust relocation offsets.
    // The offset transformation is: new_offset = section_size - old_offset - word_size
    let section_size = out.len() as u64;

    let relocations = object.relocations(section_index)?;

    let result = match relocations {
        elf::RelocationList::Rela(rela) => apply_relocations::<C, A, elf::ElfCrel<C>, _>(
            object,
            out,
            section_index,
            rela.iter().map(|r| {
                let mut crel = Crel::from_rela(r, LittleEndian, false);
                crel.r_offset = section_size.saturating_sub(crel.r_offset + word_size as u64);
                Ok(elf::ElfCrel::new(crel))
            }),
            layout,
            table_writer,
            trace,
        ),
        elf::RelocationList::Crel(crel_iter) => apply_relocations::<C, A, elf::ElfCrel<C>, _>(
            object,
            out,
            section_index,
            crel_iter.map(|r| {
                r.map(|mut crel| {
                    crel.r_offset = section_size.saturating_sub(crel.r_offset + word_size as u64);
                    elf::ElfCrel::new(crel)
                })
            }),
            layout,
            table_writer,
            trace,
        ),
    };

    result.with_context(|| {
        format!(
            "Failed to apply relocations in section `{}` of {}",
            object.object.section_display_name(section_index),
            object.input
        )
    })?;

    Ok(())
}

pub(crate) fn write_debug_section<'data, C: ElfClass, A: Arch<Platform = elf::Elf<C>>>(
    object: &ObjectLayout<'data, elf::Elf<C>>,
    layout: &ElfLayout<'data, C>,
    section: Section,
    section_index: object::SectionIndex,
    buffers: &mut OutputSectionPartMap<&mut [u8]>,
) -> Result {
    let part_id = object.section_part_id(section_index, &layout.symbol_db.section_part_ids);
    let section_id = part_id.output_section_id::<elf::Elf<C>>();

    if layout.compressed_debug_sections.get(section_id).is_some() {
        // Compressed debug sections are written by the epilogue.
        return Ok(());
    }

    let skip_payload = layout.skip_incremental_payload(object.file_id);
    let out = write_section_raw::<C, A>(
        object,
        layout,
        section,
        section_index,
        buffers,
        !skip_payload,
    )?;
    if skip_payload {
        return Ok(());
    }
    let relocations = object.relocations(section_index)?;
    let result = match relocations {
        elf::RelocationList::Rela(rela) => apply_debug_relocations::<C, A, elf::ElfRela<C>, _>(
            object,
            out,
            section_index,
            rela.iter().map(|rela| Ok(elf::ElfRela::new(*rela))),
            layout,
        ),
        elf::RelocationList::Crel(crel_iter) => {
            apply_debug_relocations::<C, A, elf::ElfCrel<C>, _>(
                object,
                out,
                section_index,
                crel_iter.map(|r| r.map(elf::ElfCrel::new)),
                layout,
            )
        }
    };
    result.with_context(|| {
        format!(
            "Failed to apply relocations in section `{}` of {}",
            object.object.section_display_name(section_index),
            object.input
        )
    })?;
    Ok(())
}

pub(crate) fn input_section_buffer_split<C: ElfClass>(
    remaining: usize,
    sec: Section,
    part_id: PartId,
    layout: &ElfLayout<C>,
    file_id: crate::input_data::FileId,
) -> (usize, usize) {
    if layout
        .output_sections
        .uses_input_order(part_id.output_section_id::<elf::Elf<C>>())
    {
        let part_layout = layout.section_part_layouts.get(part_id);
        let group_idx = file_id.group();
        let mut group_start = part_layout.mem_offset;
        for group in layout.group_layouts.iter().take(group_idx) {
            group_start += group.mem_sizes.get(part_id);
        }
        let group_file_size = layout.group_layouts[group_idx].file_sizes.get(part_id);
        let written_in_group = group_file_size.saturating_sub(remaining);
        let current_vma = group_start + written_in_group as u64;
        let aligned_vma = sec.alignment.align_up(current_vma);
        let leading_pad = (aligned_vma - current_vma) as usize;
        (leading_pad, leading_pad + sec.size as usize)
    } else {
        (0, sec.capacity(part_id, &layout.output_sections) as usize)
    }
}

pub(crate) fn write_section_raw<'out, 'data, C: ElfClass, A: Arch<Platform = elf::Elf<C>>>(
    object: &ObjectLayout<'data, elf::Elf<C>>,
    layout: &ElfLayout<C>,
    sec: Section,
    section_index: object::SectionIndex,
    buffers: &'out mut OutputSectionPartMap<&mut [u8]>,
    copy: bool,
) -> Result<&'out mut [u8]> {
    let part_id = object.section_part_id(section_index, &layout.symbol_db.section_part_ids);
    if layout
        .output_sections
        .has_data_in_file(part_id.output_section_id::<elf::Elf<C>>())
    {
        let section_buffer = buffers.get_mut(part_id);
        let (leading_pad, allocation_size) =
            input_section_buffer_split(section_buffer.len(), sec, part_id, layout, object.file_id);
        if section_buffer.len() < allocation_size {
            bail!(
                "Insufficient space allocated to section `{}`. Tried to take {} bytes, but only {} remain",
                object.object.section_display_name(section_index),
                allocation_size,
                section_buffer.len()
            );
        }
        let out = section_buffer.split_off_mut(..allocation_size).unwrap();
        let (leading, out) = out.split_at_mut(leading_pad);
        let object_section = object.object.section(section_index)?;
        let section_size = object.object.section_size(object_section)? as usize;
        if !copy {
            let n = section_size.min(out.len());
            return Ok(&mut out[..n]);
        }
        let relax_deltas = object.section_relax_deltas.get(section_index.0);

        let section_info = layout
            .output_sections
            .output_info(part_id.output_section_id::<elf::Elf<C>>());
        fill_section_padding::<C, A>(leading, section_info);
        match relax_deltas {
            None => {
                let section_size = object.object.section_size(object_section)?;
                let (out, padding) = out.split_at_mut(section_size as usize);
                object.object.copy_section_data(object_section, out)?;
                fill_section_padding::<C, A>(padding, section_info);
                Ok(out)
            }
            Some(deltas) => {
                let input_data = object.object.raw_section_data(object_section)?;
                let effective_size = sec.size as usize;

                let mut input_pos: usize = 0;
                let mut output_pos: usize = 0;

                for delta in deltas.deltas() {
                    let skip_start = delta.input_offset as usize;
                    // Copy everything from input_pos up to the deletion point.
                    let copy_len = skip_start - input_pos;
                    if copy_len > 0 {
                        out[output_pos..output_pos + copy_len]
                            .copy_from_slice(&input_data[input_pos..skip_start]);
                        output_pos += copy_len;
                    }
                    // Skip over the deleted bytes in the input.
                    input_pos = skip_start + delta.bytes_deleted as usize;
                }

                // Copy the remainder after the last deletion.
                let remaining = input_data.len() - input_pos;
                if remaining > 0 {
                    out[output_pos..output_pos + remaining]
                        .copy_from_slice(&input_data[input_pos..]);
                    output_pos += remaining;
                }
                fill_section_padding::<C, A>(&mut out[output_pos..], section_info);

                Ok(&mut out[..effective_size])
            }
        }
    } else {
        Ok(&mut [])
    }
}

pub(crate) fn write_prelude<'data, C: ElfClass, A: Arch<Platform = elf::Elf<C>>>(
    prelude: &PreludeLayout<elf::Elf<C>>,
    buffers: &mut OutputSectionPartMap<&mut [u8]>,
    table_writer: &mut TableWriter<'_, '_, C>,
    layout: &ElfLayout<'data, C>,
) -> Result {
    let gdb_buf = buffers.take(part_id::GDB_INDEX);
    let (a, b) = rayon::join(
        || {
            if let Some(scan) = &layout.gdb_index_data {
                timing_phase!("Write GDB index");
                crate::gdb_index::write_gdb_index(gdb_buf, layout, scan)
            } else {
                Ok(())
            }
        },
        || write_prelude_except_gdb_index::<C, A>(prelude, buffers, table_writer, layout),
    );
    a.and(b)
}

pub(crate) fn write_prelude_except_gdb_index<
    'data,
    C: ElfClass,
    A: Arch<Platform = elf::Elf<C>>,
>(
    prelude: &PreludeLayout<elf::Elf<C>>,
    buffers: &mut OutputSectionPartMap<&mut [u8]>,
    table_writer: &mut TableWriter<'_, '_, C>,
    layout: &ElfLayout<'data, C>,
) -> Result {
    verbose_timing_phase!("Write prelude");

    let header: &mut elf::FileHeader<C> =
        from_bytes_mut(buffers.get_mut(crate::part_id::FILE_HEADER))
            .map_err(|_| error!("Invalid file header allocation"))?
            .0;
    populate_file_header::<C, A>(layout, &prelude.header_info, header)?;

    let mut program_headers =
        ProgramHeaderWriter::<C>::new(buffers.get_mut(part_id::PROGRAM_HEADERS));
    write_program_headers(&mut program_headers, layout)?;

    write_section_headers(buffers.get_mut(part_id::SECTION_HEADERS), layout)?;

    write_section_header_strings(
        buffers.get_mut(part_id::SHSTRTAB),
        &layout.output_sections,
        &layout.output_order,
    );

    write_plt_got_entries::<C, A>(prelude, layout, table_writer)?;

    if !layout.args().should_strip_all() {
        write_symbol_table_entries(prelude, &mut table_writer.debug_symbol_writer, layout)?;
    }

    if layout.args().should_write_eh_frame_hdr
        && layout
            .section_layouts
            .get(output_section_id::EH_FRAME_HDR)
            .mem_size
            > 0
    {
        write_eh_frame_hdr(table_writer, layout)?;
    }

    write_merged_strings(prelude, buffers, layout);

    write_interp(prelude, buffers);

    // If we're emitting symbol versions, we should have only one - symbol 0 - the undefined
    // symbol. It needs to be set as local.
    if layout.gnu_version_enabled() {
        table_writer
            .version_writer
            .set_next_symbol_version(object::elf::VER_NDX_GLOBAL)?;
    }

    // Define the null dynamic symbol.
    if layout.symbol_db.output_kind.needs_dynsym() {
        table_writer.dynsym_writer.undefined_symbol(false, &[])?;
    }

    Ok(())
}

pub(crate) fn write_interp<C: ElfClass>(
    prelude: &PreludeLayout<elf::Elf<C>>,
    buffers: &mut OutputSectionPartMap<&mut [u8]>,
) {
    if let Some(dynamic_linker) = prelude.dynamic_linker.as_ref() {
        buffers
            .get_mut(part_id::INTERP)
            .copy_from_slice(dynamic_linker.as_bytes_with_nul());
    }
}

pub(crate) fn write_merged_strings<C: ElfClass>(
    prelude: &PreludeLayout<elf::Elf<C>>,
    buffers: &mut OutputSectionPartMap<&mut [u8]>,
    layout: &ElfLayout<C>,
) {
    layout.merged_strings.for_each(|section_id, merged| {
        if merged.len() > 0 {
            let buffer = buffers
                .get_mut(section_id.part_id_with_alignment::<elf::Elf<C>>(crate::alignment::MIN));

            write_merged_strings_to_buffer(merged, buffer);
        }
    });

    if layout.args().should_write_linker_identity {
        // Write linker identity into .comment section.
        let comment_buffer = buffers.get_mut(
            output_section_id::COMMENT.part_id_with_alignment::<elf::Elf<C>>(alignment::MIN),
        );
        comment_buffer
            .split_off_mut(..prelude.identity.len())
            .unwrap()
            .copy_from_slice(prelude.identity.as_bytes());
    }
}

pub(crate) fn write_merged_strings_to_buffer(
    merged: &crate::string_merging::MergedStringsSection,
    buffer: &mut &mut [u8],
) {
    let leading = merged.leading_pad();
    if leading > 0 {
        buffer.split_off_mut(..leading).unwrap().fill(0);
    }
    merged
        .buckets
        .iter()
        .map(|b| (b, buffer.split_off_mut(..b.len()).unwrap()))
        .par_bridge()
        .for_each(|(bucket, buffer)| {
            bucket.write_to(buffer);
        });
}

pub(crate) fn write_plt_got_entries<'data, C: ElfClass, A: Arch<Platform = elf::Elf<C>>>(
    prelude: &PreludeLayout<elf::Elf<C>>,
    layout: &ElfLayout<'data, C>,
    table_writer: &mut TableWriter<'_, '_, C>,
) -> Result {
    for _ in 0..prelude.format_specific.got_plt_header_entries {
        *table_writer.take_next_got_entry()? = elf::Word::<C>::from_u64(0)?;
    }

    // Write a pair of GOT entries for use by any TLSLD or TLSGD relocations.
    if let Some(got_address) = prelude.format_specific.tlsld_got_entry {
        let mut raw_value = 0;

        if layout.symbol_db.output_kind.is_executable() {
            table_writer.process_resolution::<A>(
                Some(layout),
                layout.args(),
                &Resolution {
                    raw_value: crate::elf::CURRENT_EXE_TLS_MOD,
                    dynamic_symbol_index: None,
                    format_specific: crate::elf::ResolutionExt {
                        got_address: Some(got_address),
                        plt_address: None,
                    },
                    flags: ValueFlags::GOT | ValueFlags::ABSOLUTE,
                },
            )?;

            // For executables, DTPOFF values are negative values relative to the thread pointer,
            // which is at the end of the TLS segment.
            raw_value = A::tp_offset_start(layout) - layout.tls_start_address();
        } else {
            *table_writer.take_next_got_entry()? = elf::Word::<C>::from_u64(0)?;
            table_writer.write_dtpmod_relocation::<A>(got_address.get(), 0)?;
        }

        table_writer.process_resolution::<A>(
            Some(layout),
            layout.args(),
            &Resolution {
                raw_value,
                dynamic_symbol_index: None,
                format_specific: crate::elf::ResolutionExt {
                    got_address: Some(got_address.saturating_add(C::GOT_ENTRY_SIZE)),
                    plt_address: None,
                },
                flags: ValueFlags::GOT | ValueFlags::ABSOLUTE,
            },
        )?;
    }

    write_internal_symbols_plt_got_entries::<C, A>(
        &prelude.internal_symbols,
        table_writer,
        layout,
    )?;
    Ok(())
}

pub(crate) fn write_linker_script_state<'data, C: ElfClass, A: Arch<Platform = elf::Elf<C>>>(
    script: &LinkerScriptLayoutState<elf::Elf<C>>,
    table_writer: &mut TableWriter<'_, '_, C>,
    layout: &ElfLayout<'data, C>,
) -> Result {
    verbose_timing_phase!("Write linker script state");

    write_internal_symbols(
        &script.internal_symbols,
        layout,
        &mut table_writer.debug_symbol_writer,
    )?;

    write_internal_symbols_plt_got_entries::<C, A>(&script.internal_symbols, table_writer, layout)?;

    Ok(())
}

pub(crate) fn write_synthetic_symbols<'data, C: ElfClass, A: Arch<Platform = elf::Elf<C>>>(
    syn: &SyntheticSymbolsLayout<elf::Elf<C>>,
    table_writer: &mut TableWriter<'_, '_, C>,
    layout: &ElfLayout<'data, C>,
) -> Result {
    verbose_timing_phase!("Write synthetic symbols");

    write_internal_symbols_plt_got_entries::<C, A>(&syn.internal_symbols, table_writer, layout)?;

    if !layout.args().should_strip_all() {
        write_internal_symbols(
            &syn.internal_symbols,
            layout,
            &mut table_writer.debug_symbol_writer,
        )?;
    }

    Ok(())
}

pub(crate) fn write_epilogue<C: ElfClass, A: Arch<Platform = elf::Elf<C>>>(
    epilogue: &EpilogueLayout<elf::Elf<C>>,
    buffers: &mut OutputSectionPartMap<&mut [u8]>,
    table_writer: &mut TableWriter<'_, '_, C>,
    layout: &ElfLayout<C>,
    trace: &TraceOutput,
) -> Result {
    verbose_timing_phase!("Write epilogue");

    let mut epilogue_offsets = EpilogueOffsets::default();

    if layout.symbol_db.output_kind.needs_dynamic() {
        write_epilogue_dynamic_entries(layout, table_writer, &mut epilogue_offsets)?;
    }

    let got_relr_n = layout.got_relr_n;
    if got_relr_n > 0 {
        let got_relr_base = layout
            .section_part_layouts
            .get(part_id::GOT_RELR)
            .mem_offset;
        table_writer.write_got_relr_bitmap(got_relr_n, got_relr_base)?;
    }
    write_sysv_hash_table(layout, epilogue, buffers)?;
    write_gnu_hash_tables(layout, epilogue, buffers)?;

    write_dynamic_symbol_definitions(table_writer, layout)?;

    if !layout.format_specific.gnu_property_notes.is_empty() {
        write_gnu_property_notes(layout, buffers)?;
    }
    if layout.format_specific.riscv_attributes.section_size != 0 {
        write_riscv_attributes(layout, buffers)?;
    }

    if let Some(verdefs) = &epilogue.format_specific.verdefs {
        write_verdef(
            verdefs,
            table_writer,
            layout.args().soname.as_ref().map(|s| s.as_bytes()),
            &epilogue_offsets,
        )?;
    }
    if epilogue.format_specific.needs_eh_frame_terminator {
        table_writer.write_eh_frame_terminator();
    }

    // The actual build-id will be filled in later once all writing has completed. It's important
    // that we fill it with zeros now however, since if we're overwriting an existing file, there
    // might be other data there and we don't zero it, then the build ID will be hashing that data.
    if let Some(dest_part) = layout.output_sections.gnu_build_id_dest_part() {
        let build_id_buffer = buffers.get_mut(dest_part);
        let note_size = epilogue
            .format_specific
            .gnu_build_id_note_section_size::<C>()
            .unwrap_or(0) as usize;
        let len = build_id_buffer.len();
        if note_size > 0 && len >= note_size {
            build_id_buffer[len - note_size..].fill(0);
        } else {
            build_id_buffer.fill(0);
        }
    }

    for sorted_section in &layout.script_sorted_sections {
        let crate::layout::FileLayout::Object(object) = layout.file_layout(sorted_section.file_id)
        else {
            unreachable!();
        };

        if let SectionSlot::Sorted(sec) = &object.sections[sorted_section.section_index.0] {
            write_object_section::<C, A>(
                object,
                layout,
                sec.section,
                sorted_section.section_index,
                buffers,
                table_writer,
                trace,
            )?;
        }
    }

    write_compressed_debug_sections(layout, buffers);
    Ok(())
}

pub(crate) fn write_compressed_debug_sections<C: ElfClass>(
    layout: &ElfLayout<C>,
    buffers: &mut OutputSectionPartMap<&mut [u8]>,
) {
    verbose_timing_phase!("Write compressed debug sections");

    let mut work = Vec::new();

    for (section_id, _section_info) in layout.output_sections.ids_with_info() {
        if let Some(compressed_section) = layout.compressed_debug_sections.get(section_id) {
            let part_id = section_id.part_id_with_alignment::<elf::Elf<C>>(alignment::MIN);
            let buffer = buffers.get_mut(part_id);
            for chunk in &compressed_section.compressed_chunks {
                let out = buffer.split_off_mut(..chunk.len()).unwrap();
                work.push((out, chunk));
            }
        }
    }

    work.par_iter_mut().for_each(|(out, chunk)| {
        verbose_timing_phase!("Copy compressed chunk");
        out.copy_from_slice(chunk);
    });
}

pub(crate) fn write_gnu_property_notes<C: ElfClass>(
    layout: &ElfLayout<C>,
    buffers: &mut OutputSectionPartMap<&mut [u8]>,
) -> Result {
    let (note_header, mut rest) =
        from_bytes_mut::<elf::NoteHeader<C>>(buffers.get_mut(part_id::NOTE_GNU_PROPERTY))
            .map_err(|_| error!("Insufficient .note.gnu.property allocation"))?;
    note_header.set_name_size(GNU_NOTE_NAME.len() as u32);
    note_header.set_descriptor_size(
        (layout.format_specific.gnu_property_notes.len() as u64 * C::GNU_PROPERTY_ENTRY_SIZE)
            .try_into()
            .context(".note.gnu.property descriptor overflowed 32 bits")?,
    );
    note_header.set_type(NT_GNU_PROPERTY_TYPE_0);

    let name_out = rest.split_off_mut(..GNU_NOTE_NAME.len()).unwrap();
    name_out.copy_from_slice(GNU_NOTE_NAME);

    for note in &layout.format_specific.gnu_property_notes {
        let entry_size = C::GNU_PROPERTY_ENTRY_SIZE as usize;
        let entry = rest.split_off_mut(..entry_size).unwrap();
        let (property_bytes, padding) = entry.split_at_mut(size_of::<NoteProperty>());
        let property = NoteProperty::mut_from_bytes(property_bytes).unwrap();
        property.pr_type = note.ptype.0;
        property.pr_datasz = size_of_val(&property.pr_data) as u32;
        property.pr_data = note.data;
        padding.fill(0);
    }

    Ok(())
}

pub(crate) fn write_riscv_attributes<C: ElfClass>(
    layout: &ElfLayout<C>,
    buffers: &mut OutputSectionPartMap<&mut [u8]>,
) -> Result {
    let mut writer = Cursor::new(&mut **buffers.get_mut(part_id::RISCV_ATTRIBUTES));
    writer.write_all(b"A")?;

    let riscv_attributes_length = layout.format_specific.riscv_attributes.section_size as u32;

    writer.write_all((riscv_attributes_length - 1).to_le_bytes().as_slice())?;
    writer.write_all(RISCV_ATTRIBUTE_VENDOR_NAME.as_bytes())?;
    writer.write_all(b"\0")?;
    leb128::write::unsigned(&mut writer, TAG_RISCV_WHOLE_FILE)?;
    writer.write_all(
        (riscv_attributes_length - 1 - 4 - RISCV_ATTRIBUTE_VENDOR_NAME.len() as u32 - 1)
            .to_le_bytes()
            .as_slice(),
    )?;
    for tag in &layout.format_specific.riscv_attributes.attributes {
        match tag {
            &RiscVAttribute::StackAlign(align) => {
                leb128::write::unsigned(&mut writer, TAG_RISCV_STACK_ALIGN)?;
                leb128::write::unsigned(&mut writer, align)?;
            }
            RiscVAttribute::Arch(arch) => {
                leb128::write::unsigned(&mut writer, TAG_RISCV_ARCH)?;
                writer.write_all(arch.to_attribute_string().as_bytes())?;
                writer.write_all(b"\0")?;
            }
            &RiscVAttribute::UnalignedAccess(access) => {
                leb128::write::unsigned(&mut writer, TAG_RISCV_UNALIGNED_ACCESS)?;
                leb128::write::unsigned(&mut writer, u64::from(access))?;
            }
            &RiscVAttribute::PrivilegedSpecMajor(version) => {
                leb128::write::unsigned(&mut writer, TAG_RISCV_PRIV_SPEC)?;
                leb128::write::unsigned(&mut writer, version)?;
            }
            &RiscVAttribute::PrivilegedSpecMinor(version) => {
                leb128::write::unsigned(&mut writer, TAG_RISCV_PRIV_SPEC_MINOR)?;
                leb128::write::unsigned(&mut writer, version)?;
            }
            &RiscVAttribute::PrivilegedSpecRevision(version) => {
                leb128::write::unsigned(&mut writer, TAG_RISCV_PRIV_SPEC_REVISION)?;
                leb128::write::unsigned(&mut writer, version)?;
            }
        }
    }

    Ok(())
}

pub(crate) fn write_eh_frame_hdr<C: ElfClass>(
    table_writer: &mut TableWriter<'_, '_, C>,
    layout: &ElfLayout<C>,
) -> Result {
    let header = table_writer.take_eh_frame_hdr();
    header.version = 1;

    header.table_encoding = (gimli::DW_EH_PE_sdata4 | gimli::DW_EH_PE_datarel).0;
    header.frame_pointer_encoding = (gimli::DW_EH_PE_sdata4 | gimli::DW_EH_PE_pcrel).0;
    header.frame_pointer = eh_frame_ptr(layout)?;

    header.count_encoding = (gimli::DW_EH_PE_udata4 | gimli::DW_EH_PE_absptr).0;
    header.entry_count = eh_frame_hdr_entry_count(layout)?;

    Ok(())
}

pub(crate) fn eh_frame_hdr_entry_count<C: ElfClass>(layout: &ElfLayout<C>) -> Result<u32> {
    let hdr_sec = layout.section_layouts.get(output_section_id::EH_FRAME_HDR);
    u32::try_from(
        (hdr_sec.mem_size - size_of::<elf::EhFrameHdr>() as u64)
            / size_of::<elf::EhFrameHdrEntry>() as u64,
    )
    .context(".eh_frame_hdr entries overflowed 32 bits")
}

/// Returns the address of .eh_frame relative to the location in .eh_frame_hdr where the frame
/// pointer is stored.
pub(crate) fn eh_frame_ptr<C: ElfClass>(layout: &ElfLayout<C>) -> Result<i32> {
    let eh_frame_address = layout.mem_address_of_built_in(output_section_id::EH_FRAME);
    let eh_frame_hdr_address = layout.mem_address_of_built_in(output_section_id::EH_FRAME_HDR);
    i32::try_from(
        eh_frame_address - (eh_frame_hdr_address + elf::FRAME_POINTER_FIELD_OFFSET as u64),
    )
    .context(".eh_frame more than 2GB away from .eh_frame_hdr")
}

pub(crate) fn write_internal_symbols_plt_got_entries<
    'data,
    C: ElfClass,
    A: Arch<Platform = elf::Elf<C>>,
>(
    internal_symbols: &InternalSymbols<elf::Elf<C>>,
    table_writer: &mut TableWriter<'_, '_, C>,
    layout: &ElfLayout<'data, C>,
) -> Result {
    for i in 0..internal_symbols.symbol_definitions.len() {
        let symbol_id = internal_symbols.start_symbol_id.add_usize(i);
        if !layout.symbol_db.is_canonical(symbol_id) {
            continue;
        }
        if let Some(res) = layout.local_symbol_resolution(symbol_id) {
            table_writer
                .process_resolution::<A>(Some(layout), layout.args(), res)
                .with_context(|| {
                    format!("Failed to process `{}`", layout.symbol_debug(symbol_id))
                })?;
        }

        if layout.symbol_db.args.got_plt_syms {
            write_got_plt_syms(layout, &mut table_writer.debug_symbol_writer, symbol_id)?;
        }
    }
    Ok(())
}

pub(crate) fn verify_resolution_allocation<C: ElfClass, A: Arch<Platform = elf::Elf<C>>>(
    output_sections: &OutputSections<elf::Elf<C>>,
    output_order: &OutputOrder,
    output_kind: OutputKind,
    mem_sizes: &OutputSectionPartMap<u64>,
    resolution: &Resolution<elf::Elf<C>>,
    args: &ElfArgs,
) -> Result {
    // Allocate however much space was requested.

    let mut total_bytes_allocated = 0;
    mem_sizes.output_order_map(
        output_order,
        output_sections,
        |_part_id, alignment, &size| {
            total_bytes_allocated = alignment.align_up(total_bytes_allocated) + size;
        },
    );
    total_bytes_allocated = crate::alignment::USIZE.align_up(total_bytes_allocated);
    let mut all_mem = vec![0_u64; total_bytes_allocated as usize / size_of::<u64>()];
    let mut all_mem: &mut [u8] = transmute_mut!(all_mem.as_mut_slice());
    let mut offset = 0;
    let mut buffers = mem_sizes.output_order_map(
        output_order,
        output_sections,
        |_part_id, alignment, &size| {
            let aligned_offset = alignment.align_up(offset);
            all_mem
                .split_off_mut(..(aligned_offset - offset) as usize)
                .unwrap();
            offset = aligned_offset + size;
            all_mem.split_off_mut(..size as usize).unwrap()
        },
    );

    let dynsym_writer = SymbolTableWriter::<C>::new_dynamic(0, &mut buffers, output_sections);
    let debug_symbol_writer = SymbolTableWriter::<C>::new(0, &mut buffers, output_sections);
    let mut table_writer = TableWriter::<C>::new(
        output_kind,
        0..100,
        &mut buffers,
        dynsym_writer,
        debug_symbol_writer,
        0,
        args.is_relr_enabled(),
    );
    table_writer.process_resolution::<A>(None, args, resolution)?;
    table_writer.validate_empty(mem_sizes)
}

/// Returns whether to reverse the contents of a section. This is true for .ctors/.dtors sections.
pub(crate) fn should_reverse_contents<C: ElfClass>(
    section_index: object::SectionIndex,
    part_id: PartId,
    file: &elf::File<'_, C>,
    output_sections: &OutputSections<elf::Elf<C>>,
) -> bool {
    // Getting the section name is expensive, so we only do it when the output section is
    // .init_array / .fini_array.
    let section_id =
        output_sections.primary_output_section(part_id.output_section_id::<elf::Elf<C>>());
    if section_id != output_section_id::INIT_ARRAY && section_id != output_section_id::FINI_ARRAY {
        return false;
    }

    file.section_name(section_index).is_ok_and(|section_name| {
        // .ctors and .dtors sections need their contents reversed when merged into
        // .init_array/.fini_array
        section_name.starts_with(secnames::CTORS_SECTION_NAME)
            || section_name.starts_with(secnames::DTORS_SECTION_NAME)
    })
}

pub(crate) fn link_ids<C: ElfClass>(section_id: OutputSectionId) -> &'static [OutputSectionId] {
    elf::Elf::<C>::built_in_section_details()
        .get(section_id.as_usize())
        .map(|def| def.link)
        .unwrap_or_default()
}

pub(crate) fn fill_section_padding<C: ElfClass, A: Arch<Platform = elf::Elf<C>>>(
    padding: &mut [u8],
    section_info: &SectionOutputInfo<elf::Elf<C>>,
) {
    if let Some(pattern) = section_info.fill {
        let chunks = padding.chunks_mut(4);
        for chunk in chunks {
            let len = chunk.len();
            chunk.copy_from_slice(&pattern[..len]);
        }
    } else {
        A::fill_section_padding(padding, section_info.section_attributes.flags);
    }
}

pub(crate) fn write_script_output_data<C: ElfClass>(
    layout: &ElfLayout<C>,
    section_buffers: &mut OutputSectionMap<&mut [u8]>,
) -> Result {
    if layout.output_sections.script_output_data.is_empty() {
        return Ok(());
    }

    let sizeof_headers = crate::elf::program_headers_size::<C>(&layout.prelude().header_info)
        + u64::from(C::FILE_HEADER_SIZE);
    let empty_regions = hashbrown::HashMap::new();

    for data in &layout.output_sections.script_output_data {
        let end = layout
            .resolved_location_counters
            .get(data.location_counter_index)
            .and_then(|lc| lc.section_offset)
            .unwrap_or(u64::from(data.width)) as usize;
        let offset = end.saturating_sub(usize::from(data.width));
        let value = crate::expression_eval::evaluate_expression(
            &data.value,
            &SymbolLoc::None,
            None,
            &layout.section_layouts,
            &layout.output_sections,
            &empty_regions,
            &layout.symbol_db,
            sizeof_headers,
            &layout.resolved_location_counters,
            &|name| {
                let Some(symbol_id) = layout
                    .symbol_db
                    .get_unversioned(&crate::symbol::UnversionedSymbolName::prehashed(name))
                else {
                    crate::bail!(
                        "undefined symbol `{}` in linker script BYTE/SHORT/LONG/QUAD",
                        String::from_utf8_lossy(name)
                    );
                };
                let canonical = layout.symbol_db.definition(symbol_id);
                layout
                    .symbol_resolutions
                    .get(canonical)
                    .map(|r| crate::expression_eval::ResolvedSymbolValue::Absolute(r.raw_value))
                    .with_context(|| {
                        format!(
                            "unresolved symbol `{}` in linker script BYTE/SHORT/LONG/QUAD",
                            String::from_utf8_lossy(name)
                        )
                    })
            },
        )?;
        let buf = section_buffers.get_mut(data.section_id);
        let end = offset + usize::from(data.width);
        if end > buf.len() {
            bail!(
                "BYTE/SHORT/LONG/QUAD at offset {offset} does not fit in section {}",
                layout.output_sections.display_name(data.section_id)
            );
        }
        let bytes = value.to_le_bytes();
        buf[offset..end].copy_from_slice(&bytes[..usize::from(data.width)]);
    }
    Ok(())
}
