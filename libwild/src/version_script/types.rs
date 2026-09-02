use crate::bail;
use crate::error::Result;
use crate::glob_match::unescape_pattern;
use crate::hash::PassThroughHasher;
use crate::hash::PreHashed;
use crate::symbol::UnversionedSymbolName;
use glob::Pattern;
use hashbrown::HashMap;
use hashbrown::HashSet;
use symbolic_demangle::Demangle;
use symbolic_demangle::DemangleOptions;

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct MatchRules<'data> {
    pub(crate) general: BasicMatchRules<'data>,
    pub(crate) cxx: BasicMatchRules<'data>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct VersionBody<'data> {
    pub(crate) globals: MatchRules<'data>,
    pub(crate) locals: MatchRules<'data>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Version<'data> {
    pub(crate) name: &'data [u8],
    pub(crate) parent_index: Option<u16>,
    pub(crate) version_body: VersionBody<'data>,
}

/// A general version script. See https://sourceware.org/binutils/docs/ld/VERSION.html
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct RegularVersionScript<'data> {
    pub(crate) versions: Vec<Version<'data>>,
    pub(crate) version_name_mapping: HashMap<&'data [u8], usize>,
}

/// An optimized version script for Rustc.
/// It declares all symbols as local except for the explicitly listed global symbols.
/// Only contains general (non-C++) exact symbol matchers.
/// Doesn't use actual versioning.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct RustVersionScript<'data> {
    pub(crate) global: Vec<&'data [u8]>,
}

#[derive(Debug, PartialEq, Eq)]
/// Possibly specialized version script.
/// See `RegularVersionScript` for the general case.
pub(crate) enum VersionScript<'data> {
    Regular(RegularVersionScript<'data>),
    Rust(RustVersionScript<'data>),
}

impl Default for VersionScript<'_> {
    fn default() -> Self {
        VersionScript::Regular(RegularVersionScript::default())
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub(crate) enum SymbolMatcher<'data> {
    // Exact match.
    Exact(&'data [u8]),
    // Exact match with escape sequences that need unescaping.
    EscapedExact(&'data [u8]),
    // A glob pattern with a '*' token.
    StarGlob(Pattern),
    // A glob pattern without any '*' token.
    NonstarGlob(Pattern),
    /// Glob pattern equal to '*'
    MatchesAll,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct BasicMatchRules<'data> {
    pub(crate) exact: HashSet<PreHashed<UnversionedSymbolName<'data>>, PassThroughHasher>,
    pub(crate) escaped_exact: HashSet<Vec<u8>>,
    pub(crate) star_globs: Vec<Pattern>,
    pub(crate) nonstar_globs: Vec<Pattern>,
    pub(crate) matches_all: bool,
}

impl<'data> BasicMatchRules<'data> {
    fn push(&mut self, pattern: SymbolMatcher<'data>) {
        match pattern {
            SymbolMatcher::MatchesAll => self.matches_all = true,
            SymbolMatcher::StarGlob(glob) => self.star_globs.push(glob),
            SymbolMatcher::NonstarGlob(glob) => self.nonstar_globs.push(glob),
            SymbolMatcher::Exact(exact) => {
                self.exact.insert(UnversionedSymbolName::prehashed(exact));
            }
            SymbolMatcher::EscapedExact(escaped) => {
                let unescaped = unescape_pattern(escaped);
                self.escaped_exact.insert(unescaped);
            }
        }
    }

    #[inline]
    pub(crate) fn matches_exact(
        &self,
        lookup: &mut SymbolLookupNameWrapper,
        mangled: bool,
    ) -> bool {
        // Check normal exact matches first
        if !self.exact.is_empty() {
            if mangled {
                let demangled_name = lookup.get_demangled_name();
                // The creation of UnversionedSymbolName should be relatively cheap as we construct
                // it at most twice.
                if self
                    .exact
                    .contains(&UnversionedSymbolName::prehashed(demangled_name.as_bytes()))
                {
                    return true;
                }
            } else if self.exact.contains(lookup.name) {
                return true;
            }
        }

        // Check escaped exact matches
        if !self.escaped_exact.is_empty() {
            let symbol_bytes = if mangled {
                let demangled_name = lookup.get_demangled_name();
                demangled_name.as_bytes()
            } else {
                lookup.name.bytes()
            };

            if self.escaped_exact.contains(symbol_bytes) {
                return true;
            }
        }

        false
    }

    #[inline]
    pub(crate) fn matches_glob(
        &self,
        lookup: &mut SymbolLookupNameWrapper,
        non_star: bool,
        mangled: bool,
    ) -> bool {
        let mut globs = if non_star {
            self.nonstar_globs.iter().peekable()
        } else {
            self.star_globs.iter().peekable()
        };
        // Early exit before we actually demangle the name.
        if globs.peek().is_none() {
            return false;
        }

        let name = if mangled {
            lookup.get_demangled_name()
        } else {
            lookup.get_name_string()
        };

        globs.any(|pattern| pattern.matches(name))
    }

    #[inline]
    pub(crate) fn matches_all(&self) -> bool {
        self.matches_all
    }
}

pub(crate) enum VersionRuleSection {
    Global,
    Local,
}

#[derive(Debug)]
pub(crate) enum ParsedSymbolMatcher<'data> {
    Single(SymbolMatcher<'data>),
    Multiple(Vec<SymbolMatcher<'data>>),
    CxxMatchers(Vec<SymbolMatcher<'data>>),
}

impl<'data> MatchRules<'data> {
    pub(crate) fn push(&mut self, pattern: ParsedSymbolMatcher<'data>) {
        match pattern {
            ParsedSymbolMatcher::Single(single) => {
                self.general.push(single);
            }
            ParsedSymbolMatcher::Multiple(matchers) => {
                for matcher in matchers {
                    self.general.push(matcher);
                }
            }
            ParsedSymbolMatcher::CxxMatchers(matchers) => {
                for matcher in matchers {
                    self.cxx.push(matcher);
                }
            }
        }
    }
}

pub(crate) struct SymbolLookupNameWrapper<'data> {
    name: &'data PreHashed<UnversionedSymbolName<'data>>,
    name_string: Option<&'data str>,
    demangled_name: Option<String>,
}

impl<'data> SymbolLookupNameWrapper<'data> {
    pub(crate) fn from_name(name: &'data PreHashed<UnversionedSymbolName<'data>>) -> Self {
        Self {
            name,
            name_string: None,
            demangled_name: None,
        }
    }

    pub(crate) fn get_name_string(&mut self) -> &'data str {
        self.name_string.get_or_insert_with(|| {
            str::from_utf8(self.name.bytes()).unwrap_or_else(|_| {
                panic!(
                    "Valid utf-8 identifier expected: {}",
                    String::from_utf8_lossy(self.name.bytes())
                )
            })
        })
    }

    pub(crate) fn get_demangled_name(&mut self) -> &String {
        // Extract the name string before the closure to avoid double mutable borrow
        let name_string = self.get_name_string();
        self.demangled_name.get_or_insert_with(|| {
            symbolic_common::Name::new(
                name_string,
                symbolic_common::NameMangling::Mangled,
                symbolic_common::Language::Cpp,
            )
            .demangle(DemangleOptions::complete().return_type(false))
            // Consider the original name if the demangler returns None.
            .unwrap_or_else(|| name_string.to_string())
        })
    }
}

impl<'data> RegularVersionScript<'data> {
    pub(crate) fn find_match(
        &self,
        name: &PreHashed<UnversionedSymbolName>,
    ) -> Option<(usize, VersionRuleSection)> {
        // Perform symbol lookup the same was as described for the LLD (and partially Mold) linker:
        // https://maskray.me/blog/2020-11-26-all-about-symbol-versioning#version-script
        let mut lookup_name = SymbolLookupNameWrapper::from_name(name);

        // 1) The first version tag with an exact pattern wins.
        for (i, version) in self.versions.iter().enumerate() {
            let body = &version.version_body;

            if body.globals.general.matches_exact(&mut lookup_name, false) {
                return Some((i, VersionRuleSection::Global));
            } else if body.locals.general.matches_exact(&mut lookup_name, false) {
                return Some((i, VersionRuleSection::Local));
            // Intentionally try first non-mangled names as it's much cheaper test.
            } else if body.globals.cxx.matches_exact(&mut lookup_name, true) {
                return Some((i, VersionRuleSection::Global));
            } else if body.locals.cxx.matches_exact(&mut lookup_name, true) {
                return Some((i, VersionRuleSection::Local));
            }
        }

        // 2) Otherwise, the last version tag with a non-* wildcard pattern wins ('global' should be
        //    checked first). Otherwise, the last version tag with a * pattern wins.
        for &non_star in &[true, false] {
            for (i, version) in self.versions.iter().enumerate().rev() {
                let body = &version.version_body;
                if body
                    .globals
                    .general
                    .matches_glob(&mut lookup_name, non_star, false)
                    || body
                        .globals
                        .cxx
                        .matches_glob(&mut lookup_name, non_star, true)
                {
                    return Some((i, VersionRuleSection::Global));
                } else if body
                    .locals
                    .general
                    .matches_glob(&mut lookup_name, non_star, false)
                    || body
                        .locals
                        .cxx
                        .matches_glob(&mut lookup_name, non_star, true)
                {
                    return Some((i, VersionRuleSection::Local));
                }
            }
        }

        // 3) Otherwise, the last version tag with match all (*).
        for (i, version) in self.versions.iter().enumerate().rev() {
            let body = &version.version_body;
            if body.globals.general.matches_all || body.globals.cxx.matches_all {
                return Some((i, VersionRuleSection::Global));
            } else if body.locals.general.matches_all || body.locals.cxx.matches_all {
                return Some((i, VersionRuleSection::Local));
            }
        }

        None
    }
}

impl<'data> VersionScript<'data> {
    pub(crate) fn version_count(&self) -> u16 {
        match self {
            VersionScript::Regular(script) => script.version_count(),
            VersionScript::Rust(_) => 0,
        }
    }

    pub(crate) fn parent_count(&self) -> u16 {
        match self {
            VersionScript::Regular(script) => script.parent_count(),
            VersionScript::Rust(_) => 0,
        }
    }
    pub(crate) fn version_for_symbol(
        &self,
        name: &PreHashed<UnversionedSymbolName>,
        version_name: Option<&[u8]>,
    ) -> Result<Option<object::elf::VersionIndex>> {
        match self {
            VersionScript::Regular(script) => script.version_for_symbol(name, version_name),
            VersionScript::Rust(_) => Ok(None),
        }
    }
}

impl<'data> RegularVersionScript<'data> {
    pub(crate) fn is_local(&self, name: &PreHashed<UnversionedSymbolName>) -> bool {
        self.find_match(name)
            .is_some_and(|(_, rule)| matches!(rule, VersionRuleSection::Local))
    }

    /// Number of versions in the Version Script, including the base version.
    pub(crate) fn version_count(&self) -> u16 {
        if self.versions.len() == 1 {
            // Ignore it if we have just the base version.
            0
        } else {
            self.versions.len() as u16
        }
    }

    pub(crate) fn parent_count(&self) -> u16 {
        self.versions
            .iter()
            .filter(|v| v.parent_index.is_some())
            .count() as u16
    }

    pub(crate) fn version_iter(&self) -> impl Iterator<Item = &Version<'data>> {
        self.versions.iter()
    }

    pub(crate) fn version_for_symbol(
        &self,
        name: &PreHashed<UnversionedSymbolName>,
        version_name: Option<&[u8]>,
    ) -> Result<Option<object::elf::VersionIndex>> {
        let name_bytes = name.bytes();
        if let Some(version_name) = version_name {
            // There is a quirk that I couldn't find docs for. When a symbol has an empty version
            // (e.g. "foo@"), the versioning is disabled and the symbol has "hidden global version"
            // (visible as `1h <whitespaces>` in `readelf -V`), even if that symbol appears in the
            // version script.
            if version_name.is_empty() {
                return Ok(Some(object::elf::VER_NDX_GLOBAL));
            } else if let Some(&number) = self.version_name_mapping.get(version_name) {
                return Ok(Some(object::elf::VER_NDX_GLOBAL + number as u16));
            }
            bail!(
                "Symbol {} has undefined version {}",
                String::from_utf8_lossy(name_bytes),
                String::from_utf8_lossy(version_name),
            );
        }

        Ok(self.find_match(name).and_then(|(number, _)| {
            if number == 0 {
                // Ignore the implicit version!
                None
            } else {
                Some(object::elf::VER_NDX_GLOBAL + number as u16)
            }
        }))
    }
}
