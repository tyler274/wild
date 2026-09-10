use super::MachO;
#[allow(unused_imports)]
use super::abi::*;
#[allow(unused_imports)]
use super::file::*;
#[allow(unused_imports)]
use super::output::*;
use crate::args::macho::MachOArgs;
use crate::input_data::FileId;
use object::Endianness;
use object::macho;
use object::macho::S_ATTR_EXT_RELOC;
use object::macho::S_ATTR_LOC_RELOC;
use object::macho::S_ATTR_PURE_INSTRUCTIONS;
use object::macho::S_ATTR_SOME_INSTRUCTIONS;
use object::macho::S_GB_ZEROFILL;
use object::macho::S_THREAD_LOCAL_REGULAR;
use object::macho::S_THREAD_LOCAL_ZEROFILL;
use object::macho::S_ZEROFILL;
use object::macho::SECTION_ATTRIBUTES;
use object::macho::Section64;
pub use object::macho::SectionFlags;
use std::num::NonZeroU64;
use wild_layout::layout_rules::SectionKind;
use wild_layout::output_section_id::SectionIdentity;
use wild_layout::output_section_id::SectionName;
use wild_layout::symbol_db::SymbolId;
use wild_platform as platform;
use wild_platform::Args;
use wild_util::alignment;
use wild_util::alignment::Alignment;

pub(super) const LE: Endianness = Endianness::Little;

/// Mach-O uses a zero page for all 32bit addresses and thus we begin the memory
/// offsets right after that (1GiB).
pub(crate) const MACHO_START_MEM_ADDRESS: u64 = 0x1_0000_0000;

/// The command alignment is 8B for 64-bit platforms.
pub(crate) const MACHO_COMMAND_ALIGNMENT: usize = 8;

/// A path to the default dynamic linker.
pub(crate) const DYLINKER_PATH: &[u8] = b"/usr/lib/dyld";

// TODO: Getting the number of active segments in epilogue depends on determine_header_size
// which is called later for the prologue. We potentially over-allocate a couple of bytes.
pub(crate) const MAX_SEGMENT_COUNT: usize = 6;
pub(crate) const CHAINED_FIXUP_TABLE_BASE_SIZE: u64 = (size_of::<ChainedFixupsHeader>()
    + size_of::<u32>() * (MAX_SEGMENT_COUNT + /* leading segment count */ 1)
    + size_of::<ChainedStartsInSegment>())
    as u64;
pub(crate) const CHAINED_FIXUP_IMPORT_SIZE: u64 = size_of::<u32>() as u64;
pub(crate) const CHAINED_FIXUP_PAGE_START_SIZE: u64 = size_of::<u16>() as u64;
pub(crate) const GOT_ENTRY_SIZE: u64 = 8;
pub(crate) const PLT_ENTRY_SIZE: u64 = 12;

pub(super) type SectionHeader = Section64<crate::macho::Endianness>;
pub(super) type SectionTable<'data> = &'data [Section64<crate::macho::Endianness>];
pub(super) type SymbolTable<'data> =
    object::read::macho::SymbolTable<'data, macho::MachHeader64<Endianness>>;
pub(super) type SymtabEntry = object::macho::Nlist64<Endianness>;
pub(super) type Relocation = object::macho::Relocation<Endianness>;

pub(crate) type FileHeader = object::macho::MachHeader64<Endianness>;
pub(crate) type SegmentCommand = object::macho::SegmentCommand64<Endianness>;
pub(crate) type SectionEntry = object::macho::Section64<Endianness>;
pub(crate) type EntryPointCommand = object::macho::EntryPointCommand<Endianness>;
pub(crate) type DylinkerCommand = object::macho::DylinkerCommand<Endianness>;
pub(crate) type DylibCommand = object::macho::DylibCommand<Endianness>;
pub(crate) type CodeSignatureCommand = object::macho::LinkeditDataCommand<Endianness>;
pub(crate) type DyldChainedFixupsCommand = object::macho::LinkeditDataCommand<Endianness>;
pub(crate) type ChainedFixupsHeader = object::macho::DyldChainedFixupsHeader<Endianness>;
pub(crate) type ChainedStartsInSegment = object::macho::DyldChainedStartsInSegment<Endianness>;
pub(crate) type SymtabCommand = object::macho::SymtabCommand<Endianness>;
pub(crate) type BuildVersionCommand = object::macho::BuildVersionCommand<Endianness>;
pub(crate) type UuidCommand = object::macho::UuidCommand<Endianness>;

pub(crate) const CS_SECTION_ALIGNMENT_EXP: u8 = 4;
pub(crate) const CS_SECTION_ALIGNMENT: u64 = 2u64.pow(CS_SECTION_ALIGNMENT_EXP as u32);

pub(crate) const CS_BLOB_HEADERS_SIZE: u64 =
    (size_of::<macho::CsSuperBlob>() + size_of::<macho::CsBlobIndex>()) as u64;
pub(crate) const CS_CODE_DIRECTORY_SIZE: u64 = (size_of::<macho::CsCodeDirectoryV0>()
    + size_of::<macho::CsCodeDirectoryV1>()
    + size_of::<macho::CsCodeDirectoryV2>()
    + size_of::<macho::CsCodeDirectoryV3>()
    + size_of::<macho::CsCodeDirectoryV4>()) as u64;
pub(crate) const CS_HEADERS_SIZE: u64 = CS_BLOB_HEADERS_SIZE + CS_CODE_DIRECTORY_SIZE;
pub(crate) const CS_BLOCK_SIZE_EXP: u8 = 12;
pub(crate) const CS_BLOCK_SIZE: usize = 2usize.pow(CS_BLOCK_SIZE_EXP as u32);
// SHA-256 is being used
pub(crate) const CS_HASH_SIZE: u8 = 32;

pub(crate) fn code_signature_identifier(args: &MachOArgs) -> &[u8] {
    args.output()
        .file_name()
        .expect("File name should be present at this point")
        .as_encoded_bytes()
}

pub(crate) fn code_signature_padded_identifier_size(args: &MachOArgs) -> u64 {
    (code_signature_identifier(args).len() as u64 + 1).next_multiple_of(CS_SECTION_ALIGNMENT)
}

pub(crate) fn load_dylib_command_size(path: &[u8]) -> usize {
    (size_of::<DylibCommand>() + path.len() + 1).next_multiple_of(MACHO_COMMAND_ALIGNMENT)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(crate) struct SegmentName([u8; 16]);

impl SegmentName {
    pub(crate) const PAGEZERO: Self = Self::from_bytes(b"__PAGEZERO");
    pub(crate) const TEXT: Self = Self::from_bytes(b"__TEXT");
    pub(crate) const DATA: Self = Self::from_bytes(b"__DATA");
    pub(crate) const DATA_CONST: Self = Self::from_bytes(b"__DATA_CONST");
    pub(crate) const LINKEDIT: Self = Self::from_bytes(b"__LINKEDIT");
    #[allow(dead_code)]
    pub(crate) const LLVM: Self = Self::from_bytes(b"__LLVM");

    pub(crate) const fn into_bytes(self) -> [u8; 16] {
        self.0
    }

    pub(super) const fn from_bytes(name: &[u8]) -> Self {
        assert!(name.len() <= 16);
        let mut bytes = [0; 16];
        bytes.split_at_mut(name.len()).0.copy_from_slice(name);
        Self(bytes)
    }

    pub(super) fn is_writable(self) -> bool {
        !matches!(self, Self::PAGEZERO | Self::TEXT | Self::LINKEDIT)
    }
}

impl std::fmt::Display for SegmentName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = String::from_utf8_lossy(&self.0);
        write!(f, "{}", name.trim_end_matches('\0'))
    }
}

#[derive(Debug, Default)]
pub(crate) struct LayoutExt {
    /// Imported STUB library symbols, sorted by GOT.
    pub(crate) imported_symbols: Vec<ImportedSymbolWithResolution>,
}

#[derive(Debug, Default)]
pub(crate) struct FinaliseSizesExt {
    pub(super) imported_libraries: Vec<FileId>,
    pub(super) imported_symbols: Vec<SymbolId>,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct PreludeLayoutExt {
    pub(crate) imported_library_file_ids: Vec<FileId>,
    pub(crate) load_dylib_command_sizes: Vec<usize>,
    pub(crate) load_command_count: usize,
}

#[derive(derive_more::Debug, Clone, Copy)]
pub(crate) struct ImportedSymbolWithResolution {
    pub(crate) symbol_id: SymbolId,
    pub(crate) got_address: NonZeroU64,
    pub(crate) plt_address: Option<NonZeroU64>,
}

#[derive(Debug, Copy, Clone, Default)]
pub(crate) struct SectionAttributes {
    pub(super) ty: macho::SectionType,
    pub(super) attr: SectionFlags,
    pub(super) writable: bool,
}

pub(super) const SECTION_FLAGS_PROPAGATION_MASK: SectionFlags =
    S_ATTR_EXT_RELOC.with(S_ATTR_LOC_RELOC);

impl SectionAttributes {
    pub(super) fn new(flags: SectionFlags, segment: Option<SegmentName>) -> Self {
        Self {
            ty: flags.typ(),
            attr: SectionFlags(flags.0 & SECTION_ATTRIBUTES),
            writable: segment.is_some_and(SegmentName::is_writable),
        }
    }
}

impl platform::SectionAttributes for SectionAttributes {
    type Platform = MachO;

    fn merge(&mut self, rhs: Self) {
        self.ty = self.ty.max(rhs.ty);
        self.attr |= rhs.attr;
        self.writable |= rhs.writable;
    }

    fn apply(
        &self,
        output_sections: &mut wild_layout::output_section_id::OutputSections<Self::Platform>,
        section_id: wild_layout::output_section_id::OutputSectionId,
    ) {
        let info = output_sections.section_infos.get_mut(section_id);
        // TODO: For now, we copy what ELF does to break ties in types. This acts as a workaround
        // since S_REGULAR = 0 and more specialized types should win this tiebreak.
        info.section_attributes.ty = info.section_attributes.ty.max(self.ty);
        info.section_attributes.attr |= self.attr.without(SECTION_FLAGS_PROPAGATION_MASK);
        info.section_attributes.writable |= self.writable;
    }

    fn is_null(&self) -> bool {
        false
    }

    fn is_alloc(&self) -> bool {
        true
    }

    fn is_executable(&self) -> bool {
        self.flags()
            .intersects(S_ATTR_PURE_INSTRUCTIONS | S_ATTR_SOME_INSTRUCTIONS)
    }

    fn is_tls(&self) -> bool {
        matches!(self.ty, S_THREAD_LOCAL_REGULAR | S_THREAD_LOCAL_ZEROFILL)
    }

    fn occupies_only_tls_address_space(&self) -> bool {
        false
    }

    fn is_writable(&self) -> bool {
        self.writable
    }

    fn is_no_bits(&self) -> bool {
        matches!(
            self.ty,
            S_ZEROFILL | S_GB_ZEROFILL | S_THREAD_LOCAL_ZEROFILL
        )
    }

    fn flags(&self) -> SectionFlags {
        self.attr.with_type(self.ty)
    }

    fn ty(&self) -> macho::SectionType {
        self.ty
    }

    fn set_to_default_type(&mut self) {}
}

pub(crate) struct NonAddressableIndexes {}

impl platform::NonAddressableIndexes for NonAddressableIndexes {
    fn new<P: platform::Platform>(_symbol_db: &P::SymbolDb<'_>) -> Self {
        NonAddressableIndexes {}
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct MachOSegmentType;

impl platform::SegmentType for MachOSegmentType {}

/// Represents an actual segment.
#[derive(Debug, Copy, Clone)]
pub(crate) struct ProgramSegmentDef {
    // TODO: When we implement -segprot, we should support both initprot and maxprot here.
    pub(crate) name: SegmentName,
    pub(crate) prot: macho::VmProt,
    pub(crate) flags: macho::SegmentFlags,
}

impl ProgramSegmentDef {
    pub(super) fn new(name: SegmentName) -> Self {
        let (prot, flags) = match name {
            SegmentName::TEXT => (
                macho::VM_PROT_READ | macho::VM_PROT_EXECUTE,
                macho::SegmentFlags::default(),
            ),
            SegmentName::DATA_CONST => (
                macho::VM_PROT_READ | macho::VM_PROT_WRITE,
                macho::SG_READ_ONLY,
            ),
            SegmentName::LINKEDIT => (macho::VM_PROT_READ, macho::SegmentFlags::default()),
            _ => (
                macho::VM_PROT_READ | macho::VM_PROT_WRITE,
                macho::SegmentFlags::default(),
            ),
        };

        Self { name, prot, flags }
    }
}

impl std::fmt::Display for ProgramSegmentDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.name, f)
    }
}

impl platform::ProgramSegmentDef for ProgramSegmentDef {
    fn is_writable(self) -> bool {
        self.prot.contains(macho::VM_PROT_WRITE)
    }

    fn is_executable(self) -> bool {
        self.prot.contains(macho::VM_PROT_EXECUTE)
    }

    fn always_keep(self) -> bool {
        matches!(self.name, SegmentName::TEXT | SegmentName::LINKEDIT)
    }

    fn is_loadable(self) -> bool {
        true
    }

    fn is_stack(self) -> bool {
        false
    }

    fn is_tls(self) -> bool {
        false
    }

    fn order_key(self) -> usize {
        match self.name {
            SegmentName::TEXT => 0,
            SegmentName::DATA_CONST => 1,
            SegmentName::DATA => 2,
            SegmentName::LINKEDIT => 4,
            _ => 3,
        }
    }
}

pub(crate) struct BuiltInSectionDetails {
    pub(crate) kind: SectionKind<'static, MachO>,
    pub(crate) section_flags: SectionFlags,
    pub(crate) min_alignment: Alignment,
}

impl platform::BuiltInSectionDetails for BuiltInSectionDetails {}

pub(super) const DEFAULT_DEFS: BuiltInSectionDetails = BuiltInSectionDetails {
    kind: SectionKind::Primary(SectionIdentity::new(SectionName(&[]), None)),
    section_flags: SectionFlags(0),
    min_alignment: alignment::MIN,
};

#[allow(unused)]
#[derive(Default, Debug, Clone, Copy)]
pub(crate) struct DynamicTagValues<'data> {
    pub(super) phantom: &'data [u8],
}

#[derive(Debug)]
pub(crate) struct RelocationList<'data> {
    pub(crate) relocations: &'data [Relocation],
}

impl<'data> platform::RelocationList<'data> for RelocationList<'data> {
    fn num_relocations(&self) -> usize {
        self.relocations.len()
    }
}

impl<'data> platform::DynamicTagValues<'data> for DynamicTagValues<'data> {
    fn lib_name(&self, _fallback_name: &'data [u8]) -> &'data [u8] {
        &[]
    }
}

#[derive(Debug)]
pub(crate) struct RawSymbolName<'data> {
    pub(crate) name: &'data [u8],
}

impl<'data> platform::RawSymbolName<'data> for RawSymbolName<'data> {
    fn parse(bytes: &'data [u8]) -> Self {
        Self { name: bytes }
    }

    fn name(&self) -> &'data [u8] {
        self.name
    }

    fn version_name(&self) -> Option<&'data [u8]> {
        None
    }

    fn is_default(&self) -> bool {
        // This port does not use symbol versioning, so every symbol is treated as
        // the default version.
        true
    }
}

impl std::fmt::Display for RawSymbolName<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&String::from_utf8_lossy(self.name), f)
    }
}

pub(crate) struct VerneedTable<'data> {
    // TODO
    pub(super) _phantom: &'data [u8],
}

impl<'data> platform::VerneedTable<'data> for VerneedTable<'data> {
    fn version_name(&self, _local_symbol_index: object::SymbolIndex) -> Option<&'data [u8]> {
        todo!()
    }
}
