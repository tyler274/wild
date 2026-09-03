use super::*;
use crate::args::elf::ElfArgs;
use crate::bail;
#[allow(unused_imports)]
use crate::elf::abi::*;
#[allow(unused_imports)]
use crate::elf::file::*;
#[allow(unused_imports)]
use crate::elf::output::*;
#[allow(unused_imports)]
use crate::elf::types::*;
use crate::ensure;
use crate::error::Context as _;
use crate::error::Result;
use crate::gdb_index::InputDebugIndexSection;
use crate::layout;
use crate::layout::objects_iter;
use crate::platform::Arch;
use crate::platform::ObjectFile;
use crate::timing_phase;
use hashbrown::HashMap;
use indexmap::IndexMap;
use itertools::Itertools as _;
use leb128::write::unsigned_len as uleb128_size;
use linker_utils::elf::PageMask;
use linker_utils::elf::RISCV_ATTRIBUTE_VENDOR_NAME;
use linker_utils::elf::riscvattr::TAG_RISCV_ARCH;
use linker_utils::elf::riscvattr::TAG_RISCV_ATOMIC_ABI;
use linker_utils::elf::riscvattr::TAG_RISCV_PRIV_SPEC;
use linker_utils::elf::riscvattr::TAG_RISCV_PRIV_SPEC_MINOR;
use linker_utils::elf::riscvattr::TAG_RISCV_PRIV_SPEC_REVISION;
use linker_utils::elf::riscvattr::TAG_RISCV_STACK_ALIGN;
use linker_utils::elf::riscvattr::TAG_RISCV_UNALIGNED_ACCESS;
use linker_utils::elf::riscvattr::TAG_RISCV_WHOLE_FILE;
use linker_utils::elf::riscvattr::TAG_RISCV_X3_REG_USAGE;
use linker_utils::utils::read_string;
use linker_utils::utils::read_u32;
use linker_utils::utils::read_uleb128;
use object::LittleEndian;
use object::read::elf::SectionHeader as _;
use smallvec::SmallVec;
use std::num::NonZeroU32;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

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
    pub(crate) map: IndexMap<String, (u64, u64)>,
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
    pub(crate) gnu_property_notes: Vec<GnuProperty>,
    pub(crate) riscv_attributes: Vec<RiscVAttribute>,

    pub(crate) has_eh_frame_input: bool,

    pub(crate) cies: SmallVec<[CieAtOffset<'data>; 2]>,

    pub(crate) eh_frame_size: u64,

    /// Indexed by `FrameIndex`.
    pub(crate) exception_frames: ExceptionFrames<'data, C>,

    pub(crate) debug_index_sections: Vec<InputDebugIndexSection<'data>>,
}

#[derive(Debug)]
pub(crate) struct LayoutExt {
    pub(crate) gnu_property_notes: Vec<GnuProperty>,
    pub(crate) riscv_attributes: RiscVAttributes,
    pub(crate) eflags: object::elf::FileFlags,
    pub(crate) has_eh_frame_input: bool,
    num_got_plt_header_entries: u64,
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
            num_got_plt_header_entries: A::NUM_GOT_PLT_HEADER_ENTRIES,
        })
    }

    pub(crate) fn num_got_plt_header_entries(&self, has_plt_relocations: bool) -> u64 {
        if has_plt_relocations {
            self.num_got_plt_header_entries
        } else {
            0
        }
    }
}

pub(crate) fn merge_gnu_property_notes<'states, 'data: 'states, C: ElfClass, A: Arch>(
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

pub(crate) fn merge_eflags<'files, 'data: 'files, C: ElfClass, A: Arch<Platform = Elf<C>>>(
    objects: impl Iterator<Item = &'files File<'data, C>>,
) -> Result<object::elf::FileFlags> {
    timing_phase!("Merge e_flags");

    A::merge_eflags(objects.map(|object| object.eflags))
}

pub(crate) fn merge_riscv_attributes<'groups, 'data: 'groups, C: ElfClass, A: Arch>(
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
pub(crate) const RISCV_CONFLICTING_EXT_PAIRS: &[(&str, &str)] = &[
    ("f", "zfinx"),
    ("d", "zdinx"),
    ("q", "zqinx"),
    ("zfh", "zhinx"),
    ("zfhmin", "zhinxmin"),
];

pub(crate) fn verify_riscv_ext_conflicts(arch_components: &IndexMap<String, (u64, u64)>) -> Result {
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

pub(crate) fn riscv_attributes_section_size(riscv_attributes: &[RiscVAttribute]) -> u64 {
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
