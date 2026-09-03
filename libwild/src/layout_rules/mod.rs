//! Rules for helping determine how we're going to lay out the output file.

mod builder;
mod types;

use crate::error::Result;
use crate::glob_match::GlobPatternType;
use crate::glob_match::analyze_glob_pattern;
use crate::glob_match::compile_glob_pattern;
use crate::glob_match::unescape_pattern;
use crate::hash::hash_bytes;
use crate::output_section_id::OutputSectionId;
use crate::platform::Platform;
use crate::platform::SectionHeader;
#[allow(unused_imports)]
pub(crate) use builder::*;
use glob::Pattern;
use hashbrown::HashTable;
use std::borrow::Cow;
#[allow(unused_imports)]
pub(crate) use types::*;

/// A rule for determining what should be done with some input sections.
#[derive(Debug, Clone)]
pub(crate) struct SectionRule<'data> {
    /// Determine how the section rule matches against input section names.
    name_matcher: SectionNameMatcher<'data>,

    /// Pre-compiled glob pattern for matching input filenames. `None` means the rule matches all
    /// files.
    input_file_pattern: Option<Pattern>,

    /// Files matching any of these globs are excluded even if `input_file_pattern` matches.
    exclude_file_patterns: Vec<Pattern>,

    /// What to do if the rule matches.
    outcome: SectionRuleOutcome,
}

impl<'data> SectionRule<'data> {
    pub(crate) fn new(
        pattern: &'data [u8],
        input_file_pattern: Option<&'data [u8]>,
        outcome: SectionRuleOutcome,
    ) -> Result<Self> {
        let compiled_file_pattern = input_file_pattern
            .map(|pattern| compile_glob_pattern(pattern).map_err(|e| crate::error!("{e}")))
            .transpose()?;

        let name_matcher = match analyze_glob_pattern(pattern) {
            GlobPatternType::Exact => SectionNameMatcher::Exact(Cow::Borrowed(pattern)),
            GlobPatternType::EscapedExact => {
                SectionNameMatcher::Exact(Cow::Owned(unescape_pattern(pattern)))
            }
            GlobPatternType::Star | GlobPatternType::NonStar => {
                let compiled_pattern =
                    compile_glob_pattern(pattern).map_err(|e| crate::error!("{}", e))?;

                SectionNameMatcher::Glob(pattern, compiled_pattern)
            }
        };

        Ok(Self {
            name_matcher,
            input_file_pattern: compiled_file_pattern,
            exclude_file_patterns: Vec::new(),
            outcome,
        })
    }

    pub(crate) fn with_excludes(mut self, patterns: &[&'data [u8]]) -> Result<Self> {
        self.exclude_file_patterns = patterns
            .iter()
            .map(|pattern| compile_glob_pattern(pattern).map_err(|e| crate::error!("{e}")))
            .collect::<Result<Vec<_>>>()?;
        Ok(self)
    }

    #[inline(always)]
    fn matches(&self, section_name: &[u8], file_name: Option<&[u8]>) -> bool {
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
        }
    }
}

/// Multiplier for the rule-hashtable's capacity, relative to the number of entries. We want a
/// relatively sparse hashtable, since we may have a small number of entries with the same prefix
/// and thus the same hash. Also, during lookup, if there's no rule with a matching prefix, we want
/// to increase the chances of hitting an empty slot straight away. Experimentally, at least with
/// the built-in rules, multipliers larger than 2 don't further reduce the number of comparisons.
const RULE_TABLE_CAPACITY_MULTIPLIER: usize = 2;

impl<'data> SectionRules<'data> {
    fn from_rules(rules: &[SectionRule<'data>]) -> Self {
        let mut map = SectionRules {
            rules: HashTable::with_capacity(rules.len() * RULE_TABLE_CAPACITY_MULTIPLIER),
        };
        for rule in rules {
            let hash = section_name_prefix_hash(rule.name_matcher.prefix_bytes()).unwrap_or(0);

            map.rules.insert_unique(hash, rule.clone(), |existing| {
                section_name_prefix_hash(existing.name_matcher.prefix_bytes()).unwrap_or(0)
            });
        }

        map
    }

    #[inline(always)]
    pub(crate) fn lookup<P: Platform>(
        &self,
        section_name: &[u8],
        file_name: Option<&[u8]>,
        section_header: &impl SectionHeader,
    ) -> SectionRuleOutcome {
        if section_header.should_exclude() {
            return SectionRuleOutcome::Discard;
        }

        // GNU ld and LLD ignore reloc and symbol/string-table sections for script
        // wildcards. Input `.rela.text` must not fill `.rela.dyn : { *(.rela.*) }`,
        // and input `.symtab` must not be concatenated into the linker's table.
        if section_header.skip_linker_script_matching() {
            return SectionRuleOutcome::Discard;
        }

        if let Some(hash) = section_name_prefix_hash(section_name)
            && let Some(rule) = self
                .rules
                .find(hash, |rule| rule.matches(section_name, file_name))
        {
            return rule.outcome;
        }

        if section_name.is_empty() {
            return unnamed_section_output::<P>(section_header);
        }

        SectionRuleOutcome::Custom
    }
}

/// Returns a hash of the first four bytes of the supplied name, zero-padding if shorter than 4
/// bytes. Returns `None` if the name is empty.
fn section_name_prefix_hash(name: &[u8]) -> Option<u64> {
    if name.is_empty() {
        return None;
    }
    let mut buf = [0u8; 4];
    let len = name.len().min(4);
    buf[..len].copy_from_slice(&name[..len]);
    Some(hash_bytes(&buf))
}

/// Determines, where if anywhere, we should place an input section with no name.
pub(crate) fn unnamed_section_output<P: Platform>(
    section_header: &impl SectionHeader,
) -> SectionRuleOutcome {
    if !section_header.is_alloc() {
        SectionRuleOutcome::Discard
    } else if section_header.is_prog_bits() {
        if section_header.is_executable() {
            regular_section_or_discard(P::TEXT_SECTION_ID)
        } else if section_header.is_tls() {
            regular_section_or_discard(P::TDATA_SECTION_ID)
        } else if section_header.is_writable() {
            regular_section_or_discard(P::DATA_SECTION_ID)
        } else {
            regular_section_or_discard(P::RODATA_SECTION_ID)
        }
    } else if section_header.is_no_bits() {
        if section_header.is_tls() {
            regular_section_or_discard(P::TBSS_SECTION_ID)
        } else {
            regular_section_or_discard(P::BSS_SECTION_ID)
        }
    } else {
        SectionRuleOutcome::Discard
    }
}

fn regular_section_or_discard(section_id: Option<OutputSectionId>) -> SectionRuleOutcome {
    section_id.map_or(SectionRuleOutcome::Discard, |section_id| {
        SectionRuleOutcome::Section(SectionOutputInfo::regular(section_id))
    })
}

#[test]
fn test_section_mapping() {
    let rules = SectionRules::from_rules(&crate::elf::Elf64::default_layout_rules(
        &crate::args::elf::ElfArgs::new().unwrap(),
    ));
    let header = object::elf::SectionHeader64::<object::LittleEndian> {
        sh_name: Default::default(),
        sh_type: Default::default(),
        sh_flags: Default::default(),
        sh_addr: Default::default(),
        sh_offset: Default::default(),
        sh_size: Default::default(),
        sh_link: Default::default(),
        sh_info: Default::default(),
        sh_addralign: Default::default(),
        sh_entsize: Default::default(),
    };
    let lookup_name =
        |name: &str| rules.lookup::<crate::elf::Elf64>(name.as_bytes(), None, &header);

    assert_eq!(
        lookup_name(".comment"),
        SectionRuleOutcome::Section(SectionOutputInfo {
            section_id: crate::elf::output_section_id::COMMENT,
            must_keep: true,
            sorted: false,
            sort_by_init_priority: false,
            sort_by_alignment: false,
            input_order: false,
        })
    );

    let rela_header = object::elf::SectionHeader64::<object::LittleEndian> {
        sh_type: object::U32::new(object::LittleEndian, object::elf::SHT_RELA),
        ..header
    };
    assert_eq!(
        rules.lookup::<crate::elf::Elf64>(b".rela.data", None, &rela_header),
        SectionRuleOutcome::Discard
    );

    let symtab_header = object::elf::SectionHeader64::<object::LittleEndian> {
        sh_type: object::U32::new(object::LittleEndian, object::elf::SHT_SYMTAB),
        ..header
    };
    assert_eq!(
        rules.lookup::<crate::elf::Elf64>(b".symtab", None, &symtab_header),
        SectionRuleOutcome::Discard
    );
}

#[test]
fn test_glob_section_matching() {
    let rule = SectionRule::new(b".mydata.[0-9]", None, SectionRuleOutcome::Discard).unwrap();

    assert!(rule.matches(b".mydata.0", None));
    assert!(rule.matches(b".mydata.5", None));
    assert!(!rule.matches(b".mydata.A", None));
    assert!(!rule.matches(b".mydata.10", None));
    assert!(!rule.matches(b".mydata.", None));
    assert!(!rule.matches(b".other.0", None));
}

#[test]
fn test_glob_star_anywhere() {
    let rule = SectionRule::new(b".text.*.foo", None, SectionRuleOutcome::Discard).unwrap();
    assert!(rule.matches(b".text.bar.foo", None));
    assert!(rule.matches(b".text.baz.foo", None));
    assert!(!rule.matches(b".text.bar.baz", None));
}

#[test]
fn test_glob_section_character_class() {
    let rule = SectionRule::new(b"foo[_-]bar", None, SectionRuleOutcome::Discard).unwrap();
    assert!(rule.matches(b"foo_bar", None));
    assert!(rule.matches(b"foo-bar", None));
    assert!(!rule.matches(b"foobar", None));
    assert!(!rule.matches(b"foo_barbaz", None));
    assert!(!rule.matches(b"fooxbar", None));

    // [a-z] alphabet range match
    let range_rule = SectionRule::new(b"foo[a-z]bar", None, SectionRuleOutcome::Discard).unwrap();
    assert!(range_rule.matches(b"fooabar", None));
    assert!(range_rule.matches(b"foozbar", None));
    assert!(range_rule.matches(b"foombar", None));
    assert!(!range_rule.matches(b"fooAbar", None));
    assert!(!range_rule.matches(b"foo1bar", None));

    // escaped character match
    let escape_rule = SectionRule::new(b"foo\\*bar", None, SectionRuleOutcome::Discard).unwrap();
    assert!(escape_rule.matches(b"foo*bar", None));
    assert!(!escape_rule.matches(b"fooxbar", None));
    assert!(!escape_rule.matches(b"foobar", None));
}
