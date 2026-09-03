use super::super::types::*;
use super::*;
use crate::elf;
use crate::elf::ElfClass;
use crate::error;
use crate::error::Context as _;
use crate::error::Result;
use crate::layout::ObjectLayout;
use crate::output_section_id::SectionName;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::output_trace::TraceOutput;
use crate::platform::Arch;
use crate::platform::Args as _;
use crate::platform::ObjectFile;
use crate::platform::Relocation;
use crate::platform::RelocationList;
use crate::platform::SectionHeader as _;
use crate::resolution::SectionSlot;
use crate::writable_elf::WritableRela as _;
use hashbrown::HashMap;
use linker_utils::elf::secnames::DEBUG_LOC_SECTION_NAME;
use linker_utils::elf::secnames::DEBUG_RANGES_SECTION_NAME;
use linker_utils::relaxation::RelocationModifier;
use linker_utils::relaxation::opt_input_to_output;
use linker_utils::utils::slice_from_all_bytes_mut;
use object::LittleEndian;
use object::SymbolIndex;
use object::read::elf::SectionHeader as _;
use object::read::elf::Sym as _;
use std::sync::atomic::Ordering::Relaxed;

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

impl<R> Default for RelocationCache<R> {
    fn default() -> Self {
        Self {
            previous: Default::default(),
            high_part_symbols: Default::default(),
        }
    }
}
