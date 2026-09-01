use super::symbols::*;
use super::types::*;
use crate::OutputKind;
use crate::args::elf::ElfArgs;
use crate::bail;
use crate::elf;
use crate::elf::DynamicEntry;
use crate::elf::ElfClass;
use crate::elf::ElfWord as _;
use crate::elf::GnuHashHeader;
use crate::elf::NonAddressableCounts;
use crate::elf::Vernaux;
use crate::elf::Verneed;
use crate::elf::output_section_id;
use crate::elf::part_id;
use crate::ensure;
use crate::error;
use crate::error::Context as _;
use crate::error::Result;
use crate::file_writer::insufficient_allocation;
use crate::layout::DynamicLayout;
use crate::layout::EpilogueLayout;
use crate::layout::OutputRecordLayout;
use crate::output_section_id::OutputSectionId;
use crate::output_section_map::OutputSectionMap;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::platform::Arch;
use crate::platform::ObjectFile;
use crate::symbol_db::SymbolId;
use crate::value_flags::ValueFlags;
use crate::verbose_timing_phase;
use crate::writable_elf::WritableDynamicEntry as _;
use crate::writable_elf::WritableSymbol as _;
use linker_utils::elf::DynamicRelocationKind;
use linker_utils::utils::slice_from_all_bytes_mut;
use object::LittleEndian;
use object::read::elf::Sym as _;
use zerocopy::FromBytes;

pub(crate) fn write_epilogue_dynamic_entries<C: ElfClass>(
    layout: &ElfLayout<C>,
    table_writer: &mut TableWriter<'_, '_, C>,
    epilogue_offsets: &mut EpilogueOffsets,
) -> Result {
    if let Some(rpath) = &layout.args().rpath {
        let offset = table_writer
            .dynsym_writer
            .strtab_writer
            .write_str(rpath.as_bytes());
        let rpath_tag = if layout.args().enable_new_dtags {
            object::elf::DT_RUNPATH
        } else {
            object::elf::DT_RPATH
        };
        table_writer.dynamic.write(rpath_tag, offset.into())?;
    }
    if let Some(soname) = layout.args().soname.as_ref() {
        let offset = table_writer
            .dynsym_writer
            .strtab_writer
            .write_str(soname.as_bytes());
        table_writer
            .dynamic
            .write(object::elf::DT_SONAME, offset.into())?;
        epilogue_offsets.soname.replace(offset);
    }
    for aux in &layout.args().auxiliary {
        let offset = table_writer
            .dynsym_writer
            .strtab_writer
            .write_str(aux.as_bytes());
        table_writer
            .dynamic
            .write(object::elf::DT_AUXILIARY, offset.into())?;
    }

    let inputs = DynamicEntryInputs {
        args: layout.args(),
        has_static_tls: layout.has_static_tls,
        has_variant_pcs: layout.has_variant_pcs,
        section_layouts: &layout.merged_section_layouts,
        section_part_layouts: &layout.section_part_layouts,
        non_addressable_counts: layout.non_addressable_counts,
        output_kind: layout.symbol_db.output_kind,
        rela_entry_size: C::RELA_ENTRY_SIZE,
        relr_entry_size: C::RELR_ENTRY_SIZE,
        symtab_entry_size: C::SYMTAB_ENTRY_SIZE,
    };

    for writer in EPILOGUE_DYNAMIC_ENTRY_WRITERS {
        writer.write(&mut table_writer.dynamic, &inputs)?;
    }

    table_writer.dynamic.write_unused()?;

    Ok(())
}

#[derive(Default)]
pub(crate) struct EpilogueOffsets {
    /// The offset of the shared object name in .dynsym.
    pub(crate) soname: Option<u32>,
}

pub(crate) fn write_sysv_hash_table<C: ElfClass>(
    layout: &ElfLayout<C>,
    epilogue: &EpilogueLayout<elf::Elf<C>>,
    buffers: &mut OutputSectionPartMap<&mut [u8]>,
) -> Result {
    let Some(sysv_hash_layout) = epilogue.format_specific.sysv_hash_layout.as_ref() else {
        return Ok(());
    };

    let bucket_count =
        usize::try_from(sysv_hash_layout.bucket_count).context("Too many buckets for .hash")?;
    let chain_count =
        usize::try_from(sysv_hash_layout.chain_count).context("Too many chains for .hash")?;

    if bucket_count == 0 || chain_count == 0 {
        return Ok(());
    }

    let total_words = 2usize
        .checked_add(bucket_count)
        .and_then(|v| v.checked_add(chain_count))
        .context("Insufficient .hash allocation")?;
    let required_bytes = total_words
        .checked_mul(std::mem::size_of::<u32>())
        .context("Insufficient .hash allocation")?;

    let buffer = buffers.get_mut(part_id::SYSV_HASH);
    if buffer.len() < required_bytes {
        return Err(error!("Insufficient .hash allocation"));
    }
    let buffer = &mut buffer[..required_bytes];
    buffer.fill(0);

    let (header_bytes, rest) = buffer.split_at_mut(2 * std::mem::size_of::<u32>());
    header_bytes[..4].copy_from_slice(&sysv_hash_layout.bucket_count.to_le_bytes());
    header_bytes[4..8].copy_from_slice(&sysv_hash_layout.chain_count.to_le_bytes());

    let (buckets, rest) = object::slice_from_bytes_mut::<u32>(rest, bucket_count)
        .map_err(|_| error!("Insufficient bytes for .hash buckets"))?;
    let (chains, rest) = object::slice_from_bytes_mut::<u32>(rest, chain_count)
        .map_err(|_| error!("Insufficient bytes for .hash chains"))?;

    debug_assert_eq!(rest, []);

    buckets.fill(0);
    chains.fill(0);
    let mut last_in_bucket: Vec<Option<usize>> = vec![None; bucket_count];

    for (i, sym_def) in layout.dynamic_symbol_definitions.iter().enumerate() {
        let additional = u32::try_from(i).context("Too many dynamic symbols for .hash")?;
        let sym_index = epilogue
            .dynsym_start_index
            .checked_add(additional)
            .context("Too many dynamic symbols for .hash")?;
        let sym_index_usize =
            usize::try_from(sym_index).context("Too many dynamic symbols for .hash")?;
        let hash = object::elf::hash(sym_def.name);
        let bucket = (hash % sysv_hash_layout.bucket_count) as usize;

        if buckets[bucket] == 0 {
            buckets[bucket] = sym_index;
        } else {
            let last = last_in_bucket[bucket].context("Invalid .hash bucket chain construction")?;
            chains[last] = sym_index;
        }
        last_in_bucket[bucket] = Some(sym_index_usize);
    }

    Ok(())
}

pub(crate) fn write_gnu_hash_tables<C: ElfClass>(
    layout: &ElfLayout<C>,
    epilogue: &EpilogueLayout<elf::Elf<C>>,
    buffers: &mut OutputSectionPartMap<&mut [u8]>,
) -> Result {
    let Some(gnu_hash_layout) = epilogue.format_specific.gnu_hash_layout.as_ref() else {
        return Ok(());
    };

    let buffer = buffers.get_mut(part_id::GNU_HASH);
    let (header, rest) = object::from_bytes_mut::<GnuHashHeader>(buffer)
        .map_err(|_| error!("Insufficient .gnu.hash allocation"))?;
    let e = LittleEndian;
    header.bucket_count.set(e, gnu_hash_layout.bucket_count);
    header.bloom_shift.set(e, gnu_hash_layout.bloom_shift);
    header.bloom_count.set(e, gnu_hash_layout.bloom_count);
    header.symbol_base.set(e, gnu_hash_layout.symbol_base);

    let bloom_size = (gnu_hash_layout.bloom_count as usize)
        .checked_mul(C::GNU_HASH_BLOOM_SIZE as usize)
        .context(".gnu.hash bloom filter size overflow")?;
    ensure!(
        rest.len() >= bloom_size,
        "Insufficient bytes for .gnu.hash bloom filter"
    );
    let (bloom, rest) = rest.split_at_mut(bloom_size);
    let bloom = <[elf::Word<C>]>::mut_from_bytes(bloom)
        .map_err(|_| error!("Invalid .gnu.hash bloom filter size"))?;
    let (buckets, rest) =
        object::slice_from_bytes_mut::<u32>(rest, gnu_hash_layout.bucket_count as usize)
            .map_err(|_| error!("Insufficient bytes for .gnu.hash buckets"))?;
    let (chains, rest) =
        object::slice_from_bytes_mut::<u32>(rest, layout.dynamic_symbol_definitions.len())
            .map_err(|_| error!("Insufficient bytes for .gnu.hash chains"))?;

    debug_assert_eq!(rest.len(), 0);

    // Some buckets and bloom entries might not get written below, so fill with zeros to ensure
    // deterministic output if we're editing in-place.
    buckets.fill(0);
    bloom.fill(elf::Word::<C>::from_u64(0)?);

    let mut sym_defs = layout.dynamic_symbol_definitions.iter().peekable();

    let elf_class_bits = C::ADDRESS_SIZE as u32 * 8;

    let mut start_of_chain = true;
    for (i, chain_out) in chains.iter_mut().enumerate() {
        let sym_def = sym_defs.next().unwrap();

        // For each symbol, we set two bits in the bloom filter. This speeds up dynamic loading,
        // since most symbols not defined by the shared object can be rejected just by the bloom
        // filter.
        let hash = sym_def.format_specific.hash;
        let bloom_index = ((hash / elf_class_bits) % gnu_hash_layout.bloom_count) as usize;
        let bit1 = 1 << (hash % elf_class_bits);
        let bit2 = 1 << ((hash >> gnu_hash_layout.bloom_shift) % elf_class_bits);
        bloom[bloom_index] = elf::Word::<C>::from_u64(bloom[bloom_index].into() | bit1 | bit2)?;

        // Chain values are the hashes for the corresponding symbols (shifted by symbol_base). Bit 0
        // is cleared and then later set to 1 to indicate the end of the chain.
        *chain_out = hash & !1;
        let bucket = gnu_hash_layout.bucket_for_hash(hash);
        if start_of_chain {
            buckets[bucket as usize] = (i as u32) + gnu_hash_layout.symbol_base;
            start_of_chain = false;
        }
        let last_in_chain = sym_defs.peek().is_none_or(|next| {
            gnu_hash_layout.bucket_for_hash(next.format_specific.hash) != bucket
        });
        if last_in_chain {
            *chain_out |= 1;
            start_of_chain = true;
        }
    }
    Ok(())
}

/// An upper-bound on how many dynamic entries we'll write in the epilogue. Some entries are
/// optional, so might not get written. For now, we still allocate space for these optional entries.
pub(crate) const NUM_EPILOGUE_DYNAMIC_ENTRIES: usize = EPILOGUE_DYNAMIC_ENTRY_WRITERS.len();

pub(crate) const EPILOGUE_DYNAMIC_ENTRY_WRITERS: &[DynamicEntryWriter] = &[
    DynamicEntryWriter::optional(
        object::elf::DT_INIT,
        |inputs| inputs.has_data_in_section(output_section_id::INIT),
        |inputs| inputs.vma_of_section(output_section_id::INIT),
    ),
    DynamicEntryWriter::optional(
        object::elf::DT_FINI,
        |inputs| inputs.has_data_in_section(output_section_id::FINI),
        |inputs| inputs.vma_of_section(output_section_id::FINI),
    ),
    DynamicEntryWriter::optional(
        object::elf::DT_INIT_ARRAY,
        |inputs| inputs.has_data_in_section(output_section_id::INIT_ARRAY),
        |inputs| inputs.vma_of_section(output_section_id::INIT_ARRAY),
    ),
    DynamicEntryWriter::optional(
        object::elf::DT_INIT_ARRAYSZ,
        |inputs| inputs.has_data_in_section(output_section_id::INIT_ARRAY),
        |inputs| inputs.size_of_section(output_section_id::INIT_ARRAY),
    ),
    DynamicEntryWriter::optional(
        object::elf::DT_FINI_ARRAY,
        |inputs| inputs.has_data_in_section(output_section_id::FINI_ARRAY),
        |inputs| inputs.vma_of_section(output_section_id::FINI_ARRAY),
    ),
    DynamicEntryWriter::optional(
        object::elf::DT_FINI_ARRAYSZ,
        |inputs| inputs.has_data_in_section(output_section_id::FINI_ARRAY),
        |inputs| inputs.size_of_section(output_section_id::FINI_ARRAY),
    ),
    DynamicEntryWriter::optional(
        object::elf::DT_PREINIT_ARRAY,
        |inputs| inputs.has_data_in_section(output_section_id::PREINIT_ARRAY),
        |inputs| inputs.vma_of_section(output_section_id::PREINIT_ARRAY),
    ),
    DynamicEntryWriter::optional(
        object::elf::DT_PREINIT_ARRAYSZ,
        |inputs| inputs.has_data_in_section(output_section_id::PREINIT_ARRAY),
        |inputs| inputs.size_of_section(output_section_id::PREINIT_ARRAY),
    ),
    DynamicEntryWriter::new(object::elf::DT_STRTAB, |inputs| {
        inputs.vma_of_section(output_section_id::DYNSTR)
    }),
    DynamicEntryWriter::new(object::elf::DT_STRSZ, |inputs| {
        inputs.size_of_section(output_section_id::DYNSTR)
    }),
    DynamicEntryWriter::new(object::elf::DT_SYMTAB, |inputs| {
        inputs.vma_of_section(output_section_id::DYNSYM)
    }),
    DynamicEntryWriter::new(object::elf::DT_SYMENT, |inputs| inputs.symtab_entry_size),
    DynamicEntryWriter::optional(
        object::elf::DT_VERDEF,
        |inputs| {
            inputs
                .section_part_layouts
                .get(part_id::GNU_VERSION_D)
                .mem_size
                > 0
        },
        |inputs| inputs.vma_of_section(output_section_id::GNU_VERSION_D),
    ),
    DynamicEntryWriter::optional(
        object::elf::DT_VERDEFNUM,
        |inputs| {
            inputs
                .section_part_layouts
                .get(part_id::GNU_VERSION_D)
                .mem_size
                > 0
        },
        |inputs| inputs.non_addressable_counts.verdef_count.into(),
    ),
    DynamicEntryWriter::optional(
        object::elf::DT_VERNEED,
        |inputs| {
            inputs
                .section_part_layouts
                .get(part_id::GNU_VERSION_R)
                .mem_size
                > 0
        },
        |inputs| inputs.vma_of_section(output_section_id::GNU_VERSION_R),
    ),
    DynamicEntryWriter::optional(
        object::elf::DT_VERNEEDNUM,
        |inputs| {
            inputs
                .section_part_layouts
                .get(part_id::GNU_VERSION_R)
                .mem_size
                > 0
        },
        |inputs| inputs.non_addressable_counts.verneed_count,
    ),
    DynamicEntryWriter::optional(
        object::elf::DT_VERSYM,
        |inputs| {
            inputs
                .section_part_layouts
                .get(part_id::GNU_VERSION)
                .mem_size
                > 0
        },
        |inputs| inputs.vma_of_section(output_section_id::GNU_VERSION),
    ),
    DynamicEntryWriter::optional(
        object::elf::DT_DEBUG,
        |inputs| {
            // Not sure why, but GNU ld seems to emit this for executables but not for shared
            // objects.
            inputs.output_kind.is_executable()
        },
        |_inputs| 0,
    ),
    DynamicEntryWriter::optional(
        object::elf::DT_JMPREL,
        |inputs| inputs.section_part_layouts.get(part_id::RELA_PLT).mem_size > 0,
        |inputs| inputs.vma_of_section(output_section_id::RELA_PLT),
    ),
    DynamicEntryWriter::optional(
        object::elf::DT_PLTGOT,
        |inputs| inputs.output_kind.needs_dynamic(),
        |inputs| inputs.vma_of_section(output_section_id::GOT),
    ),
    DynamicEntryWriter::optional(
        object::elf::DT_PLTREL,
        |inputs| inputs.section_part_layouts.get(part_id::RELA_PLT).mem_size > 0,
        |_| object::elf::DT_RELA.0 as u64,
    ),
    DynamicEntryWriter::optional(
        object::elf::DT_PLTRELSZ,
        |inputs| inputs.section_part_layouts.get(part_id::RELA_PLT).mem_size > 0,
        |inputs| inputs.section_part_layouts.get(part_id::RELA_PLT).mem_size,
    ),
    DynamicEntryWriter::optional(object::elf::DT_RELA, has_rela_dyn, |inputs| {
        inputs.vma_of_section(output_section_id::RELA_DYN_RELATIVE)
    }),
    DynamicEntryWriter::optional(object::elf::DT_RELASZ, has_rela_dyn, |inputs| {
        inputs.size_of_section(output_section_id::RELA_DYN_RELATIVE)
            + inputs.size_of_section(output_section_id::RELA_DYN_GENERAL)
    }),
    DynamicEntryWriter::optional(object::elf::DT_RELAENT, has_rela_dyn, |inputs| {
        inputs.rela_entry_size
    }),
    // Note, rela-count is just the count of the relative relocations and doesn't include any
    // glob-dat relocations. This is as opposed to rela-size, which includes both.
    DynamicEntryWriter::new(object::elf::DT_RELACOUNT, |inputs| {
        inputs
            .section_part_layouts
            .get(part_id::RELA_DYN_RELATIVE)
            .mem_size
            / inputs.rela_entry_size
    }),
    DynamicEntryWriter::optional(
        object::elf::DT_RELR,
        |inputs| {
            inputs.has_data_in_section(output_section_id::RELR_DYN)
                && !has_android_relr_tags(inputs)
        },
        |inputs| inputs.vma_of_section(output_section_id::RELR_DYN),
    ),
    DynamicEntryWriter::optional(
        object::elf::DT_RELRSZ,
        |inputs| {
            inputs.has_data_in_section(output_section_id::RELR_DYN)
                && !has_android_relr_tags(inputs)
        },
        |inputs| inputs.size_of_section(output_section_id::RELR_DYN),
    ),
    DynamicEntryWriter::optional(
        object::elf::DT_RELRENT,
        |inputs| {
            inputs.has_data_in_section(output_section_id::RELR_DYN)
                && !has_android_relr_tags(inputs)
        },
        |inputs| inputs.relr_entry_size,
    ),
    DynamicEntryWriter::optional(
        object::elf::DT_ANDROID_RELR,
        |inputs| {
            inputs.has_data_in_section(output_section_id::RELR_DYN) && has_android_relr_tags(inputs)
        },
        |inputs| inputs.vma_of_section(output_section_id::RELR_DYN),
    ),
    DynamicEntryWriter::optional(
        object::elf::DT_ANDROID_RELRSZ,
        |inputs| {
            inputs.has_data_in_section(output_section_id::RELR_DYN) && has_android_relr_tags(inputs)
        },
        |inputs| inputs.size_of_section(output_section_id::RELR_DYN),
    ),
    DynamicEntryWriter::optional(
        object::elf::DT_ANDROID_RELRENT,
        |inputs| {
            inputs.has_data_in_section(output_section_id::RELR_DYN) && has_android_relr_tags(inputs)
        },
        |inputs| inputs.relr_entry_size,
    ),
    DynamicEntryWriter::optional(
        object::elf::DT_HASH,
        |inputs| inputs.has_data_in_section(output_section_id::HASH),
        |inputs| inputs.vma_of_section(output_section_id::HASH),
    ),
    DynamicEntryWriter::optional(
        object::elf::DT_GNU_HASH,
        |inputs| inputs.has_data_in_section(output_section_id::GNU_HASH),
        |inputs| inputs.vma_of_section(output_section_id::GNU_HASH),
    ),
    DynamicEntryWriter::optional(
        object::elf::DT_FLAGS,
        |inputs| inputs.args.enable_new_dtags && inputs.dt_flags().0 != 0,
        |inputs| inputs.dt_flags().0,
    ),
    DynamicEntryWriter::optional(
        object::elf::DT_FLAGS_1,
        |inputs| inputs.dt_flags_1().0 != 0,
        |inputs| inputs.dt_flags_1().0,
    ),
    DynamicEntryWriter::optional(
        object::elf::DT_BIND_NOW,
        |inputs| {
            !inputs.args.enable_new_dtags && inputs.dt_flags().contains(object::elf::DF_BIND_NOW)
        },
        |_inputs| 0,
    ),
    DynamicEntryWriter::optional(
        object::elf::DT_SYMBOLIC,
        |inputs| {
            !inputs.args.enable_new_dtags && inputs.dt_flags().contains(object::elf::DF_SYMBOLIC)
        },
        |_inputs| 0,
    ),
    DynamicEntryWriter::optional(
        object::elf::DT_TEXTREL,
        |inputs| {
            !inputs.args.enable_new_dtags && inputs.dt_flags().contains(object::elf::DF_TEXTREL)
        },
        |_inputs| 0,
    ),
    DynamicEntryWriter::optional(
        object::elf::DT_AARCH64_VARIANT_PCS,
        |inputs| inputs.has_variant_pcs && inputs.args.arch == crate::arch::Architecture::AArch64,
        |_inputs| 0,
    ),
    DynamicEntryWriter::optional(
        object::elf::DT_RISCV_VARIANT_CC,
        |inputs| inputs.has_variant_pcs && inputs.args.arch == crate::arch::Architecture::RiscV64,
        |_inputs| 0,
    ),
    DynamicEntryWriter::new(object::elf::DT_NULL, |_inputs| 0),
];

pub(crate) struct DynamicEntryWriter {
    pub(crate) tag: object::elf::DynamicTag,
    pub(crate) is_present_cb: fn(&DynamicEntryInputs) -> bool,
    pub(crate) cb: fn(&DynamicEntryInputs) -> u64,
}

pub(crate) struct DynamicEntryInputs<'layout> {
    pub(crate) args: &'layout ElfArgs,
    pub(crate) has_static_tls: bool,
    pub(crate) has_variant_pcs: bool,
    pub(crate) section_layouts: &'layout OutputSectionMap<OutputRecordLayout>,
    pub(crate) section_part_layouts: &'layout OutputSectionPartMap<OutputRecordLayout>,
    pub(crate) non_addressable_counts: NonAddressableCounts,
    pub(crate) output_kind: OutputKind,
    pub(crate) rela_entry_size: u64,
    pub(crate) relr_entry_size: u64,
    pub(crate) symtab_entry_size: u64,
}

impl DynamicEntryInputs<'_> {
    pub(crate) fn dt_flags(&self) -> object::elf::DynamicFlags {
        let mut flags = object::elf::DynamicFlags(0);
        flags |= object::elf::DF_BIND_NOW;

        if !self.output_kind.is_executable() && self.has_static_tls {
            flags |= object::elf::DF_STATIC_TLS;
        }

        if self.args.needs_origin_handling {
            flags |= object::elf::DF_ORIGIN;
        }

        flags
    }

    pub(crate) fn dt_flags_1(&self) -> object::elf::DynamicFlags1 {
        let mut flags = object::elf::DynamicFlags1(0);
        flags |= object::elf::DF_1_NOW;

        if self.output_kind.is_executable() && self.output_kind.is_position_independent() {
            flags |= object::elf::DF_1_PIE;
        }

        if self.args.needs_origin_handling {
            flags |= object::elf::DF_1_ORIGIN;
        }

        if self.output_kind.is_shared_object() {
            if self.args.needs_nodelete_handling {
                flags |= object::elf::DF_1_NODELETE;
            }

            if self.args.z_interpose {
                flags |= object::elf::DF_1_INTERPOSE;
            }
        }

        flags
    }

    pub(crate) fn vma_of_section(&self, section_id: OutputSectionId) -> u64 {
        self.section_layouts.get(section_id).mem_offset
    }

    pub(crate) fn size_of_section(&self, section_id: OutputSectionId) -> u64 {
        self.section_layouts.get(section_id).file_size as u64
    }

    pub(crate) fn has_data_in_section(&self, id: OutputSectionId) -> bool {
        self.size_of_section(id) > 0
    }
}

impl DynamicEntryWriter {
    const fn new(
        tag: object::elf::DynamicTag,
        cb: fn(&DynamicEntryInputs) -> u64,
    ) -> DynamicEntryWriter {
        DynamicEntryWriter {
            tag,
            is_present_cb: |_| true,
            cb,
        }
    }

    const fn optional(
        tag: object::elf::DynamicTag,
        is_present_cb: fn(&DynamicEntryInputs) -> bool,
        cb: fn(&DynamicEntryInputs) -> u64,
    ) -> DynamicEntryWriter {
        DynamicEntryWriter {
            tag,
            is_present_cb,
            cb,
        }
    }

    pub(crate) fn is_present(&self, inputs: &DynamicEntryInputs) -> bool {
        (self.is_present_cb)(inputs)
    }

    pub(crate) fn write<C: ElfClass>(
        &self,
        out: &mut DynamicEntriesWriter<'_, C>,
        inputs: &DynamicEntryInputs,
    ) -> Result {
        if !self.is_present(inputs) {
            return Ok(());
        }
        let value = (self.cb)(inputs);
        out.write(self.tag, value)
    }
}

pub(crate) struct DynamicEntriesWriter<'out, C: ElfClass> {
    pub(crate) out: &'out mut [DynamicEntry<C>],
}

impl<'out, C: ElfClass> DynamicEntriesWriter<'out, C> {
    pub(crate) fn new(buffer: &'out mut [u8]) -> DynamicEntriesWriter<'out, C> {
        DynamicEntriesWriter {
            out: slice_from_all_bytes_mut(buffer),
        }
    }

    pub(crate) fn write(&mut self, tag: object::elf::DynamicTag, value: u64) -> Result {
        let entry = self
            .out
            .split_off_first_mut()
            .ok_or_else(|| insufficient_allocation(".dynamic"))?;
        entry.set_tag(tag)?;
        entry.set_value(value)?;
        Ok(())
    }

    /// Some dynamic entries aren't used, but we currently allocate space for them anyway. This
    /// makes sure that they're written with zeros.
    pub(crate) fn write_unused(&mut self) -> Result {
        loop {
            let Some(entry) = self.out.split_off_first_mut() else {
                return Ok(());
            };
            entry.set_tag(object::elf::DT_NULL)?;
            entry.set_value(0)?;
        }
    }
}

pub(crate) fn write_dynamic_file<'data, C: ElfClass, A: Arch<Platform = elf::Elf<C>>>(
    object: &DynamicLayout<'data, elf::Elf<C>>,
    table_writer: &mut TableWriter<'_, '_, C>,
    layout: &ElfLayout<'data, C>,
) -> Result {
    verbose_timing_phase!("Write dynamic");

    write_so_name(object, table_writer)?;

    write_copy_relocations::<C, A>(object, table_writer, layout)?;

    for ((symbol_id, resolution), symbol) in layout
        .resolutions_in_range(object.symbol_id_range)
        .zip(object.object.symbols.iter())
    {
        if layout.symbol_db.args.got_plt_syms {
            write_got_plt_syms(layout, &mut table_writer.debug_symbol_writer, symbol_id)?;
        }
        if let Some(res) = resolution {
            let name = object.object.symbol_name(symbol)?;

            if res.flags.needs_copy_relocation() {
                // Symbol needs a copy relocation, which means that the dynamic symbol will be
                // written by the epilogue not by us. However, we do need to write a regular
                // symtab entry.
                table_writer.debug_symbol_writer.copy_symbol(
                    symbol,
                    name,
                    output_section_id::BSS,
                    res.value(),
                    ValueFlags::empty(),
                )?;
            } else {
                let entry = table_writer.dynsym_writer.undefined_symbol(false, name)?;

                // Note, we copy st_info, but not st_other since we don't want to copy the
                // visibility. We want to emit the symbol with default visibility, otherwise the
                // runtime loader may ignore dynamic relocations that reference the symbol.
                entry.set_info(symbol.st_info());

                if let Some(versym) = table_writer.version_writer.versym.as_mut() {
                    copy_symbol_version(
                        object.object.symbol_versions(),
                        object.symbol_id_range.id_to_offset(symbol_id),
                        &object.format_specific.version_mapping,
                        versym,
                    )?;
                }
            }

            table_writer
                .process_resolution::<A>(Some(layout), layout.args(), res)
                .with_context(|| format!("Failed to write {}", layout.symbol_debug(symbol_id)))?;
        }
    }

    if let Some(verneed_info) = &object.format_specific.verneed_info {
        let mut verdefs = verneed_info.defs.clone();
        let e = LittleEndian;

        let strings = object.object.sections.strings(
            e,
            object.object.data,
            verneed_info.string_table_index,
        )?;

        let ver_need = table_writer.version_writer.take_verneed()?;

        let next_verneed_offset = if object.format_specific.is_last_verneed {
            0
        } else {
            (size_of::<Verneed>() + size_of::<Vernaux>() * verneed_info.version_count as usize)
                as u32
        };

        ver_need.vn_version.set(e, 1);
        ver_need.vn_cnt.set(e, verneed_info.version_count);
        ver_need.vn_aux.set(e, size_of::<Verneed>() as u32);
        ver_need.vn_next.set(e, next_verneed_offset);

        let auxes = table_writer
            .version_writer
            .take_auxes(verneed_info.version_count)?;
        let mut aux_index = 0;

        while let Some((verdef, mut aux_iterator)) = verdefs.next()? {
            let input_version = verdef.vd_ndx.get(e);
            let flags = verdef.vd_flags.get(e);
            let is_base = flags.contains(object::elf::VER_FLG_BASE);

            if is_base {
                let name_offset = table_writer
                    .dynsym_writer
                    .strtab_writer
                    .write_str(object.lib_name);

                ver_need.vn_file.set(e, name_offset);
                continue;
            }

            if input_version.is_local() {
                bail!("Invalid version index");
            }

            let output_version = object
                .format_specific
                .version_mapping
                .get(usize::from(input_version - object::elf::VER_NDX_GLOBAL))
                .copied()
                .unwrap_or_default();

            if !output_version.is_global() {
                // Every VERDEF entry should have at least one AUX entry.
                let aux_in = aux_iterator.next()?.context("VERDEF with no AUX entry")?;
                let name = aux_in.name(e, strings)?;
                let name_offset = table_writer.dynsym_writer.strtab_writer.write_str(name);
                let sysv_name_hash = object::elf::hash(name);
                let is_last_aux = aux_index + 1 == auxes.len();

                let aux_out = auxes
                    .get_mut(aux_index)
                    .context("Insufficient vernaux allocation")?;

                let vna_next = if is_last_aux {
                    0
                } else {
                    size_of::<Vernaux>() as u32
                };

                aux_out.vna_next.set(e, vna_next);
                aux_out.vna_other.set(e, output_version);
                aux_out.vna_name.set(e, name_offset);
                aux_out.vna_hash.set(e, sysv_name_hash);
                aux_out.vna_flags.set(e, object::elf::VersionFlags(0));
                aux_index += 1;
            }
        }
        debug_assert_eq!(aux_index, auxes.len());
    }

    Ok(())
}

/// Write dynamic entry to indicate name of shared object to load.
pub(crate) fn write_so_name<'data, C: ElfClass>(
    object: &DynamicLayout<'data, elf::Elf<C>>,
    table_writer: &mut TableWriter<'_, '_, C>,
) -> Result {
    let needed_offset = table_writer
        .dynsym_writer
        .strtab_writer
        .write_str(object.lib_name);
    table_writer
        .dynamic
        .write(object::elf::DT_NEEDED, needed_offset.into())?;
    Ok(())
}

pub(crate) fn write_copy_relocations<'data, C: ElfClass, A: Arch<Platform = elf::Elf<C>>>(
    object: &DynamicLayout<'data, elf::Elf<C>>,
    table_writer: &mut TableWriter<'_, '_, C>,
    layout: &ElfLayout<C>,
) -> Result {
    for &symbol_id in &object.format_specific.copy_relocation_symbols {
        write_copy_relocation_for_symbol::<C, A>(symbol_id, table_writer, layout).with_context(
            || {
                format!(
                    "Failed to write copy relocation for {}",
                    layout.symbol_debug(symbol_id)
                )
            },
        )?;
    }

    Ok(())
}

pub(crate) fn write_copy_relocation_for_symbol<C: ElfClass, A: Arch<Platform = elf::Elf<C>>>(
    symbol_id: SymbolId,
    table_writer: &mut TableWriter<'_, '_, C>,
    layout: &ElfLayout<C>,
) -> Result {
    let res = layout
        .local_symbol_resolution(symbol_id)
        .context("Internal error: Missing resolution for copy-relocated symbol")?;

    table_writer.write_rela_dyn_general(
        res.raw_value,
        res.dynamic_symbol_index()?,
        A::get_dynamic_relocation_type(DynamicRelocationKind::Copy),
        0,
    )
}

pub(crate) fn has_rela_dyn(inputs: &DynamicEntryInputs) -> bool {
    let relative = inputs.section_part_layouts.get(part_id::RELA_DYN_RELATIVE);
    let general = inputs.section_part_layouts.get(part_id::RELA_DYN_GENERAL);
    relative.mem_size > 0 || general.mem_size > 0
}

pub(crate) fn has_android_relr_tags(inputs: &DynamicEntryInputs) -> bool {
    inputs.args.use_android_relr_tags
}
