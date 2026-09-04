mod copy;
mod relocs;
mod relr;
mod rules;

use super::ELF_NUM_BUILT_IN_SECTIONS;
#[allow(unused_imports)]
use super::abi::*;
#[allow(unused_imports)]
use super::file::*;
#[allow(unused_imports)]
use super::gnu::*;
use super::output_section_id;
use super::part_id;
#[allow(unused_imports)]
use super::types::*;
use crate::alignment;
use crate::args::elf::ElfArgs;
use crate::bail;
use crate::debug_assert_bail;
use crate::error::Context as _;
use crate::error::Result;
use crate::gdb_index::InputDebugIndexSection;
use crate::layout;
use crate::layout::CommonGroupState;
use crate::layout::ObjectLayout;
use crate::layout::Resolution;
use crate::layout_rules::SectionKind;
use crate::output_kind::OutputKind;
use crate::output_section_id::SectionIdentity;
use crate::output_section_id::SectionName;
use crate::output_section_map::OutputSectionMap;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::part_id::PartId;
use crate::platform;
use crate::platform::Arch;
use crate::platform::ObjectFile;
use crate::platform::Relocation;
use crate::platform::ThunkConfig;
use crate::string_merging::MergedStringStartAddresses;
use crate::string_merging::MergedStringsSection;
use crate::value_flags::ValueFlags;
#[allow(unused_imports)]
pub(crate) use copy::*;
use linker_utils::elf::SectionFlags;
use linker_utils::elf::pt;
use linker_utils::elf::secnames::*;
use linker_utils::elf::shf;
use linker_utils::elf::sht;
use linker_utils::relaxation::RelocationModifier;
use rayon::Scope;
#[allow(unused_imports)]
pub(crate) use relocs::*;
#[allow(unused_imports)]
pub(crate) use relr::*;
#[allow(unused_imports)]
pub(crate) use rules::*;
use std::num::NonZeroU64;

impl<C: ElfClass> Elf<C> {
    pub(super) const DEFAULT_DEFS: BuiltInSectionDetails<C> = BuiltInSectionDetails {
        kind: Self::primary_section(&[]),
        section_flags: SectionFlags(0),
        link: &[],
        min_alignment: alignment::MIN,
        element_size: 0,
        ty: sht::NULL,
        is_relro: false,
        target_segment_type: None,
    };

    pub(super) const fn primary_section(name: &'static [u8]) -> SectionKind<'static, Elf<C>> {
        SectionKind::Primary(SectionIdentity::new(SectionName(name), ()))
    }

    pub(super) const SECTION_DEFINITIONS: [BuiltInSectionDetails<C>; ELF_NUM_BUILT_IN_SECTIONS] = {
        let mut defs = [Self::DEFAULT_DEFS; ELF_NUM_BUILT_IN_SECTIONS];

        // A section into which we write headers.
        defs[crate::output_section_id::FILE_HEADER.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(b""),
            section_flags: shf::ALLOC,
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::PROGRAM_HEADERS.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(PROGRAM_HEADERS_SECTION_NAME),
            section_flags: shf::ALLOC,
            min_alignment: C::PROGRAM_HEADER_ALIGNMENT,
            target_segment_type: Some(pt::PHDR),
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::SECTION_HEADERS.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(SECTION_HEADERS_SECTION_NAME),
            section_flags: shf::ALLOC,
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::SHSTRTAB.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(SHSTRTAB_SECTION_NAME),
            ty: sht::STRTAB,
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::STRTAB.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(STRTAB_SECTION_NAME),
            ty: sht::STRTAB,
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::GOT.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(GOT_SECTION_NAME),
            ty: sht::PROGBITS,
            section_flags: shf::WRITE.with(shf::ALLOC),
            element_size: C::GOT_ENTRY_SIZE,
            min_alignment: C::GOT_ENTRY_ALIGNMENT,
            is_relro: true,
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::GOT_RELR.as_usize()] = BuiltInSectionDetails {
            kind: SectionKind::Secondary(output_section_id::GOT),
            element_size: C::GOT_ENTRY_SIZE,
            min_alignment: C::GOT_ENTRY_ALIGNMENT,
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::PLT_GOT.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(PLT_GOT_SECTION_NAME),
            ty: sht::PROGBITS,
            section_flags: shf::ALLOC.with(shf::EXECINSTR),
            element_size: crate::elf::PLT_ENTRY_SIZE,
            min_alignment: alignment::PLT,
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::RELA_PLT.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(RELA_PLT_SECTION_NAME),
            ty: sht::RELA,
            section_flags: shf::ALLOC.with(shf::INFO_LINK),
            element_size: C::RELA_ENTRY_SIZE,
            link: &[output_section_id::DYNSYM, output_section_id::SYMTAB_LOCAL],
            min_alignment: C::RELA_ENTRY_ALIGNMENT,
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::EH_FRAME.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(EH_FRAME_SECTION_NAME),
            ty: sht::PROGBITS,
            section_flags: shf::ALLOC,
            min_alignment: C::ADDRESS_ALIGNMENT,
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::EH_FRAME_HDR.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(EH_FRAME_HDR_SECTION_NAME),
            ty: sht::PROGBITS,
            section_flags: shf::ALLOC,
            min_alignment: alignment::EH_FRAME_HDR,
            target_segment_type: Some(pt::GNU_EH_FRAME),
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::SFRAME.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(SFRAME_SECTION_NAME),
            ty: sht::GNU_SFRAME,
            section_flags: shf::ALLOC,
            min_alignment: C::ADDRESS_ALIGNMENT,
            target_segment_type: Some(pt::GNU_SFRAME),
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::DYNAMIC.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(DYNAMIC_SECTION_NAME),
            ty: sht::DYNAMIC,
            section_flags: shf::ALLOC.with(shf::WRITE),
            element_size: C::DYNAMIC_ENTRY_SIZE,
            link: &[output_section_id::DYNSTR],
            min_alignment: C::ADDRESS_ALIGNMENT,
            is_relro: true,
            target_segment_type: Some(pt::DYNAMIC),
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::HASH.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(HASH_SECTION_NAME),
            ty: sht::HASH,
            section_flags: shf::ALLOC,
            link: &[output_section_id::DYNSYM],
            min_alignment: alignment::SYSV_HASH,
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::GNU_HASH.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(GNU_HASH_SECTION_NAME),
            ty: sht::GNU_HASH,
            section_flags: shf::ALLOC,
            link: &[output_section_id::DYNSYM],
            min_alignment: C::GNU_HASH_ALIGNMENT,
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::DYNSYM.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(DYNSYM_SECTION_NAME),
            ty: sht::DYNSYM,
            section_flags: shf::ALLOC,
            element_size: C::SYMTAB_ENTRY_SIZE,
            link: &[output_section_id::DYNSTR],
            min_alignment: C::SYMTAB_ENTRY_ALIGNMENT,
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::DYNSTR.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(DYNSTR_SECTION_NAME),
            ty: sht::STRTAB,
            section_flags: shf::ALLOC,
            min_alignment: alignment::MIN,
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::INTERP.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(INTERP_SECTION_NAME),
            ty: sht::PROGBITS,
            section_flags: shf::ALLOC,
            target_segment_type: Some(pt::INTERP),
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::GNU_VERSION.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(GNU_VERSION_SECTION_NAME),
            ty: sht::GNU_VERSYM,
            section_flags: shf::ALLOC,
            element_size: size_of::<Versym>() as u64,
            min_alignment: alignment::VERSYM,
            link: &[output_section_id::DYNSYM],
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::GNU_VERSION_D.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(GNU_VERSION_D_SECTION_NAME),
            ty: sht::GNU_VERDEF,
            section_flags: shf::ALLOC,
            min_alignment: C::VERSION_D_ALIGNMENT,
            link: &[output_section_id::DYNSTR],
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::GNU_VERSION_R.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(GNU_VERSION_R_SECTION_NAME),
            ty: sht::GNU_VERNEED,
            section_flags: shf::ALLOC,
            min_alignment: C::VERSION_R_ALIGNMENT,
            link: &[output_section_id::DYNSTR],
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::NOTE_GNU_PROPERTY.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(NOTE_GNU_PROPERTY_SECTION_NAME),
            ty: sht::NOTE,
            section_flags: shf::ALLOC,
            min_alignment: C::GNU_PROPERTY_ALIGNMENT,
            target_segment_type: Some(pt::GNU_PROPERTY),
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::NOTE_GNU_BUILD_ID.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(NOTE_GNU_BUILD_ID_SECTION_NAME),
            ty: sht::NOTE,
            section_flags: shf::ALLOC,
            min_alignment: alignment::NOTE_GNU_BUILD_ID,
            ..Self::DEFAULT_DEFS
        };
        // Multi-part generated sections
        defs[output_section_id::SYMTAB_LOCAL.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(SYMTAB_SECTION_NAME),
            ty: sht::SYMTAB,
            element_size: C::SYMTAB_ENTRY_SIZE,
            min_alignment: C::SYMTAB_ENTRY_ALIGNMENT,
            link: &[output_section_id::STRTAB],
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::SYMTAB_GLOBAL.as_usize()] = BuiltInSectionDetails {
            kind: SectionKind::Secondary(output_section_id::SYMTAB_LOCAL),
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::RELA_DYN_RELATIVE.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(RELA_DYN_SECTION_NAME),
            ty: sht::RELA,
            section_flags: shf::ALLOC,
            element_size: C::RELA_ENTRY_SIZE,
            min_alignment: C::RELA_ENTRY_ALIGNMENT,
            link: &[output_section_id::DYNSYM],
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::RELA_DYN_GENERAL.as_usize()] = BuiltInSectionDetails {
            kind: SectionKind::Secondary(output_section_id::RELA_DYN_RELATIVE),
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::RELR_DYN.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(RELR_DYN_SECTION_NAME),
            ty: sht::RELR,
            section_flags: shf::ALLOC,
            element_size: C::RELR_ENTRY_SIZE,
            min_alignment: C::RELR_ENTRY_ALIGNMENT,
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::RISCV_ATTRIBUTES.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(RISCV_ATTRIBUTES_SECTION_NAME),
            ty: sht::RISCV_ATTRIBUTES,
            target_segment_type: Some(pt::RISCV_ATTRIBUTES),
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::RELRO_PADDING.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(RELRO_PADDING_SECTION_NAME),
            ty: sht::NOBITS,
            section_flags: shf::ALLOC.with(shf::WRITE),
            is_relro: true,
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::SYMTAB_SHNDX_LOCAL.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(SYMTAB_SHNDX_SECTION_NAME),
            ty: sht::SYMTAB_SHNDX,
            element_size: SYMTAB_SHNDX_ENTRY_SIZE,
            min_alignment: alignment::SYMTAB_SHNDX_ENTRY,
            link: &[output_section_id::SYMTAB_LOCAL],
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::SYMTAB_SHNDX_GLOBAL.as_usize()] = BuiltInSectionDetails {
            kind: SectionKind::Secondary(output_section_id::SYMTAB_SHNDX_LOCAL),
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::GDB_INDEX.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(GDB_INDEX_SECTION_NAME),
            ty: sht::PROGBITS,
            ..Self::DEFAULT_DEFS
        };
        // Start of regular sections
        defs[output_section_id::RODATA.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(RODATA_SECTION_NAME),
            ty: sht::PROGBITS,
            section_flags: shf::ALLOC,
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::INIT_ARRAY.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(INIT_ARRAY_SECTION_NAME),
            ty: sht::INIT_ARRAY,
            section_flags: shf::ALLOC.with(shf::WRITE),
            element_size: C::ADDRESS_SIZE,
            min_alignment: C::ADDRESS_ALIGNMENT,
            is_relro: true,
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::FINI_ARRAY.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(FINI_ARRAY_SECTION_NAME),
            ty: sht::FINI_ARRAY,
            section_flags: shf::ALLOC.with(shf::WRITE),
            element_size: C::ADDRESS_SIZE,
            min_alignment: C::ADDRESS_ALIGNMENT,
            is_relro: true,
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::PREINIT_ARRAY.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(PREINIT_ARRAY_SECTION_NAME),
            ty: sht::PREINIT_ARRAY,
            section_flags: shf::ALLOC.with(shf::WRITE),
            is_relro: true,
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::TEXT.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(TEXT_SECTION_NAME),
            ty: sht::PROGBITS,
            section_flags: shf::ALLOC.with(shf::EXECINSTR),
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::INIT.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(INIT_SECTION_NAME),
            ty: sht::PROGBITS,
            section_flags: shf::ALLOC.with(shf::EXECINSTR),
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::FINI.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(FINI_SECTION_NAME),
            ty: sht::PROGBITS,
            section_flags: shf::ALLOC.with(shf::EXECINSTR),
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::DATA.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(DATA_SECTION_NAME),
            ty: sht::PROGBITS,
            section_flags: shf::ALLOC.with(shf::WRITE),
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::TDATA.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(TDATA_SECTION_NAME),
            ty: sht::PROGBITS,
            section_flags: shf::WRITE.with(shf::ALLOC).with(shf::TLS),
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::TBSS.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(TBSS_SECTION_NAME),
            ty: sht::NOBITS,
            section_flags: shf::WRITE.with(shf::ALLOC).with(shf::TLS),
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::BSS.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(BSS_SECTION_NAME),
            ty: sht::NOBITS,
            section_flags: shf::ALLOC.with(shf::WRITE),
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::COMMENT.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(COMMENT_SECTION_NAME),
            ty: sht::PROGBITS,
            section_flags: shf::STRINGS.with(shf::MERGE),
            element_size: 1,
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::GCC_EXCEPT_TABLE.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(GCC_EXCEPT_TABLE_SECTION_NAME),
            ty: sht::PROGBITS,
            section_flags: shf::ALLOC,
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::NOTE_ABI_TAG.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(NOTE_ABI_TAG_SECTION_NAME),
            ty: sht::NOTE,
            section_flags: shf::ALLOC,
            ..Self::DEFAULT_DEFS
        };
        defs[output_section_id::DATA_REL_RO.as_usize()] = BuiltInSectionDetails {
            kind: Self::primary_section(DATA_REL_RO_SECTION_NAME),
            ty: sht::PROGBITS,
            section_flags: shf::ALLOC.with(shf::WRITE),
            is_relro: true,
            ..Self::DEFAULT_DEFS
        };

        defs
    };
}

impl<C: ElfClass> platform::BuiltInSectionDetails for BuiltInSectionDetails<C> {}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DynamicSymbolDefinitionExt {
    pub(crate) hash: u32,
    pub(crate) version: object::elf::VersymIndex,
    /// GNU ld emits an empty `STT_OBJECT` / `SHN_ABS` dynamic symbol named after
    /// each named version in a version script (except BASE). These are not backed
    /// by a `SymbolId`.
    pub(crate) is_version_node: bool,
}

pub(super) fn load_section_relocations<
    'scope,
    'data,
    C: ElfClass,
    A: Arch<Platform = Elf<C>>,
    R: Relocation<Platform = Elf<C>>,
>(
    state: &layout::ObjectLayoutState<'data, Elf<C>>,
    common: &mut CommonGroupState<'data, Elf<C>>,
    queue: &mut layout::LocalWorkQueue<Elf<C>>,
    resources: &'scope layout::GraphResources<'data, '_, Elf<C>>,
    section_index: object::SectionIndex,
    relocations: impl Iterator<Item = R>,
    scope: &Scope<'scope>,
) -> Result {
    let mut modifier = RelocationModifier::Normal;
    let mut relr_writer = RelrEncoder::default();
    for rel in relocations {
        if modifier == RelocationModifier::SkipNextRelocation {
            modifier = RelocationModifier::Normal;
            continue;
        }
        let section_header = state.object.section(section_index)?;
        let section_part_id =
            state.section_part_id(section_index, &resources.symbol_db.section_part_ids);
        modifier = process_relocation::<C, A, R>(
            state,
            common,
            &rel,
            section_header,
            section_part_id,
            resources,
            queue,
            false,
            scope,
            &mut relr_writer,
        )
        .with_context(|| {
            format!(
                "Failed to copy section {} from file {state}",
                layout::section_debug::<Elf<C>>(state.object, section_index)
            )
        })?;
    }

    Ok(())
}

#[derive(Default, Debug)]
pub(crate) struct PreludeLayoutStateExt {
    pub(super) needs_tlsld_got_entry: bool,
    pub(super) shstrtab_size: u64,
}

#[derive(Default, Debug)]
pub(crate) struct PreludeLayoutExt {
    pub(crate) got_plt_header_entries: u64,
    pub(crate) tlsld_got_entry: Option<NonZeroU64>,
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ResolutionExt {
    /// The base GOT address for this resolution. For pointers to symbols the GOT entry will
    /// contain a single pointer. For TLS variables there can be up to 3 pointers. If
    /// ValueFlags::GOT_TLS_OFFSET is set, then that will be the first value. If
    /// ValueFlags::GOT_TLS_MODULE is set, then there will be a pair of values (module and
    /// offset within module).
    pub(crate) got_address: Option<NonZeroU64>,
    pub(crate) plt_address: Option<NonZeroU64>,
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct SymtabShndxEntry {
    pub(crate) _shndx: u32,
}

/// Returns true if this GOT entry should go to GOT_RELR (bitmap-packed)
/// rather than GOT (flat RELR or RELA).
pub(crate) fn is_got_relr_eligible(
    flags: ValueFlags,
    has_dynamic_symbol: bool,
    args: &ElfArgs,
    output_kind: OutputKind,
) -> bool {
    args.is_relr_enabled()
        && !flags.is_ifunc()
        && !has_dynamic_symbol
        && flags.has_link_time_address()
        && !flags.is_downgraded_to_local()
        && output_kind.is_position_independent()
}

pub(super) fn got_relr_bitmap_relr_count<C: ElfClass>(n: u64) -> u64 {
    if n == 0 {
        0
    } else {
        1 + n.saturating_sub(1).div_ceil(relr_bitmap_slots::<C>())
    }
}

pub(super) fn allocate_got<C: ElfClass>(
    num_entries: u64,
    memory_offsets: &mut OutputSectionPartMap<u64>,
) -> NonZeroU64 {
    let got_address = NonZeroU64::new(memory_offsets.get(part_id::GOT)).unwrap();
    memory_offsets.increment(part_id::GOT, C::GOT_ENTRY_SIZE * num_entries);
    got_address
}

pub(super) fn allocate_got_relr<C: ElfClass>(
    memory_offsets: &mut OutputSectionPartMap<u64>,
) -> NonZeroU64 {
    let got_address = NonZeroU64::new(memory_offsets.get(part_id::GOT_RELR)).unwrap();
    memory_offsets.increment(part_id::GOT_RELR, C::GOT_ENTRY_SIZE);
    got_address
}

pub(super) fn allocate_plt(memory_offsets: &mut OutputSectionPartMap<u64>) -> NonZeroU64 {
    let plt_address = NonZeroU64::new(memory_offsets.get(part_id::PLT_GOT)).unwrap();
    memory_offsets.increment(part_id::PLT_GOT, PLT_ENTRY_SIZE);
    plt_address
}

impl<C: ElfClass> Resolution<Elf<C>> {
    pub(crate) fn got_address(&self) -> Result<u64> {
        Ok(self
            .format_specific
            .got_address
            .context("Missing GOT address")?
            .get())
    }

    pub(crate) fn got_address_for_relocation(&self) -> Result<u64> {
        let mut got_address = self.got_address()?;
        if self.flags.needs_ifunc_got_for_address()
            || self.flags.needs_canonical_plt_got_for_address()
        {
            got_address += C::GOT_ENTRY_SIZE;
        }
        Ok(got_address)
    }

    pub(crate) fn tlsgd_got_address(&self) -> Result<u64> {
        debug_assert_bail!(
            self.flags.needs_got_tls_module(),
            "Called tlsgd_got_address without GOT_TLS_MODULE being set"
        );
        // If we've got both a GOT_TLS_OFFSET and a GOT_TLS_MODULE, then the latter comes second.
        let mut got_address = self.got_address()?;
        if self.flags.needs_got_tls_offset() {
            got_address += C::GOT_ENTRY_SIZE;
        }
        Ok(got_address)
    }

    pub(crate) fn tls_descriptor_got_address(&self) -> Result<u64> {
        debug_assert_bail!(
            self.flags.needs_got_tls_descriptor(),
            "Called tls_descriptor_got_address without GOT_TLS_DESCRIPTOR being set"
        );
        // We might have both GOT_TLS_OFFSET, GOT_TLS_MODULE and GOT_TLS_DESCRIPTOR at the same time
        // for a single symbol. Then the TLS descriptor comes as the last one.
        let mut got_address = self.got_address()?;
        if self.flags.needs_got_tls_offset() {
            got_address += C::GOT_ENTRY_SIZE;
        }
        if self.flags.needs_got_tls_module() {
            got_address += 2 * C::GOT_ENTRY_SIZE;
        }

        Ok(got_address)
    }

    pub(crate) fn plt_address(&self) -> Result<u64> {
        Ok(self
            .format_specific
            .plt_address
            .context("Missing PLT address")?
            .get())
    }

    #[inline(always)]
    pub(crate) fn value_with_addend<'data>(
        &self,
        addend: i64,
        symbol_index: object::SymbolIndex,
        object_layout: &ObjectLayout<'data, Elf<C>>,
        section_part_ids: &[PartId],
        merged_strings: &OutputSectionMap<MergedStringsSection>,
        merged_string_start_addresses: &MergedStringStartAddresses,
    ) -> Result<u64> {
        if self.flags.is_ifunc() {
            return Ok(self.plt_address()?.wrapping_add(addend as u64));
        }

        // For most symbols, `raw_value` won't be zero, so we can save ourselves from looking up the
        // section to see if it's a string-merge section. For string-merge symbols with names,
        // `raw_value` will have already been computed, so we can avoid computing it again.
        if self.raw_value == 0
            && let Some(r) = crate::string_merging::get_merged_string_output_address::<Elf<C>>(
                symbol_index,
                addend,
                object_layout.object,
                &object_layout.sections,
                section_part_ids,
                object_layout.section_id_range,
                merged_strings,
                merged_string_start_addresses,
                false,
            )?
        {
            if self.raw_value != 0 {
                bail!("Merged string resolution has value 0x{}", self.raw_value);
            }
            return Ok(r);
        }
        Ok(self.raw_value.wrapping_add(addend as u64))
    }
}

#[derive(Debug, Default)]
pub(crate) struct ResolvedObjectExt<'data> {
    pub(super) debug_index_sections: Vec<InputDebugIndexSection<'data>>,
}

/// Returns the thunk config for the architecture of the given object. This is only needed in
/// contexts that aren't currently generic over Arch.
pub(super) fn thunk_config_for_object<C: ElfClass>(file: &File<'_, C>) -> Option<ThunkConfig> {
    match file.arch {
        crate::arch::Architecture::AArch64 => crate::elf_aarch64::ElfAArch64::thunk_config(),
        _ => None,
    }
}
