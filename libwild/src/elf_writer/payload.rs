use super::*;
use crate::OutputFileData;
use crate::elf;
use crate::elf::ElfClass;
use crate::elf::output_section_id;
use crate::error::Result;
use crate::file_writer::SizedOutput;
use crate::file_writer::split_buffers_by_alignment;
use crate::file_writer::split_output_by_group;
use crate::file_writer::split_output_into_sections;
use crate::layout::FileLayout;
use crate::layout::Layout;
use crate::output_section_id::OrderEvent;
use crate::output_section_map::OutputSectionMap;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::output_trace::TraceOutput;
use crate::platform::Arch;
use crate::sframe;
use crate::timing_phase;
use crate::verbose_timing_phase;
use rayon::iter::IndexedParallelIterator;
use std::sync::atomic::Ordering::Relaxed;
use zerocopy::FromBytes;

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
