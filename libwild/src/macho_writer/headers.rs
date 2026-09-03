use super::*;
use crate::bail;
use crate::ensure;
use crate::error;
use crate::error::Context;
use crate::error::Result;
use crate::layout::EpilogueLayout;
use crate::layout::OutputRecordLayout;
use crate::layout::PreludeLayout;
use crate::macho::BuildVersionCommand;
use crate::macho::CodeSignatureCommand;
use crate::macho::DYLINKER_PATH;
use crate::macho::DyldChainedFixupsCommand;
use crate::macho::DylibCommand;
use crate::macho::DylinkerCommand;
use crate::macho::EntryPointCommand;
use crate::macho::FileHeader;
use crate::macho::MACHO_COMMAND_ALIGNMENT;
use crate::macho::MACHO_START_MEM_ADDRESS;
use crate::macho::MachO;
use crate::macho::PLT_ENTRY_SIZE;
use crate::macho::SectionEntry;
use crate::macho::SegmentCommand;
use crate::macho::SegmentName;
use crate::macho::SymtabCommand;
use crate::macho::UuidCommand;
use crate::macho::get_segment_sections;
use crate::macho::load_dylib_command_size;
use crate::macho::output_section_id;
use crate::macho::output_section_id::LOAD_COMMANDS;
use crate::macho::part_id;
use crate::output_section_id::SectionName;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::verbose_timing_phase;
use linker_utils::utils::slice_from_all_bytes_mut;
use object::BigEndian;
use object::macho;
use object::macho::CPU_SUBTYPE_ARM64_ALL;
use object::macho::CPU_TYPE_ARM64;
use object::macho::LC_BUILD_VERSION;
use object::macho::LC_CODE_SIGNATURE;
use object::macho::LC_DYLD_CHAINED_FIXUPS;
use object::macho::LC_DYLD_EXPORTS_TRIE;
use object::macho::LC_LOAD_DYLIB;
use object::macho::LC_LOAD_DYLINKER;
use object::macho::LC_MAIN;
use object::macho::LC_SEGMENT_64;
use object::macho::LC_SYMTAB;
use object::macho::LC_UUID;
use object::macho::MH_CIGAM_64;
use object::macho::MH_EXECUTE;
use object::macho::PLATFORM_MACOS;
use object::macho::SegmentFlags;
use object::slice_from_bytes_mut;
use zerocopy::FromZeros;

pub(crate) fn write_prelude<'data>(
    prelude: &PreludeLayout<MachO>,
    buffers: &mut OutputSectionPartMap<&mut [u8]>,
    layout: &MachOLayout<'data>,
    exports_trie: &[u8],
) -> Result {
    verbose_timing_phase!("Write prelude");
    debug_assert_eq!(
        prelude.format_specific.imported_library_file_ids.len(),
        prelude.format_specific.load_dylib_command_sizes.len()
    );

    let header_buffer = buffers.get_mut(crate::part_id::FILE_HEADER);
    populate_file_header(layout, prelude, take_mut(header_buffer)?);
    ensure!(header_buffer.is_empty(), "Excess FILE_HEADER allocation");

    let mut load_command_buffer = slice_from_all_bytes_mut(buffers.get_mut(part_id::LOAD_COMMANDS));
    write_segment_commands(layout, &mut load_command_buffer)?;

    if layout.symbol_db.output_kind.is_executable() {
        write_entry_point_command(layout, take_mut(&mut load_command_buffer)?)?;
    }

    write_uuid_command(take_mut(&mut load_command_buffer)?);

    if layout.args().platform_version.is_some() {
        let build_version_command = take_mut(&mut load_command_buffer)?;
        write_build_version_command(layout, build_version_command)?;
    }

    let command_size = (size_of::<DylinkerCommand>() + DYLINKER_PATH.len())
        .next_multiple_of(MACHO_COMMAND_ALIGNMENT);
    let mut command_buffer = load_command_buffer.split_off_mut(..command_size).unwrap();
    let dylinker_command = take_mut(&mut command_buffer)?;
    write_dylinker_command(dylinker_command, command_buffer);

    for (&file_id, &command_size) in prelude
        .format_specific
        .imported_library_file_ids
        .iter()
        .zip(&prelude.format_specific.load_dylib_command_sizes)
    {
        let mut command_buffer = load_command_buffer.split_off_mut(..command_size).unwrap();
        let dylib_command = take_mut(&mut command_buffer)?;
        let path = crate::macho::install_name(file_id, &layout.symbol_db);

        write_dylib_command(dylib_command, command_buffer, path);
    }

    write_dyld_chained_fixups_command(layout, take_mut(&mut load_command_buffer)?);

    if layout.symbol_db.output_kind.needs_dynsym() {
        write_exports_trie_command(layout, exports_trie, take_mut(&mut load_command_buffer)?)?;
    }

    write_symtab_command(layout, take_mut(&mut load_command_buffer)?);

    write_code_signature_command(layout, take_mut(&mut load_command_buffer)?);

    ensure!(
        load_command_buffer.is_empty(),
        "Excess LOAD_COMMANDS allocation"
    );

    // Fill up one extra character as n_strx == 0 is treated as unnamed.
    buffers.get_mut(part_id::STRTAB).fill(0);

    Ok(())
}

pub(crate) fn write_epilogue(
    _epilogue: &EpilogueLayout<MachO>,
    buffers: &mut OutputSectionPartMap<&mut [u8]>,
    layout: &MachOLayout<'_>,
    exports_trie: &[u8],
) -> Result {
    verbose_timing_phase!("Write epilogue");
    write_chained_fixup_table(layout, buffers.get_mut(part_id::CHAINED_FIXUP_TABLE))?;
    let out = buffers.get_mut(part_id::EXPORTS_TRIE);
    ensure!(
        exports_trie.len() <= out.len(),
        "Mach-O exports trie exceeded its reserved size"
    );
    out[..exports_trie.len()].copy_from_slice(exports_trie);
    out[exports_trie.len()..].fill(0);

    Ok(())
}

pub(crate) fn populate_file_header(
    layout: &MachOLayout,
    prelude: &PreludeLayout<MachO>,
    header: &mut FileHeader,
) {
    let load_commands_info = layout.section_layouts.get(LOAD_COMMANDS);

    header.magic.set(BigEndian, MH_CIGAM_64);
    header.cputype.set(LE, CPU_TYPE_ARM64);
    header.cpusubtype.set(LE, CPU_SUBTYPE_ARM64_ALL.into());
    header.filetype.set(LE, MH_EXECUTE);
    header
        .ncmds
        .set(LE, prelude.format_specific.load_command_count as u32);
    header
        .sizeofcmds
        .set(LE, load_commands_info.file_size as u32);
    header.flags.set(
        LE,
        macho::MH_PIE | macho::MH_DYLDLINK | macho::MH_NOUNDEFS | macho::MH_TWOLEVEL,
    );
    header.reserved.set(LE, 0);
}

pub(crate) fn split_segment_command_buffer(
    mut bytes: &mut [u8],
    section_count: usize,
) -> Result<(&mut SegmentCommand, &mut [SectionEntry])> {
    let command = take_mut(&mut bytes)?;
    let (sections, rest) = slice_from_bytes_mut(bytes, section_count)
        .map_err(|_| error!("Invalid segment section allocation"))?;
    ensure!(
        rest.is_empty(),
        "Trailing bytes in segment command allocation"
    );
    Ok((command, sections))
}

pub(crate) fn write_segment_commands(
    layout: &MachOLayout,
    load_commands: &mut &mut [u8],
) -> Result {
    let load_cmd_err = |()| error!("Invalid LOAD_COMMANDS allocation");
    let pagezero_segment = take_mut(load_commands)?;
    write_segment(
        SegmentName::PAGEZERO,
        macho::VmProt(0),
        pagezero_segment,
        0,
        0,
        0,
        MACHO_START_MEM_ADDRESS,
        0,
        SegmentFlags::default(),
    );

    for segment_layout in &layout.segment_layouts.segments {
        let segment_id = segment_layout.id;
        let segment_def = *layout.program_segments.segment_def(segment_id);

        let segment_sections = get_segment_sections(layout, segment_id);
        let section_count = segment_sections.len();
        let command_size = size_of::<SegmentCommand>() + size_of::<SectionEntry>() * section_count;

        let (segment, sections) = split_segment_command_buffer(
            load_commands
                .split_off_mut(..command_size)
                .ok_or_else(|| load_cmd_err(()))?,
            section_count,
        )?;

        let size = segment_layout.sizes;
        write_segment(
            segment_def.name,
            segment_def.prot,
            segment,
            size.file_offset as u64,
            size.file_size as u64,
            size.mem_offset,
            size.mem_size,
            section_count,
            segment_def.flags,
        );
        write_sections(segment_def.name, sections, &segment_sections);
    }

    Ok(())
}

pub(crate) fn write_segment(
    seg_name: SegmentName,
    prot_flags: object::macho::VmProt,
    segment_cmd: &mut SegmentCommand,
    file_offset: u64,
    file_size: u64,
    mem_offset: u64,
    mem_size: u64,
    section_count: usize,
    flags: macho::SegmentFlags,
) {
    segment_cmd.cmd.set(LE, LC_SEGMENT_64);
    segment_cmd.cmdsize.set(
        LE,
        (size_of::<SegmentCommand>() + size_of::<SectionEntry>() * section_count) as u32,
    );
    segment_cmd.segname = seg_name.into_bytes();
    segment_cmd.fileoff.set(LE, file_offset);
    segment_cmd.filesize.set(LE, file_size);
    segment_cmd.vmaddr.set(LE, mem_offset);
    segment_cmd.vmsize.set(LE, mem_size);
    segment_cmd.maxprot.set(LE, prot_flags);
    segment_cmd.initprot.set(LE, prot_flags);
    segment_cmd.nsects.set(LE, section_count as u32);
    segment_cmd.flags.set(LE, flags);
}

pub(crate) fn write_sections(
    seg_name: SegmentName,
    sections: &mut [SectionEntry],
    segment_sections: &[(
        OutputRecordLayout,
        SectionName<'_>,
        crate::macho::SectionFlags,
    )],
) {
    for (section, (size, section_name, section_flags)) in sections.iter_mut().zip(segment_sections)
    {
        let section_name = section_name.0;

        section.segname = seg_name.into_bytes();
        section.sectname[..section_name.len()].copy_from_slice(section_name);
        section.sectname[section_name.len()..].zero();
        section.addr.set(LE, size.mem_offset);
        section.size.set(LE, size.mem_size);
        section.offset.set(LE, size.file_offset as u32);
        section.align.set(LE, u32::from(size.alignment.exponent));
        section.reloff.set(LE, 0);
        section.nreloc.set(LE, 0);
        section.flags.set(LE, *section_flags);
        section.reserved1.set(LE, 0);
        // TODO: find a better place
        let reserved2 =
            if section_flags.0 & macho::SECTION_TYPE == u32::from(macho::S_SYMBOL_STUBS.0) {
                PLT_ENTRY_SIZE as u32
            } else {
                0
            };
        section.reserved2.set(LE, reserved2);
        section.reserved3.set(LE, 0);
    }
}

pub(crate) fn write_entry_point_command(
    layout: &MachOLayout,
    command: &mut EntryPointCommand,
) -> Result {
    let entry_name = match layout.symbol_db.entry_point() {
        crate::platform::EntryPoint::Symbol(name) => String::from_utf8_lossy(name),
        crate::platform::EntryPoint::None | crate::platform::EntryPoint::Address(_) => {
            bail!("Mach-O executable entry point must be a symbol")
        }
    };

    let entry_address = layout
        .resolved_entry_symbol_address()?
        .with_context(|| format!("entry symbol `{entry_name}` is not defined"))?;

    let image_base = layout
        .section_layouts
        .get(crate::output_section_id::FILE_HEADER)
        .mem_offset;

    let entry_offset = entry_address
        .checked_sub(image_base)
        .context("entry point is before the Mach-O image base")?;

    command.cmd.set(LE, LC_MAIN);
    command
        .cmdsize
        .set(LE, size_of::<EntryPointCommand>() as u32);
    command.entryoff.set(LE, entry_offset);
    command.stacksize.set(LE, 0);
    Ok(())
}

pub(crate) fn write_build_version_command(
    layout: &MachOLayout,
    command: &mut BuildVersionCommand,
) -> Result {
    let platform_version = layout
        .args()
        .platform_version
        .as_ref()
        .ok_or("platform_version must be set")?;

    command.cmd.set(LE, LC_BUILD_VERSION);
    command
        .cmdsize
        .set(LE, size_of::<BuildVersionCommand>() as u32);
    command.platform.set(LE, PLATFORM_MACOS);
    command
        .minos
        .set(LE, platform_version.minimum_version.get());
    command.sdk.set(LE, platform_version.sdk_version.get());
    command.ntools.set(LE, 0);
    // TODO: We could record Wild's version here, but Mach-O only defines tool IDs
    // for Apple toolchain components, so leave the tools list empty for now.
    Ok(())
}

pub(crate) fn write_uuid_command(command: &mut UuidCommand) {
    command.cmd.set(LE, LC_UUID);
    command.cmdsize.set(LE, size_of::<UuidCommand>() as u32);
    command.uuid.zero();
}

pub(crate) fn write_dylinker_command(command: &mut DylinkerCommand, path_buffer: &mut [u8]) {
    command.cmd.set(LE, LC_LOAD_DYLINKER);
    command.cmdsize.set(
        LE,
        ((size_of::<DylinkerCommand>() + DYLINKER_PATH.len())
            .next_multiple_of(MACHO_COMMAND_ALIGNMENT)) as u32,
    );
    command
        .name
        .offset
        .set(LE, size_of::<DylinkerCommand>() as u32);

    path_buffer[0..DYLINKER_PATH.len()].copy_from_slice(DYLINKER_PATH);
    path_buffer[DYLINKER_PATH.len()..].zero();
}

pub(crate) fn write_dylib_command(command: &mut DylibCommand, path_buffer: &mut [u8], path: &[u8]) {
    command.cmd.set(LE, LC_LOAD_DYLIB);
    command
        .cmdsize
        .set(LE, load_dylib_command_size(path) as u32);
    command
        .dylib
        .name
        .offset
        .set(LE, size_of::<DylibCommand>() as u32);
    // TODO
    command.dylib.timestamp.set(LE, 2);
    // TODO
    command
        .dylib
        .current_version
        .set(LE, macho::Version(1356 << 16));
    command
        .dylib
        .compatibility_version
        .set(LE, macho::Version(1 << 16));

    path_buffer[0..path.len()].copy_from_slice(path);
    path_buffer[path.len()..].zero();
}

pub(crate) fn write_dyld_chained_fixups_command(
    layout: &MachOLayout,
    command: &mut DyldChainedFixupsCommand,
) {
    let chained_fixup_table = layout
        .section_layouts
        .get(output_section_id::CHAINED_FIXUP_TABLE);

    command.cmd.set(LE, LC_DYLD_CHAINED_FIXUPS);
    command
        .cmdsize
        .set(LE, size_of::<DyldChainedFixupsCommand>() as u32);
    command
        .dataoff
        .set(LE, chained_fixup_table.file_offset as u32);
    command
        .datasize
        .set(LE, chained_fixup_table.file_size as u32);
}

pub(crate) fn write_exports_trie_command(
    layout: &MachOLayout,
    exports_trie: &[u8],
    command: &mut ExportsTrieCommand,
) -> Result {
    let exports_trie_layout = layout.section_layouts.get(output_section_id::EXPORTS_TRIE);

    command.cmd.set(LE, LC_DYLD_EXPORTS_TRIE);
    command
        .cmdsize
        .set(LE, size_of::<ExportsTrieCommand>() as u32);
    command.dataoff.set(
        LE,
        exports_trie_layout
            .file_offset
            .try_into()
            .context("Mach-O exports trie offset exceeds 32 bits")?,
    );
    command.datasize.set(
        LE,
        exports_trie
            .len()
            .try_into()
            .context("Mach-O exports trie size exceeds 32 bits")?,
    );
    Ok(())
}

pub(crate) fn write_symtab_command(layout: &MachOLayout, command: &mut SymtabCommand) {
    let symtab = layout.section_layouts.get(output_section_id::SYMTAB_GLOBAL);
    let strtab = layout.section_layouts.get(output_section_id::STRTAB);

    command.cmd.set(LE, LC_SYMTAB);
    command.cmdsize.set(LE, size_of::<SymtabCommand>() as u32);
    command.symoff.set(LE, symtab.file_offset as u32);
    command
        .nsyms
        .set(LE, (symtab.file_size / size_of::<SymtabEntry>()) as u32);
    command.stroff.set(LE, strtab.file_offset as u32);
    command.strsize.set(LE, strtab.file_size as u32);
}

pub(crate) fn write_code_signature_command(
    layout: &MachOLayout,
    command: &mut CodeSignatureCommand,
) {
    let code_signature = layout
        .section_layouts
        .get(output_section_id::CODE_SIGNATURE);

    command.cmd.set(LE, LC_CODE_SIGNATURE);
    command
        .cmdsize
        .set(LE, size_of::<CodeSignatureCommand>() as u32);
    command.dataoff.set(LE, code_signature.file_offset as u32);
    command.datasize.set(LE, code_signature.file_size as u32);
}
