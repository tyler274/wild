mod ehframe;
mod notes;
mod versions;

#[allow(unused_imports)]
use super::abi::*;
#[allow(unused_imports)]
use super::file::*;
#[allow(unused_imports)]
use super::output::*;
use super::output_section_id;
#[allow(unused_imports)]
use super::types::*;
use crate::alignment::Alignment;
use crate::bail;
use crate::error::Context as _;
use crate::error::Result;
use crate::grouping::Group;
use crate::input_data::InputRef;
use crate::layout;
use crate::layout_rules::SectionKind;
use crate::output_section_id::OutputSectionId;
use crate::output_section_id::OutputSections;
use crate::platform;
use crate::platform::CommonSymbol;
use crate::platform::DynamicTagValues as _;
use crate::platform::ObjectFile;
use crate::platform::Platform;
use crate::platform::SectionFlags as _;
use crate::symbol_db::Visibility;
use crate::timing_phase;
#[allow(unused_imports)]
pub(crate) use ehframe::*;
use foldhash::HashSet;
use linker_utils::elf::SectionFlags;
use linker_utils::elf::SectionType;
use linker_utils::elf::SegmentFlags;
use linker_utils::elf::SegmentType;
use linker_utils::elf::pf;
use linker_utils::elf::pt;
use linker_utils::elf::shf;
use linker_utils::elf::sht;
#[allow(unused_imports)]
pub(crate) use notes::*;
use object::LittleEndian;
use object::read::elf::CompressionHeader;
use object::read::elf::Dyn as _;
use object::read::elf::SectionHeader as _;
use std::marker::PhantomData;
use std::sync::atomic::AtomicBool;
#[allow(unused_imports)]
pub(crate) use versions::*;

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

// TODO: Right now, both x86_64 and AArch64 have 16 byte long entries, but
// the size should be generic over A: Arch.
pub(crate) const PLT_ENTRY_SIZE: u64 = 0x10;

pub(crate) const SYMTAB_SHNDX_ENTRY_SIZE: u64 = size_of::<SymtabShndxEntry>() as u64;
pub(crate) const GNU_VERSION_ENTRY_SIZE: u64 = size_of::<Versym>() as u64;

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
