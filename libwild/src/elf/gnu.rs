#[allow(unused_imports)]
use super::abi::*;
#[allow(unused_imports)]
use super::file::*;
#[allow(unused_imports)]
use super::output::*;
use super::output_section_id;
use super::part_id;
#[allow(unused_imports)]
use super::types::*;
use crate::alignment::Alignment;
use crate::args::elf::ElfArgs;
use crate::bail;
use crate::ensure;
use crate::error::Context as _;
use crate::error::Result;
use crate::gdb_index::InputDebugIndexSection;
use crate::grouping::Group;
use crate::input_data::InputRef;
use crate::layout;
use crate::layout::DynamicSymbolDefinition;
use crate::layout::objects_iter;
use crate::layout_rules::SectionKind;
use crate::output_kind::OutputKind;
use crate::output_section_id::OutputSectionId;
use crate::output_section_id::OutputSections;
use crate::output_section_part_map::OutputSectionPartMap;
use crate::platform;
use crate::platform::Arch;
use crate::platform::CommonSymbol;
use crate::platform::DynamicTagValues as _;
use crate::platform::FrameIndex;
use crate::platform::ObjectFile;
use crate::platform::Platform;
use crate::platform::Relocation;
use crate::platform::SectionFlags as _;
use crate::symbol_db::SymbolDb;
use crate::symbol_db::SymbolId;
use crate::symbol_db::Visibility;
use crate::timing_phase;
use foldhash::HashSet;
use hashbrown::HashMap;
use indexmap::IndexMap;
use itertools::Itertools as _;
use leb128::write::unsigned_len as uleb128_size;
use linker_utils::elf::PageMask;
use linker_utils::elf::RISCV_ATTRIBUTE_VENDOR_NAME;
use linker_utils::elf::SectionFlags;
use linker_utils::elf::SectionType;
use linker_utils::elf::SegmentFlags;
use linker_utils::elf::SegmentType;
use linker_utils::elf::pf;
use linker_utils::elf::pt;
use linker_utils::elf::riscvattr::TAG_RISCV_ARCH;
use linker_utils::elf::riscvattr::TAG_RISCV_ATOMIC_ABI;
use linker_utils::elf::riscvattr::TAG_RISCV_PRIV_SPEC;
use linker_utils::elf::riscvattr::TAG_RISCV_PRIV_SPEC_MINOR;
use linker_utils::elf::riscvattr::TAG_RISCV_PRIV_SPEC_REVISION;
use linker_utils::elf::riscvattr::TAG_RISCV_STACK_ALIGN;
use linker_utils::elf::riscvattr::TAG_RISCV_UNALIGNED_ACCESS;
use linker_utils::elf::riscvattr::TAG_RISCV_WHOLE_FILE;
use linker_utils::elf::riscvattr::TAG_RISCV_X3_REG_USAGE;
use linker_utils::elf::shf;
use linker_utils::elf::sht;
use linker_utils::utils::read_string;
use linker_utils::utils::read_u32;
use linker_utils::utils::read_uleb128;
use object::LittleEndian;
use object::read::elf::CompressionHeader;
use object::read::elf::Dyn as _;
use object::read::elf::SectionHeader as _;
use rayon::prelude::*;
use smallvec::SmallVec;
use std::marker::PhantomData;
use std::mem::offset_of;
use std::num::NonZeroU32;
use std::sync::atomic::AtomicBool;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

impl platform::SectionHeader for object::elf::SectionHeader64<LittleEndian> {
    fn is_alloc(&self) -> bool {
        self.sh_flags(LittleEndian).is_alloc()
    }

    fn is_writable(&self) -> bool {
        self.sh_flags(LittleEndian).contains(shf::WRITE)
    }

    fn is_executable(&self) -> bool {
        self.sh_flags(LittleEndian).contains(shf::EXECINSTR)
    }

    fn is_tls(&self) -> bool {
        self.sh_flags(LittleEndian).contains(shf::TLS)
    }

    fn is_merge_section(&self) -> bool {
        self.sh_flags(LittleEndian).contains(shf::MERGE)
    }

    fn is_strings(&self) -> bool {
        self.sh_flags(LittleEndian).contains(shf::STRINGS)
    }

    fn merge_entsize(&self) -> u64 {
        self.sh_entsize(LittleEndian).into()
    }

    fn should_retain(&self) -> bool {
        self.sh_flags(LittleEndian).contains(shf::GNU_RETAIN)
    }

    fn should_exclude(&self) -> bool {
        self.sh_flags(LittleEndian).contains(shf::EXCLUDE)
    }

    fn is_group(&self) -> bool {
        self.sh_flags(LittleEndian).contains(shf::GROUP)
    }

    fn is_note(&self) -> bool {
        self.sh_type(LittleEndian) == sht::NOTE
    }

    fn is_prog_bits(&self) -> bool {
        self.sh_type(LittleEndian) == sht::PROGBITS
    }

    fn is_no_bits(&self) -> bool {
        self.sh_type(LittleEndian) == sht::NOBITS
    }

    fn skip_linker_script_matching(&self) -> bool {
        let ty = self.sh_type(LittleEndian);
        matches!(
            ty,
            sht::REL
                | sht::RELA
                | sht::SYMTAB
                | sht::STRTAB
                | sht::DYNSYM
                | sht::GROUP
                | sht::SYMTAB_SHNDX
        )
    }

    fn is_reloc_section(&self) -> bool {
        matches!(self.sh_type(LittleEndian), sht::REL | sht::RELA)
    }

    fn reloc_output_name_prefix(&self) -> Option<&'static [u8]> {
        match self.sh_type(LittleEndian) {
            sht::RELA => Some(b".rela"),
            sht::REL => Some(b".rel"),
            _ => None,
        }
    }

    fn reloc_target_section_index(&self) -> Option<object::SectionIndex> {
        if !self.is_reloc_section() {
            return None;
        }
        let info = self.sh_info(LittleEndian) as usize;
        (info != 0).then_some(object::SectionIndex(info))
    }
}

impl platform::SectionType for SectionType {
    fn is_rela(&self) -> bool {
        *self == sht::RELA
    }

    fn is_rel(&self) -> bool {
        *self == sht::REL
    }

    fn is_symtab(&self) -> bool {
        *self == sht::SYMTAB
    }

    fn is_strtab(&self) -> bool {
        *self == sht::STRTAB
    }
}

impl platform::SectionFlags for SectionFlags {
    fn is_alloc(self) -> bool {
        self.contains(shf::ALLOC)
    }
}

impl<T: ElfSymbol> platform::Symbol for T {
    fn as_common(&self) -> Option<CommonSymbol> {
        let e = LittleEndian;
        if !object::read::elf::Sym::is_common(self, e) {
            return None;
        }

        // Common symbols misuse the value field (which we access via `address()`) to store
        // the alignment.
        let Ok(alignment) = Alignment::new(object::read::elf::Sym::st_value(self, e).into()) else {
            return None;
        };
        let size = alignment.align_up(object::read::elf::Sym::st_size(self, e).into());

        let output_section_id = if self.st_type() == object::elf::STT_TLS {
            output_section_id::TBSS
        } else {
            output_section_id::BSS
        };

        let part_id = output_section_id.part_id_with_alignment::<Elf<T::Class>>(alignment);

        Some(CommonSymbol { size, part_id })
    }

    fn is_undefined(&self) -> bool {
        object::read::elf::Sym::is_undefined(self, LittleEndian)
    }

    fn is_local(&self) -> bool {
        object::read::elf::Sym::is_local(self)
    }

    fn visibility(&self) -> Visibility {
        convert_elf_visibility(self.st_visibility())
    }

    fn is_absolute(&self) -> bool {
        object::read::elf::Sym::is_absolute(self, LittleEndian)
    }

    fn is_weak(&self) -> bool {
        object::read::elf::Sym::is_weak(self)
    }

    fn value(&self) -> u64 {
        object::read::elf::Sym::st_value(self, LittleEndian).into()
    }

    fn size(&self) -> u64 {
        object::read::elf::Sym::st_size(self, LittleEndian).into()
    }

    fn has_name(&self) -> bool {
        object::read::elf::Sym::st_name(self, LittleEndian) != 0
    }

    fn is_default_strippable(&self, name: &[u8]) -> bool {
        (self.is_local() && name.starts_with(b".L"))
            || crate::symbol_db::is_mapping_symbol_name(name)
    }

    fn debug_string(&self) -> String {
        SymDebug(self).to_string()
    }

    fn is_tls(&self) -> bool {
        self.st_type() == object::elf::STT_TLS
    }

    fn is_interposable(&self) -> bool {
        self.st_visibility() == object::elf::STV_DEFAULT
    }

    fn is_func(&self) -> bool {
        self.st_type() == object::elf::STT_FUNC
    }

    fn is_ifunc(&self) -> bool {
        self.st_type() == object::elf::STT_GNU_IFUNC
    }

    fn is_hidden(&self) -> bool {
        self.st_visibility() == object::elf::STV_HIDDEN
    }

    fn is_gnu_unique(&self) -> bool {
        self.st_bind() == object::elf::STB_GNU_UNIQUE
    }

    fn with_hidden(mut self, hidden: bool) -> Self {
        self.set_visibility(if hidden {
            object::elf::STV_HIDDEN
        } else {
            object::elf::STV_DEFAULT
        });
        self
    }
}

pub(crate) fn convert_elf_visibility(st_visibility: object::elf::SymbolVisibility) -> Visibility {
    match st_visibility {
        object::elf::STV_PROTECTED => Visibility::Protected,
        object::elf::STV_HIDDEN => Visibility::Hidden,
        _ => Visibility::Default,
    }
}

pub(super) fn dynamic_tags<'data, C: ElfClass>(
    sections: &SectionTable<'data, C>,
    data: &'data [u8],
) -> Result<&'data [DynamicEntry<C>]> {
    let e = LittleEndian;
    if let Some(dynamic) = sections.dynamic(e, data).transpose() {
        return dynamic
            .map(|(dynamic, _)| dynamic)
            .context("Failed to read dynamic table");
    }
    Ok(&[])
}

pub(super) fn decompress_into<C: CompressionHeader<Endian = LittleEndian>>(
    compression: &C,
    input: &[u8],
    out: &mut [u8],
) -> Result {
    match compression.ch_type(LittleEndian) {
        object::elf::ELFCOMPRESS_ZLIB => {
            flate2::Decompress::new(true).decompress(
                input,
                out,
                flate2::FlushDecompress::Finish,
            )?;
        }
        // We might use pure Rust implementation for the decompression (ruzstd), however the
        // decompression speed is not on par with the official C library.
        // With the official library, the linking time of Clang binary (contains 1GB of debug info
        // sections) shrinks by 30%!
        object::elf::ELFCOMPRESS_ZSTD => {
            #[cfg(feature = "zstd")]
            {
                use std::io::Read as _;
                zstd::stream::Decoder::new(input)?.read_exact(out)?;
            }
            #[cfg(not(feature = "zstd"))]
            {
                bail!("wild was compiled without zstd support");
            }
        }
        c => bail!("Unsupported compression format: {}", c),
    }
    Ok(())
}

/// The module number for TLS variables in the current executable.
pub(crate) const CURRENT_EXE_TLS_MOD: u64 = 1;

/// See https://refspecs.linuxfoundation.org/LSB_1.3.0/gLSB/gLSB/ehframehdr.html
#[derive(FromBytes, IntoBytes, KnownLayout, Clone, Copy)]
#[repr(C)]
pub(crate) struct EhFrameHdr {
    pub(crate) version: u8,
    pub(crate) frame_pointer_encoding: u8,
    pub(crate) count_encoding: u8,
    pub(crate) table_encoding: u8,
    // For now we just use 32 bit pointer and count because it means that they're aligned. If we
    // need to upgrade these to u64, then we'd have to write these as unaligned fields.
    pub(crate) frame_pointer: i32,
    pub(crate) entry_count: u32,
}

pub(crate) const FRAME_POINTER_FIELD_OFFSET: usize = offset_of!(EhFrameHdr, frame_pointer);

/// The offset of the offset within the structure passed to __tls_get_addr.
#[derive(FromBytes, IntoBytes, KnownLayout, Clone, Copy)]
#[repr(C)]
pub(crate) struct EhFrameHdrEntry {
    pub(crate) frame_ptr: i32,
    pub(crate) frame_info_ptr: i32,
}

#[derive(FromBytes, Clone, Copy)]
#[repr(C)]
pub(crate) struct EhFrameEntryPrefix {
    pub(crate) length: u32,
    pub(crate) cie_id: u32,
}

pub(crate) fn is_eh_frame_terminator(data: &[u8]) -> bool {
    data.len() == size_of::<u32>() && data.iter().all(|&b| b == 0)
}

/// The offset of the pc_begin field in an FDE.
pub(crate) const FDE_PC_BEGIN_OFFSET: usize = 8;

// TODO: Right now, both x86_64 and AArch64 have 16 byte long entries, but
// the size should be generic over A: Arch.
pub(crate) const PLT_ENTRY_SIZE: u64 = 0x10;

pub(crate) const SYMTAB_SHNDX_ENTRY_SIZE: u64 = size_of::<SymtabShndxEntry>() as u64;
pub(crate) const GNU_VERSION_ENTRY_SIZE: u64 = size_of::<Versym>() as u64;

pub(crate) const GNU_NOTE_NAME: &[u8] = b"GNU\0";
/// For additional information on Elf_Prop, see
/// Linux Extensions to gABI at https://gitlab.com/x86-psABIs/Linux-ABI.
///
/// Right now, all properties have pr_datasz equal to 4. Any padding required for the ELF class is
/// written separately.
///
/// typedef struct {
/// Elf_Word pr_type;
/// Elf_Word pr_datasz;
/// unsigned char pr_data[PR_DATASZ];
/// unsigned char pr_padding[PR_PADDING];
/// } Elf_Prop;

#[derive(FromBytes, IntoBytes, KnownLayout, Clone, Copy)]
#[repr(C)]
pub(crate) struct NoteProperty {
    pub(crate) pr_type: u32,
    pub(crate) pr_datasz: u32,
    pub(crate) pr_data: u32,
}

pub(crate) struct PageMaskValue {
    pub(crate) symbol_plus_addend: u64,
    pub(crate) got_entry: u64,
    pub(crate) place: u64,
    pub(crate) got: u64,
}

impl Default for PageMaskValue {
    fn default() -> Self {
        Self {
            symbol_plus_addend: u64::MAX,
            got_entry: u64::MAX,
            place: u64::MAX,
            got: u64::MAX,
        }
    }
}

pub(crate) fn get_page_mask(mask: Option<PageMask>) -> PageMaskValue {
    let Some(mask) = mask else {
        return PageMaskValue::default();
    };

    match mask {
        PageMask::SymbolPlusAddendAndPosition(mask) => PageMaskValue {
            symbol_plus_addend: !mask,
            place: !mask,
            ..Default::default()
        },
        PageMask::GotEntryAndPosition(mask) => PageMaskValue {
            got_entry: !mask,
            place: !mask,
            ..Default::default()
        },
        PageMask::GotBase(mask) => PageMaskValue {
            got: !mask,
            ..Default::default()
        },
        PageMask::Position(mask) => PageMaskValue {
            place: !mask,
            ..Default::default()
        },
    }
}

#[derive(Default, Debug, Clone, Copy)]
pub(crate) struct DynamicTagValues<'data> {
    pub(crate) verdefnum: u64,
    pub(crate) soname: Option<&'data [u8]>,
}

impl<'data> DynamicTagValues<'data> {
    pub(super) fn read<C: ElfClass>(
        sections: &SectionTable<'data, C>,
        data: &'data [u8],
        symbols: &SymbolTable<'data, C>,
    ) -> Self {
        let mut values = DynamicTagValues::default();
        let Ok(dynamic_tags) = dynamic_tags::<C>(sections, data) else {
            return values;
        };
        let e = LittleEndian;
        for entry in dynamic_tags {
            let value: u64 = entry.d_val(e).into();
            match entry.d_tag(e) {
                object::elf::DT_VERDEFNUM => {
                    values.verdefnum = value;
                }
                object::elf::DT_SONAME => {
                    values.soname = symbols.strings().get(value as u32).ok();
                }
                _ => {}
            }
        }
        values
    }
}

impl<'data> platform::DynamicTagValues<'data> for DynamicTagValues<'data> {
    fn lib_name(&self, input: &InputRef<'data>) -> &'data [u8] {
        self.soname.unwrap_or_else(|| input.lib_name())
    }
}

pub(super) struct SymDebug<'data, T: ElfSymbol>(pub(crate) &'data T);

impl<T: ElfSymbol> std::fmt::Display for SymDebug<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let e = LittleEndian;
        let sym = self.0;

        let vis = if object::read::elf::Sym::is_local(sym) {
            "Local"
        } else if object::read::elf::Sym::is_weak(sym) {
            "Weak"
        } else {
            "Global"
        };

        let kind = if object::read::elf::Sym::is_undefined(sym, e) {
            "Undefined"
        } else {
            match object::read::elf::Sym::st_type(sym) {
                object::elf::STT_FUNC => "Func",
                object::elf::STT_GNU_IFUNC => "IFunc",
                object::elf::STT_OBJECT => "Data",
                object::elf::STT_COMMON => "Common",
                object::elf::STT_SECTION => "Section",
                object::elf::STT_FILE => "File",
                object::elf::STT_NOTYPE => "NoType",
                object::elf::STT_TLS => "Tls",
                _ => "Unknown",
            }
        };

        write!(f, "{vis} {kind}")
    }
}

pub(crate) enum PropertyClass {
    // A bit in the output pr_data is set if it is set in any relocatable input.
    // If all bits in the output pr_data field are zero, this property should be removed from
    // output.
    Or,
    // A bit in the output pr_data field is set only if it is set in all relocatable input pr_data
    // fields. If all bits in the output pr_data field are zero, this property should be
    // removed from output.
    And,
    // A bit in the output pr_data field is set if it is set in any relocatable input pr_data
    // fields and this property is present in all relocatable input files. When all bits in
    // the output pr_data field are zero, this property should not be removed from output to
    // indicate it has zero in all bits.
    AndOr,
}

#[derive(Debug)]
pub(crate) struct GnuProperty {
    pub(crate) ptype: object::elf::GnuPropertyType,
    pub(crate) data: u32,
}

#[derive(Debug)]
pub(crate) struct RiscVArch {
    pub(super) map: IndexMap<String, (u64, u64)>,
}

impl RiscVArch {
    pub(crate) fn to_attribute_string(&self) -> String {
        self.map
            .iter()
            .map(|(arch, (major, minor))| format!("{arch}{major}p{minor}"))
            .join("_")
    }
}

#[derive(Debug)]
pub(crate) struct RiscVAttributes {
    pub(crate) attributes: Vec<RiscVAttribute>,
    pub(crate) section_size: u64,
}

#[derive(Debug)]
pub(crate) enum RiscVAttribute {
    /// Indicates the stack alignment requirement in bytes.
    StackAlign(u64),
    /// Indicates the target architecture of this object.
    Arch(RiscVArch),
    /// Indicates whether to impose unaligned memory accesses in code generation.
    UnalignedAccess(bool),
    /// Indicates the major version of the privileged specification.
    PrivilegedSpecMajor(u64),
    /// Indicates the major version of the privileged specification.
    PrivilegedSpecMinor(u64),
    /// Indicates the revision version of the privileged specification.
    PrivilegedSpecRevision(u64),
}

#[derive(Default)]
pub(crate) struct ObjectLayoutStateExt<'data, C: ElfClass> {
    pub(super) gnu_property_notes: Vec<GnuProperty>,
    pub(crate) riscv_attributes: Vec<RiscVAttribute>,

    pub(super) has_eh_frame_input: bool,

    pub(super) cies: SmallVec<[CieAtOffset<'data>; 2]>,

    pub(super) eh_frame_size: u64,

    /// Indexed by `FrameIndex`.
    pub(super) exception_frames: ExceptionFrames<'data, C>,

    pub(crate) debug_index_sections: Vec<InputDebugIndexSection<'data>>,
}

#[derive(Debug)]
pub(crate) struct LayoutExt {
    pub(crate) gnu_property_notes: Vec<GnuProperty>,
    pub(crate) riscv_attributes: RiscVAttributes,
    pub(crate) eflags: object::elf::FileFlags,
    pub(super) has_eh_frame_input: bool,
}

impl LayoutExt {
    pub(crate) fn new<
        'files,
        'states,
        'data: 'files + 'states,
        C: ElfClass,
        A: Arch<Platform = Elf<C>>,
    >(
        groups: &'files [layout::GroupState<'data, Elf<C>>],
        args: &ElfArgs,
    ) -> Result<Self> {
        let states = objects_iter(groups).map(|o| &o.format_specific);
        let gnu_property_notes = merge_gnu_property_notes::<C, A>(states.clone(), args.z_isa)?;
        let riscv_attributes = merge_riscv_attributes::<C, A>(states)?;
        let eflags = merge_eflags::<C, A>(objects_iter(groups).map(|o| o.object))?;
        let has_eh_frame_input = objects_iter(groups).any(|o| o.format_specific.has_eh_frame_input);

        Ok(Self {
            gnu_property_notes,
            riscv_attributes,
            eflags,
            has_eh_frame_input,
        })
    }
}

pub(super) fn merge_gnu_property_notes<'states, 'data: 'states, C: ElfClass, A: Arch>(
    states: impl Iterator<Item = &'states ObjectLayoutStateExt<'data, C>>,
    isa_needed: Option<NonZeroU32>,
) -> Result<Vec<GnuProperty>> {
    timing_phase!("Merge GNU property notes");

    let properties_per_file = states.map(|state| &state.gnu_property_notes).collect_vec();

    // Merge bits of each property type based on type: OR or AND operation.
    // Within a single file, OR the bits (accumulate all features the file has).
    // Across files, AND the bits (only keep features all files support).
    let mut property_map: HashMap<_, (u32, PropertyClass)> = HashMap::new();

    for file_props in &properties_per_file {
        // First OR within file to accumulate all features this file has.
        let mut file_map: HashMap<_, (u32, PropertyClass)> = HashMap::new();
        for prop in *file_props {
            let property_class = A::get_property_class(prop.ptype.0)
                .ok_or_else(|| crate::error!("unclassified property type {}", prop.ptype))?;
            file_map
                .entry(prop.ptype)
                .and_modify(|entry: &mut (u32, PropertyClass)| {
                    entry.0 |= prop.data;
                })
                .or_insert_with(|| (prop.data, property_class));
        }
        // Then AND across files to keep only features all files support.
        for (ptype, (data, class)) in file_map {
            property_map
                .entry(ptype)
                .and_modify(|entry: &mut (u32, PropertyClass)| {
                    if matches!(class, PropertyClass::And) {
                        entry.0 &= data;
                    } else {
                        entry.0 |= data;
                    }
                })
                .or_insert_with(|| (data, class));
        }
    }

    // Merge needed ISA from CLI if set.
    if let Some(isa_needed) = isa_needed {
        property_map
            .entry(object::elf::GNU_PROPERTY_X86_ISA_1_NEEDED)
            .or_insert((0, PropertyClass::Or))
            .0 |= isa_needed.get();
    }

    // Iterate the properties sorted by property_type so that we have a stable output!
    let output_properties = property_map
        .into_iter()
        .sorted_by_key(|x| x.0)
        .filter_map(|(property_type, (property_value, property_class))| {
            let type_present_in_all = properties_per_file.iter().all(|props_per_file| {
                props_per_file
                    .iter()
                    .any(|prop| prop.ptype == property_type)
            });
            if match property_class {
                PropertyClass::Or => property_value != 0,
                PropertyClass::And => type_present_in_all && property_value != 0,
                PropertyClass::AndOr => type_present_in_all,
            } {
                Some(GnuProperty {
                    ptype: property_type,
                    data: property_value,
                })
            } else {
                None
            }
        })
        .collect_vec();

    Ok(output_properties)
}

pub(super) fn merge_eflags<'files, 'data: 'files, C: ElfClass, A: Arch<Platform = Elf<C>>>(
    objects: impl Iterator<Item = &'files File<'data, C>>,
) -> Result<object::elf::FileFlags> {
    timing_phase!("Merge e_flags");

    A::merge_eflags(objects.map(|object| object.eflags))
}

pub(super) fn merge_riscv_attributes<'groups, 'data: 'groups, C: ElfClass, A: Arch>(
    states: impl Iterator<Item = &'groups ObjectLayoutStateExt<'data, C>>,
) -> Result<RiscVAttributes> {
    timing_phase!("Merge .riscv.attributes sections");

    let attributes = states
        .map(|state| &state.riscv_attributes)
        // Sort by the number of ISAs: better output ordering
        .sorted_by_key(|x| x.len())
        .rev()
        .flatten()
        .collect_vec();

    let mut merged = Vec::new();

    let mut arch_components = IndexMap::new();
    for (name, version) in attributes
        .iter()
        .filter_map(|a| {
            if let RiscVAttribute::Arch(arch) = a {
                Some(&arch.map)
            } else {
                None
            }
        })
        .flatten()
    {
        arch_components
            .entry(name.clone())
            .and_modify(|v: &mut (u64, u64)| *v = (*v).max(*version))
            .or_insert(*version);
    }

    verify_riscv_ext_conflicts(&arch_components)?;

    if !arch_components.is_empty() {
        merged.push(RiscVAttribute::Arch(RiscVArch {
            map: arch_components,
        }));
    }

    if let Some(align) = attributes
        .iter()
        .filter_map(|a| {
            if let RiscVAttribute::StackAlign(align) = a {
                Some(align)
            } else {
                None
            }
        })
        .max()
    {
        merged.push(RiscVAttribute::StackAlign(*align));
    }
    if let Some(access) = attributes
        .iter()
        .filter_map(|a| {
            if let RiscVAttribute::UnalignedAccess(access) = a {
                Some(access)
            } else {
                None
            }
        })
        .max()
    {
        merged.push(RiscVAttribute::UnalignedAccess(*access));
    }
    if let Some(version) = attributes
        .iter()
        .filter_map(|a| {
            if let RiscVAttribute::PrivilegedSpecMajor(version) = a {
                Some(version)
            } else {
                None
            }
        })
        .max()
    {
        merged.push(RiscVAttribute::PrivilegedSpecMajor(*version));
    }
    if let Some(version) = attributes
        .iter()
        .filter_map(|a| {
            if let RiscVAttribute::PrivilegedSpecMinor(version) = a {
                Some(version)
            } else {
                None
            }
        })
        .max()
    {
        merged.push(RiscVAttribute::PrivilegedSpecMinor(*version));
    }
    if let Some(version) = attributes
        .iter()
        .filter_map(|a| {
            if let RiscVAttribute::PrivilegedSpecRevision(version) = a {
                Some(version)
            } else {
                None
            }
        })
        .max()
    {
        merged.push(RiscVAttribute::PrivilegedSpecRevision(*version));
    }

    let section_size = riscv_attributes_section_size(&merged);

    Ok(RiscVAttributes {
        attributes: merged,
        section_size,
    })
}

/// Conflicting pairs of RISC-V ISA extensions.
pub(super) const RISCV_CONFLICTING_EXT_PAIRS: &[(&str, &str)] = &[
    ("f", "zfinx"),
    ("d", "zdinx"),
    ("q", "zqinx"),
    ("zfh", "zhinx"),
    ("zfhmin", "zhinxmin"),
];

pub(super) fn verify_riscv_ext_conflicts(arch_components: &IndexMap<String, (u64, u64)>) -> Result {
    if arch_components.is_empty() {
        return Ok(());
    }

    let mut conflicts = Vec::new();
    for &(std_ext, inx_ext) in RISCV_CONFLICTING_EXT_PAIRS {
        if arch_components.contains_key(std_ext) && arch_components.contains_key(inx_ext) {
            conflicts.push(format!("'{std_ext}' is incompatible with '{inx_ext}'"));
        }
    }

    if conflicts.is_empty() {
        Ok(())
    } else {
        bail!(
            "Conflicting RISC-V ISA extensions in merged .riscv.attributes:\n  - {}",
            conflicts.join("\n  - ")
        );
    }
}

pub(crate) fn gnu_property_notes_section_size<C: ElfClass>(
    gnu_property_notes: &[GnuProperty],
) -> u64 {
    if gnu_property_notes.is_empty() {
        0
    } else {
        C::NOTE_HEADER_SIZE
            + GNU_NOTE_NAME.len() as u64
            + gnu_property_notes.len() as u64 * C::GNU_PROPERTY_ENTRY_SIZE
    }
}

pub(super) fn riscv_attributes_section_size(riscv_attributes: &[RiscVAttribute]) -> u64 {
    let attribute_size = |attr: &RiscVAttribute| match attr {
        RiscVAttribute::StackAlign(align) => {
            uleb128_size(TAG_RISCV_STACK_ALIGN) + uleb128_size(*align)
        }
        RiscVAttribute::Arch(arch) => {
            uleb128_size(TAG_RISCV_ARCH) + arch.to_attribute_string().len() + 1
        }
        RiscVAttribute::UnalignedAccess(_) => uleb128_size(TAG_RISCV_UNALIGNED_ACCESS) + 1,
        RiscVAttribute::PrivilegedSpecMajor(version) => {
            uleb128_size(TAG_RISCV_PRIV_SPEC) + uleb128_size(*version)
        }
        RiscVAttribute::PrivilegedSpecMinor(version) => {
            uleb128_size(TAG_RISCV_PRIV_SPEC_MINOR) + uleb128_size(*version)
        }
        RiscVAttribute::PrivilegedSpecRevision(version) => {
            uleb128_size(TAG_RISCV_PRIV_SPEC_REVISION) + uleb128_size(*version)
        }
    };

    (if riscv_attributes.is_empty() {
        0
    } else {
        1 // 'A'
            + 4 // sizeof(u32)
            + uleb128_size(TAG_RISCV_WHOLE_FILE)
            + 4 // sizeof(u32)
            + RISCV_ATTRIBUTE_VENDOR_NAME.len() + 1
            + riscv_attributes.iter().map(attribute_size).sum::<usize>()
    }) as u64
}

pub(crate) fn process_riscv_attributes(
    object: &File64,
    riscv_attributes_section_index: object::SectionIndex,
) -> Result<Vec<RiscVAttribute>> {
    let section = object.section(riscv_attributes_section_index)?;
    let e = LittleEndian;

    let content = section.data(e, object.data)?;
    ensure!(content.starts_with(b"A"), "Header must start with 'A'");
    let mut content = &content[1..];

    // Expect only one subsection
    let _size = read_u32(&mut content)?;
    let vendor = read_string(&mut content).context("Cannot read vendor string")?;
    ensure!(
        vendor == RISCV_ATTRIBUTE_VENDOR_NAME,
        "Unsupported vendor ('{vendor:?}') subsection"
    );

    // Assume only one sub-sub-section
    let tag = read_uleb128(&mut content).context("Cannot read tag of subsection")?;
    ensure!(tag == TAG_RISCV_WHOLE_FILE, "Whole file tag expected");
    let _size = read_u32(&mut content)?;
    let mut attributes = Vec::new();

    while !content.is_empty() {
        let tag = read_uleb128(&mut content).context("Cannot read tag of sub-subsection")?;
        let attribute = match tag {
            TAG_RISCV_STACK_ALIGN => {
                let align = read_uleb128(&mut content).context("Cannot read stack alignment")?;
                RiscVAttribute::StackAlign(align)
            }
            TAG_RISCV_ARCH => {
                let arch = read_string(&mut content).context("Cannot read arch attributes")?;
                let components = arch
                    .split('_')
                    .map(|part| {
                        let mut it = part.chars().rev();
                        let minor = it
                            .next()
                            .ok_or_else(|| crate::error!("Cannot parse minor"))?
                            .to_string();
                        let p = it
                            .next()
                            .ok_or_else(|| crate::error!("Cannot parse 'p' separator"))?;
                        ensure!(p == 'p', "Separator expected");
                        let major = it
                            .next()
                            .ok_or_else(|| crate::error!("Cannot parse major"))?
                            .to_string();
                        let name = it.rev().collect();
                        Ok((name, (major.parse()?, minor.parse()?)))
                    })
                    .collect::<Result<IndexMap<_, _>>>()?;

                RiscVAttribute::Arch(RiscVArch { map: components })
            }
            TAG_RISCV_UNALIGNED_ACCESS => {
                let access = read_uleb128(&mut content).context("Cannot read unaligned access")?;
                RiscVAttribute::UnalignedAccess(access > 0)
            }
            TAG_RISCV_PRIV_SPEC => {
                let version =
                    read_uleb128(&mut content).context("Cannot read privileged major version")?;
                RiscVAttribute::PrivilegedSpecMajor(version)
            }
            TAG_RISCV_PRIV_SPEC_MINOR => {
                let version =
                    read_uleb128(&mut content).context("Cannot read privileged minor version")?;
                RiscVAttribute::PrivilegedSpecMinor(version)
            }
            TAG_RISCV_PRIV_SPEC_REVISION => {
                let version = read_uleb128(&mut content)
                    .context("Cannot read privileged revision version")?;
                RiscVAttribute::PrivilegedSpecRevision(version)
            }
            TAG_RISCV_ATOMIC_ABI => {
                let _abi = read_uleb128(&mut content).context("Cannot read atomic ABI")?;
                bail!("TAG_RISCV_ATOMIC_ABI is not supported yet");
            }
            TAG_RISCV_X3_REG_USAGE => {
                let _x3 = read_uleb128(&mut content).context("Cannot read x3 register usage")?;
                bail!("TAG_RISCV_X3_REG_USAGE is not supported yet");
            }
            _ => {
                bail!("Unsupported tag: {tag}");
            }
        };
        attributes.push(attribute);
    }

    ensure!(content.is_empty(), "Unexpected multiple sub-sections");

    Ok(attributes)
}

/// Attributes that we'll take from an input section and apply to the output section into which it's
/// placed.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SectionAttributes<C: ElfClass> {
    pub(crate) flags: SectionFlags,
    pub(crate) ty: SectionType,
    pub(crate) entsize: u64,
    pub(crate) overrides: LinkerScriptOverrides,
    /// True after at least one input section's flags have been applied to this
    /// output section. `SHF_MERGE`/`SHF_STRINGS` are then intersected (GNU ld).
    pub(super) received_input_flags: bool,
    pub(super) class: PhantomData<C>,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct LinkerScriptOverrides {
    pub(crate) avoid_progpogation: SectionFlags,
    pub(crate) has_fixed_type: bool,
}

/// Section flags that should not be propagated from input sections to the output
/// section in which they are placed. `SHF_GROUP` is input-only. `SHF_MERGE` /
/// `SHF_STRINGS` are propagated only when every contributing input has them
/// (GNU ld): a dedicated `__ex_table` stays `AM`, mixed `.rodata` stays `A`.
pub(super) const SECTION_FLAGS_PROPAGATION_MASK: SectionFlags = object::elf::SHF_GROUP;

pub(super) const MERGE_STRINGS_FLAGS: SectionFlags =
    object::elf::SHF_MERGE.with(object::elf::SHF_STRINGS);

pub(super) fn flags_intersection(a: SectionFlags, b: SectionFlags) -> SectionFlags {
    a.without(a.without(b))
}

impl<C: ElfClass> platform::SectionAttributes for SectionAttributes<C> {
    type Platform = Elf<C>;

    fn merge(&mut self, rhs: Self) {
        let merge_strings = flags_intersection(
            flags_intersection(self.flags, rhs.flags),
            MERGE_STRINGS_FLAGS,
        );
        self.flags = (self.flags | rhs.flags).without(MERGE_STRINGS_FLAGS) | merge_strings;

        // We somewhat arbitrarily tie-break by selecting the maximum type. This means for example
        // that types like SHT_INIT_ARRAY win out over more generic types like SHT_PROGBITS.
        self.ty = self.ty.max(rhs.ty);

        // If all input sections specify the same entsize, then we use that. If there's any
        // inconsistency, then we set entsize to 0 and drop merge flags (GNU ld).
        if self.entsize != rhs.entsize {
            self.entsize = 0;
            self.flags = self.flags.without(MERGE_STRINGS_FLAGS);
        }
    }

    fn apply(&self, output_sections: &mut OutputSections<Elf<C>>, section_id: OutputSectionId) {
        let info = output_sections.section_infos.get_mut(section_id);

        let incoming_merge = flags_intersection(self.flags, MERGE_STRINGS_FLAGS);
        info.section_attributes.flags |= self.flags.without(
            SECTION_FLAGS_PROPAGATION_MASK
                | MERGE_STRINGS_FLAGS
                | info.section_attributes.overrides.avoid_progpogation,
        );

        if !info.section_attributes.received_input_flags {
            info.section_attributes.flags =
                info.section_attributes.flags.without(MERGE_STRINGS_FLAGS) | incoming_merge;
            info.section_attributes.entsize = self.entsize;
            info.section_attributes.received_input_flags = true;
        } else {
            let mut keep = flags_intersection(
                flags_intersection(info.section_attributes.flags, self.flags),
                MERGE_STRINGS_FLAGS,
            );
            if info.section_attributes.entsize != self.entsize {
                info.section_attributes.entsize = 0;
                keep = incoming_merge.without(MERGE_STRINGS_FLAGS);
            }
            info.section_attributes.flags =
                info.section_attributes.flags.without(MERGE_STRINGS_FLAGS) | keep;
        }

        if !info.section_attributes.overrides.has_fixed_type {
            info.section_attributes.ty = info.section_attributes.ty.max(self.ty);
        }

        if let SectionKind::Secondary(primary_id) = info.kind
            && info.location_info.is_some()
        {
            self.apply(output_sections, primary_id);
        }
    }

    fn is_null(&self) -> bool {
        self.ty == sht::NULL
    }

    fn is_alloc(&self) -> bool {
        self.flags.contains(shf::ALLOC)
    }

    fn flags(&self) -> <Self::Platform as Platform>::SectionFlags {
        self.flags
    }

    fn ty(&self) -> <Self::Platform as Platform>::SectionType {
        self.ty
    }

    fn set_to_default_type(&mut self) {
        self.ty = sht::PROGBITS;
    }

    fn set_alloc(&mut self) {
        self.flags |= shf::ALLOC;
    }

    fn set_no_bits(&mut self) {
        self.ty = sht::NOBITS;
    }

    fn set_writable(&mut self) {
        self.flags |= shf::WRITE;
    }

    fn avoids_alloc(&self) -> bool {
        self.overrides.avoid_progpogation.contains(shf::ALLOC)
    }

    fn is_executable(&self) -> bool {
        self.flags.contains(shf::EXECINSTR)
    }

    fn is_tls(&self) -> bool {
        self.flags.contains(shf::TLS)
    }

    fn occupies_only_tls_address_space(&self) -> bool {
        self.is_tls() && self.is_no_bits()
    }

    fn is_writable(&self) -> bool {
        self.flags.contains(shf::WRITE)
    }

    fn is_no_bits(&self) -> bool {
        self.ty == sht::NOBITS
    }
}

pub(crate) struct VersionNames<'data> {
    pub(crate) names: Vec<Option<&'data [u8]>>,
}

#[derive(Debug)]
pub(crate) struct RawSymbolName<'data> {
    pub(crate) name: &'data [u8],

    pub(crate) version_name: Option<&'data [u8]>,

    /// Whether the symbol can be referred to without a version.
    pub(crate) is_default: bool,
}

impl<'data> platform::RawSymbolName<'data> for RawSymbolName<'data> {
    fn parse(mut name_bytes: &'data [u8]) -> Self {
        let mut version_name = None;
        let mut is_default = true;

        // Symbols can contain version specifiers, e.g. `foo@1.1` or `foo@@2.0`. The latter,
        // with double-at specifies that it's the default version.
        if let Some(at_offset) = memchr::memchr(b'@', name_bytes) {
            if name_bytes[at_offset..].starts_with(b"@@") {
                version_name = Some(&name_bytes[at_offset + 2..]);
            } else {
                version_name = Some(&name_bytes[at_offset + 1..]);
                is_default = false;
            }

            name_bytes = &name_bytes[..at_offset];
        }

        RawSymbolName {
            name: name_bytes,
            version_name,
            is_default,
        }
    }

    fn name(&self) -> &'data [u8] {
        self.name
    }

    fn version_name(&self) -> Option<&'data [u8]> {
        self.version_name
    }

    fn is_default(&self) -> bool {
        self.is_default
    }
}

impl std::fmt::Display for RawSymbolName<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", String::from_utf8_lossy(self.name))?;
        if let Some(version) = self.version_name {
            if self.is_default {
                write!(f, "@@")?;
            } else {
                write!(f, "@")?;
            }
            write!(f, "{}", String::from_utf8_lossy(version))?;
        }

        Ok(())
    }
}

pub(crate) struct VerneedTable<'data> {
    pub(super) versym: &'data [Versym],
    pub(super) version_names_by_index: Vec<Option<&'data [u8]>>,
}

impl<'data> VerneedTable<'data> {
    pub(super) fn new<C: ElfClass>(file: &File<'data, C>) -> Result<Self> {
        Ok(Self {
            versym: file.versym,
            version_names_by_index: verneed_names_by_index(file)?,
        })
    }
}

impl<'data> platform::VerneedTable<'data> for VerneedTable<'data> {
    fn version_name(&self, local_symbol_index: object::SymbolIndex) -> Option<&'data [u8]> {
        let version_index = self.versym.get(local_symbol_index.0)?.0.get(LittleEndian);
        self.version_names_by_index
            .get(usize::from(version_index.index()))
            .copied()
            .flatten()
    }
}

pub(super) fn verneed_names_by_index<'data, C: ElfClass>(
    file: &File<'data, C>,
) -> Result<Vec<Option<&'data [u8]>>> {
    let mut version_names = Vec::new();
    let endian = LittleEndian;

    if let Some((verneeds, string_table_index)) = &file.verneed {
        let strings = file
            .sections
            .strings(endian, file.data, *string_table_index)?;

        for r in verneeds.clone() {
            let (_verneed, aux_iterator) = r?;
            for aux in aux_iterator {
                let aux = aux?;
                let version_index = usize::from(aux.vna_other.get(endian));
                let name = aux.name(endian, strings)?;

                if version_names.len() <= version_index {
                    version_names.resize_with(version_index + 1, || None);
                }
                version_names[version_index] = Some(name);
            }
        }
    }

    Ok(version_names)
}

#[derive(Debug)]
pub(crate) struct VerneedInfo<'data, C: ElfClass> {
    pub(crate) defs: VerdefIterator<'data, C>,
    pub(crate) string_table_index: object::SectionIndex,

    /// Number of symbol versions that we're going to emit. This is the number of entries in
    /// `symbol_versions_needed` that are true. Computed after graph traversal.
    pub(crate) version_count: u16,
}

#[derive(Default)]
pub(crate) struct DynamicLayoutStateExt<'data, C: ElfClass> {
    /// Which symbol versions are needed. A symbol version is needed if a symbol with that version
    /// has been loaded. The first version has index 1, so we store it at offset 0.
    pub(super) symbol_versions_needed: Vec<bool>,

    pub(super) verneed_info: Option<VerneedInfo<'data, C>>,

    pub(super) non_addressable_indexes: NonAddressableIndexes,

    /// Maps from addresses within the shared object to copy relocations at that address.
    pub(super) copy_relocations: HashMap<u64, CopyRelocationInfo>,
}

#[derive(Debug)]
pub(crate) struct DynamicLayoutExt<'data, C: ElfClass> {
    /// Mapping from input versions to output versions. Input version 1 is at index 0.
    pub(crate) version_mapping: Vec<object::elf::VersionIndex>,

    pub(crate) verneed_info: Option<VerneedInfo<'data, C>>,

    /// Whether this is the last DynamicLayout that puts content into .gnu.version_r.
    pub(crate) is_last_verneed: bool,

    pub(crate) copy_relocation_symbols: Vec<SymbolId>,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct NonAddressableIndexes {
    /// The version index that will be used for the next `.gnu.version_r` entry that we define.
    pub(super) next_gnu_version_r_index: object::elf::VersionIndex,
}

impl platform::NonAddressableIndexes for NonAddressableIndexes {
    fn new<P: Platform>(symbol_db: &crate::symbol_db::SymbolDb<P>) -> Self {
        Self {
            // Allocate version indexes starting from after the local and global indexes and any
            // versions defined by a version script.
            next_gnu_version_r_index: object::elf::VER_NDX_GLOBAL
                + 1.max(symbol_db.version_script.version_count()),
        }
    }
}

pub(super) struct CopyRelocationInfo {
    /// The symbol ID for which we'll actually generate the copy relocation. Initially, this is
    /// just the first symbol at a particular address for which we requested a copy relocation,
    /// then later we may update it to point to a different symbol if that first symbol was
    /// weak.
    pub(super) symbol_id: SymbolId,

    pub(super) is_weak: bool,
}

#[derive(Debug, Copy, Clone, Default)]
pub(crate) struct NonAddressableCounts {
    /// The number of shared objects that want to emit a verneed record.
    pub(crate) verneed_count: u64,
    /// The number of verdef records provided in version script.
    pub(crate) verdef_count: u16,
}

#[derive(Debug)]
pub(crate) struct EpilogueLayoutExt {
    pub(crate) sysv_hash_layout: Option<SysvHashLayout>,
    pub(crate) gnu_hash_layout: Option<GnuHashLayout>,
    pub(crate) verdefs: Option<Vec<VersionDef>>,
    pub(super) build_id_size: Option<usize>,
    pub(crate) needs_eh_frame_terminator: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct GnuHashLayout {
    pub(crate) num_defs: u32,
    pub(crate) bucket_count: u32,
    pub(crate) bloom_shift: u32,
    pub(crate) bloom_count: u32,
    pub(crate) symbol_base: u32,
}

pub(super) fn create_gnu_hash_layout<C: ElfClass>(
    args: &ElfArgs,
    output_kind: OutputKind,
    dynamic_symbol_definitions: &mut [DynamicSymbolDefinition<'_, Elf<C>>],
) -> Option<GnuHashLayout> {
    if !args.hash_style.includes_gnu() || !output_kind.needs_dynamic() {
        return None;
    }

    // Our number of buckets is computed somewhat arbitrarily so that we have on average 2
    // symbols per bucket, but then we round up to a power of two.
    let num_defs = dynamic_symbol_definitions.len();
    let gnu_hash_layout = GnuHashLayout {
        num_defs: dynamic_symbol_definitions.len() as u32,
        bucket_count: (num_defs / 2).next_power_of_two() as u32,
        bloom_shift: 6,
        bloom_count: 1,
        // `symbol_base` is set later in `finalise_layout`.
        symbol_base: 0,
    };

    // If we're going to emit .gnu.hash, then we need to stort the dynamic symbols by bucket.
    // Tie-break by name for determinism. We can use an unstable sort because names should be
    // unique. We use a parallel sort because we're processing symbols from potentially many
    // input objects, so there can be a lot.
    dynamic_symbol_definitions.par_sort_unstable_by_key(|d| {
        (
            gnu_hash_layout.bucket_for_hash(d.format_specific.hash),
            d.name,
        )
    });

    Some(gnu_hash_layout)
}

impl GnuHashLayout {
    /// Allocates space required for .gnu.hash. Also sorts dynamic symbol definitions by their hash
    /// bucket as required by .gnu.hash.
    pub(super) fn allocate<C: ElfClass>(&self, mem_sizes: &mut OutputSectionPartMap<u64>) {
        mem_sizes.increment(
            part_id::GNU_HASH,
            (size_of::<GnuHashHeader>()
                + C::GNU_HASH_BLOOM_SIZE as usize * self.bloom_count as usize
                + size_of::<u32>() * self.bucket_count as usize
                + size_of::<u32>() * self.num_defs as usize) as u64,
        );
    }

    pub(crate) fn bucket_for_hash(&self, hash: u32) -> u32 {
        hash % self.bucket_count
    }
}

#[derive(Default, Debug, Clone, Copy)]
pub(crate) struct SysvHashLayout {
    pub(crate) bucket_count: u32,
    pub(crate) chain_count: u32,
}

#[derive(derive_more::Debug)]
pub(crate) struct VersionDef {
    #[debug("{}", String::from_utf8_lossy(name))]
    pub(crate) name: Vec<u8>,
    pub(crate) parent_index: Option<u16>,
}

impl SysvHashLayout {
    pub(super) fn byte_size(self) -> Result<u64> {
        let words = 2u64
            .checked_add(u64::from(self.bucket_count))
            .and_then(|v| v.checked_add(u64::from(self.chain_count)))
            .context("Too many dynamic symbols for .hash")?;
        Ok(words * size_of::<u32>() as u64)
    }
}

pub(super) fn finalise_gnu_version_size<'data, C: ElfClass>(
    mem_sizes: &mut OutputSectionPartMap<u64>,
    symbol_db: &SymbolDb<'data, crate::elf::Elf<C>>,
) {
    if symbol_db.output_kind.should_output_symbol_versions() {
        let num_dynamic_symbols = mem_sizes.get(part_id::DYNSYM) / C::SYMTAB_ENTRY_SIZE;
        // Note, sets the GNU_VERSION allocation rather than incrementing it. Assuming there are
        // multiple files in our group, we'll update this same value multiple times, each time
        // with a possibly revised dynamic symbol count. The important thing is that when we're
        // done finalising the group sizes, the GNU_VERSION size should be consistent with the
        // DYNSYM size.
        *mem_sizes.get_mut(part_id::GNU_VERSION) =
            num_dynamic_symbols * crate::elf::GNU_VERSION_ENTRY_SIZE;
    }
}

/// A "common information entry". This is part of the .eh_frame data in ELF.
#[derive(PartialEq, Eq, Hash)]
pub(super) struct Cie<'data> {
    pub(super) bytes: &'data [u8],
    pub(super) eligible_for_deduplication: bool,
}

pub(super) struct CieAtOffset<'data> {
    // TODO: Use or remove. I think we need this when we implement deduplication of CIEs.
    /// Offset within .eh_frame
    #[allow(dead_code)]
    pub(super) offset: u32,
    pub(super) cie: Cie<'data>,
}

pub(super) enum ExceptionFrames<'data, C: ElfClass> {
    Rela(Vec<ExceptionFrame<'data, ElfRela<C>>>),
    Crel(Vec<ExceptionFrame<'data, ElfCrel<C>>>),
}

impl<'data, C: ElfClass> ExceptionFrames<'data, C> {
    pub(super) fn extend(&mut self, other: Self) {
        match (self, other) {
            (ExceptionFrames::Rela(a), ExceptionFrames::Rela(b)) => a.extend(b),
            (ExceptionFrames::Crel(a), ExceptionFrames::Crel(b)) => a.extend(b),
            (a, b) if a.is_empty() => *a = b,
            _ => panic!("Mixed exception frame relocations"),
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        match self {
            ExceptionFrames::Rela(a) => a.is_empty(),
            ExceptionFrames::Crel(a) => a.is_empty(),
        }
    }
}

impl<'data, C: ElfClass> Default for ExceptionFrames<'data, C> {
    fn default() -> Self {
        ExceptionFrames::Rela(Vec::new())
    }
}

pub(super) struct ExceptionFrame<'data, R: Relocation> {
    /// The relocations that need to be processed if we load this frame.
    pub(super) relocations: R::Sequence<'data>,

    /// Number of bytes required to store this frame.
    pub(super) frame_size: u32,

    /// The index of the previous frame that is for the same section.
    pub(super) previous_frame_for_section: Option<FrameIndex>,

    pub(super) eh_frame_section_index: object::SectionIndex,
}

pub(super) struct EhFrameSizes {
    pub(super) num_frames: u64,
    pub(super) eh_frame_size: u64,
}

impl<'data, C: ElfClass> ExceptionFrames<'data, C> {
    pub(super) fn len(&self) -> usize {
        match self {
            ExceptionFrames::Rela(f) => f.len(),
            ExceptionFrames::Crel(f) => f.len(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct GroupLayoutExt {
    pub(crate) eh_frame_start_address: u64,
}

#[derive(Debug, Default)]
pub(crate) struct CommonGroupStateExt {
    pub(crate) exception_frame_relocations: usize,
    pub(crate) exception_frame_count: usize,
}

/// Return whether all DT_NEEDED entries for this shared object correspond to input files that
/// we have loaded.
pub(super) fn has_complete_deps<'data, C: ElfClass>(
    file: &File<'data, C>,
    resources: &layout::GraphResources<'data, '_, Elf<C>>,
) -> bool {
    let Ok(dynamic_tags) = file.dynamic_tags() else {
        return true;
    };

    let e = LittleEndian;
    for entry in dynamic_tags {
        let value: u64 = entry.d_val(e).into();
        match entry.d_tag(e) {
            object::elf::DT_NEEDED => {
                let Ok(value) = value.try_into() else {
                    return false;
                };
                let Ok(name) = file.symbols.strings().get(value) else {
                    return false;
                };
                if !resources.layout_resources_ext.sonames.contains(name) {
                    return false;
                }
            }
            _ => {}
        }
    }

    true
}

#[derive(Debug)]
pub(crate) struct LayoutResourcesExt<'data> {
    pub(super) sonames: Sonames<'data>,
    pub(super) uses_tlsld: AtomicBool,
}

#[derive(Debug)]
pub(super) struct Sonames<'data>(pub(super) HashSet<&'data [u8]>);

impl<'data> Sonames<'data> {
    /// Builds an index of the DT_SONAMEs of the input dynamic objects. Note, that we include
    /// --as-needed shared objects that we're not actually linking against. This means that we can
    /// report --no-shlib-undefined errors for shared libraries that have all of their dependencies
    /// as inputs, even if we weren't going to add them as direct dependencies of our output file.
    pub(super) fn new<C: ElfClass>(groups: &[Group<'data, Elf<C>>]) -> Self {
        timing_phase!("Build SONAME index");

        Sonames(
            groups
                .iter()
                .flat_map(|group| {
                    let objects = match group {
                        Group::Objects(objects) => *objects,
                        _ => &[],
                    };
                    objects.iter().filter_map(|input| {
                        input
                            .parsed
                            .object
                            .dynamic_tag_values()
                            .map(|tag_values| tag_values.lib_name(&input.parsed.input))
                    })
                })
                .collect(),
        )
    }

    pub(super) fn contains(&self, name: &[u8]) -> bool {
        self.0.contains(name)
    }
}

impl platform::SegmentType for SegmentType {}

impl EpilogueLayoutExt {
    pub(crate) fn gnu_build_id_note_section_size<C: ElfClass>(&self) -> Option<u64> {
        Some(C::NOTE_HEADER_SIZE + GNU_NOTE_NAME.len() as u64 + self.build_id_size? as u64)
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ProgramSegmentDef {
    pub(crate) segment_type: SegmentType,
    pub(crate) segment_flags: SegmentFlags,
}

/// The different kinds of program segments that we generate based on section properties. Note, this
/// doesn't include the PT_GNU_STACK segment, since it isn't generated in response to any sections
/// because it doesn't contain any.
pub(super) const PROGRAM_SEGMENT_DEFS: &[ProgramSegmentDef] = &[
    ProgramSegmentDef {
        segment_type: pt::PHDR,
        segment_flags: pf::READABLE,
    },
    ProgramSegmentDef {
        segment_type: pt::INTERP,
        segment_flags: pf::READABLE,
    },
    ProgramSegmentDef {
        segment_type: pt::NOTE,
        segment_flags: pf::READABLE,
    },
    ProgramSegmentDef {
        segment_type: pt::GNU_PROPERTY,
        segment_flags: pf::READABLE,
    },
    ProgramSegmentDef {
        segment_type: pt::LOAD,
        segment_flags: pf::READABLE,
    },
    ProgramSegmentDef {
        segment_type: pt::LOAD,
        segment_flags: pf::READABLE.with(pf::EXECUTABLE),
    },
    ProgramSegmentDef {
        segment_type: pt::LOAD,
        segment_flags: pf::READABLE.with(pf::WRITABLE),
    },
    ProgramSegmentDef {
        segment_type: pt::LOAD,
        segment_flags: pf::READABLE.with(pf::WRITABLE).with(pf::EXECUTABLE),
    },
    ProgramSegmentDef {
        segment_type: pt::TLS,
        segment_flags: pf::READABLE,
    },
    ProgramSegmentDef {
        segment_type: pt::GNU_EH_FRAME,
        segment_flags: pf::READABLE,
    },
    ProgramSegmentDef {
        segment_type: pt::GNU_SFRAME,
        segment_flags: pf::READABLE,
    },
    ProgramSegmentDef {
        segment_type: pt::DYNAMIC,
        segment_flags: pf::READABLE.with(pf::WRITABLE),
    },
    ProgramSegmentDef {
        segment_type: pt::GNU_RELRO,
        segment_flags: pf::READABLE,
    },
    ProgramSegmentDef {
        segment_type: pt::RISCV_ATTRIBUTES,
        segment_flags: pf::READABLE,
    },
];

pub(crate) const STACK_SEGMENT_DEF: ProgramSegmentDef = ProgramSegmentDef {
    segment_type: pt::GNU_STACK,
    segment_flags: pf::READABLE.with(pf::WRITABLE),
};

impl platform::ProgramSegmentDef for ProgramSegmentDef {
    fn is_writable(self) -> bool {
        self.segment_flags.contains(pf::WRITABLE)
    }

    fn is_executable(self) -> bool {
        self.segment_flags.contains(pf::EXECUTABLE)
    }

    fn always_keep(self) -> bool {
        false
    }

    fn is_loadable(self) -> bool {
        self.segment_type == pt::LOAD
    }

    fn is_stack(self) -> bool {
        self.segment_type == pt::GNU_STACK
    }

    fn is_tls(self) -> bool {
        self.segment_type == pt::TLS
    }

    fn order_key(self) -> usize {
        // Segment types that we put first. Other types
        const TYPE_ORDER: &[SegmentType] = &[pt::PHDR, pt::INTERP, pt::LOAD, pt::DYNAMIC];

        TYPE_ORDER
            .iter()
            .position(|t| *t == self.segment_type)
            .unwrap_or(TYPE_ORDER.len() + self.segment_type.0 as usize)
    }

    fn should_cut_rw_segment_when_ending(self) -> bool {
        self.segment_type == pt::GNU_RELRO
    }

    fn from_linker_script(ptype: u32, flags: u32) -> Self {
        Self {
            segment_type: SegmentType(ptype),
            segment_flags: SegmentFlags(flags),
        }
    }
}

impl std::fmt::Display for ProgramSegmentDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}, {}",
            pt::Display(self.segment_type),
            pf::Display(self.segment_flags),
        )
    }
}

#[derive(Debug)]
pub(crate) struct BuiltInSectionDetails<C: ElfClass> {
    pub(crate) kind: SectionKind<'static, Elf<C>>,
    pub(crate) section_flags: SectionFlags,
    /// Sections to try to link to. The first section that we're outputting is the one used.
    pub(crate) link: &'static [OutputSectionId],
    pub(crate) min_alignment: Alignment,
    pub(crate) element_size: u64,
    pub(crate) ty: SectionType,
    pub(crate) is_relro: bool,
    pub(crate) target_segment_type: Option<SegmentType>,
}
