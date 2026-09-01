use super::link_ids;
use super::types::*;
use crate::OutputKind;
use crate::alignment;
use crate::debug_assert_bail;
use crate::elf;
use crate::elf::ElfClass;
use crate::elf::output_section_id;
use crate::elf::part_id;
use crate::ensure;
use crate::error;
use crate::error::Context as _;
use crate::error::Result;
use crate::layout::HeaderInfo;
use crate::malfunction;
use crate::output_section_id::OrderEvent;
use crate::output_section_id::OutputOrder;
use crate::output_section_id::OutputSections;
use crate::output_section_id::SectionName;
use crate::output_section_map::OutputSectionMap;
use crate::platform::Arch;
use crate::platform::Args as _;
use crate::writable_elf::WritableFileHeader as _;
use crate::writable_elf::WritableProgramHeader as _;
use crate::writable_elf::WritableSectionHeader as _;
use linker_utils::elf::pf;
use linker_utils::elf::shf;
use linker_utils::elf::sht;
use linker_utils::utils::slice_from_all_bytes_mut;

pub(crate) fn write_program_headers<C: ElfClass>(
    program_headers_out: &mut ProgramHeaderWriter<'_, C>,
    layout: &ElfLayout<C>,
) -> Result {
    if layout.args().should_output_partial_object() {
        return Ok(());
    }
    for segment_layout in &layout.segment_layouts.segments {
        let segment_sizes = &segment_layout.sizes;
        let segment_id = segment_layout.id;
        let segment_header = program_headers_out.take_header()?;
        let mut alignment = segment_sizes.alignment;

        if layout.program_segments.is_load_segment(segment_id) {
            alignment = alignment.max(layout.args().loadable_segment_alignment());
        } else if layout.program_segments.is_stack_segment(segment_id) {
            alignment = alignment::STACK_ALIGNMENT;
        }

        let segment_details = layout.program_segments.segment_def(segment_id);

        segment_header.set_type(segment_details.segment_type);

        // Support executable stack (Wild defaults to non-executable stack)
        let mut segment_flags = segment_details.segment_flags;
        if layout.program_segments.is_stack_segment(segment_id) && layout.args().execstack {
            segment_flags |= pf::EXECUTABLE;
        }

        segment_header.set_flags(segment_flags);
        segment_header.set_offset(segment_sizes.file_offset as u64)?;
        segment_header.set_virtual_address(segment_sizes.mem_offset)?;
        let p_paddr = layout
            .program_segments
            .at_lma(segment_id)
            .unwrap_or(segment_sizes.lma_offset);
        segment_header.set_physical_address(p_paddr)?;
        segment_header.set_file_size(segment_sizes.file_size as u64)?;
        segment_header.set_memory_size(segment_sizes.mem_size)?;
        segment_header.set_alignment(alignment.value())?;
    }
    Ok(())
}

pub(crate) fn populate_file_header<C: ElfClass, A: Arch<Platform = elf::Elf<C>>>(
    layout: &ElfLayout<C>,
    header_info: &HeaderInfo,
    header: &mut elf::FileHeader<C>,
) -> Result {
    let output_kind = layout.symbol_db.output_kind;
    let mut ty = if output_kind.is_partial_link() {
        object::elf::ET_REL
    } else if output_kind.is_position_independent() {
        object::elf::ET_DYN
    } else {
        object::elf::ET_EXEC
    };

    if malfunction::malfunction_point("elf-incorrect-type") {
        ty = object::elf::ET_CORE;
    }

    let ident = header.ident_mut();
    ident.magic = object::elf::ELFMAG;
    ident.class = elf::FileHeader::<C>::CLASS;
    ident.data = object::elf::ELFDATA2LSB;
    ident.version = object::elf::EV_CURRENT;
    ident.os_abi = object::elf::ELFOSABI_NONE;
    ident.abi_version = 0;
    ident.padding = Default::default();
    header.set_type(ty);
    header.set_machine(A::arch_identifier());
    header.set_version(object::elf::EV_CURRENT.0.into());
    header.set_entry(elf_entry_address(layout)?)?;
    header.set_program_header_offset(if output_kind.is_partial_link() {
        0
    } else {
        u64::from(C::FILE_HEADER_SIZE)
    })?;
    header.set_section_header_offset(
        u64::from(C::FILE_HEADER_SIZE) + crate::elf::program_headers_size::<C>(header_info),
    )?;
    header.set_flags(layout.format_specific.eflags);
    header.set_header_size(C::FILE_HEADER_SIZE);
    header.set_program_header_entry_size(if output_kind.is_partial_link() {
        0
    } else {
        C::PROGRAM_HEADER_SIZE
    });
    header.set_program_header_count(header_info.active_segment_ids.len() as u16);
    header.set_section_header_entry_size(C::SECTION_HEADER_SIZE);
    let shnum = header_info.num_output_sections_with_content;
    header.set_section_header_count(if shnum >= u32::from(object::elf::SHN_LORESERVE) {
        0
    } else {
        shnum as u16
    });
    let shstrndx = layout
        .output_sections
        .output_index_of_section(output_section_id::SHSTRTAB)
        .expect("we always write .shstrtab");
    header.set_section_name_table_index(object::elf::SymbolSection::new(shstrndx));
    Ok(())
}

pub(crate) fn elf_entry_address<C: ElfClass>(layout: &ElfLayout<C>) -> Result<u64> {
    if layout.args().should_output_partial_object() {
        return Ok(0);
    }

    let entry_name = match layout.symbol_db.entry_point() {
        crate::platform::EntryPoint::None => return Ok(0),
        crate::platform::EntryPoint::Address(address) => return Ok(address),
        crate::platform::EntryPoint::Symbol(name) => name,
    };

    if let Some(address) = layout.resolved_entry_symbol_address()? {
        return Ok(address);
    }
    if layout.symbol_db.output_kind == OutputKind::SharedObject {
        return Ok(0);
    }

    let entry_name = String::from_utf8_lossy(entry_name);
    let text_layout = layout.section_layouts.get(output_section_id::TEXT);
    if text_layout.mem_size == 0 {
        layout.symbol_db.warning(format!(
            "cannot find entry symbol `{entry_name}` and .text is empty, not setting entry point"
        ));
        return Ok(0);
    }

    layout.symbol_db.warning(format!(
        "cannot find entry symbol `{entry_name}`, defaulting to 0x{:x}",
        text_layout.mem_offset
    ));
    Ok(text_layout.mem_offset)
}

pub(crate) fn write_section_headers<C: ElfClass>(out: &mut [u8], layout: &ElfLayout<C>) -> Result {
    let entries: &mut [elf::SectionHeader<C>] = slice_from_all_bytes_mut(out);
    let output_sections = &layout.output_sections;
    let mut entries = entries.iter_mut();
    let mut name_offset = 0;
    let info_values = compute_info_values(layout);

    let mut order = layout.output_order.into_iter().peekable();

    while let Some(event) = order.next() {
        let OrderEvent::Section(section_id) = event else {
            continue;
        };

        let output_info = output_sections.output_info(section_id);
        let section_type = output_info.section_attributes.ty;
        let section_layout = layout.merged_section_layouts.get(section_id);

        if output_sections
            .output_index_of_section(section_id)
            .is_none()
        {
            continue;
        }

        let entsize = output_info.section_attributes.entsize.max(
            section_id
                .opt_built_in_details::<elf::Elf<C>>()
                .map_or(0, |details| details.element_size),
        );

        let size;
        let alignment;
        let mut link = link_ids::<C>(section_id)
            .iter()
            .find_map(|link_id| output_sections.output_index_of_section(*link_id))
            .unwrap_or(0);

        if section_type == sht::NULL {
            alignment = 0;
            if entries.len() >= usize::from(object::elf::SHN_LORESERVE) {
                size = entries.len() as u64;
            } else {
                size = 0;
            }

            let shstrndx = layout
                .output_sections
                .output_index_of_section(output_section_id::SHSTRTAB)
                .unwrap();
            if shstrndx >= u32::from(object::elf::SHN_LORESERVE) {
                link = shstrndx;
            } else {
                link = 0;
            }
        } else {
            size = section_layout.mem_size;
            alignment = section_layout.alignment.value();

            while let Some(OrderEvent::Section(next_section_id)) = order.peek()
                && let Some(primary_id) = output_sections.merge_target(*next_section_id)
            {
                debug_assert_bail!(
                    primary_id == section_id,
                    "Section order mismatch {} != {}",
                    output_sections.section_debug(primary_id),
                    output_sections.section_debug(section_id),
                );
                order.next();
            }
        }

        let entry = entries.next().unwrap();
        entry.set_name(name_offset);

        let sh_type = if layout.args().use_android_relr_tags && section_type == sht::RELR {
            object::elf::SHT_ANDROID_RELR
        } else {
            section_type
        };
        entry.set_type(sh_type);

        let mut flags = output_sections.section_flags(section_id);

        if layout.compressed_debug_sections.get(section_id).is_some() {
            flags = flags.with(shf::COMPRESSED);
        } else {
            flags = flags.without(shf::COMPRESSED);
        }

        entry.set_flags(flags)?;

        let name = layout.output_sections.name(section_id).with_context(|| {
            format!(
                "Missing name for section {}",
                layout.output_sections.section_debug(section_id)
            )
        })?;

        let mut info_value = *info_values.get(section_id);

        if layout.args().should_copy_input_relocs()
            && section_type == sht::RELA
            && section_id.is_custom::<elf::Elf<C>>()
        {
            if let Some(symtab_idx) =
                output_sections.output_index_of_section(output_section_id::SYMTAB_LOCAL)
            {
                link = symtab_idx;
            }
            if let Some(target_name) = name
                .bytes()
                .strip_prefix(b".rela")
                .or_else(|| name.bytes().strip_prefix(b".rel"))
                && let Some(target_id) =
                    output_sections.section_id_by_name(SectionName(target_name))
                && let Some(target_idx) = output_sections.output_index_of_section(target_id)
            {
                info_value = target_idx;
            }
        }

        entry.set_address(if layout.symbol_db.args.should_output_partial_object() {
            0
        } else {
            section_layout.mem_offset
        })?;
        entry.set_offset(section_layout.file_offset as u64)?;
        entry.set_size(size)?;
        entry.set_link(link);
        entry.set_info(info_value);
        entry.set_alignment(alignment)?;
        entry.set_entry_size(entsize)?;

        name_offset += name.len() as u32 + 1;
    }
    ensure!(
        entries.next().is_none(),
        "Allocated section entries that weren't used (leftover section headers)"
    );

    Ok(())
}

/// Computes the value of the info field for all the section headers.
pub(crate) fn compute_info_values<C: ElfClass>(layout: &ElfLayout<C>) -> OutputSectionMap<u32> {
    let mut infos = layout.output_sections.new_section_map();

    // .rela.plt contains relocations for .got, so should link to it.
    *infos.get_mut(output_section_id::RELA_PLT) = layout
        .output_sections
        .output_index_of_section(output_section_id::GOT)
        .unwrap_or(0);

    // The only local we ever write to .dynsym is the null symbol, so this is unconditionally 1.
    *infos.get_mut(output_section_id::DYNSYM) = 1;

    *infos.get_mut(output_section_id::GNU_VERSION_D) =
        layout.non_addressable_counts.verdef_count.into();

    *infos.get_mut(output_section_id::GNU_VERSION_R) =
        layout.non_addressable_counts.verneed_count as u32;

    // For SYMTAB, the info field holds the index of the first non-local symbol.
    *infos.get_mut(output_section_id::SYMTAB_LOCAL) = (layout
        .section_part_layouts
        .get(part_id::SYMTAB_LOCAL)
        .file_size
        / C::SYMTAB_ENTRY_SIZE as usize)
        as u32;

    infos
}

pub(crate) fn write_section_header_strings<C: ElfClass>(
    mut out: &mut [u8],
    sections: &OutputSections<elf::Elf<C>>,
    output_order: &OutputOrder,
) {
    for event in output_order {
        if let OrderEvent::Section(id) = event
            && sections.output_index_of_section(id).is_some()
            && let Some(name) = sections.name(id)
        {
            let name_out = out.split_off_mut(..=name.len()).unwrap();
            name_out[..name.len()].copy_from_slice(name.bytes());
            name_out[name.len()] = 0;
        }
    }
}

pub(crate) struct ProgramHeaderWriter<'out, C: ElfClass> {
    pub(crate) headers: &'out mut [elf::ProgramHeader<C>],
}

impl<'out, C: ElfClass> ProgramHeaderWriter<'out, C> {
    pub(crate) fn new(bytes: &'out mut [u8]) -> Self {
        Self {
            headers: slice_from_all_bytes_mut(bytes),
        }
    }

    pub(crate) fn take_header(&mut self) -> Result<&mut elf::ProgramHeader<C>> {
        self.headers
            .split_off_first_mut()
            .ok_or_else(|| error!("Insufficient header slots"))
    }
}
