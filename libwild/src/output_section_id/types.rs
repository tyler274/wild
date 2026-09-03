use super::ids::*;
use crate::alignment::Alignment;
use crate::layout_rules::SectionKind;
use crate::linker_script::Expression;
use crate::platform::Platform;
use std::fmt::Debug;
use std::fmt::Display;
use std::hash::Hash;
use std::hash::Hasher;

#[derive(Debug)]
pub(crate) struct CustomSectionDetails<'data, P: Platform> {
    pub(crate) identity: SectionIdentity<'data, P>,
    pub(crate) index: object::SectionIndex,
    pub(crate) alignment: Alignment,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct InitFiniSectionDetail {
    pub(crate) index: u32,
    pub(crate) primary: OutputSectionId,
    pub(crate) priority: u16,
    pub(crate) alignment: Alignment,
}

/// How a linker script maps the generated `.note.gnu.build-id` section.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum GnuBuildIdPlacement {
    /// Keep the dedicated builtin section (no script matcher, or the script named that section).
    #[default]
    Builtin,
    /// Merge into this output section (e.g. kernel `.notes : { KEEP(*(.note.*)) }`).
    Merge(OutputSectionId),
    /// A `/DISCARD/` matcher matched `.note.gnu.build-id`.
    Discard,
}
#[derive(Default)]
pub(crate) struct CustomSectionIds {
    pub(crate) ro: Vec<OutputSectionId>,
    pub(crate) exec: Vec<OutputSectionId>,
    pub(crate) data: Vec<OutputSectionId>,
    pub(crate) bss: Vec<OutputSectionId>,
    pub(crate) nonalloc: Vec<OutputSectionId>,
    pub(crate) tdata: Vec<OutputSectionId>,
    pub(crate) tbss: Vec<OutputSectionId>,
}
// TODO: There's also a type with this name in layout_rules. Rename one of them to avoid confusion.
#[derive(Debug)]
pub(crate) struct SectionOutputInfo<'data, P: Platform> {
    pub(crate) kind: SectionKind<'data, P>,
    pub(crate) section_attributes: P::SectionAttributes,
    pub(crate) min_alignment: Alignment,
    pub(crate) location_info: Option<SectionLocationInfo<'data>>,
    pub(crate) secondary_order: Option<SecondaryOrder>,
    pub(crate) region_name: Option<&'data [u8]>,
    pub(crate) fill: Option<[u8; 4]>,
    pub(crate) phdrs: Vec<&'data [u8]>,
    /// Place inputs in command-line / section-index order, aligning each to its own
    /// `sh_addralign` (GNU ld linker-script default). Alignment-bucket parts are not used.
    pub(crate) input_order: bool,
}
#[derive(Debug, Clone, Copy)]
pub(crate) struct SectionIdentity<'data, P: Platform> {
    name: SectionName<'data>,
    format_specific: P::SectionIdentityExt,
}

impl<'data, P: Platform> SectionIdentity<'data, P> {
    pub(crate) const fn new(
        name: SectionName<'data>,
        format_specific: P::SectionIdentityExt,
    ) -> Self {
        Self {
            name,
            format_specific,
        }
    }

    pub(crate) fn section_name(&self) -> SectionName<'data> {
        self.name
    }

    pub(crate) fn format_specific(&self) -> P::SectionIdentityExt {
        self.format_specific
    }
}

impl<'data, P: Platform> PartialEq for SectionIdentity<'data, P> {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.format_specific == other.format_specific
    }
}

impl<'data, P: Platform> Eq for SectionIdentity<'data, P> {}

impl<'data, P: Platform> Hash for SectionIdentity<'data, P> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.hash(state);
        self.format_specific.hash(state);
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct SectionName<'data>(pub(crate) &'data [u8]);

impl SectionName<'_> {
    pub(crate) fn bytes(&self) -> &[u8] {
        self.0
    }
}

impl Debug for SectionName<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{}", String::from_utf8_lossy(self.0)))
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum SecondaryOrder {
    InitFini { priority: u16 },
}
impl<P: Platform> Display for SectionIdentity<'_, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        P::fmt_section_identity(self.name, &self.format_specific, f)
    }
}

impl Display for SectionName<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", String::from_utf8_lossy(self.0))
    }
}
pub(crate) type LocationCounterIndex = usize;

#[derive(Debug)]
pub(crate) struct ScriptOutputData<'data> {
    pub(crate) section_id: OutputSectionId,
    pub(crate) location_counter_index: LocationCounterIndex,
    pub(crate) width: u8,
    pub(crate) value: Expression<'data>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct OverlayPlacement {
    pub(crate) group: u32,
    pub(crate) member: u32,
    pub(crate) is_last: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct SectionLocationInfo<'data> {
    /// End is exclusive
    pub(crate) location_counters: (LocationCounterIndex, LocationCounterIndex),
    pub(crate) location: Option<Expression<'data>>,
    pub(crate) at_location: Option<Expression<'data>>,
    pub(crate) at_region: Option<&'data [u8]>,
    pub(crate) is_top_level: bool,
    pub(crate) overlay: Option<OverlayPlacement>,
}
