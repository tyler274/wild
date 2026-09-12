use super::{DynamicTagValues, NoteProperty};
use crate::writable_elf::{
    WritableCompressionHeader, WritableDynamicEntry, WritableFileHeader, WritableNoteHeader,
    WritableProgramHeader, WritableRela, WritableRelr, WritableSectionHeader, WritableSymbol,
};
use object::LittleEndian;
use object::read::elf::{Crel, CrelIterator};
use std::marker::PhantomData;
use std::ops::Range;
use wild_error::error;
use wild_error::error::Result;
use wild_platform as platform;
use wild_platform::{Relocation, RelocationSequence};
use wild_util::alignment::Alignment;
use wild_util::arch::Architecture;
use zerocopy::{FromBytes, IntoBytes};

pub trait ElfWord: Copy + FromBytes + IntoBytes + Into<u64> + Send + Sync {
    fn from_u64(value: u64) -> Result<Self>;
    fn from_le_bytes(bytes: &[u8]) -> Self;
}

impl ElfWord for u32 {
    fn from_u64(value: u64) -> Result<Self> {
        u32::try_from(value).map_err(|_| error!("ELF word value 0x{value:x} does not fit in ELF32"))
    }

    fn from_le_bytes(bytes: &[u8]) -> Self {
        u32::from_le_bytes(bytes.try_into().unwrap())
    }
}

impl ElfWord for u64 {
    fn from_u64(value: u64) -> Result<Self> {
        Ok(value)
    }

    fn from_le_bytes(bytes: &[u8]) -> Self {
        u64::from_le_bytes(bytes.try_into().unwrap())
    }
}

pub trait ElfClass: Copy + Default + Send + Sync + std::fmt::Debug + 'static {
    type FileHeader: object::read::elf::FileHeader<
            Endian = LittleEndian,
            Word: ElfWord,
            CompressionHeader: WritableCompressionHeader + Default,
            NoteHeader: WritableNoteHeader,
            ProgramHeader: WritableProgramHeader,
            Relr: WritableRelr,
            SectionHeader: platform::SectionHeader + WritableSectionHeader,
            Sym: ElfSymbol<Class = Self>,
            Dyn: WritableDynamicEntry + Send + Sync,
            Rela: WritableRela + Send + Sync + Copy + 'static,
        > + WritableFileHeader;

    const FILE_HEADER_SIZE: u16 = size_of::<Self::FileHeader>() as u16;
    const PROGRAM_HEADER_SIZE: u16 = size_of::<ProgramHeader<Self>>() as u16;
    const SECTION_HEADER_SIZE: u16 = size_of::<SectionHeader<Self>>() as u16;
    const ADDRESS_SIZE: u64 = size_of::<Word<Self>>() as u64;
    const ADDRESS_ALIGNMENT: Alignment = Alignment {
        exponent: Self::ADDRESS_SIZE.trailing_zeros() as u8,
    };
    const GOT_ENTRY_SIZE: u64 = Self::ADDRESS_SIZE;
    const RELA_ENTRY_SIZE: u64 = size_of::<Rela<Self>>() as u64;
    const RELR_ENTRY_SIZE: u64 = size_of::<Relr<Self>>() as u64;
    const SYMTAB_ENTRY_SIZE: u64 = size_of::<SymtabEntry<Self>>() as u64;
    const DYNAMIC_ENTRY_SIZE: u64 = size_of::<DynamicEntry<Self>>() as u64;
    const NOTE_HEADER_SIZE: u64 = size_of::<NoteHeader<Self>>() as u64;
    const GNU_HASH_BLOOM_SIZE: u64 = Self::ADDRESS_SIZE;
    const PROGRAM_HEADER_ALIGNMENT: Alignment = Self::ADDRESS_ALIGNMENT;
    const SECTION_HEADER_ALIGNMENT: Alignment = Self::ADDRESS_ALIGNMENT;
    const GOT_ENTRY_ALIGNMENT: Alignment = Self::ADDRESS_ALIGNMENT;
    const RELA_ENTRY_ALIGNMENT: Alignment = Self::ADDRESS_ALIGNMENT;
    const RELR_ENTRY_ALIGNMENT: Alignment = Self::ADDRESS_ALIGNMENT;
    const GNU_HASH_ALIGNMENT: Alignment = Self::ADDRESS_ALIGNMENT;
    const SYMTAB_ENTRY_ALIGNMENT: Alignment = Self::ADDRESS_ALIGNMENT;
    const VERSION_D_ALIGNMENT: Alignment = Self::ADDRESS_ALIGNMENT;
    const VERSION_R_ALIGNMENT: Alignment = Self::ADDRESS_ALIGNMENT;
    const GNU_PROPERTY_ALIGNMENT: Alignment = Self::ADDRESS_ALIGNMENT;
    const GNU_PROPERTY_ENTRY_SIZE: u64 =
        Self::GNU_PROPERTY_ALIGNMENT.align_up(size_of::<NoteProperty>() as u64);
}

pub trait ElfSymbol:
    object::read::elf::Sym<Endian = LittleEndian>
    + WritableSymbol
    + platform::Symbol
    + Default
    + std::fmt::Debug
    + Copy
    + Send
    + Sync
    + 'static
{
    type Class: ElfClass;
}

#[derive(Debug, Copy, Clone, Default)]
pub struct Class64;

impl ElfClass for Class64 {
    type FileHeader = object::elf::FileHeader64<LittleEndian>;
}

impl ElfSymbol for object::elf::Sym64<LittleEndian> {
    type Class = Class64;
}

pub(crate) type FileHeader<C> = <C as ElfClass>::FileHeader;
pub(crate) type Word<C> = <FileHeader<C> as object::read::elf::FileHeader>::Word;
pub(crate) type ProgramHeader<C> = <FileHeader<C> as object::read::elf::FileHeader>::ProgramHeader;
pub(crate) type SectionHeader<C> = <FileHeader<C> as object::read::elf::FileHeader>::SectionHeader;
pub(crate) type SymtabEntry<C> = <FileHeader<C> as object::read::elf::FileHeader>::Sym;
pub(crate) type DynamicEntry<C> = <FileHeader<C> as object::read::elf::FileHeader>::Dyn;
pub(crate) type CompressionHeaderEntry<C> =
    <FileHeader<C> as object::read::elf::FileHeader>::CompressionHeader;
pub(crate) type Rela<C> = <FileHeader<C> as object::read::elf::FileHeader>::Rela;
pub(crate) type Relr<C> = <FileHeader<C> as object::read::elf::FileHeader>::Relr;
pub(crate) type NoteHeader<C> = <FileHeader<C> as object::read::elf::FileHeader>::NoteHeader;
pub(crate) type GnuHashHeader = object::elf::GnuHashHeader<LittleEndian>;
pub(crate) type Verdef = object::elf::Verdef<LittleEndian>;
pub(crate) type Verdaux = object::elf::Verdaux<LittleEndian>;
pub(crate) type Verneed = object::elf::Verneed<LittleEndian>;
pub(crate) type Vernaux = object::elf::Vernaux<LittleEndian>;
pub(crate) type Versym = object::elf::Versym<LittleEndian>;
pub(crate) type VerdefIterator<'data, C> = object::read::elf::VerdefIterator<'data, FileHeader<C>>;
pub(super) type VerneedIterator<'data, C> =
    object::read::elf::VerneedIterator<'data, FileHeader<C>>;
pub(super) type SectionTable<'data, C> = object::read::elf::SectionTable<'data, FileHeader<C>>;
pub(super) type SymbolTable<'data, C> = object::read::elf::SymbolTable<'data, FileHeader<C>>;

#[derive(Debug, Copy, Clone, Default)]
pub struct Elf<C: ElfClass>(PhantomData<C>);

impl<C: ElfClass> wild_layout::EnginePlatform for Elf<C> {
    fn process_plugin_input<'data>(
        plugin: &mut Self::LinkerPlugin<'data>,
        input_ref: wild_args::InputRef<'data>,
        file: &std::fs::File,
        kind: wild_platform::FileKind,
    ) -> wild_error::error::Result<Option<wild_layout::grouping::UnsequencedLtoInput<'data>>> {
        Ok(plugin
            .process_input(input_ref, file, kind)?
            .map(|info| info.into_unsequenced()))
    }

    fn plugin_lto_codegen<'data>(
        plugin: &mut Self::LinkerPlugin<'data>,
        symbol_db: &mut wild_layout::symbol_db::SymbolDb<'data, Self>,
        resolver: &mut wild_layout::resolution::Resolver<'data, Self>,
        per_symbol_flags: &mut wild_platform::value_flags::PerSymbolFlags,
    ) -> wild_error::error::Result<Option<Vec<wild_args::Input>>> {
        plugin.lto_codegen(symbol_db, resolver, per_symbol_flags)
    }

    fn plugin_integrate_lto_objects<'data>(
        plugin: &mut Self::LinkerPlugin<'data>,
        symbol_db: &mut wild_layout::symbol_db::SymbolDb<'data, Self>,
        resolver: &mut wild_layout::resolution::Resolver<'data, Self>,
        per_symbol_flags: &mut wild_platform::value_flags::PerSymbolFlags,
        output_sections: &mut wild_layout::output_section_id::OutputSections<'data, Self>,
        layout_rules_builder: &mut wild_layout::layout_rules::LayoutRulesBuilder<'data>,
        loaded: wild_layout::symbol_db::LoadedInputs<'data, Self>,
    ) -> wild_error::error::Result {
        plugin.integrate_lto_objects(
            symbol_db,
            resolver,
            per_symbol_flags,
            output_sections,
            layout_rules_builder,
            loaded,
        )
    }
}
impl<'data, 'scope, C: ElfClass> wild_layout::EngineScope<'data, 'scope> for Elf<C> where
    'data: 'scope
{
}
impl<'writer, 'out, C: ElfClass> wild_layout::EngineWriter<'writer, 'out> for Elf<C> where
    'out: 'writer
{
}

pub(crate) type File64<'data> = File<'data, Class64>;
pub(crate) type RelocationList64<'data> = RelocationList<'data, Class64>;

#[derive(derive_more::Debug)]
pub struct File<'data, C: ElfClass> {
    pub(crate) arch: Architecture,
    #[debug(skip)]
    pub(crate) data: &'data [u8],
    #[debug(skip)]
    pub(crate) sections: SectionTable<'data, C>,
    /// This may be symtab or dynsym depending on the file type.
    #[debug(skip)]
    pub(crate) symbols: SymbolTable<'data, C>,
    #[debug(skip)]
    pub(crate) versym: &'data [Versym],

    /// An iterator over the version definitions and the corresponding linked string table index.
    pub(crate) verdef: Option<(VerdefIterator<'data, C>, object::SectionIndex)>,

    /// Number of verdef versions according to `sh_info` of `.gnu._version_d` section.
    pub(crate) verdefnum: u32,

    /// An iterator over the version references and the corresponding linked string table index.
    pub(crate) verneed: Option<(VerneedIterator<'data, C>, object::SectionIndex)>,

    /// e_flags from the header.
    pub(crate) eflags: object::elf::FileFlags,

    pub(crate) dynamic_tag_values: Option<DynamicTagValues<'data>>,
}

#[derive(Clone, Copy)]
pub(crate) struct ElfRela<C: ElfClass> {
    pub(super) raw: Rela<C>,
}

impl<C: ElfClass> ElfRela<C> {
    pub(crate) fn new(raw: Rela<C>) -> Self {
        Self { raw }
    }
}

impl<C: ElfClass> Relocation for ElfRela<C> {
    type Sequence<'data> = RelaSequence<'data, C>;
    type Platform = Elf<C>;

    fn symbol(&self) -> Option<object::SymbolIndex> {
        object::read::elf::Rela::symbol(&self.raw, LittleEndian, false)
    }

    fn raw_type(&self) -> object::elf::RelocationType {
        object::read::elf::Rela::r_type(&self.raw, LittleEndian, false)
    }

    fn offset(&self) -> u64 {
        object::read::elf::Rela::r_offset(&self.raw, LittleEndian).into()
    }

    fn addend(&self) -> i64 {
        object::read::elf::Rela::r_addend(&self.raw, LittleEndian).into()
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ElfCrel<C: ElfClass> {
    pub(super) raw: Crel,
    pub(super) class: PhantomData<C>,
}

#[derive(Clone)]
pub(crate) struct CrelSequence<C: ElfClass>(pub(crate) Vec<ElfCrel<C>>);

impl<C: ElfClass> ElfCrel<C> {
    pub(crate) fn new(raw: Crel) -> Self {
        Self {
            raw,
            class: PhantomData,
        }
    }
}

impl<C: ElfClass> Relocation for ElfCrel<C> {
    type Sequence<'data> = CrelSequence<C>;
    type Platform = Elf<C>;

    fn symbol(&self) -> Option<object::SymbolIndex> {
        object::read::elf::Crel::symbol(&self.raw)
    }

    fn raw_type(&self) -> object::elf::RelocationType {
        self.raw.r_type
    }

    fn offset(&self) -> u64 {
        self.raw.r_offset
    }

    fn addend(&self) -> i64 {
        self.raw.r_addend
    }
}

/// A list of relocations that supports iteration.
#[derive(Clone)]
pub enum RelocationList<'data, C: ElfClass> {
    Rela(&'data [Rela<C>]),
    Crel(CrelIterator<'data>),
}

impl<'data, C: ElfClass> platform::RelocationList<'data> for RelocationList<'data, C> {
    fn num_relocations(&self) -> usize {
        match self {
            RelocationList::Rela(rela) => rela.len(),
            RelocationList::Crel(crel) => crel.len(),
        }
    }

    fn for_each_symbol(&self, f: &mut dyn FnMut(object::SymbolIndex)) {
        match self {
            RelocationList::Rela(rela) => {
                for raw in *rela {
                    if let Some(sym) = object::read::elf::Rela::symbol(raw, LittleEndian, false) {
                        f(sym);
                    }
                }
            }
            RelocationList::Crel(crel) => {
                for rel in crel.clone().flatten() {
                    if let Some(sym) = object::read::elf::Crel::symbol(&rel) {
                        f(sym);
                    }
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct RelaSequence<'data, C: ElfClass>(pub(super) &'data [Rela<C>]);

impl<'data, C: ElfClass> RelocationSequence<'data> for RelaSequence<'data, C> {
    type Rel = ElfRela<C>;

    fn rel_iter(&self) -> impl Iterator<Item = ElfRela<C>> {
        self.0.iter().copied().map(|raw| ElfRela { raw })
    }

    fn subsequence(&self, range: Range<usize>) -> Self {
        Self(&self.0[range])
    }

    fn num_relocations(&self) -> usize {
        self.0.len()
    }
}

impl<'data, C: ElfClass> RelocationSequence<'data> for CrelSequence<C> {
    type Rel = ElfCrel<C>;

    fn rel_iter(&self) -> impl Iterator<Item = ElfCrel<C>> {
        self.0.clone().into_iter()
    }

    fn subsequence(&self, range: Range<usize>) -> Self {
        Self(self.0[range].to_vec())
    }

    fn num_relocations(&self) -> usize {
        self.0.len()
    }
}

// Not needing Drop opens the option of storing this type in an arena that doesn't support dropping
// its contents.
const _: () = assert!(!core::mem::needs_drop::<File64>());

/// Returns the name to use when writing a symbol into .symtab's string table.
/// Unlike .dynsym (which encodes version info separately in .gnu.version), .symtab has no
/// .gnu.version section, so version suffixes must be embedded in the name itself
/// (e.g. `foo@VER_1.0`, `bar@@VER_1.0`, `remain_unversioned@`), matching GNU ld behaviour.
pub(crate) fn symtab_name_for_strtab(raw_name: &[u8]) -> &[u8] {
    raw_name
}
