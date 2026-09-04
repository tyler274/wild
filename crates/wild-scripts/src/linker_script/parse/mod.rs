mod expr;
mod sections;

use super::ast::*;
use crate::inputs::Input;
use crate::inputs::InputSpec;
use crate::inputs::Modifiers;
#[allow(unused_imports)]
pub use expr::*;
use object::Wrap;
#[allow(unused_imports)]
pub use sections::*;
use std::path::Path;
use wild_error::error;
use wild_error::error::Context as _;
use wild_error::error::Result;
use winnow::BStr;
use winnow::Parser as _;
use winnow::ascii::multispace0;
use winnow::combinator::alt;
use winnow::combinator::eof;
use winnow::combinator::opt;
use winnow::combinator::repeat_till;
use winnow::error::ContextError;
use winnow::error::FromExternalError;
use winnow::token::take_until;
use winnow::token::take_while;

impl<'data> LinkerScript<'data> {
    pub fn parse(bytes: &'data [u8], path: &Path) -> Result<LinkerScript<'data>> {
        let commands = parse_commands.parse(BStr::new(bytes)).map_err(|error| {
            error!(
                "Failed to parse linker script `{}`:\n{error}",
                path.display()
            )
        })?;

        Ok(LinkerScript { commands })
    }

    /// Recursively expand `INCLUDE` commands. `load` maps an include path to the included file's
    /// bytes (which must live as long as `'data`).
    pub fn expand_includes(
        &mut self,
        load: &mut dyn FnMut(&[u8]) -> Result<&'data [u8]>,
    ) -> Result {
        let mut stack = Vec::new();
        self.commands = expand_commands(std::mem::take(&mut self.commands), load, &mut stack)?;
        Ok(())
    }

    pub fn foreach_input(
        &self,
        starting_modifiers: Modifiers,
        mut cb: impl FnMut(Input) -> Result,
    ) -> Result {
        foreach_input(&self.commands, starting_modifiers, &mut cb)?;
        Ok(())
    }

    pub fn get_version_script_content(&self) -> Option<&'data [u8]> {
        self.commands.iter().find_map(|cmd| match cmd {
            Command::Version(content) => Some(*content),
            _ => None,
        })
    }
}

pub fn parse_token<'input>(input: &mut &'input BStr) -> winnow::Result<&'input [u8]> {
    if input.starts_with(b"\"") {
        '"'.parse_next(input)?;
        let content = take_until(0.., "\"").parse_next(input)?;
        '"'.parse_next(input)?;

        Ok(content)
    } else {
        take_while(1.., |b| !b" (){};,\n\t".contains(&b)).parse_next(input)
    }
}

pub fn skip_comments_and_whitespace(input: &mut &BStr) -> winnow::Result<()> {
    loop {
        multispace0(input)?;

        if input.starts_with(b"#") {
            take_until(1.., "\n").parse_next(input)?;
        } else if input.starts_with(b"/*") {
            take_until(1.., "*/")
                .parse_next(input)
                .map_err(|_: ContextError| {
                    ContextError::from_external_error(input, LinkerScriptError::UnclosedComment)
                })?;
            "*/".parse_next(input)?;
        } else {
            return Ok(());
        }
    }
}

pub fn parse_paren_group<'input>(input: &mut &'input BStr) -> winnow::Result<Vec<Command<'input>>> {
    '('.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    let (group_contents, _) = repeat_till(0.., parse_command, ')').parse_next(input)?;
    Ok(group_contents)
}

pub fn parse_command<'input>(input: &mut &'input BStr) -> winnow::Result<Command<'input>> {
    let command_str = parse_token(input)?;

    skip_comments_and_whitespace(input)?;

    let command = match command_str {
        b"GROUP" | b"INPUT" => Command::Group(parse_paren_group(input)?),
        b"OUTPUT_FORMAT" => Command::OutputFormat(parse_output_format(input)?),
        b"OUTPUT_ARCH" => Command::OutputArch(parse_output_arch(input)?),
        b"AS_NEEDED" => Command::AsNeeded(parse_paren_group(input)?),
        b"SECTIONS" => Command::Sections(parse_sections(input)?),
        b"ENTRY" => Command::Entry(parse_entry(input)?),
        b"VERSION" => Command::Version(parse_version(input)?),
        b"PROVIDE" => Command::Provide(parse_provide(input, false)?),
        b"PROVIDE_HIDDEN" => Command::Provide(parse_provide(input, true)?),
        b"ASSERT" => {
            let assert = parse_assert(input)?;
            opt(';').parse_next(input)?;
            skip_comments_and_whitespace(input)?;
            Command::Assert(assert)
        }
        b"MEMORY" => Command::Memory(parse_memory(input)?),
        b"PHDRS" => Command::Phdrs(parse_phdrs(input)?),
        b"INCLUDE" => {
            let path = parse_include_path(input)?;
            Command::Include(path)
        }
        other => {
            if let Some(op) = opt(parse_assignment_op).parse_next(input)? {
                // Symbol definition
                skip_comments_and_whitespace(input)?;
                let value = parse_expression.parse_next(input)?;
                skip_comments_and_whitespace(input)?;
                opt(';').parse_next(input)?;
                let expanded = op.expand(other, value);
                if other == b"." {
                    Command::SetLocation(Location { address: expanded })
                } else {
                    Command::SymbolDefinition {
                        name: other,
                        value: expanded,
                    }
                }
            } else {
                Command::Arg(other)
            }
        }
    };

    skip_comments_and_whitespace(input)?;

    Ok(command)
}

pub fn parse_provide<'input>(
    input: &mut &'input BStr,
    hidden: bool,
) -> winnow::Result<ProvideSymbolDefinition<'input>> {
    '('.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    let name = parse_token(input)?;
    skip_comments_and_whitespace(input)?;
    '='.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    let value = parse_expression.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    ')'.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    opt(';').parse_next(input)?;
    skip_comments_and_whitespace(input)?;

    Ok(ProvideSymbolDefinition {
        name,
        value,
        hidden,
    })
}

pub fn parse_assert<'input>(input: &mut &'input BStr) -> winnow::Result<AssertCommand<'input>> {
    let remainder: &'input [u8] = input;
    '('.parse_next(input)?;
    skip_comments_and_whitespace(input)?;

    // Parse expression using winnow - it will consume as much as it can
    let expression = parse_expression.parse_next(input)?;

    skip_comments_and_whitespace(input)?;
    ','.parse_next(input)?;
    skip_comments_and_whitespace(input)?;

    // Parse message (quoted string)
    let message = parse_token(input)?;

    skip_comments_and_whitespace(input)?;
    ')'.parse_next(input)?;
    skip_comments_and_whitespace(input)?;

    Ok(AssertCommand {
        expression: Box::new(expression),
        message,
        remainder,
    })
}

pub fn parse_include_path<'input>(input: &mut &'input BStr) -> winnow::Result<&'input [u8]> {
    skip_comments_and_whitespace(input)?;
    let has_paren = opt('(').parse_next(input)?.is_some();
    skip_comments_and_whitespace(input)?;
    let path = parse_token(input)?;
    skip_comments_and_whitespace(input)?;
    if has_paren {
        ')'.parse_next(input)?;
        skip_comments_and_whitespace(input)?;
    }
    opt(';').parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    Ok(path)
}

pub fn parse_memory_flags(input: &mut &BStr) -> winnow::Result<MemoryFlags> {
    '('.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    let flags_bytes = take_while(0.., |b: u8| b != b')').parse_next(input)?;
    ')'.parse_next(input)?;
    let mut flags = MemoryFlags::default();
    for &b in flags_bytes {
        match b {
            b'r' | b'R' => flags.read = true,
            b'w' | b'W' => flags.write = true,
            b'x' | b'X' => flags.exec = true,
            b'a' | b'A' => flags.alloc = true,
            _ => {}
        }
    }
    Ok(flags)
}

pub fn parse_memory_region<'input>(
    input: &mut &'input BStr,
) -> winnow::Result<MemoryRegion<'input>> {
    let name = parse_token(input)?;
    skip_comments_and_whitespace(input)?;

    let flags = if input.starts_with(b"(") {
        let flags = parse_memory_flags(input)?;
        skip_comments_and_whitespace(input)?;
        Some(flags)
    } else {
        None
    };

    // Parse the colon separator
    ':'.parse_next(input)?;
    skip_comments_and_whitespace(input)?;

    // Parse the Origin block
    alt(("ORIGIN", "org", "o")).parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    '='.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    let origin = parse_expression.parse_next(input)?;
    skip_comments_and_whitespace(input)?;

    // Parse the comma separator
    ','.parse_next(input)?;
    skip_comments_and_whitespace(input)?;

    // Parse the Length block
    alt(("LENGTH", "len", "l")).parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    '='.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    let length = parse_expression.parse_next(input)?;

    Ok(MemoryRegion {
        name,
        origin,
        length,
        flags,
    })
}

pub fn parse_memory<'input>(input: &mut &'input BStr) -> winnow::Result<Vec<MemoryRegion<'input>>> {
    '{'.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    let (regions, _) = repeat_till(0.., parse_memory_region, '}').parse_next(input)?;
    skip_comments_and_whitespace(input)?;

    Ok(regions)
}

pub fn parse_phdr<'input>(input: &mut &'input BStr) -> winnow::Result<Phdr<'input>> {
    let name = parse_token(input)?;
    skip_comments_and_whitespace(input)?;

    let ptype = if input.starts_with(b"PT_") {
        let ptype_str = parse_token(input)?;

        match ptype_str {
            b"PT_NULL" => Expression::Number(object::elf::PT_NULL.into_inner().into()),
            b"PT_LOAD" => Expression::Number(object::elf::PT_LOAD.into_inner().into()),
            b"PT_DYNAMIC" => Expression::Number(object::elf::PT_DYNAMIC.into_inner().into()),
            b"PT_INTERP" => Expression::Number(object::elf::PT_INTERP.into_inner().into()),
            b"PT_NOTE" => Expression::Number(object::elf::PT_NOTE.into_inner().into()),
            b"PT_SHLIB" => Expression::Number(object::elf::PT_SHLIB.into_inner().into()),
            b"PT_PHDR" => Expression::Number(object::elf::PT_PHDR.into_inner().into()),
            b"PT_TLS" => Expression::Number(object::elf::PT_TLS.into_inner().into()),
            b"PT_GNU_EH_FRAME" => {
                Expression::Number(object::elf::PT_GNU_EH_FRAME.into_inner().into())
            }
            b"PT_GNU_STACK" => Expression::Number(object::elf::PT_GNU_STACK.into_inner().into()),
            b"PT_GNU_RELRO" => Expression::Number(object::elf::PT_GNU_RELRO.into_inner().into()),
            b"PT_GNU_PROPERTY" => {
                Expression::Number(object::elf::PT_GNU_PROPERTY.into_inner().into())
            }
            b"PT_GNU_SFRAME" => Expression::Number(object::elf::PT_GNU_SFRAME.into_inner().into()),
            _ => {
                return Err(ContextError::default());
            }
        }
    } else {
        parse_expression.parse_next(input)?
    };

    skip_comments_and_whitespace(input)?;

    let mut flags = None;
    let mut has_filehdr = false;
    let mut has_phdrs = false;
    let mut at_address = None;
    while let Some(prefix) = opt(alt((b"FLAGS", b"FILEHDR", b"PHDRS", b"AT"))).parse_next(input)? {
        skip_comments_and_whitespace(input)?;
        match prefix {
            b"FLAGS" => {
                '('.parse_next(input)?;
                flags = Some(parse_expression.parse_next(input)?);
                ')'.parse_next(input)?;
            }
            b"FILEHDR" => has_filehdr = true,
            b"PHDRS" => has_phdrs = true,
            b"AT" => {
                at_address = Some(parse_at_address.parse_next(input)?);
            }
            _ => unreachable!(),
        }
        skip_comments_and_whitespace(input)?;
    }

    opt(';').parse_next(input)?;
    skip_comments_and_whitespace(input)?;

    Ok(Phdr {
        name,
        ptype,
        flags,
        has_filehdr,
        has_phdrs,
        at_address,
    })
}

pub fn parse_phdrs<'input>(input: &mut &'input BStr) -> winnow::Result<Vec<Phdr<'input>>> {
    '{'.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    let (phdrs, _) = repeat_till(0.., parse_phdr, '}').parse_next(input)?;
    skip_comments_and_whitespace(input)?;

    Ok(phdrs)
}

pub fn parse_output_format<'input>(
    input: &mut &'input BStr,
) -> winnow::Result<OutputFormat<'input>> {
    '('.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    let default = parse_token(input)?;
    skip_comments_and_whitespace(input)?;
    let mut big = None;
    let mut little = None;
    if opt(",").parse_next(input)?.is_some() {
        skip_comments_and_whitespace(input)?;
        big = Some(parse_token(input)?);
        skip_comments_and_whitespace(input)?;
        ",".parse_next(input)?;
        skip_comments_and_whitespace(input)?;
        little = Some(parse_token(input)?);
        skip_comments_and_whitespace(input)?;
    }
    ')'.parse_next(input)?;

    Ok(OutputFormat {
        default,
        big,
        little,
    })
}

pub fn parse_output_arch<'input>(input: &mut &'input BStr) -> winnow::Result<&'input [u8]> {
    skip_comments_and_whitespace(input)?;
    '('.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    let arch = parse_token(input)?;
    skip_comments_and_whitespace(input)?;
    ')'.parse_next(input)?;
    Ok(arch)
}

pub fn parse_commands<'input>(input: &mut &'input BStr) -> winnow::Result<Vec<Command<'input>>> {
    skip_comments_and_whitespace(input)?;

    Ok(repeat_till(0.., parse_command, eof).parse_next(input)?.0)
}

pub fn expand_commands<'data>(
    commands: Vec<Command<'data>>,
    load: &mut dyn FnMut(&[u8]) -> Result<&'data [u8]>,
    stack: &mut Vec<Vec<u8>>,
) -> Result<Vec<Command<'data>>> {
    let mut out = Vec::with_capacity(commands.len());
    for cmd in commands {
        match cmd {
            Command::Include(path) => {
                let included = load_included_script(path, load, stack)?;
                out.extend(included);
            }
            Command::Sections(mut sections) => {
                sections.commands =
                    expand_section_commands(std::mem::take(&mut sections.commands), load, stack)?;
                out.push(Command::Sections(sections));
            }
            Command::Group(inner) => {
                out.push(Command::Group(expand_commands(inner, load, stack)?));
            }
            Command::AsNeeded(inner) => {
                out.push(Command::AsNeeded(expand_commands(inner, load, stack)?));
            }
            other => out.push(other),
        }
    }
    Ok(out)
}

pub fn expand_section_commands<'data>(
    commands: Vec<SectionCommand<'data>>,
    load: &mut dyn FnMut(&[u8]) -> Result<&'data [u8]>,
    stack: &mut Vec<Vec<u8>>,
) -> Result<Vec<SectionCommand<'data>>> {
    let mut out = Vec::with_capacity(commands.len());
    for cmd in commands {
        match cmd {
            SectionCommand::Include(path) => {
                out.extend(load_included_section_commands(path, load, stack)?);
            }
            other => out.push(other),
        }
    }
    Ok(out)
}

pub fn load_included_script<'data>(
    path: &[u8],
    load: &mut dyn FnMut(&[u8]) -> Result<&'data [u8]>,
    stack: &mut Vec<Vec<u8>>,
) -> Result<Vec<Command<'data>>> {
    push_include_path(path, stack)?;
    let bytes = load(path)?;
    let parsed = parse_commands
        .parse(BStr::new(bytes))
        .map_err(|error| error!("Failed to parse included linker script:\n{error}"))?;
    let expanded = expand_commands(parsed, load, stack)?;
    stack.pop();
    Ok(expanded)
}

pub fn load_included_section_commands<'data>(
    path: &[u8],
    load: &mut dyn FnMut(&[u8]) -> Result<&'data [u8]>,
    stack: &mut Vec<Vec<u8>>,
) -> Result<Vec<SectionCommand<'data>>> {
    push_include_path(path, stack)?;
    let bytes = load(path)?;
    let section_cmds = if let Ok(cmds) = parse_section_command_list.parse(BStr::new(bytes)) {
        cmds
    } else {
        let parsed = parse_commands
            .parse(BStr::new(bytes))
            .map_err(|error| error!("Failed to parse included linker script:\n{error}"))?;
        section_commands_from_top_level(parsed)?
    };
    let expanded = expand_section_commands(section_cmds, load, stack)?;
    stack.pop();
    Ok(expanded)
}

pub fn section_commands_from_top_level<'data>(
    commands: Vec<Command<'data>>,
) -> Result<Vec<SectionCommand<'data>>> {
    let mut out = Vec::new();
    for cmd in commands {
        match cmd {
            Command::Sections(sections) => out.extend(sections.commands),
            Command::SetLocation(loc) => out.push(SectionCommand::SetLocation(loc)),
            Command::Assert(assert_cmd) => out.push(SectionCommand::Assert(assert_cmd)),
            Command::Provide(provide) => out.push(SectionCommand::Provide(provide)),
            Command::SymbolDefinition { name, value } => {
                out.push(SectionCommand::SymbolAssignment(SymbolAssignment {
                    name,
                    expr: value,
                }));
            }
            Command::Include(path) => out.push(SectionCommand::Include(path)),
            Command::Arg(name) => {
                wild_error::bail!(
                    "INCLUDE inside SECTIONS cannot contain `{}`",
                    String::from_utf8_lossy(name)
                );
            }
            _ => {
                wild_error::bail!("INCLUDE inside SECTIONS cannot contain that top-level command");
            }
        }
    }
    Ok(out)
}

pub fn push_include_path(path: &[u8], stack: &mut Vec<Vec<u8>>) -> Result {
    if stack.iter().any(|p| p == path) {
        wild_error::bail!("cyclic INCLUDE of `{}`", String::from_utf8_lossy(path));
    }
    stack.push(path.to_vec());
    Ok(())
}

pub fn parse_entry<'input>(input: &mut &'input BStr) -> winnow::Result<&'input [u8]> {
    skip_comments_and_whitespace(input)?;
    '('.parse_next(input)?;
    skip_comments_and_whitespace(input)?;
    let symbol_name = parse_token(input)?;
    skip_comments_and_whitespace(input)?;
    ')'.parse_next(input)?;
    Ok(symbol_name)
}

pub fn parse_version<'input>(input: &mut &'input BStr) -> winnow::Result<&'input [u8]> {
    skip_comments_and_whitespace(input)?;
    '{'.parse_next(input)?;
    skip_comments_and_whitespace(input)?;

    let mut brace_count = 1;
    let mut pos = 0;

    while brace_count > 0 && pos < input.len() {
        match input[pos] {
            b'{' => brace_count += 1,
            b'}' => brace_count -= 1,
            _ => {}
        }
        pos += 1;
    }

    if brace_count != 0 {
        return Err(ContextError::new());
    }

    let version_content = &input[..pos - 1];
    *input = &input[pos..];

    skip_comments_and_whitespace(input)?;

    opt(';').parse_next(input)?;

    Ok(version_content)
}

/// Call `cb` for each input file requested by `commands`.
pub fn foreach_input(
    commands: &[Command],
    modifiers: Modifiers,
    cb: &mut impl FnMut(Input) -> Result,
) -> Result {
    for command in commands {
        match command {
            Command::Arg(arg) => {
                let spec = if let Some(lib_name) = arg.strip_prefix(b"-l") {
                    InputSpec::Lib(Box::from(to_str(lib_name)?))
                } else {
                    InputSpec::File(Box::from(Path::new(to_str(arg)?)))
                };
                cb(Input {
                    spec,
                    search_first: None,
                    modifiers,
                })?;
            }
            Command::Group(subs) => foreach_input(subs, modifiers, cb)?,
            Command::AsNeeded(subs) => {
                let sub_modifiers = Modifiers {
                    as_needed: true,
                    ..modifiers
                };
                foreach_input(subs, sub_modifiers, cb)?;
            }
            _ => {}
        }
    }

    Ok(())
}

pub fn to_str(bytes: &[u8]) -> Result<&str> {
    std::str::from_utf8(bytes)
        .with_context(|| format!("Expected UTF-8, found `{}`", String::from_utf8_lossy(bytes)))
}

#[derive(Debug)]
pub enum LinkerScriptError {
    InvalidAlignment,
    UnclosedComment,
    UnsupportedNestedSort,
    InvalidSectionType,
}

impl std::error::Error for LinkerScriptError {}

impl std::fmt::Display for LinkerScriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkerScriptError::InvalidAlignment => write!(f, "Invalid alignment"),
            LinkerScriptError::UnclosedComment => write!(f, "Unclosed comment"),
            LinkerScriptError::UnsupportedNestedSort => write!(
                f,
                "Nested sorting commands in linker scripts is not supported"
            ),
            LinkerScriptError::InvalidSectionType => {
                write!(f, "invalid TYPE for output section")
            }
        }
    }
}

#[cfg(test)]
#[path = "../parse_tests.rs"]
mod tests;
