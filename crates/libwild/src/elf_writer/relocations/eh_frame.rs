use super::super::types::*;
use super::*;
use crate::bail;
use crate::elf;
use crate::elf::EhFrameHdrEntry;
use crate::elf::ElfClass;
use crate::elf::output_section_id;
use crate::error;
use crate::error::Context as _;
use crate::error::Result;
use crate::layout::ObjectLayout;
use crate::output_trace::TraceOutput;
use crate::platform::Arch;
use crate::platform::ObjectFile;
use crate::platform::Relocation;
use hashbrown::HashMap;
use linker_utils::relaxation::opt_input_to_output;
use object::LittleEndian;
use object::read::elf::SectionHeader as _;
use object::read::elf::Sym as _;
use std::iter;
use zerocopy::FromBytes;

pub(crate) fn write_eh_frame_data<'data, C: ElfClass, A: Arch<Platform = elf::Elf<C>>>(
    object: &ObjectLayout<'data, elf::Elf<C>>,
    eh_frame_section_index: object::SectionIndex,
    layout: &ElfLayout<'data, C>,
    table_writer: &mut TableWriter<'_, '_, C>,
    trace: &TraceOutput,
) -> Result {
    let eh_frame_section = object.object.section(eh_frame_section_index)?;
    match object.relocations(eh_frame_section_index)? {
        elf::RelocationList::Rela(relocations) => {
            write_eh_frame_relocations::<C, A, elf::ElfRela<C>>(
                object,
                layout,
                table_writer,
                trace,
                eh_frame_section,
                relocations.iter().copied().map(elf::ElfRela::new),
            )
        }
        elf::RelocationList::Crel(relocations) => {
            write_eh_frame_relocations::<C, A, elf::ElfCrel<C>>(
                object,
                layout,
                table_writer,
                trace,
                eh_frame_section,
                relocations.filter_map(|r| r.ok().map(elf::ElfCrel::new)),
            )
        }
    }
}

pub(crate) fn write_eh_frame_relocations<
    'data,
    C: ElfClass,
    A: Arch<Platform = elf::Elf<C>>,
    R: Relocation<Platform = elf::Elf<C>>,
>(
    object: &ObjectLayout<'data, elf::Elf<C>>,
    layout: &ElfLayout<'data, C>,
    table_writer: &mut TableWriter<'_, '_, C>,
    trace: &TraceOutput,
    eh_frame_section: &elf::SectionHeader<C>,
    relocations: impl Iterator<Item = R>,
) -> std::result::Result<(), error::Error> {
    let data = object.object.raw_section_data(eh_frame_section)?;
    const PREFIX_LEN: usize = size_of::<elf::EhFrameEntryPrefix>();
    let e = LittleEndian;
    let section_flags = eh_frame_section.sh_flags(LittleEndian);
    let mut relocations = relocations.peekable();
    let mut input_pos = 0;
    let mut output_pos = 0;
    let frame_info_ptr_base = table_writer.eh_frame_start_address;
    let eh_frame_hdr_address = layout.mem_address_of_built_in(output_section_id::EH_FRAME_HDR);

    // Map from input offset to output offset of each CIE.
    let mut cies_offset_conversion: HashMap<u32, u32> = HashMap::new();

    while input_pos + PREFIX_LEN <= data.len() {
        let prefix =
            elf::EhFrameEntryPrefix::read_from_bytes(&data[input_pos..input_pos + PREFIX_LEN])
                .unwrap();
        if prefix.length == 0 {
            input_pos = data.len();
            break;
        }
        let size = size_of_val(&prefix.length) + prefix.length as usize;
        let next_input_pos = input_pos + size;
        let next_output_pos = output_pos + size;
        if next_input_pos > data.len() {
            bail!("Invalid .eh_frame data");
        }
        let mut should_keep = false;
        let mut output_cie_offset = None;
        if prefix.cie_id == 0 {
            // This is a CIE
            cies_offset_conversion.insert(input_pos as u32, output_pos as u32);
            should_keep = true;
        } else {
            // This is an FDE
            if let Some(rel) = relocations.peek() {
                let rel_offset = rel.offset();
                if rel_offset < next_input_pos as u64 {
                    let is_pc_begin = (rel_offset as usize - input_pos) == elf::FDE_PC_BEGIN_OFFSET;

                    if is_pc_begin {
                        let Some(index) = rel.symbol() else {
                            bail!("Unexpected absolute relocation in .eh_frame pc-begin");
                        };
                        let elf_symbol = &object.object.symbol(index)?;
                        let Some(section_index) =
                            object.object.symbol_section(elf_symbol, index)?
                        else {
                            bail!(".eh_frame pc-begin refers to symbol that's not defined in file");
                        };
                        let offset_in_section = (Into::<u64>::into(elf_symbol.st_value(e)) as i64
                            + rel.addend()) as u64;
                        if let Some(section_address) =
                            object.section_resolutions[section_index.0].address()
                            && object
                                .object
                                .section(section_index)?
                                .sh_size(LittleEndian)
                                .into()
                                != 0
                        {
                            should_keep = true;
                            let cie_pointer_pos = input_pos as u32 + 4;
                            let input_cie_pos = cie_pointer_pos
                                .checked_sub(prefix.cie_id)
                                .with_context(|| {
                                    format!(
                                        "CIE pointer is {}, but we're at offset {}",
                                        prefix.cie_id, cie_pointer_pos
                                    )
                                })?;

                            if let Some(hdr_out) = table_writer.take_eh_frame_hdr_entry() {
                                // When relaxation has deleted bytes from the target section, the
                                // symbol's input offset no longer matches the output position.
                                let output_offset_in_section = opt_input_to_output(
                                    object.section_relax_deltas.get(section_index.0),
                                    offset_in_section,
                                );
                                let frame_ptr = (section_address + output_offset_in_section) as i64
                                    - eh_frame_hdr_address as i64;
                                let frame_info_ptr = (frame_info_ptr_base + output_pos as u64)
                                    as i64
                                    - eh_frame_hdr_address as i64;
                                *hdr_out = EhFrameHdrEntry {
                                    frame_ptr: i32::try_from(frame_ptr)
                                        .context("32 bit overflow in frame_ptr")?,
                                    frame_info_ptr: i32::try_from(frame_info_ptr)
                                        .context("32 bit overflow when computing frame_info_ptr")?,
                                };
                            }
                            // TODO: Experiment with skipping this lookup if the `input_cie_pos`
                            // is the same as the previous entry.
                            let output_cie_pos = cies_offset_conversion.get(&input_cie_pos).with_context(|| format!("FDE referenced CIE at {input_cie_pos}, but no CIE at that position"))?;
                            output_cie_offset = Some(output_pos as u32 + 4 - *output_cie_pos);
                        }
                    }
                }
            }
        }
        if should_keep {
            let entry_out = table_writer.take_eh_frame_data(next_output_pos - output_pos)?;
            entry_out.copy_from_slice(&data[input_pos..next_input_pos]);
            if let Some(output_cie_offset) = output_cie_offset {
                entry_out[4..8].copy_from_slice(&output_cie_offset.to_le_bytes());
            }
            while let Some(rel) = relocations.peek() {
                let rel_offset = rel.offset();
                if rel_offset >= next_input_pos as u64 {
                    // This relocation belongs to the next entry.
                    break;
                }
                apply_relocation::<C, A, R, _>(
                    object,
                    rel_offset - input_pos as u64,
                    rel,
                    SectionInfo {
                        section_address: output_pos as u64 + table_writer.eh_frame_start_address,
                        is_writable: false,
                        section_flags,
                        // .eh_frame relocations never need thunks; use the eh_frame section's
                        // base part as a placeholder so the thunk lookup always misses.
                        part_id: output_section_id::EH_FRAME.base_part_id::<elf::Elf<C>>(),
                    },
                    layout,
                    entry_out,
                    table_writer,
                    trace,
                    &RelocationCache::default(),
                    &iter::empty(),
                    None,
                )
                .with_context(|| {
                    format!(
                        "Failed to apply eh_frame {}",
                        display_relocation::<C, A, R>(object, rel, layout)
                    )
                })?;
                relocations.next();
            }
            output_pos = next_output_pos;
        } else {
            // We're ignoring this entry, skip any relocations for it.
            while let Some(rel) = relocations.peek() {
                if rel.offset() < next_input_pos as u64 {
                    relocations.next();
                } else {
                    break;
                }
            }
        }
        input_pos = next_input_pos;
    }

    // Copy any remaining bytes in .eh_frame that aren't large enough to constitute an actual
    // entry. crtend.o has a single u32 equal to 0 as an end marker.
    let remaining = data.len() - input_pos;
    if remaining > 0 && !elf::is_eh_frame_terminator(&data[input_pos..input_pos + remaining]) {
        table_writer
            .take_eh_frame_data(remaining)?
            .copy_from_slice(&data[input_pos..input_pos + remaining]);
        output_pos += remaining;
    }

    table_writer.eh_frame_start_address += output_pos as u64;

    Ok(())
}
