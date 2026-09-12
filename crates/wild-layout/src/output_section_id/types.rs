use super::OutputSectionId;
use crate::layout_rules::SectionKind;
use wild_platform::Platform;
#[allow(unused_imports)]
pub use wild_platform::custom_section_ids::*;
#[allow(unused_imports)]
pub use wild_platform::section_identity::*;
use wild_scripts::linker_script::{Expression, OnlyIf};
use wild_util::alignment::Alignment;

#[derive(Debug)]
pub struct CustomSectionDetails<'data, P: Platform> {
    pub identity: SectionIdentity<'data, P>,
    pub index: object::SectionIndex,
    pub alignment: Alignment,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InitFiniSectionDetail {
    pub index: u32,
    pub primary: OutputSectionId,
    pub priority: u16,
    pub alignment: Alignment,
}

/// How a linker script maps the generated `.note.gnu.build-id` section.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GnuBuildIdPlacement {
    /// Keep the dedicated builtin section (no script matcher, or the script named that section).
    #[default]
    Builtin,
    /// Merge into this output section (e.g. kernel `.notes : { KEEP(*(.note.*)) }`).
    Merge(OutputSectionId),
    /// A `/DISCARD/` matcher matched `.note.gnu.build-id`.
    Discard,
}

/// One of the two GNU `ONLY_IF_RO` / `ONLY_IF_RW` placements for an output section.
#[derive(Debug, Clone)]
pub struct OnlyIfPlacement<'data> {
    pub order_index: usize,
    pub location_info: SectionLocationInfo<'data>,
    pub phdrs: Vec<&'data [u8]>,
}

/// Paired (or unpaired) `ONLY_IF_*` copies of the same output section name.
#[derive(Debug, Clone, Default)]
pub struct OnlyIfSlots<'data> {
    pub ro: Option<OnlyIfPlacement<'data>>,
    pub rw: Option<OnlyIfPlacement<'data>>,
    /// After seeing inputs, use the RW copy when this is set.
    pub prefer_rw: bool,
}

impl<'data> OnlyIfSlots<'data> {
    pub fn slot_mut(&mut self, only_if: OnlyIf) -> &mut Option<OnlyIfPlacement<'data>> {
        match only_if {
            OnlyIf::Ro => &mut self.ro,
            OnlyIf::Rw => &mut self.rw,
        }
    }

    pub fn chosen(&self) -> Option<&OnlyIfPlacement<'data>> {
        if self.prefer_rw {
            self.rw.as_ref().or(self.ro.as_ref())
        } else {
            self.ro.as_ref().or(self.rw.as_ref())
        }
    }
}

// TODO: There's also a type with this name in layout_rules. Rename one of them to avoid confusion.
#[derive(Debug)]
pub struct SectionOutputInfo<'data, P: Platform> {
    pub kind: SectionKind<'data, P>,
    pub section_attributes: P::SectionAttributes,
    pub min_alignment: Alignment,
    pub location_info: Option<SectionLocationInfo<'data>>,
    pub secondary_order: Option<SecondaryOrder>,
    pub region_name: Option<&'data [u8]>,
    pub fill: Option<[u8; 4]>,
    pub phdrs: Vec<&'data [u8]>,
    /// Place inputs in command-line / section-index order, aligning each to its own
    /// `sh_addralign` (GNU ld linker-script default). Alignment-bucket parts are not used.
    pub input_order: bool,
    /// GNU `SUBALIGN(n)`: force every input's alignment to `n`.
    pub subalign: Option<Alignment>,
}

#[derive(Debug, Clone, Copy)]
pub enum SecondaryOrder {
    InitFini { priority: u16 },
}

pub type LocationCounterIndex = usize;

#[derive(Debug)]
pub struct ScriptOutputData<'data> {
    pub section_id: OutputSectionId,
    pub location_counter_index: LocationCounterIndex,
    pub width: u8,
    pub value: Expression<'data>,
}

#[derive(Debug, Clone, Copy)]
pub struct OverlayPlacement {
    pub group: u32,
    pub member: u32,
    pub is_last: bool,
}

#[derive(Debug, Clone)]
pub struct SectionLocationInfo<'data> {
    /// End is exclusive
    pub location_counters: (LocationCounterIndex, LocationCounterIndex),
    pub location: Option<Expression<'data>>,
    pub at_location: Option<Expression<'data>>,
    pub at_region: Option<&'data [u8]>,
    pub is_top_level: bool,
    /// GNU `ALIGN_WITH_INPUT`: add the VMA alignment pad to LMA instead of
    /// aligning LMA independently.
    pub align_with_input: bool,
    pub overlay: Option<OverlayPlacement>,
}
