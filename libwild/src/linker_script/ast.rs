use crate::alignment::Alignment;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct LinkerScript<'a> {
    pub(crate) commands: Vec<Command<'a>>,
}

#[derive(derive_more::Debug, PartialEq, Eq)]
pub(crate) enum Command<'a> {
    #[debug("{}", String::from_utf8_lossy(_0))]
    Arg(&'a [u8]),
    Group(Vec<Command<'a>>),
    AsNeeded(Vec<Command<'a>>),
    Ignored,
    Sections(Sections<'a>),
    #[debug("{}", String::from_utf8_lossy(_0))]
    Entry(&'a [u8]),
    #[debug("{}", String::from_utf8_lossy(_0))]
    Version(&'a [u8]),
    SymbolDefinition {
        name: &'a [u8],
        value: Expression<'a>,
    },
    SetLocation(Location<'a>),
    Provide(ProvideSymbolDefinition<'a>),
    Assert(AssertCommand<'a>),
    Memory(Vec<MemoryRegion<'a>>),
    Phdrs(Vec<Phdr<'a>>),
    OutputFormat(OutputFormat<'a>),
    Include(&'a [u8]),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Sections<'a> {
    pub(crate) commands: Vec<SectionCommand<'a>>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SectionCommand<'a> {
    Section(Section<'a>),
    SetLocation(Location<'a>),
    Assert(AssertCommand<'a>),
    Provide(ProvideSymbolDefinition<'a>),
    SymbolAssignment(SymbolAssignment<'a>),
    Overlay(Overlay<'a>),
    Include(&'a [u8]),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Overlay<'a> {
    pub(crate) start_address: Option<Expression<'a>>,
    pub(crate) at_address: Option<Expression<'a>>,
    pub(crate) nocrossrefs: bool,
    pub(crate) sections: Vec<Section<'a>>,
    pub(crate) region: Option<&'a [u8]>,
    pub(crate) at_region: Option<&'a [u8]>,
    pub(crate) phdrs: Vec<&'a [u8]>,
    pub(crate) fill: Option<Fill<'a>>,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub(crate) struct Location<'a> {
    pub(crate) address: Expression<'a>,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub(crate) struct Fill<'a> {
    pub(crate) value: Expression<'a>,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub(crate) enum SectionAttributes {
    Noload,
    Readonly,
    Dsect,
    Copy,
    Info,
    Overlay,
    /// `(TYPE = SHT_NOTE)` / `(TYPE = 7)` — ELF `sh_type` used when the section
    /// has no input-driven type (GNU ld: BYTE/LONG/etc.).
    Type(u32),
    /// `(READONLY (TYPE = ...))` — READONLY plus `TYPE=`.
    ReadonlyType(u32),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Section<'a> {
    pub(crate) output_section_name: &'a [u8],
    pub(crate) commands: Vec<ContentsCommand<'a>>,
    pub(crate) alignment: Option<Alignment>,
    pub(crate) start_address_expression: Option<Expression<'a>>,
    pub(crate) phdrs: Vec<&'a [u8]>,
    pub(crate) at_address: Option<Expression<'a>>,
    pub(crate) region: Option<&'a [u8]>,
    pub(crate) at_region: Option<&'a [u8]>,
    pub(crate) fill: Option<Fill<'a>>,
    pub(crate) attributes: Option<SectionAttributes>,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Default)]
pub(crate) struct MemoryFlags {
    pub(crate) read: bool,
    pub(crate) write: bool,
    pub(crate) exec: bool,
    pub(crate) alloc: bool,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub(crate) struct MemoryRegion<'a> {
    pub(crate) name: &'a [u8],
    pub(crate) origin: Expression<'a>,
    pub(crate) length: Expression<'a>,
    pub(crate) flags: Option<MemoryFlags>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ContentsCommand<'a> {
    Matcher(Matcher<'a>),
    SymbolAssignment(SymbolAssignment<'a>),
    Provide(ProvideSymbolDefinition<'a>),
    SetLocation(Location<'a>),
    Constructors,
    Assert(AssertCommand<'a>),
    Fill(Fill<'a>),
    OutputData(OutputData<'a>),
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum OutputDataWidth {
    Byte = 1,
    Short = 2,
    Long = 4,
    Quad = 8,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub(crate) struct OutputData<'a> {
    pub(crate) width: OutputDataWidth,
    pub(crate) value: Expression<'a>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SymbolAssignment<'a> {
    pub(crate) name: &'a [u8],
    pub(crate) expr: Expression<'a>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ProvideSymbolDefinition<'a> {
    pub(crate) name: &'a [u8],
    pub(crate) value: Expression<'a>,
    pub(crate) hidden: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct AssertCommand<'a> {
    pub(crate) expression: Box<Expression<'a>>,
    pub(crate) message: &'a [u8],
    /// Remaining input at the point this ASSERT was parsed. Used to lazily compute
    /// the line number only when an error occurs.
    pub(crate) remainder: &'a [u8],
}

impl<'a> PartialEq for AssertCommand<'a> {
    fn eq(&self, other: &Self) -> bool {
        self.expression == other.expression && self.message == other.message
    }
}

impl<'a> Eq for AssertCommand<'a> {}

#[derive(Debug, PartialEq, Eq, Clone)]
pub(crate) struct Phdr<'a> {
    pub(crate) name: &'a [u8],
    pub(crate) ptype: Expression<'a>,
    pub(crate) flags: Option<Expression<'a>>,
    pub(crate) has_filehdr: bool,
    pub(crate) has_phdrs: bool,
    pub(crate) at_address: Option<Expression<'a>>,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub(crate) struct OutputFormat<'a> {
    pub(crate) default: &'a [u8],
    pub(crate) big: Option<&'a [u8]>,
    pub(crate) little: Option<&'a [u8]>,
}

/// Represents a parsed expression in linker scripts (e.g., in ASSERT commands).
///
/// Currently supports:
/// - Arithmetic: +, -, *, /, %
/// - Comparison: <, >, <=, >=, ==, !=
/// - Bitwise: &, |, ^, ~, <<, >>
/// - Logical: &&, ||
/// - Unary: -, !, ~
/// - Functions: SIZEOF, ALIGNOF, LENGTH, ORIGIN, ADDR, LOADADDR, ALIGN, MIN, MAX, SEGMENT_START,
///   DEFINED, ABSOLUTE
/// - Numbers (hex/decimal), symbols, location counter (.)
/// - Parentheses for grouping
/// - Ternary operator (? :)
#[derive(Debug, PartialEq, Eq, Clone)]
pub(crate) enum Expression<'a> {
    /// A numeric literal (e.g., 0x1000, 42)
    Number(u64),
    /// A symbol reference (e.g., __bss_start)
    Symbol(&'a [u8]),
    /// The location counter '.'
    LocationCounter,
    /// Binary arithmetic: +, -, *, /, %
    Add(Box<Expression<'a>>, Box<Expression<'a>>),
    Subtract(Box<Expression<'a>>, Box<Expression<'a>>),
    Multiply(Box<Expression<'a>>, Box<Expression<'a>>),
    Divide(Box<Expression<'a>>, Box<Expression<'a>>),
    Modulo(Box<Expression<'a>>, Box<Expression<'a>>),
    /// Comparison operators: <, >, <=, >=, ==, !=
    LessThan(Box<Expression<'a>>, Box<Expression<'a>>),
    GreaterThan(Box<Expression<'a>>, Box<Expression<'a>>),
    LessEqual(Box<Expression<'a>>, Box<Expression<'a>>),
    GreaterEqual(Box<Expression<'a>>, Box<Expression<'a>>),
    Equal(Box<Expression<'a>>, Box<Expression<'a>>),
    NotEqual(Box<Expression<'a>>, Box<Expression<'a>>),
    /// Function calls
    Sizeof(&'a [u8]),
    Alignof(&'a [u8]),
    Origin(&'a [u8]),
    Length(&'a [u8]),
    Addr(&'a [u8]),
    Loadaddr(&'a [u8]),
    Align(Box<Expression<'a>>, Option<Box<Expression<'a>>>),
    /// MIN and MAX functions (take two expressions)
    Min(Box<Expression<'a>>, Box<Expression<'a>>),
    Max(Box<Expression<'a>>, Box<Expression<'a>>),
    /// SEGMENT_START("segment-name", default) - returns the `-T` command-line override for the
    /// named segment if one was provided, otherwise returns `default`.
    /// Unknown segment names always return `default` (matching GNU ld behavior).
    SegmentStart(crate::parsing::SegmentName, Box<Expression<'a>>),
    /// Bitwise AND, OR and XOR
    BitwiseAnd(Box<Expression<'a>>, Box<Expression<'a>>),
    BitwiseOr(Box<Expression<'a>>, Box<Expression<'a>>),
    BitwiseXor(Box<Expression<'a>>, Box<Expression<'a>>),
    /// Shift Operators
    LeftShift(Box<Expression<'a>>, Box<Expression<'a>>),
    RightShift(Box<Expression<'a>>, Box<Expression<'a>>),
    /// Logical Operators
    LogicalAnd(Box<Expression<'a>>, Box<Expression<'a>>),
    LogicalOr(Box<Expression<'a>>, Box<Expression<'a>>),
    /// Unary Operators
    LogicalNot(Box<Expression<'a>>),
    BitwiseNot(Box<Expression<'a>>),
    Negate(Box<Expression<'a>>),
    SizeofHeaders,
    Ternary(
        Box<Expression<'a>>,
        Box<Expression<'a>>,
        Box<Expression<'a>>,
    ),
    Defined(&'a [u8]),
    Assert(AssertCommand<'a>),
    /// `ABSOLUTE(expr)` — evaluate `expr` as a VMA and force `SHN_ABS`.
    Absolute(Box<Expression<'a>>),
}

/// The relocatable term that should determine `st_shndx` for a symbol assignment.
///
/// GNU ld puts an assignment in a section when the expression has exactly one relocatable
/// residual (a symbol or `.`). `ABSOLUTE()`, a difference of two section symbols, and pure
/// constants have no residual and become `SHN_ABS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RelocatableAnchor<'a> {
    LocationCounter,
    Symbol(&'a [u8]),
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Default)]
pub(crate) enum SortKind {
    #[default]
    None,
    Name,
    Alignment,
    InitPriority,
}

impl SortKind {
    pub(crate) fn needs_sort(self) -> bool {
        !matches!(self, SortKind::None)
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub(crate) struct SectionPattern<'a> {
    pub(crate) name: &'a [u8],
    pub(crate) sort: SortKind,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Matcher<'a> {
    pub(crate) must_keep: bool,
    /// Optional glob pattern for matching input filenames. `None` means match all files (i.e. the
    /// `*` wildcard was used, or no filename was specified).
    pub(crate) input_file_pattern: Option<&'a [u8]>,
    /// Glob patterns of files to skip even when `input_file_pattern` matches.
    pub(crate) exclude_file_patterns: Vec<&'a [u8]>,
    pub(crate) input_section_name_patterns: Vec<SectionPattern<'a>>,
}

impl<'a> Expression<'a> {
    pub(crate) fn visit_expressions(&self, cb: &mut impl FnMut(&Self) -> bool) {
        if !cb(self) {
            return;
        }
        match self {
            Expression::Number(_)
            | Expression::LocationCounter
            | Expression::Sizeof(_)
            | Expression::Alignof(_)
            | Expression::Origin(_)
            | Expression::Length(_)
            | Expression::Addr(_)
            | Expression::Loadaddr(_)
            | Expression::Symbol(_)
            | Expression::SizeofHeaders
            | Expression::Defined(_) => {}
            Expression::Add(l, r)
            | Expression::Subtract(l, r)
            | Expression::Multiply(l, r)
            | Expression::Divide(l, r)
            | Expression::Modulo(l, r)
            | Expression::LessThan(l, r)
            | Expression::GreaterThan(l, r)
            | Expression::LessEqual(l, r)
            | Expression::GreaterEqual(l, r)
            | Expression::Equal(l, r)
            | Expression::NotEqual(l, r)
            | Expression::Min(l, r)
            | Expression::Max(l, r)
            | Expression::BitwiseAnd(l, r)
            | Expression::BitwiseOr(l, r)
            | Expression::BitwiseXor(l, r)
            | Expression::LeftShift(l, r)
            | Expression::RightShift(l, r)
            | Expression::LogicalAnd(l, r)
            | Expression::LogicalOr(l, r)
            | Expression::Align(l, Some(r)) => {
                l.visit_expressions(cb);
                r.visit_expressions(cb);
            }
            Expression::Align(e, None)
            | Expression::LogicalNot(e)
            | Expression::BitwiseNot(e)
            | Expression::Negate(e)
            | Expression::Absolute(e)
            | Expression::Assert(AssertCommand { expression: e, .. }) => e.visit_expressions(cb),
            Expression::SegmentStart(_, default_expr) => default_expr.visit_expressions(cb),
            Expression::Ternary(expression, expression1, expression2) => {
                expression.visit_expressions(cb);
                expression1.visit_expressions(cb);
                expression2.visit_expressions(cb);
            }
        }
    }

    pub(crate) fn relocatable_anchor(&self) -> Option<RelocatableAnchor<'_>> {
        match self {
            Expression::Absolute(_) => None,
            Expression::LocationCounter => Some(RelocatableAnchor::LocationCounter),
            Expression::Symbol(name) => Some(RelocatableAnchor::Symbol(name)),
            Expression::Number(_)
            | Expression::Sizeof(_)
            | Expression::Alignof(_)
            | Expression::Origin(_)
            | Expression::Length(_)
            | Expression::Addr(_)
            | Expression::Loadaddr(_)
            | Expression::SizeofHeaders
            | Expression::Defined(_)
            | Expression::SegmentStart(..) => None,
            Expression::Add(l, r) => {
                add_dot_residual(l.relocatable_anchor(), r.relocatable_anchor())
            }
            Expression::Subtract(l, r) => {
                sub_dot_residual(l.relocatable_anchor(), r.relocatable_anchor())
            }
            Expression::Align(_, Some(value)) => value.relocatable_anchor(),
            Expression::Align(_, None) => Some(RelocatableAnchor::LocationCounter),
            Expression::Min(l, r) | Expression::Max(l, r) => {
                let left = l.relocatable_anchor();
                let right = r.relocatable_anchor();
                if left == right { left } else { None }
            }
            Expression::Ternary(_, if_true, if_false) => {
                let if_true = if_true.relocatable_anchor();
                let if_false = if_false.relocatable_anchor();
                if if_true == if_false { if_true } else { None }
            }
            Expression::LogicalNot(_)
            | Expression::BitwiseNot(_)
            | Expression::Negate(_)
            | Expression::Multiply(_, _)
            | Expression::Divide(_, _)
            | Expression::Modulo(_, _)
            | Expression::LessThan(_, _)
            | Expression::GreaterThan(_, _)
            | Expression::LessEqual(_, _)
            | Expression::GreaterEqual(_, _)
            | Expression::Equal(_, _)
            | Expression::NotEqual(_, _)
            | Expression::BitwiseAnd(_, _)
            | Expression::BitwiseOr(_, _)
            | Expression::BitwiseXor(_, _)
            | Expression::LeftShift(_, _)
            | Expression::RightShift(_, _)
            | Expression::LogicalAnd(_, _)
            | Expression::LogicalOr(_, _) => None,
            Expression::Assert(AssertCommand { expression, .. }) => expression.relocatable_anchor(),
        }
    }
}

/// GNU ld keeps `. ± const` section-relative, but `symbol ± const` is `SHN_ABS`.
fn add_dot_residual<'a>(
    left: Option<RelocatableAnchor<'a>>,
    right: Option<RelocatableAnchor<'a>>,
) -> Option<RelocatableAnchor<'a>> {
    match (left, right) {
        (None, Some(RelocatableAnchor::LocationCounter))
        | (Some(RelocatableAnchor::LocationCounter), None) => {
            Some(RelocatableAnchor::LocationCounter)
        }
        _ => None,
    }
}

fn sub_dot_residual<'a>(
    left: Option<RelocatableAnchor<'a>>,
    right: Option<RelocatableAnchor<'a>>,
) -> Option<RelocatableAnchor<'a>> {
    match (left, right) {
        (Some(RelocatableAnchor::LocationCounter), None) => {
            Some(RelocatableAnchor::LocationCounter)
        }
        _ => None,
    }
}
