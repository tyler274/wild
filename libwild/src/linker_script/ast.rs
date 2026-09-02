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
///   DEFINED
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
    pub(crate) fn needs_name_sort(self) -> bool {
        matches!(self, SortKind::Name | SortKind::InitPriority)
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
            | Expression::Assert(AssertCommand { expression: e, .. }) => e.visit_expressions(cb),
            Expression::SegmentStart(_, default_expr) => default_expr.visit_expressions(cb),
            Expression::Ternary(expression, expression1, expression2) => {
                expression.visit_expressions(cb);
                expression1.visit_expressions(cb);
                expression2.visit_expressions(cb);
            }
        }
    }
}
