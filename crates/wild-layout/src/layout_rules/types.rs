use super::*;
use crate::EnginePlatform;
use crate::output_section_id::OutputSectionId;
use crate::output_section_id::SectionIdentity;
use crate::parsing::SymbolLoc;
use hashbrown::HashTable;
use wild_platform::Platform;
use wild_platform::SectionOutputInfo;
use wild_platform::SectionRuleOutcome;
use wild_scripts::linker_script;

pub struct LayoutRules<'data> {
    pub section_rules: SectionRules<'data>,
}
#[derive(Debug, Clone, Copy)]
pub enum SectionKind<'data, P: Platform> {
    /// This is the primary section.
    Primary(SectionIdentity<'data, P>),

    /// This is a secondary section that will be merged into the primary. The ID of the primary is
    /// supplied.
    Secondary(OutputSectionId),
}

/// Rules governing how input sections should be mapped to output sections.
pub struct SectionRules<'data> {
    /// Rules by the hash of the first 4 bytes of the name.
    pub rules: HashTable<SectionRule<'data>>,
}

pub fn section_rule_from_id<P: EnginePlatform>(
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocationCounter<'data> {
    Absolute(linker_script::Expression<'data>, SymbolLoc),
    Relative(linker_script::Expression<'data>, SymbolLoc, OutputSectionId),
}

impl<'data> LocationCounter<'data> {
    pub fn get_expression(&self) -> &linker_script::Expression<'data> {
        match self {
            LocationCounter::Absolute(expr, ..) => expr,
            LocationCounter::Relative(expr, ..) => expr,
        }
    }
}
