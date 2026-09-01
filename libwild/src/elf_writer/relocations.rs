use self::elf::get_page_mask;
use super::types::*;
use crate::OutputKind;
use crate::bail;
use crate::elf;
use crate::elf::EhFrameHdrEntry;
use crate::elf::ElfClass;
use crate::elf::output_section_id;
use crate::ensure;
use crate::error;
use crate::error::Context as _;
use crate::error::Result;
use crate::layout::FileLayout;
use crate::layout::Layout;
use crate::layout::ObjectLayout;
use crate::layout::Resolution;
use crate::output_section_id::SectionName;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::output_trace::HexU64;
use crate::output_trace::TraceOutput;
use crate::part_id::PartId;
use crate::platform;
use crate::platform::Arch;
use crate::platform::Args as _;
use crate::platform::ObjectFile;
use crate::platform::Platform;
use crate::platform::PreviousRelocationInfo;
use crate::platform::Relaxation as _;
use crate::platform::Relocation;
use crate::platform::RelocationList;
use crate::platform::SectionFlags as _;
use crate::platform::SectionHeader as _;
use crate::resolution::SectionSlot;
use crate::string_merging::get_merged_string_output_address;
use crate::symbol_db::SymbolDb;
use crate::symbol_db::SymbolId;
use crate::thunks::ThunkBlockId;
use crate::value_flags::PerSymbolFlags;
use crate::value_flags::ValueFlags;
use crate::writable_elf::WritableRela as _;
use hashbrown::HashMap;
use linker_utils::elf::DynamicRelocationKind;
use linker_utils::elf::RelocationKind;
use linker_utils::elf::RelocationKindInfo;
use linker_utils::elf::RelocationSize;
use linker_utils::elf::SectionFlags;
use linker_utils::elf::secnames::DEBUG_LOC_SECTION_NAME;
use linker_utils::elf::secnames::DEBUG_RANGES_SECTION_NAME;
use linker_utils::loongarch64::highest_relocation_with_bias;
use linker_utils::relaxation::RelocationModifier;
use linker_utils::relaxation::SectionRelaxDeltas;
use linker_utils::relaxation::opt_input_to_output;
use linker_utils::utils::slice_from_all_bytes_mut;
use object::LittleEndian;
use object::SymbolIndex;
use object::read::elf::SectionHeader as _;
use object::read::elf::Sym as _;
use std::fmt::Display;
use std::iter;
use std::marker::PhantomData;
use std::ops::BitAnd;
use std::ops::Sub;
use std::sync::atomic::Ordering::Relaxed;
use zerocopy::FromBytes;

/// A cache for managing ELF relocations and optimization of relocation entries.
#[derive(Debug)]
pub(crate) struct RelocationCache<R> {
    /// The last relocation entry processed, used to optimize consecutive relocations.
    pub(crate) previous: Option<R>,
    /// A cache mapping symbol addresses to their relocation entries, optimizing
    /// lookups for relocations involving the high parts of address.
    pub(crate) high_part_symbols: HashMap<u64, R>,
}

pub(crate) fn write_rela_sections<'data, C: ElfClass>(
    object: &ObjectLayout<'data, elf::Elf<C>>,
    buffers: &mut OutputSectionPartMap<&mut [u8]>,
    layout: &ElfLayout<'data, C>,
    sym_index_map: &[Option<u32>],
) -> Result {
    let e = LittleEndian;

    for (sec_idx, header) in object.object.enumerate_sections() {
        let section_name = object.object.section_name(sec_idx).unwrap_or_default();
        if !section_name.starts_with(b".rela") && !section_name.starts_with(b".crel") {
            continue;
        }

        let part_id = object.section_part_id(sec_idx, &layout.symbol_db.section_part_ids);
        if part_id == crate::part_id::UNMAPPED {
            continue;
        }
        if !matches!(
            object.sections.get(sec_idx.0),
            Some(SectionSlot::Loaded(_) | SectionSlot::LoadedDebugInfo(_))
        ) {
            continue;
        }

        let target_sec_idx = object::SectionIndex(header.sh_info(e) as usize);
        let section_address = object.section_resolutions[target_sec_idx.0]
            .address()
            .unwrap_or(0);

        let relocations = object.relocations(target_sec_idx).with_context(|| {
            format!(
                "Failed to get relocations from rela section {:?} in {}",
                SectionName(section_name),
                object.input
            )
        })?;

        let num_rela = relocations.num_relocations();
        if num_rela == 0 {
            continue;
        }

        let num_bytes = num_rela * C::RELA_ENTRY_SIZE as usize;
        let part_buf = buffers.get_mut(part_id);
        let out_buf = part_buf
            .split_off_mut(..num_bytes)
            .with_context(|| format!("Insufficient buffer for rela section {sec_idx:?}"))?;
        let out_relas: &mut [elf::Rela<C>] = slice_from_all_bytes_mut(out_buf);
        let mut rela_iter = out_relas.iter_mut();

        let mut write_one = |offset: u64,
                             sym: Option<SymbolIndex>,
                             r_type: object::elf::RelocationType,
                             addend: i64| {
            let Some(out) = rela_iter.next() else {
                return Ok(());
            };
            let sym_idx = sym
                .and_then(|s| {
                    let symbol_id = object.symbol_id_range.input_to_id(s);
                    if let Some(idx) = sym_index_map.get(symbol_id.as_usize()).copied().flatten() {
                        return Some(idx);
                    }
                    let canonical_id = layout.symbol_db.definition(symbol_id);
                    sym_index_map
                        .get(canonical_id.as_usize())
                        .copied()
                        .flatten()
                })
                .unwrap_or(0);
            let addend = sym
                .and_then(|s| {
                    let sym_entry = object.object.symbol(s).ok()?;
                    if sym_entry.st_type() != object::elf::STT_SECTION {
                        return None;
                    }
                    let sec_idx = object.object.symbol_section(sym_entry, s).ok()??;
                    object.section_resolutions[sec_idx.0].address()
                })
                .map_or(addend, |offset| addend + offset as i64);
            let output_offset =
                opt_input_to_output(object.section_relax_deltas.get(target_sec_idx.0), offset);
            out.set_offset(section_address + output_offset)?;
            out.set_addend(addend)?;
            out.set_info(sym_idx, r_type)?;
            Ok::<_, error::Error>(())
        };

        match relocations {
            elf::RelocationList::Rela(relas) => {
                for raw in relas {
                    let rel: elf::ElfRela<C> = elf::ElfRela::new(*raw);
                    write_one(rel.offset(), rel.symbol(), rel.raw_type(), rel.addend())?;
                }
            }
            elf::RelocationList::Crel(crel) => {
                for raw in crel.flatten() {
                    let rel: elf::ElfCrel<C> = elf::ElfCrel::new(raw);
                    write_one(rel.offset(), rel.symbol(), rel.raw_type(), rel.addend())?;
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn apply_relocations<
    'data,
    C: ElfClass,
    A: Arch<Platform = elf::Elf<C>>,
    R: Relocation<Platform = elf::Elf<C>>,
    I: Iterator<Item = object::Result<R>> + Clone,
>(
    object: &ObjectLayout<'data, elf::Elf<C>>,
    out: &mut [u8],
    section_index: object::SectionIndex,
    mut relocations: I,
    layout: &ElfLayout<'data, C>,
    table_writer: &mut TableWriter<'_, '_, C>,
    trace: &TraceOutput,
) -> Result {
    let section_address = object.section_resolutions[section_index.0]
        .address()
        .context("Attempted to apply relocations to a section that we didn't load")?;
    let object_section = object.object.section(section_index)?;
    let section_flags = object_section.sh_flags(LittleEndian);
    let section_info = SectionInfo {
        section_address,
        is_writable: object_section.is_writable(),
        section_flags,
        part_id: object.section_part_id(section_index, &layout.symbol_db.section_part_ids),
    };
    let mut modifier = RelocationModifier::Normal;

    let mut relocation_count = 0;
    let mut relocation_cache = RelocationCache::<R>::default();
    let relax_deltas = object.section_relax_deltas.get(section_index.0);
    let mut relax_cursor = relax_deltas.map(|deltas| deltas.cursor());

    while let Some(rel) = relocations.next() {
        let rel = rel?;
        relocation_count += 1;
        if A::high_part_relocations().contains(&rel.raw_type()) {
            let cache_offset = opt_input_to_output(relax_deltas, rel.offset());
            relocation_cache.high_part_symbols.insert(cache_offset, rel);
        }

        if modifier == RelocationModifier::SkipNextRelocation {
            modifier = RelocationModifier::Normal;
            relocation_cache.previous = Some(rel);
            continue;
        }

        // When relaxation deltas are present, translate the relocation's input
        // offset to the corresponding output offset so that it points to the
        // correct position in the (compacted) output buffer.
        let offset_in_section = match relax_cursor.as_mut() {
            Some(cursor) => cursor.translate(rel.offset()),
            None => rel.offset(),
        };

        if layout.args().common().incremental
            && let Some(sym) = rel.symbol()
        {
            layout.record_reverse_reloc(
                object.symbol_id_range.input_to_id(sym),
                reloc_file_offset(layout, section_info, offset_in_section),
                section_address.wrapping_add(offset_in_section),
                rel.addend(),
                rel.raw_type().0,
                object.file_id,
            );
        }

        modifier = apply_relocation::<C, A, R, _>(
            object,
            offset_in_section,
            &rel,
            section_info,
            layout,
            out,
            table_writer,
            trace,
            &relocation_cache,
            &relocations,
            relax_deltas,
        )
        .with_context(|| {
            format!(
                "Failed to apply {} at offset 0x{offset_in_section:x}",
                display_relocation::<C, A, R>(object, &rel, layout)
            )
        })?;
        relocation_cache.previous = Some(rel);
    }

    layout
        .relocation_statistics
        .get(
            object
                .section_part_id(section_index, &layout.symbol_db.section_part_ids)
                .output_section_id::<elf::Elf<C>>(),
        )
        .fetch_add(relocation_count, Relaxed);
    Ok(())
}

pub(crate) fn apply_debug_relocations<
    'data,
    C: ElfClass,
    A: Arch<Platform = elf::Elf<C>>,
    R: Relocation<Platform = elf::Elf<C>>,
    I: Iterator<Item = object::Result<R>> + Clone,
>(
    object: &ObjectLayout<'data, elf::Elf<C>>,
    out: &mut [u8],
    section_index: object::SectionIndex,
    relocations: I,
    layout: &ElfLayout<'data, C>,
) -> Result {
    let section_name = object.object.section_name(section_index)?;

    // TODO: Starting with DWARF 6, the tombstone value will be defined as -1 and -2.
    // However, the change is premature as consumers of the DWARF format don't fully support
    // the new tombstone values.
    //
    // Link: https://dwarfstd.org/issues/200609.1.html
    let tombstone_value: u64 =
        if section_name == DEBUG_LOC_SECTION_NAME || section_name == DEBUG_RANGES_SECTION_NAME {
            // These sections use zero as a list terminator.
            1
        } else {
            0
        };

    let mut relocation_count = 0;
    let mut relocation_cache = RelocationCache::default();

    for rel in relocations {
        relocation_count += 1;
        let rel = rel?;
        let offset_in_section = rel.offset();
        apply_debug_relocation::<C, A, R>(
            object,
            offset_in_section,
            &rel,
            layout,
            tombstone_value,
            out,
            &relocation_cache,
        )
        .with_context(|| {
            format!(
                "Failed to apply {} at offset 0x{offset_in_section:x}",
                display_relocation::<C, A, R>(object, &rel, layout)
            )
        })?;
        relocation_cache.previous = Some(rel);
    }
    layout
        .relocation_statistics
        .get(
            object
                .section_part_id(section_index, &layout.symbol_db.section_part_ids)
                .output_section_id::<elf::Elf<C>>(),
        )
        .fetch_add(relocation_count, Relaxed);
    Ok(())
}

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

pub(crate) fn display_relocation<
    'a,
    'data,
    C: ElfClass,
    A: Arch<Platform = elf::Elf<C>>,
    R: Relocation,
>(
    object: &'a ObjectLayout<'data, elf::Elf<C>>,
    rel: &'a R,
    layout: &'a ElfLayout<'data, C>,
) -> DisplayRelocation<'a, 'data, C, A, R> {
    DisplayRelocation::<'a, 'data, C, A, R> {
        rel,
        symbol_db: &layout.symbol_db,
        per_symbol_flags: &layout.per_symbol_flags,
        object,
        phantom: PhantomData,
    }
}

pub(crate) struct DisplayRelocation<
    'a,
    'data,
    C: ElfClass,
    A: Arch<Platform = elf::Elf<C>>,
    R: Relocation,
> {
    pub(crate) rel: &'a R,
    pub(crate) symbol_db: &'a SymbolDb<'data, elf::Elf<C>>,
    pub(crate) per_symbol_flags: &'a PerSymbolFlags,
    pub(crate) object: &'a ObjectLayout<'data, elf::Elf<C>>,
    pub(crate) phantom: PhantomData<A>,
}

impl<'a, 'data, C: ElfClass, A: Arch<Platform = elf::Elf<C>>, R: Relocation<Platform = elf::Elf<C>>>
    Display for DisplayRelocation<'a, 'data, C, A, R>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "relocation of type {} to ",
            A::rel_type_to_string(self.rel.raw_type())
        )?;
        match self.rel.symbol() {
            None => write!(f, "absolute")?,
            Some(local_symbol_index) => {
                let symbol_id = self.object.symbol_id_range.input_to_id(local_symbol_index);
                write!(
                    f,
                    "{}",
                    self.symbol_db
                        .symbol_debug(self.per_symbol_flags, symbol_id)
                )?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub(crate) struct SectionInfo<S: platform::SectionFlags> {
    pub(crate) section_address: u64,
    pub(crate) is_writable: bool,
    pub(crate) section_flags: S,
    pub(crate) part_id: PartId,
}

pub(crate) fn reloc_file_offset<C: ElfClass, S: platform::SectionFlags>(
    layout: &ElfLayout<C>,
    section_info: SectionInfo<S>,
    offset_in_section: u64,
) -> u64 {
    let rec = layout.section_part_layouts.get(section_info.part_id);
    rec.file_offset as u64
        + section_info.section_address.wrapping_sub(rec.mem_offset)
        + offset_in_section
}

pub(crate) fn get_resolution<'data, C: ElfClass, R: Relocation>(
    rel: &R,
    object_layout: &ObjectLayout<'data, elf::Elf<C>>,
    layout: &ElfLayout<C>,
) -> Result<(Resolution<elf::Elf<C>>, SymbolIndex, SymbolId)> {
    let symbol_index = rel.symbol().context("Unsupported absolute relocation")?;
    let local_symbol_id = object_layout.symbol_id_range.input_to_id(symbol_index);
    let sym = object_layout.object.symbol(symbol_index)?;
    let section_index = object_layout.object.symbol_section(sym, symbol_index)?;
    let resolution = layout
        .merged_symbol_resolution(local_symbol_id)
        .or_else(|| {
            section_index.and_then(|section_index| {
                let section_address =
                    object_layout.section_resolutions[section_index.0].address()?;
                let output_offset = opt_input_to_output(
                    object_layout.section_relax_deltas.get(section_index.0),
                    crate::platform::Symbol::value(sym),
                );

                Some(Resolution {
                    raw_value: section_address + output_offset,
                    dynamic_symbol_index: None,
                    flags: ValueFlags::empty(),
                    format_specific: Default::default(),
                })
            })
        })
        .with_context(|| {
            format!(
                "Missing resolution for: {}",
                layout.symbol_debug(local_symbol_id)
            )
        })?;
    Ok((resolution, symbol_index, local_symbol_id))
}

/// Returns the `st_other` byte of the canonical definition of `symbol_id`, or 0 if it isn't
/// defined by a regular object. Used for ppc64 local-entry-point computation.
pub(crate) fn callee_st_other<C: ElfClass>(layout: &ElfLayout<C>, symbol_id: SymbolId) -> u8 {
    let canonical = layout.symbol_db.definition(symbol_id);
    let file_id = layout.symbol_db.file_id_for_symbol(canonical);
    if let FileLayout::Object(obj) = layout.file_layout(file_id)
        && let Ok(sym) = obj.object.symbol(canonical.to_input(obj.symbol_id_range))
    {
        return sym.st_other().0;
    }
    0
}

#[inline(always)]
pub(crate) fn get_pair_subtraction_relocation_value<
    'data,
    C: ElfClass,
    A: Arch<Platform = elf::Elf<C>>,
    R: Relocation<Platform = elf::Elf<C>>,
>(
    object_layout: &ObjectLayout<'data, elf::Elf<C>>,
    rel: &R,
    layout: &ElfLayout<C>,
    resolution: Resolution<elf::Elf<C>>,
    symbol_index: SymbolIndex,
    addend: i64,
    set_rel: &R,
    expected_r_type: object::elf::RelocationType,
) -> Result<u64> {
    ensure!(
        set_rel.offset() == rel.offset(),
        "PairSubtractionULEB128 relocation must have equal offset"
    );
    ensure!(
        set_rel.raw_type() == expected_r_type,
        "unexpected previous relocation: expected: {}, was: {}",
        A::rel_type_to_string(expected_r_type),
        A::rel_type_to_string(set_rel.raw_type())
    );
    let (set_resolution, set_symbol_index, _) = get_resolution(set_rel, object_layout, layout)?;

    let set_resolution_val = set_resolution.value_with_addend(
        set_rel.addend(),
        set_symbol_index,
        object_layout,
        &layout.symbol_db.section_part_ids,
        &layout.merged_strings,
        &layout.merged_string_start_addresses,
    )?;
    let sub_resolution_val = resolution.value_with_addend(
        addend,
        symbol_index,
        object_layout,
        &layout.symbol_db.section_part_ids,
        &layout.merged_strings,
        &layout.merged_string_start_addresses,
    )?;
    Ok(set_resolution_val.wrapping_sub(sub_resolution_val))
}

/// Applies the relocation `rel` at `offset_in_section`, where the section bytes are `out`. See "ELF
/// Handling For Thread-Local Storage" for details about some of the TLS-related relocations and
/// transformations that are applied.
#[inline(always)]
pub(crate) fn apply_relocation<
    'data,
    C: ElfClass,
    A: Arch<Platform = elf::Elf<C>>,
    R: Relocation<Platform = elf::Elf<C>>,
    I: Iterator<Item = object::Result<R>> + Clone,
>(
    object_layout: &ObjectLayout<'data, elf::Elf<C>>,
    mut offset_in_section: u64,
    rel: &R,
    section_info: SectionInfo<linker_utils::elf::SectionFlags>,
    layout: &ElfLayout<'data, C>,
    out: &mut [u8],
    table_writer: &mut TableWriter<'_, '_, C>,
    trace: &TraceOutput,
    relocation_cache: &RelocationCache<R>,
    relocation_iterator: &I,
    relax_deltas: Option<&SectionRelaxDeltas>,
) -> Result<RelocationModifier> {
    let section_address = section_info.section_address;
    let original_place = section_address + offset_in_section;
    let _span = tracing::trace_span!(
        "relocation",
        address = original_place,
        address_hex = %HexU64::new(original_place)
    )
    .entered();

    let r_type = rel.raw_type();
    let mut addend = rel.addend();

    match A::relocation_from_raw(r_type)?.kind {
        RelocationKind::None => return Ok(RelocationModifier::Normal),
        RelocationKind::Alignment => {
            let addend = addend as u64;
            let removed_bytes =
                relax_deltas.map_or(0u64, |d| u64::from(d.delta_bytes_at(rel.offset())));
            let padding_bytes = addend.saturating_sub(removed_bytes) as usize;
            let offset_in_section = offset_in_section as usize;
            A::fill_nop_padding(&mut out[offset_in_section..offset_in_section + padding_bytes]);

            return Ok(RelocationModifier::Normal);
        }
        RelocationKind::Relative if rel.symbol().is_none() => {
            if layout.symbol_db.output_kind.is_position_independent() {
                bail!(
                    "relocation of type {} to absolute address cannot be used in \
                    position-independent output; recompile with -fPIC",
                    rel.raw_type()
                );
            }
            let place = section_info.section_address + offset_in_section;
            let value = (rel.addend() as u64).wrapping_sub(place);
            A::relocation_from_raw(r_type)?
                .write_to_buffer(value, &mut out[offset_in_section as usize..])?;
            return Ok(RelocationModifier::Normal);
        }
        RelocationKind::Absolute
            if layout.symbol_db.output_kind.is_shared_object()
                && A::is_disallowed_in_shared_object(r_type) =>
        {
            bail!(
                "relocation type {} cannot be used when making a shared object; \
                 recompile with -fPIC",
                A::rel_type_to_string(r_type)
            );
        }
        _ => {}
    }
    let (resolution, symbol_index, local_symbol_id) = get_resolution(rel, object_layout, layout)?;
    let flags = layout.flags_for_symbol(local_symbol_id);
    if layout.symbol_db.output_kind.is_position_independent()
        && (flags.is_interposable() || flags.is_dynamic())
        && !flags.needs_copy_relocation()
        && !flags.needs_plt()
        && A::is_disallowed_for_interposable_symbols(r_type)
    {
        bail!(
            "relocation {} cannot be used against symbol; recompile with -fPIC",
            A::rel_type_to_string(r_type)
        );
    }
    let mut next_modifier = RelocationModifier::Normal;
    let rel_info;
    let output_kind = layout.symbol_db.output_kind;

    let relaxation = A::new_relaxation(
        r_type,
        out,
        offset_in_section,
        flags,
        output_kind,
        section_info.section_flags,
        relax_deltas,
        resolution.raw_value,
        section_address,
        rel.addend(),
        relocation_cache
            .previous
            .as_ref()
            .filter(|r| r.symbol() == rel.symbol())
            .map(|r| PreviousRelocationInfo {
                kind: r.raw_type(),
                offset: r.offset(),
                symbol: r.symbol(),
                addend: r.addend(),
            }),
    )
    .filter(|relaxation| layout.args().relax || relaxation.is_mandatory());

    if let Some(relaxation) = &relaxation {
        rel_info = relaxation.rel_info();
        relaxation.apply(out, &mut offset_in_section, &mut addend);
        next_modifier = relaxation.next_modifier();
    } else {
        rel_info = A::relocation_from_raw(r_type)?;
    }

    // Compute place to which IP-relative relocations will be relative. This is different to
    // `original_place` in that our `offset_in_section` may have been adjusted by a relaxation.
    let place = section_address + offset_in_section;

    let mask = get_page_mask(rel_info.mask);
    let bias = rel_info.bias;
    // For ppc64 calls, branch to the callee's local entry point (we share its TOC, so the global
    // entry's r2 setup is unnecessary). Zero for every other architecture and relocation.
    let branch_local_entry = if rel_info.size.is_ppc64_branch() {
        A::local_entry_offset(callee_st_other(layout, local_symbol_id))
    } else {
        0
    };
    let mut value = match rel_info.kind {
        RelocationKind::Absolute => write_absolute_relocation::<C, A>(
            table_writer,
            resolution,
            place,
            addend,
            section_info,
            symbol_index,
            object_layout,
            layout,
        )?,
        RelocationKind::AbsoluteSet
        | RelocationKind::AbsoluteSetWord6
        | RelocationKind::AbsoluteAddition
        | RelocationKind::AbsoluteAdditionWord6
        | RelocationKind::AbsoluteSubtraction
        | RelocationKind::AbsoluteSubtractionWord6 => resolution.value_with_addend(
            addend,
            symbol_index,
            object_layout,
            &layout.symbol_db.section_part_ids,
            &layout.merged_strings,
            &layout.merged_string_start_addresses,
        )?,
        RelocationKind::AbsoluteLowPart => resolution
            .value_with_addend(
                addend,
                symbol_index,
                object_layout,
                &layout.symbol_db.section_part_ids,
                &layout.merged_strings,
                &layout.merged_string_start_addresses,
            )?
            .bitand(mask.symbol_plus_addend),
        RelocationKind::Relative => resolution
            .value_with_addend(
                addend,
                symbol_index,
                object_layout,
                &layout.symbol_db.section_part_ids,
                &layout.merged_strings,
                &layout.merged_string_start_addresses,
            )?
            .wrapping_add(branch_local_entry)
            .wrapping_add(bias)
            .bitand(mask.symbol_plus_addend)
            .wrapping_sub(place.bitand(mask.place)),
        RelocationKind::RelativeLoongArchHigh => highest_relocation_with_bias(
            resolution.value_with_addend(
                addend,
                symbol_index,
                object_layout,
                &layout.symbol_db.section_part_ids,
                &layout.merged_strings,
                &layout.merged_string_start_addresses,
            )?,
            place,
        ),
        RelocationKind::RelativeRiscVLow12 => {
            // The iterator is used for e.g. R_RISCV_PCREL_HI20 & R_RISCV_PCREL_LO12_I pair of
            // relocations where the later one actually points to a label of the HI20
            // relocations and thus we need to find it. The relocation is typically
            // right before the LO12_* relocation.
            ensure!(
                addend == 0,
                "Unexpected addend for R_RISCV_PCREL_LO12 relocation"
            );
            let hi_offset_in_section = resolution
                .value_with_addend(
                    addend,
                    symbol_index,
                    object_layout,
                    &layout.symbol_db.section_part_ids,
                    &layout.merged_strings,
                    &layout.merged_string_start_addresses,
                )?
                .wrapping_sub(section_address);
            let hi_rel = relocation_cache
                .high_part_symbols
                .get(&hi_offset_in_section)
                .copied()
                .or_else(|| {
                    // It's very unlikely that a high part follows the low part:
                    relocation_iterator.clone().find_map(|r| {
                        if let Ok(r) = r
                            && A::high_part_relocations().contains(&r.raw_type())
                        {
                            let r_output_offset = opt_input_to_output(relax_deltas, r.offset());
                            if r_output_offset == hi_offset_in_section {
                                return Some(r);
                            }
                        }
                        None
                    })
                })
                .context("Missing High relocation connected with R_RISCV_PCREL_LO12")?;

            let hi_rel_info = A::relocation_from_raw(hi_rel.raw_type())?;
            let addend = hi_rel.addend();
            let (resolution, symbol_index, _) = get_resolution(&hi_rel, object_layout, layout)
                .with_context(|| {
                    "Missing High resolution connected to R_RISCV_PCREL_LO12".to_string()
                })?;
            let place = section_address + hi_offset_in_section;

            // Only a subset of relocations is referenced by R_RISCV_PCREL_LO12 relocations.
            match hi_rel_info.kind {
                RelocationKind::Relative => resolution
                    .value_with_addend(
                        addend,
                        symbol_index,
                        object_layout,
                        &layout.symbol_db.section_part_ids
                            [object_layout.section_id_range.as_usize()],
                        &layout.merged_strings,
                        &layout.merged_string_start_addresses,
                    )?
                    .wrapping_add(bias)
                    .wrapping_sub(place),
                RelocationKind::GotRelative => resolution
                    .got_address_for_relocation()?
                    .wrapping_add(addend as u64)
                    .wrapping_add(bias)
                    .wrapping_sub(place),
                RelocationKind::TlsGd => resolution
                    .tlsgd_got_address()?
                    .wrapping_add(addend as u64)
                    .wrapping_add(bias)
                    .wrapping_sub(place),
                RelocationKind::TlsLd => layout
                    .prelude()
                    .format_specific
                    .tlsld_got_entry
                    .unwrap()
                    .get()
                    .wrapping_add(addend as u64)
                    .wrapping_add(bias)
                    .wrapping_sub(place),
                RelocationKind::GotTpOff => resolution
                    .got_address()?
                    .wrapping_add(addend as u64)
                    .wrapping_add(bias)
                    .wrapping_sub(place),
                _ => bail!(
                    "Unsupported high part relocation {:?} connected with R_RISCV_PCREL_LO12",
                    hi_rel_info.kind
                ),
            }
        }
        RelocationKind::PairSubtractionULEB128(expected_r_type) => {
            get_pair_subtraction_relocation_value::<C, A, R>(
                object_layout,
                rel,
                layout,
                resolution,
                symbol_index,
                addend,
                // It must be the previous relocation
                &relocation_cache.previous.with_context(|| {
                    "Missing previous relocation for PairSubtractionULEB128".to_owned()
                })?,
                expected_r_type,
            )?
        }
        RelocationKind::GotRelative => resolution
            .got_address_for_relocation()?
            .wrapping_add(bias)
            .wrapping_add(addend as u64)
            .bitand(mask.got_entry)
            .wrapping_sub(place.bitand(mask.place)),
        RelocationKind::GotRelativeLoongArch64 => highest_relocation_with_bias(
            resolution
                .got_address_for_relocation()?
                .wrapping_add(addend as u64),
            place,
        ),
        RelocationKind::GotRelGotBase => resolution
            .got_address_for_relocation()?
            .wrapping_add(addend as u64)
            .wrapping_add(bias)
            .bitand(mask.got_entry)
            .wrapping_sub(layout.got_base().bitand(mask.got)),
        RelocationKind::Got => {
            // The LoongArch64 psABI does not provide a separate GOT Low part relocation for the
            // TLSGD relocation. So we need to distinguish between a classical GOT
            // slot and one corresponding to TLSGD.
            //
            // Note: TLSLD is unsupported by the target (https://github.com/loongson/la-abi-specs/issues/19).
            if resolution.flags.needs_got_tls_module() {
                resolution.tlsgd_got_address()?
            } else {
                resolution.got_address_for_relocation()?
            }
            .wrapping_add(bias)
            .bitand(mask.got_entry)
        }
        RelocationKind::SymRelGotBase => resolution
            .value_with_addend(
                addend,
                symbol_index,
                object_layout,
                &layout.symbol_db.section_part_ids,
                &layout.merged_strings,
                &layout.merged_string_start_addresses,
            )?
            .wrapping_add(bias)
            .bitand(mask.symbol_plus_addend)
            .wrapping_sub(layout.got_base().bitand(mask.got)),
        RelocationKind::PltRelGotBase => resolution
            .plt_address()?
            .wrapping_add(bias)
            .wrapping_sub(layout.got_base().bitand(mask.got)),
        RelocationKind::PltRelative => resolution
            .plt_address()?
            .wrapping_add(addend as u64)
            .wrapping_add(bias)
            .wrapping_sub(place.bitand(mask.place)),
        // TLS-related relocations
        RelocationKind::TlsGd => resolution
            .tlsgd_got_address()?
            .wrapping_add(addend as u64)
            .wrapping_add(bias)
            .bitand(mask.got_entry)
            .wrapping_sub(place.bitand(mask.place)),
        RelocationKind::TlsGdGot => resolution
            .tlsgd_got_address()?
            .wrapping_add(addend as u64)
            .wrapping_add(bias)
            .bitand(mask.got_entry),
        RelocationKind::TlsGdGotBase => resolution
            .tlsgd_got_address()?
            .wrapping_add(addend as u64)
            .wrapping_add(bias)
            .bitand(mask.got_entry)
            .wrapping_sub(layout.got_base().bitand(mask.got)),
        RelocationKind::TlsLd => layout
            .prelude()
            .format_specific
            .tlsld_got_entry
            .unwrap()
            .get()
            .wrapping_add(addend as u64)
            .wrapping_add(bias)
            .bitand(mask.got_entry)
            .wrapping_sub(place.bitand(mask.place)),
        RelocationKind::TlsLdGot => layout
            .prelude()
            .format_specific
            .tlsld_got_entry
            .unwrap()
            .get()
            .wrapping_add(addend as u64)
            .wrapping_add(bias)
            .bitand(mask.got_entry),
        RelocationKind::TlsLdGotBase => layout
            .prelude()
            .format_specific
            .tlsld_got_entry
            .unwrap()
            .get()
            .wrapping_add(addend as u64)
            .wrapping_add(bias)
            .bitand(mask.got_entry)
            .wrapping_sub(layout.got_base().bitand(mask.got)),
        RelocationKind::DtpOff if output_kind == OutputKind::SharedObject => resolution
            .value()
            .wrapping_add(addend as u64)
            .wrapping_add(bias)
            .sub(layout.tls_start_address()),
        RelocationKind::DtpOff => resolution
            .value()
            .wrapping_add(addend as u64)
            .wrapping_add(bias)
            .wrapping_sub(layout.tls_end_address()),
        RelocationKind::GotTpOff => resolution
            .got_address()?
            .wrapping_add(addend as u64)
            .wrapping_add(bias)
            .bitand(mask.got_entry)
            .wrapping_sub(place.bitand(mask.place)),
        RelocationKind::GotTpOffLoongArch64 => highest_relocation_with_bias(
            resolution.got_address()?.wrapping_add(addend as u64),
            place,
        ),
        RelocationKind::GotTpOffGot => resolution
            .got_address()?
            .wrapping_add(addend as u64)
            .wrapping_add(bias)
            .bitand(mask.got_entry),
        RelocationKind::GotTpOffGotBase => resolution
            .got_address()?
            .wrapping_add(addend as u64)
            .wrapping_add(bias)
            .bitand(mask.got_entry)
            .wrapping_sub(layout.got_base().bitand(mask.got)),
        RelocationKind::TpOff
            if layout
                .symbol_db
                .is_undefined(layout.symbol_db.definition(local_symbol_id)) =>
        {
            // An undefined weak TLS symbol has no offset within the TLS block, so we somewhat
            // arbitrarily give the 0 offset which at least some other linkers also do and most
            // importantly is a value guaranteed to fit within the range of any relocation.
            (addend as u64).wrapping_add(bias)
        }
        RelocationKind::TpOff => resolution
            .value()
            .wrapping_add(addend as u64)
            .wrapping_add(bias)
            .wrapping_sub(A::tp_offset_start(layout)),
        RelocationKind::TlsDesc => resolution
            .tls_descriptor_got_address()?
            .wrapping_add(addend as u64)
            .wrapping_add(bias)
            .bitand(mask.got_entry)
            .wrapping_sub(place.bitand(mask.place)),
        RelocationKind::TlsDescLoongArch64 => highest_relocation_with_bias(
            resolution
                .tls_descriptor_got_address()?
                .wrapping_add(addend as u64),
            place,
        ),
        RelocationKind::TlsDescGot => resolution
            .tls_descriptor_got_address()?
            .wrapping_add(addend as u64)
            .wrapping_add(bias)
            .bitand(mask.got_entry),
        RelocationKind::TlsDescGotBase => resolution
            .tls_descriptor_got_address()?
            .wrapping_add(addend as u64)
            .wrapping_add(bias)
            .bitand(mask.got_entry)
            .wrapping_sub(layout.got_base().bitand(mask.got)),
        RelocationKind::None | RelocationKind::TlsDescCall => 0,
        RelocationKind::Alignment => unreachable!(),
    };

    let offset_in_section = offset_in_section as usize;

    // Handle addition and subtraction relocation kinds.
    if matches!(
        rel_info.kind,
        RelocationKind::AbsoluteAddition
            | RelocationKind::AbsoluteAdditionWord6
            | RelocationKind::AbsoluteSubtraction
            | RelocationKind::AbsoluteSetWord6
            | RelocationKind::AbsoluteSubtractionWord6
    ) {
        value = rel_info.adjust_value_based_on_content(value, out, offset_in_section)?;
    }

    if let Some(relaxation) = relaxation {
        trace.emit(original_place, || {
            format!(
                "relaxation applied relaxation={kind:?}, flags={flags},\n\
                rel_kind={rel_kind:?},\n\
                value=0x{value:x}, symbol_name={symbol_name}",
                kind = relaxation.debug_kind(),
                rel_kind = rel_info.kind,
                symbol_name = layout.symbol_db.symbol_name_for_display(local_symbol_id),
            )
        });
        tracing::trace!(
            %flags,
            relaxation_kind = ?relaxation.debug_kind(),
            ?rel_info.kind,
            %rel_info.size,
            value,
            value_hex = %HexU64::new(value),
            symbol_name = %layout.symbol_db.symbol_name_for_display(local_symbol_id),
            "relaxation applied");
    } else {
        trace.emit(original_place, || {
            format!(
                "relocation applied flags={flags},\n\
                rel_kind={rel_kind:?},\n\
                value=0x{value:x}, symbol_name={symbol_name}",
                rel_kind = rel_info.kind,
                symbol_name = layout.symbol_db.symbol_name_for_display(local_symbol_id),
            )
        });
        tracing::trace!(
            %flags,
            ?rel_info.kind,
            %rel_info.size,
            value,
            value_hex = %HexU64::new(value),
            symbol_name = %layout.symbol_db.symbol_name_for_display(local_symbol_id),
            "relocation applied");
    }

    if let Some(thunked_value) = maybe_get_thunk_for_relocation::<C, A>(
        object_layout,
        section_info,
        layout,
        rel_info,
        local_symbol_id,
        place,
        value,
    )? {
        value = thunked_value;
    }

    rel_info.write_to_buffer(value, &mut out[offset_in_section..])?;

    Ok(next_modifier)
}

/// Checks if we need to use a thunk for a relocation and if we do, return the value to use for the
/// thunk.
pub(crate) fn maybe_get_thunk_for_relocation<C: ElfClass, A: Arch<Platform = elf::Elf<C>>>(
    object_layout: &ObjectLayout<elf::Elf<C>>,
    section_info: SectionInfo<SectionFlags>,
    layout: &Layout<elf::Elf<C>>,
    rel_info: RelocationKindInfo,
    local_symbol_id: SymbolId,
    place: u64,
    value: u64,
) -> Result<Option<u64>> {
    let Some(config) = A::thunk_config() else {
        return Ok(None);
    };

    if !rel_info.thunkable {
        return Ok(None);
    }

    if rel_info.range.contains(value as i64) {
        return Ok(None);
    }

    let canonical_id = layout.symbol_db.definition(local_symbol_id);

    let thunk_id = if section_info.part_id == config.primary_function_part_id {
        object_layout.thunk_block_id
    } else {
        ThunkBlockId::FIRST
    };

    let thunk_address_opt = layout
        .thunk_block_addresses
        .get(thunk_id.as_usize())
        .and_then(|m| m.get(&canonical_id))
        .copied();

    if let Some(thunk_address) = thunk_address_opt {
        if thunk_address == 0 {
            bail!(
                "Thunk address not yet allocated for symbol {}",
                layout.symbol_db.symbol_name_for_display(local_symbol_id)
            );
        }

        let mask = get_page_mask(rel_info.mask);
        let new_value = thunk_address
            .wrapping_add(rel_info.bias)
            .bitand(mask.symbol_plus_addend)
            .wrapping_sub(place.bitand(mask.place));

        tracing::trace!(
            old_value = value,
            new_value,
            thunk_address,
            "Using thunk instead of out-of-range branch"
        );

        return Ok(Some(new_value));
    }

    bail!(
        "Branch relocation out of range by {over} for symbol {sym} \
         but no thunk allocated. Part: {part}. Offset: {offset}",
        over = rel_info.range.overrun(value as i64),
        sym = layout.symbol_db.symbol_name_for_display(local_symbol_id),
        part = layout.output_sections.part_debug(section_info.part_id),
        offset = value as i64,
    );
}

pub(crate) fn apply_debug_relocation<
    'data,
    C: ElfClass,
    A: Arch<Platform = elf::Elf<C>>,
    R: Relocation<Platform = elf::Elf<C>>,
>(
    object_layout: &ObjectLayout<'data, elf::Elf<C>>,
    offset_in_section: u64,
    rel: &R,
    layout: &ElfLayout<C>,
    section_tombstone_value: u64,
    out: &mut [u8],
    relocation_cache: &RelocationCache<R>,
) -> Result<()> {
    let symbol_index = rel.symbol().context("Unsupported absolute relocation")?;
    let sym = object_layout.object.symbol(symbol_index)?;
    let section_index = object_layout.object.symbol_section(sym, symbol_index)?;

    let addend = rel.addend();
    let r_type = rel.raw_type();
    let rel_info = A::relocation_from_raw(r_type)?;

    let resolution = layout
        .merged_symbol_resolution(object_layout.symbol_id_range.input_to_id(symbol_index))
        .or_else(|| {
            section_index.and_then(|section_index| {
                let section_address =
                    object_layout.section_resolutions[section_index.0].address()?;
                // Include the symbol's offset within the section (adjusted for any relaxation
                // deltas). This is necessary on architectures like RISC-V and LoongArch64 where
                // debug info references local symbols (e.g. .LFB0, .LFE0) whose value is their
                // offset within the section, rather than section symbols where the offset is
                // encoded in the relocation addend.
                let output_offset = opt_input_to_output(
                    object_layout.section_relax_deltas.get(section_index.0),
                    crate::platform::Symbol::value(sym),
                );

                Some(Resolution {
                    raw_value: section_address + output_offset,
                    dynamic_symbol_index: None,
                    flags: ValueFlags::empty(),
                    format_specific: Default::default(),
                })
            })
        });

    let value = if let Some(resolution) = resolution {
        match rel_info.kind {
            RelocationKind::Absolute
            | RelocationKind::AbsoluteSet
            | RelocationKind::AbsoluteSetWord6
            | RelocationKind::AbsoluteAddition
            | RelocationKind::AbsoluteAdditionWord6
            | RelocationKind::AbsoluteSubtraction
            | RelocationKind::AbsoluteSubtractionWord6 => {
                let mut value = resolution.value_with_addend(
                    addend,
                    symbol_index,
                    object_layout,
                    &layout.symbol_db.section_part_ids,
                    &layout.merged_strings,
                    &layout.merged_string_start_addresses,
                )?;
                // Adjust the relocation value based on the value at the place.
                if matches!(
                    rel_info.kind,
                    RelocationKind::AbsoluteAddition
                        | RelocationKind::AbsoluteSubtraction
                        | RelocationKind::AbsoluteSetWord6
                        | RelocationKind::AbsoluteSubtractionWord6
                ) {
                    value = rel_info.adjust_value_based_on_content(
                        value,
                        out,
                        offset_in_section as usize,
                    )?;
                }
                value
            }
            RelocationKind::DtpOff => resolution
                .value()
                .wrapping_sub(layout.tls_end_address())
                .wrapping_add(addend as u64),
            RelocationKind::PairSubtractionULEB128(expected_r_type) => {
                get_pair_subtraction_relocation_value::<C, A, R>(
                    object_layout,
                    rel,
                    layout,
                    resolution,
                    symbol_index,
                    addend,
                    // Must be the previous relocation.
                    &relocation_cache.previous.with_context(|| {
                        "Missing previous relocation for PairSubtractionULEB128".to_owned()
                    })?,
                    expected_r_type,
                )?
            }
            // Skip R_RISCV_SET_ULEB128
            RelocationKind::Relative if rel_info.size == RelocationSize::ByteSize(0) => 0,
            kind => bail!("Unsupported debug relocation kind {kind:?}"),
        }
    } else if let Some(section_index) = section_index {
        match object_layout.sections[section_index.0] {
            SectionSlot::MergeStrings(..) => get_merged_string_output_address::<elf::Elf<C>>(
                symbol_index,
                addend,
                object_layout.object,
                &object_layout.sections,
                &layout.symbol_db.section_part_ids,
                object_layout.section_id_range,
                &layout.merged_strings,
                &layout.merged_string_start_addresses,
                false,
            )?
            .context("Cannot get merged string offset for a debug info section")?,
            SectionSlot::Discard | SectionSlot::Unloaded(..) => section_tombstone_value,
            _ => bail!("Could not find a relocation resolution for a debug info section"),
        }
    } else {
        // Debug info can sometimes contain relocations for symbols from other objects. If we didn't
        // load those symbols, then we need to use the tombstone value. Careful, we don't have any
        // tests for this, but building chromium does trigger this branch.
        section_tombstone_value
    };

    rel_info.write_to_buffer(value, &mut out[offset_in_section as usize..])?;

    Ok(())
}

#[inline(always)]
pub(crate) fn write_absolute_relocation<'data, C: ElfClass, A: Arch<Platform = elf::Elf<C>>>(
    table_writer: &mut TableWriter<'_, '_, C>,
    resolution: Resolution<elf::Elf<C>>,
    place: u64,
    addend: i64,
    section_info: SectionInfo<<A::Platform as Platform>::SectionFlags>,
    symbol_index: object::SymbolIndex,
    object_layout: &ObjectLayout<'data, elf::Elf<C>>,
    layout: &ElfLayout<C>,
) -> Result<u64> {
    if !section_info.section_flags.is_alloc() {
        resolution.value_with_addend(
            addend,
            symbol_index,
            object_layout,
            &layout.symbol_db.section_part_ids,
            &layout.merged_strings,
            &layout.merged_string_start_addresses,
        )
    } else if resolution.flags.is_dynamic()
        && resolution.flags.is_absolute()
        && !section_info.is_writable
    {
        // Weak undefined symbol referenced from a read-only section. Fill in as zero.
        Ok(0)
    } else if resolution.flags.is_interposable() && section_info.is_writable {
        table_writer.write_dynamic_symbol_relocation::<A>(
            place,
            addend,
            resolution.dynamic_symbol_index()?,
            DynamicRelocationKind::Absolute,
        )?;

        Ok(0)
    } else if resolution.flags.is_ifunc()
        && section_info.is_writable
        && table_writer.output_kind.is_position_independent()
    {
        table_writer
            .write_ifunc_relocation_for_data::<A>(place, resolution.raw_value as i64 + addend)?;
        Ok(0)
    } else if table_writer.output_kind.is_position_independent() && !resolution.is_absolute() {
        let address = resolution.value_with_addend(
            addend,
            symbol_index,
            object_layout,
            &layout.symbol_db.section_part_ids,
            &layout.merged_strings,
            &layout.merged_string_start_addresses,
        )?;
        table_writer.write_address_relocation::<A>(place, address)
    } else {
        resolution.value_with_addend(
            addend,
            symbol_index,
            object_layout,
            &layout.symbol_db.section_part_ids,
            &layout.merged_strings,
            &layout.merged_string_start_addresses,
        )
    }
}

impl<R> Default for RelocationCache<R> {
    fn default() -> Self {
        Self {
            previous: Default::default(),
            high_part_symbols: Default::default(),
        }
    }
}
