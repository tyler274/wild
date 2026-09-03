use super::super::dynamic::*;
use super::super::types::*;
use crate::bail;
use crate::elf;
use crate::elf::ElfClass;
use crate::elf::Verdaux;
use crate::elf::Verdef;
use crate::elf::Vernaux;
use crate::elf::Verneed;
use crate::elf::VersionDef;
use crate::elf::Versym;
use crate::elf::part_id;
use crate::error;
use crate::error::Context as _;
use crate::error::Result;
use crate::file_writer::excessive_allocation;
use crate::file_writer::insufficient_allocation;
use crate::output_section_part_map::OutputSectionPartMap;
use object::LittleEndian;

#[derive(Default)]
pub(crate) struct VersionWriter<'out> {
    pub(crate) version_d: &'out mut [u8],
    pub(crate) version_r: &'out mut [u8],

    /// None if versioning is disabled, which we do if no symbols have versions.
    pub(crate) versym: Option<&'out mut [Versym]>,
}

impl<'out> VersionWriter<'out> {
    pub(crate) fn new(
        version_d: &'out mut [u8],
        version_r: &'out mut [u8],
        versym: Option<&'out mut [Versym]>,
    ) -> Self {
        Self {
            version_d,
            version_r,
            versym,
        }
    }

    pub(crate) fn set_next_symbol_version(&mut self, index: object::elf::VersionIndex) -> Result {
        if let Some(versym_table) = self.versym.as_mut() {
            let versym = versym_table
                .split_off_first_mut()
                .ok_or_else(|| insufficient_allocation(".gnu.version"))?;
            versym.0.set(LittleEndian, index.into());
        }
        Ok(())
    }

    pub(crate) fn take_bytes(&mut self, size: usize) -> Result<&'out mut [u8]> {
        self.version_r
            .split_off_mut(..size)
            .ok_or_else(|| insufficient_allocation(".gnu.version_r"))
    }

    pub(crate) fn take_verneed(&mut self) -> Result<&'out mut Verneed> {
        let bytes = self.take_bytes(size_of::<Verneed>())?;
        Ok(object::from_bytes_mut(bytes)
            .map_err(|_| error!("Incorrect .gnu.version_r alignment"))?
            .0)
    }

    pub(crate) fn take_auxes(&mut self, version_count: u16) -> Result<&'out mut [Vernaux]> {
        let bytes = self.take_bytes(size_of::<Vernaux>() * usize::from(version_count))?;
        object::slice_from_all_bytes_mut::<Vernaux>(bytes)
            .map_err(|_| error!("Invalid .gnu.version_r allocation"))
    }

    pub(crate) fn take_bytes_d(&mut self, size: usize) -> Result<&'out mut [u8]> {
        self.version_d
            .split_off_mut(..size)
            .ok_or_else(|| insufficient_allocation(".gnu.version_d"))
    }

    pub(crate) fn take_verdef(&mut self) -> Result<&'out mut Verdef> {
        let bytes = self.take_bytes_d(size_of::<Verdef>())?;
        Ok(object::from_bytes_mut::<Verdef>(bytes)
            .map_err(|_| error!("Incorrect .gnu.version_d alignment"))?
            .0)
    }

    pub(crate) fn take_verdaux(&mut self) -> Result<&'out mut Verdaux> {
        let bytes = self.take_bytes_d(size_of::<Verdaux>())?;
        Ok(object::from_bytes_mut::<Verdaux>(bytes)
            .map_err(|_| error!("Incorrect .gnu.version_d aux alignment"))?
            .0)
    }

    pub(crate) fn check_exhausted(&self, mem_sizes: &OutputSectionPartMap<u64>) -> Result {
        if let Some(versym) = self.versym.as_ref()
            && !versym.is_empty()
        {
            return Err(excessive_allocation(
                ".gnu.version",
                versym.len() as u64 * elf::GNU_VERSION_ENTRY_SIZE,
                mem_sizes.get(part_id::GNU_VERSION),
            ));
        }
        if !self.version_r.is_empty() {
            bail!(
                "Allocated too much space in .gnu.version_r. {} of {} bytes remain",
                self.version_r.len(),
                mem_sizes.get(part_id::GNU_VERSION_R)
            );
        }
        if !self.version_d.is_empty() {
            bail!(
                "Allocated too much space in .gnu.version_d. {} of {} bytes remain",
                self.version_d.len(),
                mem_sizes.get(part_id::GNU_VERSION_D)
            );
        }
        Ok(())
    }

    pub(crate) fn take_prefix(&mut self, num_symbols: usize) -> Option<&'out mut [Versym]> {
        Some(self.versym.as_mut()?.split_off_mut(..num_symbols).unwrap())
    }
}
pub(crate) fn write_verdef<C: ElfClass>(
    verdefs: &[VersionDef],
    table_writer: &mut TableWriter<'_, '_, C>,
    soname: Option<&[u8]>,
    epilogue_offsets: &EpilogueOffsets,
) -> Result {
    let e = LittleEndian;

    // Offsets of version strings, except the base version
    let mut version_string_offsets = Vec::with_capacity(verdefs.len() - 1);

    for (i, verdef) in verdefs.iter().enumerate() {
        let verdef_out = table_writer.version_writer.take_verdef()?;

        // Base version may use (already allocated) soname
        let (name, name_offset) = if i == 0 {
            if let Some(soname) = soname {
                (
                    soname,
                    epilogue_offsets
                        .soname
                        .expect("Soname offset must be present at this point"),
                )
            } else {
                let offset = table_writer
                    .dynsym_writer
                    .strtab_writer
                    .write_str(&verdef.name);
                (verdef.name.as_slice(), offset)
            }
        } else {
            let offset = table_writer
                .dynsym_writer
                .strtab_writer
                .write_str(&verdef.name);
            version_string_offsets.push(offset);
            (verdef.name.as_slice(), offset)
        };

        verdef_out.vd_version.set(e, object::elf::VER_DEF_CURRENT);
        // Mark first entry as base version
        verdef_out.vd_flags.set(
            e,
            if i == 0 {
                object::elf::VER_FLG_BASE
            } else {
                object::elf::VersionFlags(0)
            },
        );
        verdef_out
            .vd_ndx
            .set(e, object::elf::VER_NDX_GLOBAL + i as u16);
        let aux_count = if verdef.parent_index.is_some() { 2 } else { 1 };
        verdef_out.vd_cnt.set(e, aux_count);
        verdef_out.vd_hash.set(e, object::elf::hash(name));
        verdef_out
            .vd_aux
            .set(e, size_of::<crate::elf::Verdef>() as u32);
        // Offset to the next entry, unless it's the last one
        let offset = if i < verdefs.len() - 1 {
            (size_of::<crate::elf::Verdef>()
                + size_of::<crate::elf::Verdaux>() * aux_count as usize) as u32
        } else {
            0
        };
        verdef_out.vd_next.set(e, offset);

        let verdaux = table_writer.version_writer.take_verdaux()?;
        verdaux.vda_name.set(e, name_offset);
        let next_vda = if verdef.parent_index.is_some() {
            size_of::<crate::elf::Verdaux>() as u32
        } else {
            0
        };
        verdaux.vda_next.set(e, next_vda);

        if let Some(parent_index) = &verdef.parent_index {
            let name_offset = *version_string_offsets
                .get(*parent_index as usize - 1)
                .unwrap();
            let verdaux = table_writer.version_writer.take_verdaux()?;
            verdaux.vda_name.set(e, name_offset);
            verdaux.vda_next.set(e, 0);
        }
    }

    Ok(())
}

pub(crate) fn copy_symbol_version(
    versym_in: &[Versym],
    local_symbol_index: usize,
    version_mapping: &[object::elf::VersionIndex],
    versym_out: &mut &mut [Versym],
) -> Result {
    let output_version =
        versym_in
            .get(local_symbol_index)
            .map_or(object::elf::VER_NDX_GLOBAL, |versym| {
                let input_version = versym.0.get(LittleEndian).index();
                if input_version.is_special() {
                    input_version
                } else {
                    version_mapping[usize::from(input_version - object::elf::VER_NDX_GLOBAL)]
                }
            });

    write_symbol_version(versym_out, output_version.into())
}

pub(crate) fn write_symbol_version(
    versym_out: &mut &mut [Versym],
    version: object::elf::VersymIndex,
) -> Result {
    versym_out
        .split_off_first_mut()
        .context("Insufficient .gnu.version allocation")?
        .0
        .set(LittleEndian, version);

    Ok(())
}
