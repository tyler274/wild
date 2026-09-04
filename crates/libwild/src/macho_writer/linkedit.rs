use super::*;
use crate::OutputFileData;
use crate::alignment::MACHO_PAGE_ALIGNMENT;
use crate::bail;
use crate::ensure;
use crate::error;
use crate::error::Context;
use crate::error::Result;
use crate::file_writer::SizedOutput;
use crate::file_writer::split_output_into_sections;
use crate::layout::FileLayout;
use crate::macho::CHAINED_FIXUP_PAGE_START_SIZE;
use crate::macho::CS_BLOB_HEADERS_SIZE;
use crate::macho::CS_BLOCK_SIZE;
use crate::macho::CS_BLOCK_SIZE_EXP;
use crate::macho::CS_CODE_DIRECTORY_SIZE;
use crate::macho::CS_HASH_SIZE;
use crate::macho::CS_HEADERS_SIZE;
use crate::macho::ChainedFixupsHeader;
use crate::macho::ChainedStartsInSegment;
use crate::macho::MAX_SEGMENT_COUNT;
use crate::macho::SegmentName;
use crate::macho::UuidCommand;
use crate::macho::code_signature_identifier;
use crate::macho::code_signature_padded_identifier_size;
use crate::macho::output_section_id;
use crate::platform::ObjectFile;
use crate::platform::Symbol;
use crate::symbol_db::SymbolId;
use crate::verbose_timing_phase;
use itertools::Itertools;
use object::Endianness;
use object::U16;
use object::U32;
use object::from_bytes_mut;
use object::macho::CS_ADHOC;
use object::macho::CS_EXECSEG_MAIN_BINARY;
use object::macho::CS_HASHTYPE_SHA256;
use object::macho::CS_LINKER_SIGNED;
use object::macho::CS_SUPPORTSEXECSEG;
use object::macho::CSSLOT_CODEDIRECTORY;
use object::macho::DYLD_CHAINED_IMPORT;
use object::macho::DYLD_CHAINED_PTR_64_OFFSET;
use object::macho::LC_UUID;
use object::macho::LoadCommand;
use object::slice_from_bytes_mut;
use object::write::macho::CodeDirectory;
use object::write::macho::CodeSignatureEncoder;
use rayon::iter::ParallelIterator;
use rayon::slice::ParallelSlice;
use sha2::Digest;
use sha2::Sha256;
use zerocopy::FromZeros;

pub(crate) fn build_exports_trie(layout: &MachOLayout<'_>) -> Result<Vec<u8>> {
    if !layout.symbol_db.output_kind.needs_dynsym() {
        return Ok(Vec::new());
    }

    let text_segment = layout
        .segment_layouts
        .segments
        .iter()
        .find(|segment| layout.program_segments.segment_def(segment.id).name == SegmentName::TEXT)
        .context("Missing Mach-O __TEXT segment")?;

    let image_base = text_segment.sizes.mem_offset;

    let mut symbols = layout
        .dynamic_symbol_definitions
        .iter()
        .map(|symbol| {
            let resolution = layout
                .symbol_resolutions
                .get(symbol.symbol_id)
                .with_context(|| {
                    format!(
                        "Missing resolution for exported symbol `{}`",
                        String::from_utf8_lossy(symbol.name)
                    )
                })?;

            let (address, mut flags) = if resolution.is_absolute() {
                (
                    resolution.raw_value,
                    object::macho::EXPORT_SYMBOL_FLAGS_KIND_ABSOLUTE.into(),
                )
            } else {
                (
                    resolution
                        .raw_value
                        .checked_sub(image_base)
                        .with_context(|| {
                            format!(
                                "Exported symbol `{}` is before the Mach-O image base",
                                String::from_utf8_lossy(symbol.name)
                            )
                        })?,
                    object::macho::ExportSymbolFlags(0),
                )
            };

            if exported_symbol_is_weak(layout, symbol.symbol_id)? {
                flags |= object::macho::EXPORT_SYMBOL_FLAGS_WEAK_DEFINITION;
            }

            Ok(crate::trie::Symbol {
                name: symbol.name,
                address,
                flags,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(crate::trie::build(&mut symbols))
}

pub(crate) fn exported_symbol_is_weak(
    layout: &MachOLayout<'_>,
    symbol_id: SymbolId,
) -> Result<bool> {
    let file_id = layout.symbol_db.file_id_for_symbol(symbol_id);
    let FileLayout::Object(object) = layout.file_layout(file_id) else {
        return Ok(false);
    };
    let symbol_index = object.symbol_id_range.id_to_input(symbol_id);
    Ok(object.object.symbol(symbol_index)?.is_weak())
}

pub(crate) fn write_chained_fixup_table(
    layout: &MachOLayout,
    chained_fixup_table: &mut [u8],
) -> Result {
    let symbols = &layout.format_specific.imported_symbols;
    let active_segments = &layout.segment_layouts.segments;

    // The __PAGEZERO segment needs to be added manually.
    let segment_count = active_segments.len() + 1;
    ensure!(
        segment_count <= MAX_SEGMENT_COUNT,
        "unexpected number of active segments"
    );
    let starts_in_image_len = size_of::<u32>() * (segment_count + 1);
    let starts_in_segment_len =
        size_of::<ChainedStartsInSegment>() + CHAINED_FIXUP_PAGE_START_SIZE as usize;
    let imports_len = size_of::<u32>() * symbols.len();

    let starts_offset = size_of::<ChainedFixupsHeader>();
    let imports_offset = starts_offset + starts_in_image_len + starts_in_segment_len;
    let symbols_offset = imports_offset + imports_len;

    let (header, rest) = from_bytes_mut::<ChainedFixupsHeader>(chained_fixup_table)
        .map_err(|_| error!("Invalid chained fixups header allocation"))?;
    let (starts_in_image, rest) = slice_from_bytes_mut::<U32<Endianness>>(rest, segment_count + 1)
        .map_err(|_| error!("Invalid chained fixups starts allocation"))?;

    // 1) fill up ChainedFixupsHeader
    header.fixups_version.set(LE, 0);
    header.starts_offset.set(LE, starts_offset as u32);
    header.imports_offset.set(LE, imports_offset as u32);
    header.symbols_offset.set(LE, symbols_offset as u32);
    header.imports_count.set(LE, symbols.len() as u32);
    header.imports_format.set(LE, DYLD_CHAINED_IMPORT);
    header.symbols_format.set(LE, 0);

    // 2) fill up dyld_chained_starts_in_image, which is `seg_count` (u32) followed by
    //    `seg_info_offset` ([u32; seg_count]); only __DATA_CONST,__got segment is covered
    starts_in_image[0].set(LE, segment_count as u32);
    starts_in_image[1..].fill(U32::new(LE, 0));

    // Early exit if we don't have any GOT entry to be encoded.
    if layout.section_layouts.get(output_section_id::GOT).mem_size == 0 {
        rest.zero();
        return Ok(());
    }

    let (data_const_segment_index, data_const_segment) = active_segments
        .iter()
        .enumerate()
        .find(|(_, segment)| {
            layout.program_segments.segment_def(segment.id).name == SegmentName::DATA_CONST
        })
        .ok_or_else(|| error!("non-empty __got requires __DATA_CONST segment"))?;

    // Accounts for both seg_count and __PAGEZERO.
    starts_in_image[data_const_segment_index + 2].set(LE, starts_in_image_len as u32);

    let (starts_in_segment, rest) = from_bytes_mut::<ChainedStartsInSegment>(rest)
        .map_err(|_| error!("Invalid chained fixups starts in segment allocation"))?;
    let (page_starts, rest) = slice_from_bytes_mut::<U16<Endianness>>(rest, 1)
        .map_err(|_| error!("Invalid chained fixups page starts allocation"))?;
    let (imports, string_pool) = slice_from_bytes_mut::<U32<Endianness>>(rest, symbols.len())
        .map_err(|_| error!("Invalid chained fixups imports allocation"))?;

    // 3) fill up DyldChainedStartsInSegment for the __got section
    starts_in_segment.size.set(LE, starts_in_segment_len as u32);
    starts_in_segment
        .page_size
        .set(LE, MACHO_PAGE_ALIGNMENT.value() as u16);
    starts_in_segment
        .pointer_format
        .set(LE, DYLD_CHAINED_PTR_64_OFFSET);
    starts_in_segment
        .segment_offset
        .set(LE, data_const_segment.sizes.file_offset as u64);
    starts_in_segment.max_valid_pointer.set(LE, 0);
    // TODO:
    starts_in_segment.page_count.set(LE, 1);
    page_starts[0].set(LE, 0);

    // 4) fill up all imported symbols chunked by the pages
    // TODO: support more pages
    assert!(symbols.len() < MACHO_PAGE_ALIGNMENT.value() as usize / size_of::<u32>());

    let sorted_symbols = &layout.format_specific.imported_symbols;
    let mut symbol_offsets = Vec::with_capacity(sorted_symbols.len());
    let mut str_offset = 0;
    for imported_symbol in sorted_symbols {
        let symbol_name = layout
            .symbol_db
            .symbol_name(imported_symbol.symbol_id)
            .unwrap()
            .bytes();
        string_pool[str_offset..str_offset + symbol_name.len()].copy_from_slice(symbol_name);
        string_pool[str_offset + symbol_name.len()] = b'\0';
        symbol_offsets.push(str_offset);
        str_offset += symbol_name.len() + 1;
    }

    // Emit `dyld_chained_import` that is built by 3 pieces:
    // lib_ordinal: 8
    // weak_import: 1
    // name_offset: 23
    for (i, imported_symbol) in sorted_symbols.iter().enumerate() {
        let file_id = layout
            .symbol_db
            .file_id_for_symbol(imported_symbol.symbol_id);

        let dynamic = match layout.file_layout(file_id) {
            FileLayout::StubLibrary(file) => &file.format_specific,
            FileLayout::Dynamic(file) => &file.format_specific,
            _ => {
                bail!("Internal error: Internal symbol refers to non-stub library");
            }
        };

        let lib_ordinal = dynamic.ordinal.get();

        imports[i].set(
            Endianness::Little,
            u32::from(lib_ordinal) | ((symbol_offsets[i] as u32) << 9),
        );
    }

    // Pad a couple of bytes (related to the MAX_SEGMENT_COUNT).
    string_pool[str_offset..].fill(0);

    Ok(())
}

pub(crate) fn write_uuid(
    layout: &MachOLayout,
    sized_output: &mut SizedOutput<impl OutputFileData>,
) -> Result {
    verbose_timing_phase!("Write UUID");

    let hash = blake3::Hasher::new()
        .update_rayon(&sized_output.out)
        .finalize();

    let mut section_buffers = split_output_into_sections(layout, &mut sized_output.out).0;
    let load_commands = section_buffers.get_mut(output_section_id::LOAD_COMMANDS);

    while !load_commands.is_empty() {
        let header = object::from_bytes::<LoadCommand<Endianness>>(load_commands)
            .map_err(|_| error!("Invalid load command header"))?
            .0;
        let cmd_type = header.cmd.get(LE);
        let cmd_size = header.cmdsize.get(LE) as usize;
        let mut cmd = load_commands
            .split_off_mut(..cmd_size)
            .context("Invalid load command allocation")?;

        if cmd_type == LC_UUID {
            let uuid_cmd = take_mut::<UuidCommand>(&mut cmd)?;
            let uuid_size = uuid_cmd.uuid.len();

            uuid_cmd.uuid.copy_from_slice(&hash.as_bytes()[..uuid_size]);
            // Match lld's UUID Version 3 from RFC 9562.
            uuid_cmd.uuid[6] = (uuid_cmd.uuid[6] & 0x0f) | 0x30;
            uuid_cmd.uuid[8] = (uuid_cmd.uuid[8] & 0x3f) | 0x80;
            return Ok(());
        }
    }

    bail!("Missing LC_UUID");
}

pub(crate) fn write_code_signature_metadata(
    layout: &MachOLayout,
    sized_output: &mut SizedOutput<impl OutputFileData>,
) -> Result {
    verbose_timing_phase!("Write code signature metadata");

    let code_signature_section = layout
        .section_layouts
        .get(output_section_id::CODE_SIGNATURE);
    let code_signature_identifier = code_signature_identifier(layout.args());
    let padded_identifier_size = code_signature_padded_identifier_size(layout.args()) as usize;

    let mut section_buffers = split_output_into_sections(layout, &mut sized_output.out).0;
    let code_signature = section_buffers.get_mut(output_section_id::CODE_SIGNATURE);

    let encoder = CodeSignatureEncoder;
    let code_directory_size = encoder.code_directory_size(CS_SUPPORTSEXECSEG);
    ensure!(
        u64::from(code_directory_size) == CS_CODE_DIRECTORY_SIZE,
        "Unexpected code directory size"
    );

    let text_segment = layout
        .segment_layouts
        .segments
        .iter()
        .find(|segment| layout.program_segments.segment_def(segment.id).name == SegmentName::TEXT)
        .ok_or_else(|| error!("__TEXT segment is mandatory"))?;

    let code_directory = CodeDirectory {
        length: (code_signature_section.file_size - CS_BLOB_HEADERS_SIZE as usize) as u32,
        version: CS_SUPPORTSEXECSEG,
        flags: CS_ADHOC | CS_LINKER_SIGNED,
        hash_offset: code_directory_size + padded_identifier_size as u32,
        ident_offset: code_directory_size,
        n_special_slots: 0,
        n_code_slots: code_signature_section.file_offset.div_ceil(CS_BLOCK_SIZE) as u32,
        code_limit: code_signature_section.file_offset as u64,
        hash_size: CS_HASH_SIZE,
        hash_type: CS_HASHTYPE_SHA256,
        platform: 0,
        page_size: CS_BLOCK_SIZE_EXP,
        scatter_offset: 0,
        team_offset: 0,
        exec_seg_base: text_segment.sizes.file_offset as u64,
        exec_seg_limit: text_segment.sizes.file_size as u64,
        // TODO: change once shared libraries are supported
        exec_seg_flags: CS_EXECSEG_MAIN_BINARY,
    };

    let mut rest: &mut [u8] = code_signature;
    encoder.signature_super_blob(&mut rest, code_signature_section.file_size as u32, 1);
    encoder.blob_index(&mut rest, CSSLOT_CODEDIRECTORY, CS_BLOB_HEADERS_SIZE as u32);
    encoder.code_directory(&mut rest, &code_directory);

    let (identifier, hashes) = rest.split_at_mut(padded_identifier_size);
    identifier[..code_signature_identifier.len()].copy_from_slice(code_signature_identifier);
    identifier[code_signature_identifier.len()..].zero();
    hashes.zero();

    Ok(())
}

pub(crate) fn write_code_signature_hashes(
    layout: &MachOLayout,
    sized_output: &mut SizedOutput<impl OutputFileData>,
) -> Result {
    verbose_timing_phase!("Write code signature hashes");

    let code_signature_section = layout
        .section_layouts
        .get(output_section_id::CODE_SIGNATURE);
    let calculated_hashes: Vec<_> = sized_output.out[..code_signature_section.file_offset]
        .par_chunks(CS_BLOCK_SIZE)
        .map(Sha256::digest)
        .collect();
    let calculated_hashes = calculated_hashes.into_iter().flatten().collect_vec();

    let mut section_buffers = split_output_into_sections(layout, &mut sized_output.out).0;
    let code_signature = section_buffers.get_mut(output_section_id::CODE_SIGNATURE);
    let hashes_offset =
        (CS_HEADERS_SIZE + code_signature_padded_identifier_size(layout.args())) as usize;
    let hashes = code_signature
        .get_mut(hashes_offset..)
        .ok_or_else(|| error!("Invalid CODE_SIGNATURE allocation"))?;

    hashes.copy_from_slice(&calculated_hashes);

    // Match lld's workaround for the macOS kernel caching signature-verification
    // data before the final code signature has been written:
    //
    // https://openradar.appspot.com/FB8914231
    sized_output
        .out
        .invalidate(code_signature_section.file_offset + code_signature_section.file_size);

    Ok(())
}
