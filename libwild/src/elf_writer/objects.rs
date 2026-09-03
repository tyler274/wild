use super::*;
use crate::elf;
use crate::elf::ElfClass;
use crate::ensure;
use crate::error::Result;
use crate::layout::ObjectLayout;
use crate::layout::Section;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::output_trace::TraceOutput;
use crate::part_id::PartId;
use crate::platform::Arch;
use crate::platform::ObjectFile;
use crate::resolution::SectionSlot;
use crate::value_flags::ValueFlags;
use crate::verbose_timing_phase;
use object::LittleEndian;
use object::read::elf::Crel;
use std::collections::BTreeMap;
use tracing::debug_span;
use zerocopy::FromBytes;

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
