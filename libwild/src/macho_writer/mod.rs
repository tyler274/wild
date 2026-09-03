use crate::OutputFileData;
use crate::error;
use crate::error::Context;
use crate::error::Result;
use crate::file_writer::SizedOutput;
use crate::file_writer::split_buffers_by_alignment;
use crate::file_writer::split_output_by_group;
use crate::file_writer::split_output_into_sections;
use crate::layout::FileLayout;
use crate::layout::Layout;
use crate::macho::MachO;
use crate::macho::output_section_id;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::output_trace::TraceOutput;
use crate::platform::Arch;
use crate::timing_phase;
use crate::verbose_timing_phase;
use object::Endianness;
use object::from_bytes_mut;
use rayon::iter::IntoParallelIterator;
use rayon::iter::ParallelIterator;

pub(crate) mod headers;
pub(crate) mod linkedit;
pub(crate) mod objects;
pub(crate) mod symbols;

#[allow(unused_imports)]
pub(crate) use headers::*;
#[allow(unused_imports)]
pub(crate) use linkedit::*;
#[allow(unused_imports)]
pub(crate) use objects::*;
#[allow(unused_imports)]
pub(crate) use symbols::*;

pub(crate) const LE: Endianness = Endianness::Little;

pub(crate) type MachOLayout<'data> = Layout<'data, MachO>;
pub(crate) type SymtabEntry = object::macho::Nlist64<Endianness>;
pub(crate) type ExportsTrieCommand = object::macho::LinkeditDataCommand<Endianness>;

pub(crate) fn write<'data, A: Arch<Platform = MachO>>(
    sized_output: &mut SizedOutput<impl OutputFileData>,
    layout: &MachOLayout<'data>,
) -> Result {
    timing_phase!("Write data to file");
    let exports_trie = build_exports_trie(layout)?;
    let (mut section_buffers, mut padding) =
        split_output_into_sections(layout, &mut sized_output.out);
    padding.fill_zero();

    let mut writable_buckets = split_buffers_by_alignment(&mut section_buffers, layout);
    let groups_and_buffers = split_output_by_group(layout, &mut writable_buckets);
    groups_and_buffers
        .into_par_iter()
        .try_for_each(|(group, mut buffers)| -> Result {
            verbose_timing_phase!("Write group");

            let mut symbol_writer = MachOSymbolTableWriter {
                next_strtab_offset: group.strtab_start_offset,
            };
            for file in &group.files {
                write_file::<A>(
                    file,
                    &mut buffers,
                    layout,
                    &sized_output.trace,
                    &mut symbol_writer,
                    &exports_trie,
                )
                .with_context(|| format!("Failed copying from {file} to output file"))?;
            }
            Ok(())
        })?;

    let mut section_buffers = split_output_into_sections(layout, &mut sized_output.out).0;
    write_got_entries(layout, section_buffers.get_mut(output_section_id::GOT))?;
    write_plt_entries::<A>(layout, section_buffers.get_mut(output_section_id::PLT_GOT))?;

    write_code_signature_metadata(layout, sized_output)?;
    write_uuid(layout, sized_output)?;
    write_code_signature_hashes(layout, sized_output)?;

    Ok(())
}

fn write_file<'data, A: Arch<Platform = MachO>>(
    file: &FileLayout<'data, MachO>,
    buffers: &mut OutputSectionPartMap<&mut [u8]>,
    layout: &MachOLayout<'data>,
    _trace: &TraceOutput,
    symbol_writer: &mut MachOSymbolTableWriter,
    exports_trie: &[u8],
) -> Result {
    match file {
        FileLayout::Object(s) => {
            write_object::<A>(s, buffers, layout, symbol_writer)?;
        }
        FileLayout::Prelude(s) => write_prelude(s, buffers, layout, exports_trie)?,
        FileLayout::Epilogue(s) => write_epilogue(s, buffers, layout, exports_trie)?,
        _ => {
            // TODO
        }
    }
    Ok(())
}

/// Takes enough bytes from `bytes` for a T, returning those bytes as an `&mut T`.
pub(crate) fn take_mut<'out, T: object::Pod>(bytes: &mut &'out mut [u8]) -> Result<&'out mut T> {
    let bytes = bytes
        .split_off_mut(..size_of::<T>())
        .context("Insufficient allocation")?;
    from_bytes_mut::<T>(bytes)
        .map_err(|()| error!("Unaligned write"))
        .map(|(a, _)| a)
}
