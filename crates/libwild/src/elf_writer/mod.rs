use crate::OutputFileData;
use crate::args::elf::BuildIdOption;
use crate::elf;
use crate::elf::ElfClass;
use crate::elf::ElfWord as _;
use crate::elf::GNU_NOTE_NAME;
use crate::elf::output_section_id;
use crate::error::Context as _;
use crate::error::Result;
use crate::file_writer::SizedOutput;
use crate::file_writer::insufficient_allocation;
use crate::file_writer::split_output_into_sections;
use crate::platform::Arch;
use crate::platform::Args as _;
use crate::platform::SectionAttributes as _;
use crate::platform::SectionType as _;
use crate::timing_phase;
use crate::writable_elf::WritableNoteHeader as _;
use crate::writable_elf::WritableSymbol as _;
use linker_utils::elf::RelocationKind;
use linker_utils::elf::secnames::NOTE_GNU_BUILD_ID_SECTION_NAME_STR;
use object::elf::NT_GNU_BUILD_ID;
use object::from_bytes_mut;
use rayon::iter::IntoParallelIterator as _;
use rayon::iter::IntoParallelRefMutIterator as _;
use rayon::iter::ParallelBridge as _;
use rayon::iter::ParallelIterator as _;
use rayon::slice::ParallelSliceMut as _;
use uuid::Uuid;

pub(crate) mod dynamic;
pub(crate) mod headers;
pub(crate) mod relocations;
pub(crate) mod symbols;
pub(crate) mod types;

pub(crate) use dynamic::*;
pub(crate) use headers::*;
pub(crate) use relocations::*;
pub(crate) use symbols::*;
pub(crate) use types::*;

pub(crate) mod epilogue;
pub(crate) mod objects;
pub(crate) mod payload;

#[allow(unused_imports)]
pub(crate) use epilogue::*;
#[allow(unused_imports)]
pub(crate) use objects::*;
#[allow(unused_imports)]
pub(crate) use payload::*;

pub(crate) fn write<'data, C: ElfClass, A: Arch<Platform = elf::Elf<C>>>(
    sized_output: &mut SizedOutput<impl OutputFileData>,
    layout: &ElfLayout<'data, C>,
) -> Result {
    write_file_contents::<C, A>(sized_output, layout)?;
    apply_incremental_reloc_patches::<C, A>(sized_output, layout)?;
    if layout.args().common().validate_output {
        crate::validation::validate_bytes(layout, &sized_output.out)?;
    }

    let mut section_buffers = split_output_into_sections(layout, &mut sized_output.out).0;

    if layout.args().should_write_eh_frame_hdr
        && layout
            .section_layouts
            .get(output_section_id::EH_FRAME_HDR)
            .mem_size
            > 0
    {
        sort_eh_frame_hdr_entries(section_buffers.get_mut(output_section_id::EH_FRAME_HDR));
    }

    write_sframe_section(section_buffers.get_mut(output_section_id::SFRAME), layout)?;

    write_gnu_build_id_note(sized_output, &layout.args().build_id, layout)?;
    Ok(())
}

pub(crate) fn apply_incremental_reloc_patches<C: ElfClass, A: Arch<Platform = elf::Elf<C>>>(
    sized_output: &mut SizedOutput<impl OutputFileData>,
    layout: &ElfLayout<C>,
) -> Result {
    let Some(job) = &layout.incremental_patch else {
        return Ok(());
    };
    if layout.incremental_skip_payloads.is_empty() {
        return Ok(());
    }

    let new_res: Vec<u64> = layout.symbol_resolutions.raw_values().collect();
    let atom_to_file: hashbrown::HashMap<crate::incremental::AtomId, crate::input_data::FileId> =
        layout
            .incremental_atoms
            .iter()
            .map(|(file_id, atom)| (*atom, *file_id))
            .collect();
    let out = &mut sized_output.out;
    let mut patched = 0u64;
    let mut patch_error = None;
    for (file_id, atom) in &layout.incremental_atoms {
        let Some(old_vals) = job.old_resolutions.get(*atom) else {
            continue;
        };
        let range = layout.symbol_db.file(*file_id).symbol_id_range();
        for (local, symbol_id) in range.into_iter().enumerate() {
            let new = new_res.get(symbol_id.as_usize()).copied().unwrap_or(0);
            let old = old_vals.get(local).copied().unwrap_or(0);
            if old == new {
                continue;
            }
            job.reverse_relocs.for_each_site(*atom, local, |node| {
                if patch_error.is_some() {
                    return;
                }
                let Some(owner_file) = atom_to_file.get(&node.owner) else {
                    return;
                };
                if !layout.incremental_skip_payloads.contains(owner_file) {
                    return;
                }
                match patch_skipped_reloc_site::<C, A>(out, node, new) {
                    Ok(()) => patched += 1,
                    Err(error) => patch_error = Some(error),
                }
            });
            if let Some(error) = patch_error.take() {
                return Err(error);
            }
        }
    }
    if patched > 0 {
        tracing::debug!(patched, "incremental reverse-reloc patches");
    }
    Ok(())
}

pub(crate) fn patch_skipped_reloc_site<C: ElfClass, A: Arch<Platform = elf::Elf<C>>>(
    out: &mut [u8],
    node: &crate::incremental::ReverseRelocNode,
    new_s: u64,
) -> Result {
    let r_type = object::elf::RelocationType(node.r_type);
    let Ok(rel_info) = A::relocation_from_raw(r_type) else {
        return Ok(());
    };
    let value = match rel_info.kind {
        RelocationKind::Absolute | RelocationKind::AbsoluteSet => {
            new_s.wrapping_add(node.addend as u64)
        }
        RelocationKind::Relative => new_s
            .wrapping_add(node.addend as u64)
            .wrapping_sub(node.place),
        _ => return Ok(()),
    };
    let start = usize::try_from(node.file_offset).context("reloc file offset overflow")?;
    if start >= out.len() {
        return Ok(());
    }
    rel_info.write_to_buffer(value, &mut out[start..])?;
    Ok(())
}

pub(crate) fn write_gnu_build_id_note<C: ElfClass>(
    sized_output: &mut SizedOutput<impl OutputFileData>,
    build_id_option: &BuildIdOption,
    layout: &ElfLayout<C>,
) -> Result {
    let hash_placeholder;
    let uuid_placeholder;
    let build_id = match build_id_option {
        BuildIdOption::Fast => {
            hash_placeholder = compute_hash(sized_output);
            hash_placeholder.as_bytes()
        }
        BuildIdOption::Hex(hex) => hex.as_slice(),
        BuildIdOption::Uuid => {
            uuid_placeholder = Uuid::new_v4();
            uuid_placeholder.as_bytes()
        }
        BuildIdOption::None => return Ok(()),
    };

    let dest_part = match layout.output_sections.gnu_build_id_dest_part() {
        Some(part) => part,
        None => return Ok(()),
    };
    let section_id = dest_part.output_section_id::<elf::Elf<C>>();
    let mut buffers = split_output_into_sections(layout, &mut sized_output.out).0;
    let section_buf = buffers.get_mut(section_id);
    let part_layout = layout.section_part_layouts.get(dest_part);
    let section_layout = layout.section_layouts.get(section_id);
    if part_layout.file_size == 0 {
        return Ok(());
    }
    let part_start = part_layout
        .file_offset
        .saturating_sub(section_layout.file_offset);
    let part_end = part_start + part_layout.file_size;
    let part_buf = section_buf
        .get_mut(part_start..part_end)
        .ok_or_else(|| insufficient_allocation(NOTE_GNU_BUILD_ID_SECTION_NAME_STR))?;
    let note_size = C::NOTE_HEADER_SIZE as usize + GNU_NOTE_NAME.len() + build_id.len();
    let start = part_buf
        .len()
        .checked_sub(note_size)
        .ok_or_else(|| insufficient_allocation(NOTE_GNU_BUILD_ID_SECTION_NAME_STR))?;
    let (note_header, mut rest) = from_bytes_mut::<elf::NoteHeader<C>>(&mut part_buf[start..])
        .map_err(|_| insufficient_allocation(NOTE_GNU_BUILD_ID_SECTION_NAME_STR))?;
    note_header.set_name_size(GNU_NOTE_NAME.len() as u32);
    note_header.set_descriptor_size(build_id.len() as u32);
    note_header.set_type(NT_GNU_BUILD_ID);

    let name_out = rest.split_off_mut(..GNU_NOTE_NAME.len()).unwrap();
    name_out.copy_from_slice(GNU_NOTE_NAME);

    rest.copy_from_slice(build_id);

    Ok(())
}

pub(crate) fn compute_hash(sized_output: &SizedOutput<impl OutputFileData>) -> blake3::Hash {
    timing_phase!("Compute build ID");
    blake3::Hasher::new()
        .update_rayon(&sized_output.out)
        .finalize()
}
