use super::*;
use crate::layout::EnginePlatform;
use crate::linker_script;
use crate::output_section_id::OutputSectionId;
use crate::output_section_id::SectionIdentity;
use crate::parsing::SymbolLoc;
use crate::platform::Platform;
use crate::platform::SectionOutputInfo;
use crate::platform::SectionRuleOutcome;
use hashbrown::HashTable;

pub(crate) struct LayoutRules<'data> {
    pub(crate) section_rules: SectionRules<'data>,
}
#[derive(Debug, Clone, Copy)]
pub(crate) enum SectionKind<'data, P: Platform> {
    /// This is the primary section.
    Primary(SectionIdentity<'data, P>),

    /// This is a secondary section that will be merged into the primary. The ID of the primary is
    /// supplied.
    Secondary(OutputSectionId),
}

/// Rules governing how input sections should be mapped to output sections.
pub(crate) struct SectionRules<'data> {
    /// Rules by the hash of the first 4 bytes of the name.
    pub(crate) rules: HashTable<SectionRule<'data>>,
}

impl SectionRuleOutcome {
    pub(crate) fn section_rule_from_id<P: EnginePlatform>(
        section_id: OutputSectionId,
        output_info: SectionOutputInfo,
    ) -> SectionRuleOutcome {
        if Some(section_id) == P::EH_FRAME_SECTION_ID {
            SectionRuleOutcome::EhFrame
        } else if Some(section_id) == P::NOTE_GNU_PROPERTY_SECTION_ID {
            SectionRuleOutcome::NoteGnuProperty
        } else if Some(section_id) == P::RISCV_ATTRIBUTES_SECTION_ID {
            SectionRuleOutcome::RiscVAttribute
        } else {
            SectionRuleOutcome::Section(output_info)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LocationCounter<'data> {
    Absolute(linker_script::Expression<'data>, SymbolLoc),
    Relative(linker_script::Expression<'data>, SymbolLoc, OutputSectionId),
}

impl<'data> LocationCounter<'data> {
    pub(crate) fn get_expression(&self) -> &linker_script::Expression<'data> {
        match self {
            LocationCounter::Absolute(expr, ..) => expr,
            LocationCounter::Relative(expr, ..) => expr,
        }
    }
}
