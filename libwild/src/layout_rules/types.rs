use super::*;
use crate::linker_script;
use crate::output_section_id::OutputSectionId;
use crate::output_section_id::SectionIdentity;
use crate::parsing::SymbolLoc;
use crate::platform::Platform;
use glob::Pattern;
use hashbrown::HashTable;
use std::borrow::Cow;

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

/// Determines how a section name pattern is matched against input section names.
#[derive(Debug, Clone)]
pub(crate) enum SectionNameMatcher<'data> {
    /// Matches sections whose name is exactly equal to the stored bytes.
    Exact(Cow<'data, [u8]>),

    /// Matches sections whose name starts with the stored bytes.
    Prefix(&'data [u8]),

    /// Matches sections whose name matches the glob pattern. The byte slice is the
    /// literal prefix used as hash table key.
    Glob(&'data [u8], Pattern),
}

/// Return the literal byte prefix of this matcher, used for hash table keying.
impl<'data> SectionNameMatcher<'data> {
    pub(crate) fn prefix_bytes(&self) -> &[u8] {
        match self {
            Self::Exact(n) => n.as_ref(),
            Self::Prefix(n) | Self::Glob(n, _) => n,
        }
    }
}
/// What should be done with a particular input section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SectionRuleOutcome {
    Section(SectionOutputInfo),
    Discard,
    Custom,
    EhFrame,
    NoteGnuProperty,
    NoteGnuStack,
    Debug,
    DebugIndex,
    RiscVAttribute,
    SortedSection(SectionOutputInfo),
}

impl SectionRuleOutcome {
    pub(crate) fn section_rule_from_id<P: Platform>(
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SectionOutputInfo {
    pub(crate) section_id: OutputSectionId,
    pub(crate) must_keep: bool,
    pub(crate) sorted: bool,
    pub(crate) sort_by_init_priority: bool,
    /// GNU ld default for script matchers without `SORT*`: input order, each input
    /// aligned to its own `sh_addralign`.
    pub(crate) input_order: bool,
}

impl SectionOutputInfo {
    pub(crate) const fn regular(section_id: OutputSectionId) -> Self {
        Self {
            section_id,
            must_keep: false,
            sorted: false,
            sort_by_init_priority: false,
            input_order: false,
        }
    }

    pub(crate) const fn keep(section_id: OutputSectionId) -> Self {
        Self {
            section_id,
            must_keep: true,
            sorted: false,
            sort_by_init_priority: false,
            input_order: false,
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
