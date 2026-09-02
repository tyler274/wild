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
use crate::alignment::Alignment;
use crate::args::elf::ElfArgs;
use crate::bail;
use crate::debug_assert_bail;
use crate::error::Context as _;
use crate::error::Result;
use crate::gdb_index::InputDebugIndexSection;
use crate::layout;
use crate::layout::CommonGroupState;
use crate::layout::HandlerData as _;
use crate::layout::ObjectLayout;
use crate::layout::ObjectLayoutState;
use crate::layout::Resolution;
use crate::layout_rules::SectionKind;
use crate::layout_rules::SectionRule;
use crate::layout_rules::SectionRuleOutcome;
use crate::output_kind::OutputKind;
use crate::output_section_id::SectionIdentity;
use crate::output_section_id::SectionName;
use crate::output_section_map::OutputSectionMap;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::part_id::PartId;
use crate::platform;
use crate::platform::Arch;
use crate::platform::Args as _;
use crate::platform::ObjectFile;
use crate::platform::Platform;
use crate::platform::Relaxation as _;
use crate::platform::Relocation;
use crate::platform::SectionFlags as _;
use crate::platform::SectionHeader as _;
use crate::platform::Symbol as _;
use crate::platform::ThunkConfig;
use crate::string_merging::MergedStringStartAddresses;
use crate::string_merging::MergedStringsSection;
use crate::symbol_db::SymbolDb;
use crate::symbol_db::SymbolId;
use crate::timing_phase;
use crate::value_flags::AtomicPerSymbolFlags;
use crate::value_flags::ValueFlags;
use crate::verbose_timing_phase;
use hashbrown::HashMap;
use itertools::Itertools as _;
use linker_utils::elf::RelocationKind;
use linker_utils::elf::SectionFlags;
use linker_utils::elf::pt;
use linker_utils::elf::secnames;
use linker_utils::elf::secnames::*;
use linker_utils::elf::shf;
use linker_utils::elf::sht;
use linker_utils::relaxation::RelocationModifier;
use object::LittleEndian;
use object::read::elf::SectionHeader as _;
use rayon::Scope;
use rayon::prelude::*;
use std::marker::PhantomData;
use std::num::NonZeroU64;
use std::ops::Range;
use std::sync::atomic;

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

pub(crate) const fn relr_bitmap_slots<C: ElfClass>() -> u64 {
    C::RELR_ENTRY_SIZE * 8 - 1
}

pub(super) struct RelrBitmap<C: ElfClass> {
    pub(super) range: Range<u64>,
    pub(super) encoded: u64,
    pub(super) class: PhantomData<C>,
}

impl<C: ElfClass> RelrBitmap<C> {
    // Return bitmap starting after the current address.
    pub(super) fn after(address: u64) -> Self {
        let start = address + C::RELR_ENTRY_SIZE;
        Self {
            range: start..start + relr_bitmap_slots::<C>() * C::RELR_ENTRY_SIZE,
            encoded: 1,
            class: PhantomData,
        }
    }

    // Return bitmap that will follow after the current bitmap range.
    pub(super) fn next(&self) -> Self {
        let address_range = relr_bitmap_slots::<C>() * C::RELR_ENTRY_SIZE;
        Self {
            range: self.range.start + address_range..self.range.end + address_range,
            encoded: 1,
            class: PhantomData,
        }
    }

    // Encoding address if properly aligned and fits in the current range. If fits, true is
    // returned.
    pub(super) fn insert(&mut self, address: u64) -> bool {
        let offset = address.wrapping_sub(self.range.start);
        if !self.range.contains(&address) || !offset.is_multiple_of(C::RELR_ENTRY_SIZE) {
            false
        } else {
            self.encoded |= 1 << (offset / C::RELR_ENTRY_SIZE + 1);
            true
        }
    }
}

/// Tracks RELR bitmap packing within one input section. Runs deliberately don't
/// cross section boundaries because layout of separate sections is parallel.
#[derive(Default)]
pub(super) enum RelrState<C: ElfClass> {
    #[default]
    NoRun,
    AddressOnly {
        next_bitmap: RelrBitmap<C>,
    },
    WithBitmap {
        bitmap: RelrBitmap<C>,
    },
}

#[derive(Clone, Copy)]
pub(crate) enum RelrEntryEncoding {
    New,
    Update,
}

#[derive(Default)]
pub(crate) struct RelrEncoder<C: ElfClass> {
    pub(super) state: RelrState<C>,
}

// RELR bitmap packing state used for both allocation and the actual writing of relocations
// to the output stream.
impl<C: ElfClass> RelrEncoder<C> {
    pub(crate) fn encode(
        &mut self,
        address: u64,
        mut encode_fn: impl FnMut(u64, RelrEntryEncoding) -> Result,
    ) -> Result {
        self.state = match std::mem::take(&mut self.state) {
            RelrState::NoRun => {
                encode_fn(address, RelrEntryEncoding::New)?;
                RelrState::AddressOnly {
                    next_bitmap: RelrBitmap::after(address),
                }
            }
            RelrState::AddressOnly { mut next_bitmap } => {
                if next_bitmap.insert(address) {
                    encode_fn(next_bitmap.encoded, RelrEntryEncoding::New)?;
                    RelrState::WithBitmap {
                        bitmap: next_bitmap,
                    }
                } else {
                    encode_fn(address, RelrEntryEncoding::New)?;
                    RelrState::AddressOnly {
                        next_bitmap: RelrBitmap::after(address),
                    }
                }
            }
            RelrState::WithBitmap { mut bitmap } => {
                if bitmap.insert(address) {
                    encode_fn(bitmap.encoded, RelrEntryEncoding::Update)?;
                    RelrState::WithBitmap { bitmap }
                } else {
                    // Current window has bits — try next window.
                    // lld only advances to a new bitmap if the current one is
                    // non-empty (breaks on empty bitmap). Same rule here.
                    let mut next_bitmap = bitmap.next();
                    if next_bitmap.insert(address) {
                        encode_fn(next_bitmap.encoded, RelrEntryEncoding::New)?;
                        RelrState::WithBitmap {
                            bitmap: next_bitmap,
                        }
                    } else {
                        // Gap too large — start new address entry.
                        encode_fn(address, RelrEntryEncoding::New)?;
                        RelrState::AddressOnly {
                            next_bitmap: RelrBitmap::after(address),
                        }
                    }
                }
            }
        };
        Ok(())
    }
}

#[inline(always)]
pub(super) fn process_relocation<
    'data,
    'scope,
    C: ElfClass,
    A: Arch<Platform = Elf<C>>,
    R: Relocation<Platform = Elf<C>>,
>(
    object: &ObjectLayoutState<'data, Elf<C>>,
    common: &mut CommonGroupState<'data, Elf<C>>,
    rel: &R,
    section: &<A::Platform as Platform>::SectionHeader,
    section_part_id: PartId,
    resources: &'scope layout::GraphResources<'data, '_, Elf<C>>,
    queue: &mut layout::LocalWorkQueue<Elf<C>>,
    is_debug_section: bool,
    scope: &Scope<'scope>,
    relr_writer: &mut RelrEncoder<C>,
) -> Result<RelocationModifier> {
    let Some(local_sym_index) = rel.symbol() else {
        return Ok(RelocationModifier::Normal);
    };

    let mut classified =
        classify_symbol_relocation::<C, A, R>(object, rel, section, local_sym_index, resources)?;

    materialize_relocation_requirements::<C, A, R>(
        common,
        rel,
        section,
        resources,
        is_debug_section,
        relr_writer,
        &mut classified,
    )?;

    let previous_flags =
        note_relocation_symbol_reference::<C, A>(&classified, resources, queue, scope);

    if !is_debug_section {
        crate::thunks::handle_thunk_extensions_for_relocation::<A>(
            section_part_id,
            resources,
            classified.local_symbol_id,
            classified.symbol_id,
            classified.r_type,
        );
    }

    layout::check_for_undefined::<A>(
        object,
        section,
        classified.rel_offset,
        local_sym_index,
        classified.flags,
        classified.symbol_id,
        resources,
    )?;

    if classified.flags_to_add.needs_copy_relocation() && !previous_flags.needs_copy_relocation() {
        queue.send_copy_relocation_request::<A>(classified.symbol_id, resources, scope);
    }

    Ok(classified.next_modifier)
}

/// Symbol and relocation-kind info needed for both GC edges and output accounting.
pub(super) struct ClassifiedSymbolRelocation {
    pub(super) local_symbol_id: SymbolId,
    pub(super) symbol_id: SymbolId,
    /// Definition (and local) value flags before this relocation's contributions.
    pub(super) flags: ValueFlags,
    pub(super) flags_to_add: ValueFlags,
    pub(super) rel_offset: u64,
    pub(super) r_type: object::elf::RelocationType,
    pub(super) rel_kind: linker_utils::elf::RelocationKind,
    pub(super) next_modifier: RelocationModifier,
    pub(super) section_is_writable: bool,
}

/// Resolve the relocated symbol and determine the effective relocation kind / initial flags.
#[inline(always)]
pub(super) fn classify_symbol_relocation<
    'data,
    C: ElfClass,
    A: Arch<Platform = Elf<C>>,
    R: Relocation<Platform = Elf<C>>,
>(
    object: &ObjectLayoutState<'data, Elf<C>>,
    rel: &R,
    section: &<A::Platform as Platform>::SectionHeader,
    local_sym_index: object::SymbolIndex,
    resources: &layout::GraphResources<'data, '_, Elf<C>>,
) -> Result<ClassifiedSymbolRelocation> {
    let args = resources.symbol_db.args;
    let symbol_db = resources.symbol_db;
    let local_symbol_id = object.symbol_id_range.input_to_id(local_sym_index);
    let symbol_id = symbol_db.definition(local_symbol_id);
    let mut flags = resources.local_flags_for_symbol(symbol_id);
    flags.merge(resources.local_flags_for_symbol(local_symbol_id));
    let rel_offset = rel.offset();
    let r_type = rel.raw_type();
    let section_flags = section.sh_flags(LittleEndian);

    let mut next_modifier = RelocationModifier::Normal;
    let rel_info = if let Some(relaxation) = A::new_relaxation(
        r_type,
        object.object.raw_section_data(section)?,
        rel_offset,
        flags,
        symbol_db.output_kind,
        section_flags,
        None,
        1,
        0,
        0,
        None,
    )
    .filter(|relaxation| args.should_relax() || relaxation.is_mandatory())
    {
        next_modifier = relaxation.next_modifier();
        relaxation.rel_info()
    } else {
        A::relocation_from_raw(r_type)?
    };

    Ok(ClassifiedSymbolRelocation {
        local_symbol_id,
        symbol_id,
        flags,
        flags_to_add: layout::resolution_flags(rel_info.kind),
        rel_offset,
        r_type,
        rel_kind: rel_info.kind,
        next_modifier,
        section_is_writable: section.is_writable(),
    })
}

/// Account for GOT/PLT/dynamic-reloc/TLS sizes implied by this relocation.
#[inline(always)]
pub(super) fn materialize_relocation_requirements<
    'data,
    C: ElfClass,
    A: Arch<Platform = Elf<C>>,
    R: Relocation<Platform = Elf<C>>,
>(
    common: &mut CommonGroupState<'data, Elf<C>>,
    rel: &R,
    section: &<A::Platform as Platform>::SectionHeader,
    resources: &layout::GraphResources<'data, '_, Elf<C>>,
    is_debug_section: bool,
    relr_writer: &mut RelrEncoder<C>,
    classified: &mut ClassifiedSymbolRelocation,
) -> Result {
    let args = resources.symbol_db.args;
    let symbol_db = resources.symbol_db;
    let section_flags = section.sh_flags(LittleEndian);
    let flags = classified.flags;
    let symbol_id = classified.symbol_id;
    let r_type = classified.r_type;
    let section_is_writable = classified.section_is_writable;
    let flags_to_add = &mut classified.flags_to_add;
    let rel_kind = classified.rel_kind;

    if !section_flags.is_alloc() {
        // Non-alloc sections never get dynamic relocations, so there's nothing to do here.
    } else if rel_kind.is_tls() {
        if does_relocation_require_static_tls(rel_kind) {
            resources
                .has_static_tls
                .store(true, atomic::Ordering::Relaxed);
        }

        if layout::needs_tlsld(rel_kind)
            && !resources
                .layout_resources_ext
                .uses_tlsld
                .load(atomic::Ordering::Relaxed)
        {
            resources
                .layout_resources_ext
                .uses_tlsld
                .store(true, atomic::Ordering::Relaxed);
        }
    } else if flags_to_add.needs_direct() && flags.is_interposable() {
        if symbol_db.output_kind.is_shared_object()
            && A::is_disallowed_for_interposable_symbols(r_type)
        {
            bail!(
                "relocation {} cannot be used when making a shared object; \
                recompile with -fPIC",
                A::rel_type_to_string(r_type),
            );
        }
        if section_is_writable {
            common.allocate(part_id::RELA_DYN_GENERAL, C::RELA_ENTRY_SIZE);
        } else if flags.is_function() {
            // Create a PLT entry for the function and refer to that instead.
            flags_to_add.remove(ValueFlags::DIRECT);
            *flags_to_add |= ValueFlags::PLT | ValueFlags::GOT;
        } else if !flags.is_absolute() {
            match args.copy_relocations_enabled() {
                crate::args::CopyRelocations::Allowed => {
                    *flags_to_add |= ValueFlags::COPY_RELOCATION;
                }
                crate::args::CopyRelocations::Disallowed(reason) => {
                    // We don't at present support text relocations, so if we can't apply a copy
                    // relocation, we error instead.
                    bail!(
                        "Direct relocation ({}) to dynamic symbol from non-writable section, \
                        but copy relocations are disabled because {reason}. {}",
                        A::rel_type_to_string(r_type),
                        resources.symbol_debug(symbol_id),
                    );
                }
            }
        }
    } else if flags.is_ifunc()
        && rel_kind == RelocationKind::Absolute
        && section_is_writable
        && symbol_db.output_kind.is_position_independent()
    {
        common.allocate(part_id::RELA_DYN_GENERAL, C::RELA_ENTRY_SIZE);
    } else if symbol_db.output_kind.is_position_independent()
        && rel_kind == RelocationKind::Absolute
        && flags.has_link_time_address()
    {
        if section_is_writable {
            // Odd offsets can't be encoded as RELR address entries (LSB used as
            // bitmap marker), so fall back to RELA for them.
            if resources.symbol_db.args.is_relr_enabled() && rel.offset().is_multiple_of(2) {
                relr_writer.encode(rel.offset(), |_, encoding| {
                    if matches!(encoding, RelrEntryEncoding::New) {
                        common.allocate(part_id::RELR_DYN, C::RELR_ENTRY_SIZE);
                    }
                    Ok(())
                })?;
            } else {
                common.allocate(part_id::RELA_DYN_RELATIVE, C::RELA_ENTRY_SIZE);
            }
        } else if !is_debug_section {
            bail!(
                "Cannot apply relocation {} to read-only section. \
                Please recompile with -fPIC or link with -no-pie",
                A::rel_type_to_string(r_type),
            );
        }
    }

    // For ifunc symbols with GOT-relative references (like R_X86_64_GOTPCRELX), we need a
    // separate GOT entry for address equality. The main GOT entry will be used by the PLT stub
    // with an IRELATIVE relocation, while this extra entry will contain the PLT stub address so
    // that all references to the ifunc return the same address.

    let relocation_needs_got = flags_to_add.needs_got();

    if flags.is_ifunc() && !symbol_db.output_kind.is_static_executable() {
        *flags_to_add |= ValueFlags::GOT | ValueFlags::PLT;
    }

    if flags.is_ifunc() && relocation_needs_got && symbol_db.output_kind.has_fixed_load_address() {
        *flags_to_add |= ValueFlags::IFUNC_GOT_FOR_ADDRESS;
    }

    Ok(())
}

/// Record that a live section references `classified.symbol_id` and enqueue graph work if needed.
#[inline(always)]
pub(super) fn note_relocation_symbol_reference<
    'data,
    'scope,
    C: ElfClass,
    A: Arch<Platform = Elf<C>>,
>(
    classified: &ClassifiedSymbolRelocation,
    resources: &'scope layout::GraphResources<'data, '_, Elf<C>>,
    queue: &mut layout::LocalWorkQueue<Elf<C>>,
    scope: &Scope<'scope>,
) -> ValueFlags {
    let symbol_id = classified.symbol_id;
    let flags = classified.flags;
    let flags_to_add = classified.flags_to_add;

    let atomic_flags = &resources.per_symbol_flags.get_atomic(symbol_id);
    let previous_flags = atomic_flags.fetch_or(flags_to_add);

    if !previous_flags.has_resolution() {
        if flags.is_ifunc() && resources.symbol_db.output_kind.is_static_executable() {
            atomic_flags.fetch_or(ValueFlags::GOT | ValueFlags::PLT);
        }

        queue.send_symbol_request::<A>(symbol_id, resources, scope);
    }

    previous_flags
}

/// Returns whether the supplied relocation type requires static TLS. If true and we're writing a
/// shared object, then the STATIC_TLS will be set in the shared object which is a signal to the
/// runtime loader that the shared object cannot be loaded at runtime (e.g. with dlopen).
pub(super) fn does_relocation_require_static_tls(rel_kind: RelocationKind) -> bool {
    layout::resolution_flags(rel_kind) == ValueFlags::GOT_TLS_OFFSET
}

#[derive(Default, Debug)]
pub(crate) struct PreludeLayoutStateExt {
    pub(super) needs_tlsld_got_entry: bool,
    pub(super) shstrtab_size: u64,
}

#[derive(Default, Debug)]
pub(crate) struct PreludeLayoutExt {
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
        if self.flags.needs_ifunc_got_for_address() {
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

/// Rules that map input sections to built-in output sections when no linker script is in use.
pub(super) const DEFAULT_SECTION_PLACEMENT_RULES: &[SectionRule<'static>] = &[
    SectionRule::exact_section_keep(secnames::INIT_SECTION_NAME, output_section_id::INIT),
    SectionRule::exact_section_keep(secnames::FINI_SECTION_NAME, output_section_id::FINI),
    SectionRule::exact_section_keep(
        secnames::PREINIT_ARRAY_SECTION_NAME,
        output_section_id::PREINIT_ARRAY,
    ),
    SectionRule::exact_section_keep(secnames::COMMENT_SECTION_NAME, output_section_id::COMMENT),
    SectionRule::exact_section_keep(
        secnames::NOTE_ABI_TAG_SECTION_NAME,
        output_section_id::NOTE_ABI_TAG,
    ),
    SectionRule::exact_section(
        secnames::NOTE_GNU_BUILD_ID_SECTION_NAME,
        output_section_id::NOTE_GNU_BUILD_ID,
    ),
    SectionRule::prefix_section(secnames::RODATA_SECTION_NAME, output_section_id::RODATA),
    SectionRule::prefix_section(secnames::TEXT_SECTION_NAME, output_section_id::TEXT),
    SectionRule::prefix_section(
        secnames::DATA_REL_RO_SECTION_NAME,
        output_section_id::DATA_REL_RO,
    ),
    SectionRule::prefix_section(secnames::DATA_SECTION_NAME, output_section_id::DATA),
    SectionRule::prefix_section(secnames::BSS_SECTION_NAME, output_section_id::BSS),
    SectionRule::prefix_section_sort(
        secnames::INIT_ARRAY_SECTION_NAME,
        output_section_id::INIT_ARRAY,
    ),
    SectionRule::prefix_section_sort(secnames::CTORS_SECTION_NAME, output_section_id::INIT_ARRAY),
    SectionRule::prefix_section_sort(
        secnames::FINI_ARRAY_SECTION_NAME,
        output_section_id::FINI_ARRAY,
    ),
    SectionRule::prefix_section_sort(secnames::DTORS_SECTION_NAME, output_section_id::FINI_ARRAY),
    SectionRule::prefix_section(secnames::TDATA_SECTION_NAME, output_section_id::TDATA),
    SectionRule::prefix_section(secnames::TBSS_SECTION_NAME, output_section_id::TBSS),
    SectionRule::prefix_section(
        secnames::GCC_EXCEPT_TABLE_SECTION_NAME,
        output_section_id::GCC_EXCEPT_TABLE,
    ),
];

/// Rules for input sections that the linker processes itself instead of copying them into an output
/// section.
pub(super) const LINKER_MANAGED_SECTION_RULES: &[SectionRule<'static>] = &[
    SectionRule::prefix(secnames::RELA_SECTION_NAME, SectionRuleOutcome::Discard),
    SectionRule::prefix(secnames::CREL_SECTION_NAME, SectionRuleOutcome::Discard),
    SectionRule::exact(
        secnames::NOTE_GNU_STACK_SECTION_NAME,
        SectionRuleOutcome::NoteGnuStack,
    ),
    SectionRule::exact(secnames::STRTAB_SECTION_NAME, SectionRuleOutcome::Discard),
    SectionRule::exact(secnames::SYMTAB_SECTION_NAME, SectionRuleOutcome::Discard),
    SectionRule::exact(secnames::SHSTRTAB_SECTION_NAME, SectionRuleOutcome::Discard),
    SectionRule::exact(secnames::GROUP_SECTION_NAME, SectionRuleOutcome::Discard),
    SectionRule::exact(secnames::EH_FRAME_SECTION_NAME, SectionRuleOutcome::EhFrame),
    SectionRule::exact(
        secnames::NOTE_GNU_PROPERTY_SECTION_NAME,
        SectionRuleOutcome::NoteGnuProperty,
    ),
    SectionRule::exact(
        secnames::RISCV_ATTRIBUTES_SECTION_NAME,
        SectionRuleOutcome::RiscVAttribute,
    ),
    SectionRule::exact(
        secnames::SYMTAB_SHNDX_SECTION_NAME,
        SectionRuleOutcome::Discard,
    ),
    SectionRule::prefix(b".debug_", SectionRuleOutcome::Debug),
];

pub(crate) fn init_fini_priority(name: &[u8]) -> Option<u16> {
    if name == secnames::INIT_ARRAY_SECTION_NAME || name == secnames::FINI_ARRAY_SECTION_NAME {
        return Some(u16::MAX);
    }

    if let Some(rest) = name.strip_prefix(b".init_array.") {
        return parse_priority_suffix(rest);
    }

    if let Some(rest) = name.strip_prefix(b".fini_array.") {
        return parse_priority_suffix(rest);
    }

    // .ctors and .dtors without suffix have the same priority as .init_array/.fini_array
    if name == secnames::CTORS_SECTION_NAME || name == secnames::DTORS_SECTION_NAME {
        return Some(u16::MAX);
    }

    // .ctors uses descending order (65535 = lowest priority, 0 = highest)
    // while .init_array uses ascending order (0 = highest priority, 65535 = lowest)
    if let Some(rest) = name.strip_prefix(b".ctors.") {
        return parse_priority_suffix(rest).map(|p| u16::MAX.saturating_sub(p));
    }

    if let Some(rest) = name.strip_prefix(b".dtors.") {
        return parse_priority_suffix(rest).map(|p| u16::MAX.saturating_sub(p));
    }

    None
}

pub(super) fn parse_priority_suffix(suffix: &[u8]) -> Option<u16> {
    if suffix.is_empty() || !suffix.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }

    let value = core::str::from_utf8(suffix).ok()?.parse::<u32>().ok()?;
    Some(u16::try_from(value).unwrap_or(u16::MAX))
}

pub(crate) fn program_headers_size<C: ElfClass>(header_info: &layout::HeaderInfo) -> u64 {
    u64::from(C::PROGRAM_HEADER_SIZE) * header_info.active_segment_ids.len() as u64
}

pub(super) fn section_headers_size<C: ElfClass>(header_info: &layout::HeaderInfo) -> u64 {
    u64::from(C::SECTION_HEADER_SIZE) * u64::from(header_info.num_output_sections_with_content)
}

/// Where we've decided that we need copy relocations, look for symbols with the same address as the
/// symbols with copy relocations. If the other symbol is non-weak, then we do the copy relocation
/// for that symbol instead. We also request dynamic symbol definitions for each copy relocation.
/// For that reason, this needs to be done before we merge dynamic symbol definitions.
pub(super) fn finalise_copy_relocations<'data, C: ElfClass>(
    group_states: &mut [layout::GroupState<'data, Elf<C>>],
    symbol_db: &SymbolDb<'data, Elf<C>>,
    symbol_flags: &AtomicPerSymbolFlags,
) -> Result {
    timing_phase!("Finalise copy relocations");

    group_states.par_iter_mut().try_for_each(|group| {
        verbose_timing_phase!("Finalise copy relocations for group");
        for file in &mut group.files {
            if let layout::FileLayoutState::Dynamic(dynamic) = file {
                // Skip iterating over our symbol table if we don't have any copy relocations.
                if dynamic.format_specific.copy_relocations.is_empty() {
                    continue;
                }

                select_copy_relocation_alternatives(
                    dynamic,
                    symbol_flags,
                    &mut group.common,
                    symbol_db,
                )?;
            }
        }

        Ok(())
    })
}

/// Looks for any non-weak symbols at the same addresses as any of our copy relocations. If
/// found, we'll generate the copy relocation for the strong symbol instead of weak symbol at
/// the same address.
pub(super) fn select_copy_relocation_alternatives<'data, C: ElfClass>(
    state: &mut layout::DynamicLayoutState<'data, Elf<C>>,
    per_symbol_flags: &AtomicPerSymbolFlags,
    common: &mut CommonGroupState<'data, Elf<C>>,
    symbol_db: &SymbolDb<'data, Elf<C>>,
) -> Result {
    for (i, symbol) in state.object.enumerate_symbols() {
        let address = symbol.value();
        let Some(info) = state.format_specific.copy_relocations.get_mut(&address) else {
            continue;
        };

        let symbol_id = state.symbol_id_range.input_to_id(i);

        if !symbol_db.is_canonical(symbol_id) {
            continue;
        }

        layout::export_dynamic(common, symbol_id, symbol_db)?;

        per_symbol_flags
            .get_atomic(symbol_id)
            .fetch_or(ValueFlags::COPY_RELOCATION);

        if symbol.is_weak() || !info.is_weak || info.symbol_id == symbol_id {
            continue;
        }

        info.symbol_id = symbol_id;
        info.is_weak = false;
    }

    Ok(())
}

pub(super) fn allocate_for_copy_relocations<'data, C: ElfClass>(
    state: &layout::DynamicLayoutState<'data, Elf<C>>,
    common: &mut CommonGroupState<'data, Elf<C>>,
) -> Result {
    for value in state.format_specific.copy_relocations.values() {
        let symbol_id = value.symbol_id;

        let symbol_index = state.symbol_id_range().id_to_input(symbol_id);
        let symbol = state.object.symbol(symbol_index)?;

        let section_index = state
            .object
            .symbol_section(symbol, symbol_index)?
            .context("Copy relocation for undefined symbol")?;
        let section = state.object.section(section_index)?;

        let alignment = Alignment::new(state.object.section_alignment(section)?)?;

        // Allocate space in BSS for the copy of the symbol.
        let size = symbol.size();
        common.allocate(
            output_section_id::BSS.part_id_with_alignment::<Elf<C>>(alignment),
            alignment.align_up(size),
        );

        // Allocate space required for the copy relocation itself.
        common.allocate(part_id::RELA_DYN_GENERAL, C::RELA_ENTRY_SIZE);
    }

    Ok(())
}

pub(super) fn assign_copy_relocation_addresses<'data, C: ElfClass>(
    state: &layout::DynamicLayoutState<'data, Elf<C>>,
    copy_relocation_symbols: &[SymbolId],
    memory_offsets: &mut OutputSectionPartMap<u64>,
) -> Result<HashMap<u64, u64>> {
    copy_relocation_symbols
        .iter()
        .map(|symbol_id| {
            let symbol_index = state.symbol_id_range.id_to_input(*symbol_id);
            let symbol = state.object.symbol(symbol_index)?;

            let section_index = state
                .object
                .symbol_section(symbol, symbol_index)?
                .context("Copy relocation for undefined symbol")?;
            let section = state.object.section(section_index)?;

            let alignment = Alignment::new(state.object.section_alignment(section)?)?;

            let input_address = symbol.value();
            let output_address =
                assign_copy_relocation_address::<C>(alignment, symbol.size(), memory_offsets);

            Ok((input_address, output_address))
        })
        .try_collect()
}

/// Assigns the address in BSS for the copy relocation of a symbol.
pub(super) fn assign_copy_relocation_address<C: ElfClass>(
    alignment: Alignment,
    size: u64,
    memory_offsets: &mut OutputSectionPartMap<u64>,
) -> u64 {
    let bss =
        memory_offsets.get_mut(output_section_id::BSS.part_id_with_alignment::<Elf<C>>(alignment));
    let a = *bss;
    *bss += alignment.align_up(size);
    a
}

impl CopyRelocationInfo {
    pub(super) fn add_symbol<'data, P: Platform>(
        &mut self,
        symbol_id: SymbolId,
        is_weak: bool,
        resources: &layout::GraphResources<'data, '_, P>,
    ) {
        if self.symbol_id == symbol_id || is_weak {
            return;
        }

        if !self.is_weak {
            resources.symbol_db.warning(format!(
                "Multiple non-weak symbols at the same address have copy relocations: {}, {}",
                resources.symbol_debug(self.symbol_id),
                resources.symbol_debug(symbol_id)
            ));
        }

        self.symbol_id = symbol_id;
        self.is_weak = false;
    }
}

/// Returns the thunk config for the architecture of the given object. This is only needed in
/// contexts that aren't currently generic over Arch.
pub(super) fn thunk_config_for_object<C: ElfClass>(file: &File<'_, C>) -> Option<ThunkConfig> {
    match file.arch {
        crate::arch::Architecture::AArch64 => crate::elf_aarch64::ElfAArch64::thunk_config(),
        _ => None,
    }
}
