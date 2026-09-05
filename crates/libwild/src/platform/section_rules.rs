use super::output_section_id::OutputSectionId;
use glob::Pattern;
use hashbrown::HashSet;
use std::borrow::Cow;
use wild_error::error::Result;
use wild_scripts::linker_script::OnlyIf;
use wild_util::glob_match::GlobPatternType;
use wild_util::glob_match::analyze_glob_pattern;
use wild_util::glob_match::compile_glob_pattern;
use wild_util::glob_match::unescape_pattern;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SectionOutputInfo {
    pub(crate) section_id: OutputSectionId,
    pub(crate) must_keep: bool,
    pub(crate) sorted: bool,
    pub(crate) sort_by_init_priority: bool,
    pub(crate) sort_by_alignment: bool,
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
            sort_by_alignment: false,
            input_order: false,
        }
    }

    pub(crate) const fn keep(section_id: OutputSectionId) -> Self {
        Self {
            section_id,
            must_keep: true,
            sorted: false,
            sort_by_init_priority: false,
            sort_by_alignment: false,
            input_order: false,
        }
    }
}

/// A rule for determining what should be done with some input sections.
#[derive(Debug, Clone)]
pub(crate) struct SectionRule<'data> {
    /// Determine how the section rule matches against input section names.
    pub(crate) name_matcher: SectionNameMatcher<'data>,

    /// Pre-compiled glob pattern for matching input filenames. `None` means the rule matches all
    /// files.
    input_file_pattern: Option<Pattern>,

    /// Files matching any of these globs are excluded even if `input_file_pattern` matches.
    exclude_file_patterns: Vec<Pattern>,

    /// What to do if the rule matches.
    pub(crate) outcome: SectionRuleOutcome,

    /// GNU `ONLY_IF_RO` / `ONLY_IF_RW` on the output section this rule feeds.
    pub(crate) only_if: Option<OnlyIf>,

    /// Output section used to group an `ONLY_IF_RO` copy with its `ONLY_IF_RW` pair.
    pub(crate) only_if_section_id: Option<OutputSectionId>,
}

impl<'data> SectionRule<'data> {
    pub(crate) fn new(
        pattern: &'data [u8],
        input_file_pattern: Option<&'data [u8]>,
        outcome: SectionRuleOutcome,
    ) -> Result<Self> {
        let compiled_file_pattern = input_file_pattern
            .map(|pattern| compile_glob_pattern(pattern).map_err(|e| wild_error::error!("{e}")))
            .transpose()?;

        let name_matcher = match analyze_glob_pattern(pattern) {
            GlobPatternType::Exact => SectionNameMatcher::Exact(Cow::Borrowed(pattern)),
            GlobPatternType::EscapedExact => {
                SectionNameMatcher::Exact(Cow::Owned(unescape_pattern(pattern)))
            }
            GlobPatternType::Star | GlobPatternType::NonStar => {
                let compiled_pattern =
                    compile_glob_pattern(pattern).map_err(|e| wild_error::error!("{}", e))?;

                SectionNameMatcher::Glob(pattern, compiled_pattern)
            }
        };

        Ok(Self {
            name_matcher,
            input_file_pattern: compiled_file_pattern,
            exclude_file_patterns: Vec::new(),
            outcome,
            only_if: None,
            only_if_section_id: None,
        })
    }

    pub(crate) fn with_only_if(
        mut self,
        only_if: Option<OnlyIf>,
        section_id: OutputSectionId,
    ) -> Self {
        self.only_if = only_if;
        if only_if.is_some() {
            self.only_if_section_id = Some(section_id);
        }
        self
    }

    pub(crate) fn with_excludes(mut self, patterns: &[&'data [u8]]) -> Result<Self> {
        self.exclude_file_patterns = patterns
            .iter()
            .map(|pattern| compile_glob_pattern(pattern).map_err(|e| wild_error::error!("{e}")))
            .collect::<Result<Vec<_>>>()?;
        Ok(self)
    }

    #[inline(always)]
    pub(crate) fn matches(&self, section_name: &[u8], file_name: Option<&[u8]>) -> bool {
        let section_matches = match &self.name_matcher {
            SectionNameMatcher::Exact(name) => section_name == name.as_ref(),
            SectionNameMatcher::Prefix(prefix) => section_name.starts_with(prefix),
            SectionNameMatcher::Glob(_, pattern)
                if let Ok(name_str) = std::str::from_utf8(section_name) =>
            {
                pattern.matches(name_str)
            }
            SectionNameMatcher::Glob(_, _) => false,
        };

        if !section_matches {
            return false;
        }

        // If the rule has no file pattern, it matches all files.
        let Some(pattern) = &self.input_file_pattern else {
            return !self.file_is_excluded(file_name);
        };

        // If the caller didn't provide a filename, only match rules with no file filter.
        let Some(name) = file_name else {
            return false;
        };

        // Convert the filename bytes to a string for glob matching.
        let Ok(name_str) = std::str::from_utf8(name) else {
            return false;
        };

        pattern.matches(name_str) && !self.file_is_excluded(file_name)
    }

    fn file_is_excluded(&self, file_name: Option<&[u8]>) -> bool {
        if self.exclude_file_patterns.is_empty() {
            return false;
        }
        let Some(name) = file_name else {
            return false;
        };
        let Ok(name_str) = std::str::from_utf8(name) else {
            return false;
        };
        self.exclude_file_patterns
            .iter()
            .any(|pattern| pattern.matches(name_str))
    }

    pub(crate) const fn exact_section(
        name: &'data [u8],
        section_id: OutputSectionId,
    ) -> SectionRule<'data> {
        Self::exact(
            name,
            SectionRuleOutcome::Section(SectionOutputInfo::regular(section_id)),
        )
    }

    pub(crate) const fn exact_section_keep(
        name: &'data [u8],
        section_id: OutputSectionId,
    ) -> SectionRule<'data> {
        Self::exact(
            name,
            SectionRuleOutcome::Section(SectionOutputInfo::keep(section_id)),
        )
    }

    pub(crate) const fn prefix_section(
        name: &'data [u8],
        section_id: OutputSectionId,
    ) -> SectionRule<'data> {
        Self::prefix(
            name,
            SectionRuleOutcome::Section(SectionOutputInfo::regular(section_id)),
        )
    }

    pub(crate) const fn prefix_section_sort(
        name: &'data [u8],
        section_id: OutputSectionId,
    ) -> SectionRule<'data> {
        SectionRule {
            name_matcher: SectionNameMatcher::Prefix(name),
            input_file_pattern: None,
            exclude_file_patterns: Vec::new(),
            outcome: SectionRuleOutcome::SortedSection(SectionOutputInfo::keep(section_id)),
            only_if: None,
            only_if_section_id: None,
        }
    }

    pub(crate) const fn exact(
        name: &'data [u8],
        outcome: SectionRuleOutcome,
    ) -> SectionRule<'data> {
        SectionRule {
            name_matcher: SectionNameMatcher::Exact(Cow::Borrowed(name)),
            input_file_pattern: None,
            exclude_file_patterns: Vec::new(),
            outcome,
            only_if: None,
            only_if_section_id: None,
        }
    }

    pub(crate) const fn prefix(
        name: &'data [u8],
        outcome: SectionRuleOutcome,
    ) -> SectionRule<'data> {
        SectionRule {
            name_matcher: SectionNameMatcher::Prefix(name),
            input_file_pattern: None,
            exclude_file_patterns: Vec::new(),
            outcome,
            only_if: None,
            only_if_section_id: None,
        }
    }

    pub(crate) fn allows_only_if(&self, writable_sections: &HashSet<OutputSectionId>) -> bool {
        match self.only_if {
            None => true,
            Some(OnlyIf::Ro) => !self
                .only_if_section_id
                .is_some_and(|id| writable_sections.contains(&id)),
            Some(OnlyIf::Rw) => self
                .only_if_section_id
                .is_some_and(|id| writable_sections.contains(&id)),
        }
    }
}
