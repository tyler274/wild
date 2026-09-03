use super::*;
use crate::OutputKind;
use crate::alignment;
use crate::args::elf::ElfArgs;
use crate::bail;
use crate::elf;
use crate::elf::ElfClass;
use crate::elf::GNU_NOTE_NAME;
use crate::elf::NoteProperty;
use crate::elf::RiscVAttribute;
use crate::elf::output_section_id;
use crate::elf::part_id;
use crate::error;
use crate::error::Result;
use crate::layout::EpilogueLayout;
use crate::layout::InternalSymbols;
use crate::layout::LinkerScriptLayoutState;
use crate::layout::ObjectLayout;
use crate::layout::PreludeLayout;
use crate::layout::Resolution;
use crate::layout::Section;
use crate::layout::SyntheticSymbolsLayout;
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
use crate::platform::ObjectFile;
use crate::platform::Platform;
use crate::resolution::SectionSlot;
use crate::sharding::ShardKey;
use crate::timing_phase;
use crate::value_flags::ValueFlags;
use crate::verbose_timing_phase;
use linker_utils::elf::RISCV_ATTRIBUTE_VENDOR_NAME;
use linker_utils::elf::riscvattr::TAG_RISCV_ARCH;
use linker_utils::elf::riscvattr::TAG_RISCV_PRIV_SPEC;
use linker_utils::elf::riscvattr::TAG_RISCV_PRIV_SPEC_MINOR;
use linker_utils::elf::riscvattr::TAG_RISCV_PRIV_SPEC_REVISION;
use linker_utils::elf::riscvattr::TAG_RISCV_STACK_ALIGN;
use linker_utils::elf::riscvattr::TAG_RISCV_UNALIGNED_ACCESS;
use linker_utils::elf::riscvattr::TAG_RISCV_WHOLE_FILE;
use linker_utils::elf::secnames;
use object::elf::NT_GNU_PROPERTY_TYPE_0;
use object::from_bytes_mut;
use std::io::Cursor;
use std::io::Write;
use zerocopy::FromBytes;
use zerocopy::transmute_mut;

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
            &OutputSectionPartMap::default(),
            &mut |name| {
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
                    .map(|r| crate::expression_eval::SymbolValue::Absolute(r.raw_value))
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
