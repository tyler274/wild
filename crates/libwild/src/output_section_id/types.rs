use super::ids::*;
use crate::alignment::Alignment;
use crate::layout_rules::SectionKind;
use crate::linker_script::Expression;
use crate::linker_script::OnlyIf;
use crate::platform::Platform;
#[allow(unused_imports)]
pub(crate) use crate::platform::custom_section_ids::*;
#[allow(unused_imports)]
pub(crate) use crate::platform::section_identity::*;

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

/// One of the two GNU `ONLY_IF_RO` / `ONLY_IF_RW` placements for an output section.
#[derive(Debug, Clone)]
pub(crate) struct OnlyIfPlacement<'data> {
    pub(crate) order_index: usize,
    pub(crate) location_info: SectionLocationInfo<'data>,
    pub(crate) phdrs: Vec<&'data [u8]>,
}

/// Paired (or unpaired) `ONLY_IF_*` copies of the same output section name.
#[derive(Debug, Clone, Default)]
pub(crate) struct OnlyIfSlots<'data> {
    pub(crate) ro: Option<OnlyIfPlacement<'data>>,
    pub(crate) rw: Option<OnlyIfPlacement<'data>>,
    /// After seeing inputs, use the RW copy when this is set.
    pub(crate) prefer_rw: bool,
}

impl<'data> OnlyIfSlots<'data> {
    pub(crate) fn slot_mut(&mut self, only_if: OnlyIf) -> &mut Option<OnlyIfPlacement<'data>> {
        match only_if {
            OnlyIf::Ro => &mut self.ro,
            OnlyIf::Rw => &mut self.rw,
        }
    }

    pub(crate) fn chosen(&self) -> Option<&OnlyIfPlacement<'data>> {
        if self.prefer_rw {
            self.rw.as_ref().or(self.ro.as_ref())
        } else {
            self.ro.as_ref().or(self.rw.as_ref())
        }
    }
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
pub(crate) enum SecondaryOrder {
    InitFini { priority: u16 },
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
