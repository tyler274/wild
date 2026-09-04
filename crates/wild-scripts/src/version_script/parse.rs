use super::types::*;
use crate::linker_script::skip_comments_and_whitespace;
use crate::script_data::ScriptData;
use glob::Pattern;
use hashbrown::HashMap;
use wild_error::error;
use wild_error::error::Result;
use wild_util::glob_match::GlobPatternType;
use wild_util::glob_match::analyze_glob_pattern;
use wild_util::glob_match::compile_glob_pattern;
use winnow::BStr;
use winnow::Parser;
use winnow::error::ContextError;
use winnow::error::FromExternalError;
use winnow::token::take_until;
use winnow::token::take_while;

fn parse_version_script<'input>(input: &mut &'input BStr) -> winnow::Result<VersionScript<'input>> {
    // List of version names in the script, used to map parent version to version indexes
    let mut version_names: Vec<&[u8]> = Vec::new();

    skip_comments_and_whitespace(input)?;

    // Simple version script, only defines symbols visibility
    // May be rust-style version script.
    if input.starts_with(b"{") {
        let version_body = parse_version_section(input)?;

        ";".parse_next(input)?;

        skip_comments_and_whitespace(input)?;

        return Ok(version_body.into());
    }

    // Multiple versions, won't be rust-style.
    let mut version_script = RegularVersionScript::default();

    // Base version placeholder
    version_names.push(b"");
    version_script.versions.push(Version::default());

    while !input.is_empty() {
        let name = parse_token(input)?;

        skip_comments_and_whitespace(input)?;

        let version_body = parse_version_section(input)?.into();

        let parent_name = take_until(0.., b';').parse_next(input)?;

        let parent_index = if parent_name.is_empty() {
            None
        } else {
            // We don't expect lots of versions, so a linear scan seems reasonable.
            Some(
                version_names
                    .iter()
                    .position(|v| v == &parent_name)
                    .ok_or_else(|| {
                        ContextError::from_external_error(
                            input,
                            VersionScriptError::UnknownParentVersion,
                        )
                    })? as u16,
            )
        };

        ";".parse_next(input)?;

        skip_comments_and_whitespace(input)?;

        version_names.push(name);
        version_script.versions.push(Version {
            name,
            parent_index,
            version_body,
        });
        version_script
            .version_name_mapping
            .insert(name, version_script.versions.len() - 1);
    }

    Ok(VersionScript::Regular(version_script))
}
impl<'data> VersionScript<'data> {
    pub fn parse(data: ScriptData<'data>) -> Result<VersionScript<'data>> {
        let _span = tracing::info_span!("Parse version script").entered();

        parse_version_script
            .parse(BStr::new(data.raw))
            .map_err(|err| error!("Failed to parse version script:\n{err}"))
    }
}

impl<'data> RegularVersionScript<'data> {
    #[cfg(test)]
    fn parse(data: ScriptData<'data>) -> Result<RegularVersionScript<'data>> {
        match VersionScript::parse(data)? {
            VersionScript::Regular(script) => Ok(script),
            VersionScript::Rust(_) => {
                wild_error::bail!(
                    "Rust-style version script cannot be used as a regular version script"
                )
            }
        }
    }
}

#[derive(Debug, Default)]
/// A generic parsed version body before version script specialization optimizations.
struct RawVersionBody<'data> {
    pub globals: Vec<ParsedSymbolMatcher<'data>>,
    pub locals: Vec<ParsedSymbolMatcher<'data>>,
}

impl<'data> RawVersionBody<'data> {
    fn rust_like(&self) -> bool {
        // one of the local has to be match-all `*` wildcard
        if !self.locals.iter().any(|matcher| {
            matches!(
                matcher,
                ParsedSymbolMatcher::Single(SymbolMatcher::MatchesAll)
            )
        }) {
            return false;
        }

        // and only exact matchers in global
        self.globals.iter().all(|matcher| {
            matches!(
                matcher,
                ParsedSymbolMatcher::Single(SymbolMatcher::Exact(_))
            )
        })
    }
}

impl<'data> TryFrom<RawVersionBody<'data>> for RustVersionScript<'data> {
    type Error = RawVersionBody<'data>;

    fn try_from(body: RawVersionBody<'data>) -> Result<Self, Self::Error> {
        if !body.rust_like() {
            return Err(body);
        }
        let global = body
            .globals
            .into_iter()
            .map(|matcher| {
                if let ParsedSymbolMatcher::Single(SymbolMatcher::Exact(name)) = matcher {
                    name
                } else {
                    unreachable!()
                }
            })
            .collect();

        Ok(RustVersionScript { global })
    }
}

impl<'data> From<RawVersionBody<'data>> for VersionScript<'data> {
    fn from(body: RawVersionBody<'data>) -> Self {
        match RustVersionScript::try_from(body) {
            Ok(rust_script) => VersionScript::Rust(rust_script),
            Err(body) => {
                let version_body = body.into();
                VersionScript::Regular(RegularVersionScript {
                    versions: vec![Version {
                        version_body,
                        ..Default::default()
                    }],
                    version_name_mapping: HashMap::new(),
                })
            }
        }
    }
}

impl<'data> From<RawVersionBody<'data>> for VersionBody<'data> {
    fn from(body: RawVersionBody<'data>) -> Self {
        let mut out = VersionBody::default();
        for global in body.globals {
            out.globals.push(global);
        }
        for local in body.locals {
            out.locals.push(local);
        }
        out
    }
}

fn parse_version_section<'data>(input: &mut &'data BStr) -> winnow::Result<RawVersionBody<'data>> {
    let mut section = None;

    let mut out = RawVersionBody::default();

    '{'.parse_next(input)?;

    loop {
        skip_comments_and_whitespace(input)?;

        if try_take(input, b"}") {
            skip_comments_and_whitespace(input)?;
            break;
        }

        if try_take(input, b"global:") {
            section = Some(VersionRuleSection::Global);
        } else if try_take(input, b"local:") {
            section = Some(VersionRuleSection::Local);
        } else {
            let matcher = parse_matcher(input, false)?;

            match section {
                Some(VersionRuleSection::Global) | None => {
                    out.globals.push(matcher);
                }
                Some(VersionRuleSection::Local) => {
                    out.locals.push(matcher);
                }
            }
        }
    }

    Ok(out)
}

pub fn parse_matcher<'data>(
    input: &mut &'data BStr,
    without_semicolon: bool, // e.g. symbol to export passed via CLI arg
) -> winnow::Result<ParsedSymbolMatcher<'data>> {
    if try_take(input, b"extern ") {
        let mut matchers = Vec::new();
        let cxx = if try_take(input, b"\"C++\"") {
            true
        } else if try_take(input, b"\"C\"") {
            false
        } else {
            let unsupported_extern: String = "{".parse_to().parse_next(input)?;
            return Err(ContextError::from_external_error(
                input,
                VersionScriptError::UnsupportedExtern(unsupported_extern),
            ));
        };
        skip_comments_and_whitespace(input)?;
        '{'.parse_next(input)?;

        loop {
            skip_comments_and_whitespace(input)?;

            if try_take(input, b"};") {
                skip_comments_and_whitespace(input)?;
                break;
            }

            // Symbols at the end of `extern` blocks may omit semicolons
            let expect_semicolon = {
                let remaining = &**input;
                if let Some(close_pos) = remaining.windows(2).position(|w| w == b"};") {
                    remaining[..close_pos].contains(&b';')
                } else {
                    without_semicolon
                }
            };

            let matcher = parse_matcher(input, !expect_semicolon)?;
            let ParsedSymbolMatcher::Single(matcher) = matcher else {
                let unexpected_extern = if matches!(matcher, ParsedSymbolMatcher::CxxMatchers(_)) {
                    "C++"
                } else {
                    "C"
                };
                return Err(ContextError::from_external_error(
                    input,
                    VersionScriptError::UnexpectedExtern(unexpected_extern.to_string()),
                ));
            };

            matchers.push(matcher);
        }

        if cxx {
            return Ok(ParsedSymbolMatcher::CxxMatchers(matchers));
        }
        return Ok(ParsedSymbolMatcher::Multiple(matchers));
    }

    let token = if without_semicolon {
        if input.contains(&b'}') {
            take_until(1.., b'}').parse_next(input)?
        } else {
            // TODO: Clippy bug
            #[allow(clippy::needless_borrow)]
            &input
        }
    } else {
        take_until(1.., b';').parse_next(input)?
    };

    skip_comments_and_whitespace(input)?;

    try_take(input, b";");

    let token = token.trim_ascii_end();

    Ok(ParsedSymbolMatcher::Single(
        if let Some(unquoted) = token
            .strip_prefix(b"\"")
            .and_then(|t| t.strip_suffix(b"\""))
        {
            SymbolMatcher::Exact(unquoted)
        } else if token == b"*" {
            SymbolMatcher::MatchesAll
        } else {
            let glob_type = analyze_glob_pattern(token);

            let create_pattern = |token: &[u8]| -> winnow::Result<Pattern> {
                compile_glob_pattern(token).map_err(|e| {
                    ContextError::from_external_error(
                        input,
                        match e {
                            "Invalid UTF-8 string" => VersionScriptError::InvalidUtf8String,
                            _ => VersionScriptError::InvalidGlobPattern,
                        },
                    )
                })
            };

            match glob_type {
                GlobPatternType::Exact => SymbolMatcher::Exact(token),
                GlobPatternType::EscapedExact => SymbolMatcher::EscapedExact(token),
                GlobPatternType::Star => SymbolMatcher::StarGlob(create_pattern(token)?),
                GlobPatternType::NonStar => SymbolMatcher::NonstarGlob(create_pattern(token)?),
            }
        },
    ))
}

/// Consumes `exact` from `input` or returns false if that's not what is next.
fn try_take(input: &mut &BStr, mut exact: &[u8]) -> bool {
    let result: Result<_, ContextError> = exact.parse_next(input);
    result.is_ok()
}

fn parse_token<'input>(input: &mut &'input BStr) -> winnow::Result<&'input [u8]> {
    take_while(1.., |b| !b" (){}\n\t".contains(&b)).parse_next(input)
}

#[derive(Debug)]
enum VersionScriptError {
    UnknownParentVersion,
    InvalidUtf8String,
    InvalidGlobPattern,
    UnexpectedExtern(String),
    UnsupportedExtern(String),
}

impl std::error::Error for VersionScriptError {}

impl std::fmt::Display for VersionScriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VersionScriptError::InvalidGlobPattern => write!(f, "Invalid glob pattern"),
            VersionScriptError::InvalidUtf8String => write!(f, "Invalid utf-8 string"),
            VersionScriptError::UnknownParentVersion => write!(f, "Unknown parent version"),
            VersionScriptError::UnexpectedExtern(s) => {
                write!(f, "Unexpected extern \"{s}\" in parsing")
            }
            VersionScriptError::UnsupportedExtern(s) => write!(f, "Unsupported extern \"{s}\""),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashbrown::HashSet;
    use itertools::Itertools;
    use itertools::assert_equal;
    use wild_util::symbol_name::UnversionedSymbolName;

    fn is_matching_global<'data>(script: &RegularVersionScript<'data>, name: &str) -> bool {
        let Some(m) = script.find_match(&UnversionedSymbolName::prehashed(name.as_bytes())) else {
            return true;
        };
        matches!(m.1, VersionRuleSection::Global)
    }

    #[test]
    fn parse_rust_version_script() {
        let data = ScriptData {
            raw: br"
                    {
                    global:
                        foo;
                        bar;
                    local:
                        *;
                    };",
        };
        let script = VersionScript::parse(data).unwrap();
        let VersionScript::Rust(rust_script) = script else {
            panic!("Expected Rust-style version script");
        };
        assert_equal(
            rust_script
                .global
                .iter()
                .map(|sym| String::from_utf8_lossy(sym)),
            ["foo", "bar"],
        );
    }

    #[test]
    fn test_parse_simple_version_script() {
        let data = ScriptData {
            raw: br"
                    # Comment starting with a hash
                    {global:
                        /* Single-line comment */
                        foo; /* Trailing comment */
                        bar*;
                        best_*_fn*;
                        *_wrapper  ;
                    local:
                        /* Multi-line
                           comment */
                        *;
                    };",
        };
        let script = RegularVersionScript::parse(data).unwrap();
        let version_body = &script.versions[0].version_body;
        assert_equal(
            version_body
                .globals
                .general
                .exact
                .iter()
                .map(|s| std::str::from_utf8(s.bytes()).unwrap()),
            ["foo"],
        );
        assert_equal(
            version_body
                .globals
                .general
                .star_globs
                .iter()
                .map(|glob| glob.as_str()),
            ["bar*", "best_*_fn*", "*_wrapper"],
        );

        assert!(is_matching_global(&script, "main_wrapper"));
        assert!(is_matching_global(&script, "bar_bar_bar"));
        assert!(is_matching_global(&script, "best_foo_fn_barus"));
        assert!(!is_matching_global(&script, "best_fn"));
    }

    #[test]
    fn test_parse_version_script() {
        let data = ScriptData {
            raw: br"
                VERS_1.1 {
                    global:
                        foo1;
                    local:
                        old*;
                };

                VERS_1.2 {
                    foo2;
                } VERS_1.1;
            ",
        };
        let script = RegularVersionScript::parse(data).unwrap();
        assert_eq!(script.versions.len(), 3);

        let version = &script.versions[1];
        assert_eq!(version.name, b"VERS_1.1");
        assert_eq!(version.parent_index, None);
        assert_equal(
            version
                .version_body
                .globals
                .general
                .exact
                .iter()
                .map(|s| std::str::from_utf8(s.bytes()).unwrap()),
            ["foo1"],
        );
        assert_equal(
            version
                .version_body
                .locals
                .general
                .star_globs
                .iter()
                .map(|glob| glob.as_str()),
            ["old*"],
        );

        let version = &script.versions[2];
        assert_eq!(version.name, b"VERS_1.2");
        assert_eq!(version.parent_index, Some(1));
        assert_equal(
            version
                .version_body
                .globals
                .general
                .exact
                .iter()
                .map(|s| std::str::from_utf8(s.bytes()).unwrap()),
            ["foo2"],
        );
    }

    #[test]
    fn single_line_version_script() {
        let data = ScriptData {
            raw: br"VERSION42 { global: *; };",
        };
        RegularVersionScript::parse(data).unwrap();
    }

    #[test]
    fn extern_cxx_version_script() {
        let data = ScriptData {
            raw: br#"
                "VERSION42 {
                    local:
                        foo;
                        bar;
                        extern "C++" {
                            ns::*;
                            "f(int**,double)";
                            "std::vector<Loc<1>, std::allocator<Loc<1> > >::_M_realloc_append<Loc<1> const&>(Loc<1> const&)::_Guard_elts::_Guard_elts(Loc<1>*, std::allocator<Loc<1> >&)";
                            "WebKit::WebProcessMain(int, char**)";
                        };
                };"#,
        };
        let script = RegularVersionScript::parse(data).unwrap();
        let version_body = &script.versions[1].version_body;

        assert_equal(
            version_body
                .locals
                .cxx
                .exact
                .iter()
                .map(|s| std::str::from_utf8(s.bytes()).unwrap())
                .sorted(),
            [
                "WebKit::WebProcessMain(int, char**)",
                "f(int**,double)",
                "std::vector<Loc<1>, std::allocator<Loc<1> > >::_M_realloc_append<Loc<1> const&>(Loc<1> const&)::_Guard_elts::_Guard_elts(Loc<1>*, std::allocator<Loc<1> >&)",
            ],
        );
        assert_equal(
            version_body
                .locals
                .cxx
                .star_globs
                .iter()
                .map(|glob| glob.as_str()),
            ["ns::*"],
        );

        assert!(!is_matching_global(&script, "foo"));
        // Test "ns::" c++ namespace glob pattern.
        assert!(!is_matching_global(
            &script,
            "_ZN2ns8generateB5cxx11ENSt7__cxx1112basic_stringIcSt11char_traitsIcESaIcEEEb"
        ));
        // Test exact matches after C++ demangling.
        assert!(!is_matching_global(
            &script,
            "_ZZNSt6vectorI3LocILi1EESaIS1_EE17_M_realloc_appendIJRKS1_EEEvDpOT_EN11_Guard_eltsC2EPS1_RS2_"
        ));
        assert!(!is_matching_global(
            &script,
            "_ZN6WebKit14WebProcessMainEiPPc"
        ));
        assert!(is_matching_global(
            &script,
            "_ZTVN10__cxxabiv120__si_class_type_infoE"
        ));
    }

    #[test]
    fn extern_c_version_script() {
        let data = ScriptData {
            raw: br#"
                "VERSION42 {
                    local:
                        foo;
                        bar;
                        extern "C" {
                            baz;
                        };
                };"#,
        };
        let script = RegularVersionScript::parse(data).unwrap();
        let version_body = &script.versions[1].version_body;

        assert_equal(
            version_body
                .locals
                .general
                .exact
                .iter()
                .map(|s| std::str::from_utf8(s.bytes()).unwrap())
                .sorted(),
            ["bar", "baz", "foo"],
        );
    }

    #[test]
    fn extern_without_semicolon_version_script() {
        let data = ScriptData {
            raw: br#"
                {
                    extern "C" {
                        foo
                    };
                };"#,
        };
        let script = RegularVersionScript::parse(data).unwrap();
        let version_body = &script.versions[0].version_body;

        assert_equal(
            version_body
                .globals
                .general
                .exact
                .iter()
                .map(|s| std::str::from_utf8(s.bytes()).unwrap()),
            ["foo"],
        );

        let data = ScriptData {
            raw: br#"
                {
                    extern "C++" {
                        bar;
                        baz
                    };
                };"#,
        };
        let script = RegularVersionScript::parse(data).unwrap();
        let version_body = &script.versions[0].version_body;

        assert_equal(
            version_body
                .globals
                .cxx
                .exact
                .iter()
                .map(|s| std::str::from_utf8(s.bytes()).unwrap())
                .sorted(),
            ["bar", "baz"],
        );
    }

    #[test]
    fn invalid_version_scripts() {
        #[track_caller]
        fn assert_invalid(src: &str) {
            let data = ScriptData {
                raw: src.as_bytes(),
            };
            assert!(VersionScript::parse(data).is_err());
        }

        // Missing ';'
        assert_invalid("{}");
        assert_invalid("{*};");
        assert_invalid("{foo};");

        // Missing '}'
        assert_invalid("{foo;");
        assert_invalid("VER1 {foo;}; VER2 {bar;} VER1");

        // Missing parent version
        assert_invalid("VER2 {bar;} VER1;");
    }

    #[test]
    fn test_version_order() {
        let data = ScriptData {
            raw: br"
                VERS_1.1 {
                    foo;
                    foo?;
                    f*;
                    bar*;
                };

                VERS_1.2 {
                    foo*;
                    bar;
                } VERS_1.1;
            ",
        };
        let script = RegularVersionScript::parse(data).unwrap();
        let sym = UnversionedSymbolName::prehashed;

        // Exact match wins
        assert_eq!(script.find_match(&sym(b"foo")).unwrap().0, 1);
        assert_eq!(script.find_match(&sym(b"bar")).unwrap().0, 2);

        // Non-star match
        assert_eq!(script.find_match(&sym(b"foox")).unwrap().0, 1);

        // Star match
        assert_eq!(script.find_match(&sym(b"foo_bar")).unwrap().0, 2);
    }

    #[test]
    fn test_escape_sequences() {
        let data = ScriptData {
            raw: br"
                {
                    global:
                        foo\*bar;
                        baz\?;
                        foo1\\foo2;
                        foo3?foo4*;
                        b*;
                        f?;
                };
            ",
        };
        let script = RegularVersionScript::parse(data).unwrap();
        let version_body = &script.versions[0].version_body;

        let escaped_patterns: HashSet<&[u8]> = version_body
            .globals
            .general
            .escaped_exact
            .iter()
            .map(|v| v.as_slice())
            .collect();

        assert!(escaped_patterns.contains(&b"foo*bar"[..]));
        assert!(escaped_patterns.contains(&b"baz?"[..]));
        assert!(escaped_patterns.contains(&b"foo1\\foo2"[..]));

        let star_patterns: Vec<&str> = version_body
            .globals
            .general
            .star_globs
            .iter()
            .map(|glob| glob.as_str())
            .collect();

        assert!(star_patterns.contains(&"b*"));
        assert!(star_patterns.contains(&"foo3?foo4*"));

        let nonstar_patterns: Vec<&str> = version_body
            .globals
            .general
            .nonstar_globs
            .iter()
            .map(|glob| glob.as_str())
            .collect();

        assert!(nonstar_patterns.contains(&"f?"));
    }

    #[test]
    fn test_negation() {
        let data = ScriptData {
            raw: br"
                VERS_1.1 {
                    f*;
                };

                VERS_1.2 {
                    f[^o]*;
                } VERS_1.1;
            ",
        };
        let script = RegularVersionScript::parse(data).unwrap();
        let sym = UnversionedSymbolName::prehashed;

        assert_eq!(script.find_match(&sym(b"foo")).unwrap().0, 1);
        assert_eq!(script.find_match(&sym(b"fxxx")).unwrap().0, 2);
    }
}
