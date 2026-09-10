use super::link_ids;
use super::types::ElfLayout;
use crate::elf;
use crate::elf::ElfClass;
use crate::elf::output_section_id;
use crate::elf::part_id;
use crate::ensure;
use crate::error;
use crate::error::Context as _;
use crate::error::Result;
use crate::file_writer::insufficient_allocation;
use crate::malfunction;
use crate::verbose_timing_phase;
use crate::writable_elf::WritableFileHeader as _;
use crate::writable_elf::WritableProgramHeader as _;
use crate::writable_elf::WritableSectionHeader as _;
use linker_utils::elf::pf;
use linker_utils::elf::shf;
use linker_utils::elf::sht;
use linker_utils::utils::slice_from_all_bytes_mut;
use object::LittleEndian;
use object::read::elf::SectionHeader as _;
use rayon::iter::IntoParallelIterator as _;
use rayon::iter::ParallelIterator as _;
use wild_layout::FileLayout;
use wild_layout::HeaderInfo;
use wild_layout::ObjectLayout;
use wild_layout::PartialLinkSingleton;
use wild_layout::output_section_id::OutputSections;
use wild_layout::output_section_id::SectionName;
use wild_platform::Arch;
use wild_platform::Args as _;
use wild_platform::EntryPoint;
use wild_platform::ObjectFile;
use wild_platform::OutputKind;
use wild_platform::output_section_map::OutputSectionMap;
use wild_util::alignment;

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
        EntryPoint::None => return Ok(0),
        EntryPoint::Address(address) => return Ok(address),
        EntryPoint::Symbol(name) => name,
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

pub(crate) fn write_section_headers<C: ElfClass>(
    headers_out: &mut [u8],
    shstrtab_out: &mut [u8],
    layout: &ElfLayout<C>,
) -> Result {
    let entries: &mut [elf::SectionHeader<C>] = slice_from_all_bytes_mut(headers_out);
    let output_sections = &layout.output_sections;
    let mut entries = entries.iter_mut();
    let shstrtab = elf::shstrtab_from_sections(output_sections);
    let info_values = compute_info_values(layout);

    for section_id in wild_layout::output_section_id::section_header_order(
        &layout.output_order,
        &layout.output_sections,
    ) {
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
        }

        let name = layout.output_sections.name(section_id).with_context(|| {
            format!(
                "Missing name for section {}",
                layout.output_sections.section_debug(section_id)
            )
        })?;
        let name_offset = shstrtab.offset(name.bytes())?;

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
    }

    let leftover_headers = entries.into_slice();
    let singleton_names = write_section_header_strings(shstrtab_out, output_sections)?;
    write_partial_link_singleton_headers(
        leftover_headers,
        singleton_names,
        layout,
        shstrtab.bytes.len() as u32,
    )?;
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

pub(crate) fn write_section_header_strings<'out, C: ElfClass>(
    out: &'out mut [u8],
    sections: &OutputSections<elf::Elf<C>>,
) -> Result<&'out mut [u8]> {
    let tab = elf::shstrtab_from_sections(sections);
    ensure!(
        out.len() >= tab.bytes.len(),
        "Allocated {} bytes for .shstrtab, but suffix-merged table is {} bytes",
        out.len(),
        tab.bytes.len()
    );
    let (merged, rest) = out.split_at_mut(tab.bytes.len());
    merged.copy_from_slice(&tab.bytes);
    Ok(rest)
}

fn write_partial_link_singleton_headers<C: ElfClass>(
    mut headers: &mut [elf::SectionHeader<C>],
    mut names: &mut [u8],
    layout: &ElfLayout<C>,
    mut name_offset: u32,
) -> Result {
    if layout.partial_link.is_empty() {
        ensure!(
            headers.is_empty(),
            "Allocated section entries that weren't used (leftover section headers)"
        );
        ensure!(
            names.is_empty(),
            "Allocated extra .shstrtab bytes with no partial-link singleton names"
        );
        return Ok(());
    }

    verbose_timing_phase!("Write partial link singleton headers");

    let mut work = Vec::with_capacity(layout.group_layouts.len());
    for (group, (header_count, name_bytes)) in layout
        .group_layouts
        .iter()
        .zip(layout.partial_link.group_sizes())
    {
        let group_headers = headers
            .split_off_mut(..header_count)
            .ok_or_else(|| insufficient_allocation("section headers"))?;
        let group_names = names
            .split_off_mut(..name_bytes)
            .ok_or_else(|| insufficient_allocation(".shstrtab"))?;
        if header_count != 0 {
            work.push((group, group_headers, group_names, name_offset));
        }
        name_offset += name_bytes as u32;
    }

    ensure!(
        headers.is_empty(),
        "Allocated section entries that weren't used (leftover section headers)"
    );
    ensure!(
        names.is_empty(),
        "Excess partial-link singleton name allocation"
    );

    work.into_par_iter().try_for_each(
        |(group, mut headers, mut names, mut name_offset)| -> Result {
            for file in &group.files {
                let FileLayout::Object(object) = file else {
                    continue;
                };

                for (raw_index, slot) in object.sections.iter().enumerate() {
                    let section_index = object::SectionIndex(raw_index);
                    let Some(singleton) = slot.singleton() else {
                        continue;
                    };

                    let header_out = headers
                        .split_off_first_mut()
                        .ok_or_else(|| insufficient_allocation("section headers"))?;
                    write_partial_link_singleton_header(
                        header_out,
                        layout,
                        object,
                        section_index,
                        singleton,
                        name_offset,
                    )
                    .with_context(|| {
                        format!(
                            "Failed to write partial-link singleton header for {} in {}",
                            object.object.section_display_name(section_index),
                            object.input,
                        )
                    })?;

                    let name = object.object.section_name(section_index)?;
                    let out = names
                        .split_off_mut(..=name.len())
                        .ok_or_else(|| insufficient_allocation(".shstrtab"))?;
                    out[..name.len()].copy_from_slice(name);
                    out[name.len()] = 0;
                    name_offset += name.len() as u32 + 1;
                }
            }
            ensure!(
                headers.is_empty(),
                "Excess partial-link singleton header allocation"
            );
            ensure!(
                names.is_empty(),
                "Excess partial-link singleton name allocation"
            );
            Ok(())
        },
    )?;

    Ok(())
}

fn write_partial_link_singleton_header<C: ElfClass>(
    header_out: &mut elf::SectionHeader<C>,
    layout: &ElfLayout<C>,
    object: &ObjectLayout<elf::Elf<C>>,
    section_index: object::SectionIndex,
    singleton: &PartialLinkSingleton,
    name_offset: u32,
) -> Result {
    let e = LittleEndian;

    let input_header = object.object.section(section_index)?;
    let part_id = object.section_part_id(section_index, &layout.symbol_db.section_part_ids);
    let part_layout = layout.section_part_layouts.get(part_id);

    let section_address = object.section_resolutions[section_index.0]
        .address()
        .context("Missing address for partial-link singleton section")?;

    let offset_in_part = section_address
        .checked_sub(part_layout.mem_offset)
        .context("Partial-link singleton precedes its output section part")?;

    header_out.set_address(0)?;
    header_out.set_size(singleton.section.size)?;
    let section_type = input_header.sh_type(e);

    let section_flags = input_header
        .sh_flags(e)
        .without(shf::COMPRESSED | shf::GROUP);

    header_out.set_type(section_type);
    header_out.set_flags(section_flags)?;
    header_out.set_alignment(object.object.section_alignment(input_header)?)?;

    let is_relocation_section =
        section_type == object::elf::SHT_RELA || section_type == object::elf::SHT_REL;
    let output_link = if is_relocation_section {
        layout
            .output_sections
            .output_index_of_section(output_section_id::SYMTAB_LOCAL)
            .context("Missing symbol table for partial-link relocation section")?
    } else {
        let input_link = object::SectionIndex(input_header.sh_link(e) as usize);

        partial_link_output_index_for_input_section(object, input_link, layout)
            .context("Missing linked section for partial-link singleton")?
    };
    header_out.set_link(output_link);

    let input_info = input_header.sh_info(e);
    let output_info = if is_relocation_section || section_flags.contains(shf::INFO_LINK) {
        partial_link_output_index_for_input_section(
            object,
            object::SectionIndex(input_info as usize),
            layout,
        )
        .context("Missing sh_info target for partial-link singleton")?
    } else {
        input_info
    };
    header_out.set_info(output_info);

    header_out.set_offset(part_layout.file_offset as u64 + offset_in_part)?;
    header_out.set_entry_size(input_header.sh_entsize(e).into())?;
    header_out.set_name(name_offset);

    Ok(())
}

fn partial_link_output_index_for_input_section<C: ElfClass>(
    object: &ObjectLayout<elf::Elf<C>>,
    section_index: object::SectionIndex,
    layout: &ElfLayout<C>,
) -> Option<u32> {
    if section_index.0 == 0 {
        return Some(0);
    }
    if let Some(singleton) = object.sections.get(section_index.0)?.singleton() {
        return Some(layout.partial_link.output_index(singleton));
    }

    let input_header = object.object.section(section_index).ok()?;
    if input_header.sh_type(LittleEndian) == object::elf::SHT_SYMTAB {
        return layout
            .output_sections
            .output_index_of_section(output_section_id::SYMTAB_LOCAL);
    }

    let part_id = object.section_part_id(section_index, &layout.symbol_db.section_part_ids);
    if part_id == wild_layout::part_id::UNMAPPED {
        return None;
    }

    let section_id = layout
        .output_sections
        .primary_output_section(part_id.output_section_id::<elf::Elf<C>>());

    layout.output_sections.output_index_of_section(section_id)
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
