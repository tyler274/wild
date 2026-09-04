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
    #[debug("{}", String::from_utf8_lossy(_0))]
    OutputArch(&'a [u8]),
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

/// GNU `ONLY_IF_RO` / `ONLY_IF_RW` on an output section. When both copies of
/// the same name appear (GNU default `.eh_frame`), Wild keeps one output
/// section and selects the RO or RW placement from whether any matching input
/// has `SHF_WRITE`.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum OnlyIf {
    Ro,
    Rw,
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
    pub(crate) only_if: Option<OnlyIf>,
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
    /// GNU `LINKER_VERSION` in an output section. On ELF this is a nop unless
    /// `--enable-linker-version`; Wild always writes identity into `.comment`.
    LinkerVersion,
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
///   DEFINED, ABSOLUTE, CONSTANT, DATA_SEGMENT_ALIGN, DATA_SEGMENT_RELRO_END, DATA_SEGMENT_END
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
    /// `CONSTANT(MAXPAGESIZE)` — `-z max-page-size` / architecture default.
    ConstantMaxPageSize,
    /// `CONSTANT(COMMONPAGESIZE)` — `-z common-page-size`, capped at max page size.
    ConstantCommonPageSize,
    /// `DATA_SEGMENT_ALIGN(maxpagesize, commonpagesize)` — GNU: next max-page with the same
    /// in-page offset, so the data segment does not share a page with the text segment.
    DataSegmentAlign(Box<Expression<'a>>, Box<Expression<'a>>),
    /// `DATA_SEGMENT_RELRO_END(offset, exp)` — pad so `exp + offset` is page-aligned when RELRO
    /// is on. Returns the new location counter (typically assigned to `.`).
    DataSegmentRelroEnd(Box<Expression<'a>>, Box<Expression<'a>>),
    /// `DATA_SEGMENT_END(exp)` — marks the end of the data segment; returns `exp`.
    DataSegmentEnd(Box<Expression<'a>>),
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
            | Expression::Defined(_)
            | Expression::ConstantMaxPageSize
            | Expression::ConstantCommonPageSize => {}
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
            Expression::DataSegmentAlign(l, r) | Expression::DataSegmentRelroEnd(l, r) => {
                l.visit_expressions(cb);
                r.visit_expressions(cb);
            }
            Expression::DataSegmentEnd(e) => e.visit_expressions(cb),
            Expression::Ternary(expression, expression1, expression2) => {
                expression.visit_expressions(cb);
                expression1.visit_expressions(cb);
                expression2.visit_expressions(cb);
            }
        }
    }

    pub(crate) fn contains_next_section(&self) -> bool {
        let mut found = false;
        self.visit_expressions(&mut |expr| {
            if matches!(
                expr,
                Expression::Alignof(b"NEXT_SECTION") | Expression::Sizeof(b"NEXT_SECTION")
            ) {
                found = true;
                false
            } else {
                true
            }
        });
        found
    }

    /// GNU `ALIGNOF(NEXT_SECTION)` / `SIZEOF(NEXT_SECTION)`: the next allocated output
    /// section in the script, or 0 if there is none.
    pub(crate) fn rewrite_next_section(&self, align: u64, size: u64) -> Self {
        match self {
            Expression::Alignof(b"NEXT_SECTION") => Expression::Number(align),
            Expression::Sizeof(b"NEXT_SECTION") => Expression::Number(size),
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
            | Expression::Defined(_)
            | Expression::ConstantMaxPageSize
            | Expression::ConstantCommonPageSize => self.clone(),
            Expression::Add(l, r) => Expression::Add(
                Box::new(l.rewrite_next_section(align, size)),
                Box::new(r.rewrite_next_section(align, size)),
            ),
            Expression::Subtract(l, r) => Expression::Subtract(
                Box::new(l.rewrite_next_section(align, size)),
                Box::new(r.rewrite_next_section(align, size)),
            ),
            Expression::Multiply(l, r) => Expression::Multiply(
                Box::new(l.rewrite_next_section(align, size)),
                Box::new(r.rewrite_next_section(align, size)),
            ),
            Expression::Divide(l, r) => Expression::Divide(
                Box::new(l.rewrite_next_section(align, size)),
                Box::new(r.rewrite_next_section(align, size)),
            ),
            Expression::Modulo(l, r) => Expression::Modulo(
                Box::new(l.rewrite_next_section(align, size)),
                Box::new(r.rewrite_next_section(align, size)),
            ),
            Expression::LessThan(l, r) => Expression::LessThan(
                Box::new(l.rewrite_next_section(align, size)),
                Box::new(r.rewrite_next_section(align, size)),
            ),
            Expression::GreaterThan(l, r) => Expression::GreaterThan(
                Box::new(l.rewrite_next_section(align, size)),
                Box::new(r.rewrite_next_section(align, size)),
            ),
            Expression::LessEqual(l, r) => Expression::LessEqual(
                Box::new(l.rewrite_next_section(align, size)),
                Box::new(r.rewrite_next_section(align, size)),
            ),
            Expression::GreaterEqual(l, r) => Expression::GreaterEqual(
                Box::new(l.rewrite_next_section(align, size)),
                Box::new(r.rewrite_next_section(align, size)),
            ),
            Expression::Equal(l, r) => Expression::Equal(
                Box::new(l.rewrite_next_section(align, size)),
                Box::new(r.rewrite_next_section(align, size)),
            ),
            Expression::NotEqual(l, r) => Expression::NotEqual(
                Box::new(l.rewrite_next_section(align, size)),
                Box::new(r.rewrite_next_section(align, size)),
            ),
            Expression::Min(l, r) => Expression::Min(
                Box::new(l.rewrite_next_section(align, size)),
                Box::new(r.rewrite_next_section(align, size)),
            ),
            Expression::Max(l, r) => Expression::Max(
                Box::new(l.rewrite_next_section(align, size)),
                Box::new(r.rewrite_next_section(align, size)),
            ),
            Expression::BitwiseAnd(l, r) => Expression::BitwiseAnd(
                Box::new(l.rewrite_next_section(align, size)),
                Box::new(r.rewrite_next_section(align, size)),
            ),
            Expression::BitwiseOr(l, r) => Expression::BitwiseOr(
                Box::new(l.rewrite_next_section(align, size)),
                Box::new(r.rewrite_next_section(align, size)),
            ),
            Expression::BitwiseXor(l, r) => Expression::BitwiseXor(
                Box::new(l.rewrite_next_section(align, size)),
                Box::new(r.rewrite_next_section(align, size)),
            ),
            Expression::LeftShift(l, r) => Expression::LeftShift(
                Box::new(l.rewrite_next_section(align, size)),
                Box::new(r.rewrite_next_section(align, size)),
            ),
            Expression::RightShift(l, r) => Expression::RightShift(
                Box::new(l.rewrite_next_section(align, size)),
                Box::new(r.rewrite_next_section(align, size)),
            ),
            Expression::LogicalAnd(l, r) => Expression::LogicalAnd(
                Box::new(l.rewrite_next_section(align, size)),
                Box::new(r.rewrite_next_section(align, size)),
            ),
            Expression::LogicalOr(l, r) => Expression::LogicalOr(
                Box::new(l.rewrite_next_section(align, size)),
                Box::new(r.rewrite_next_section(align, size)),
            ),
            Expression::Align(l, Some(r)) => Expression::Align(
                Box::new(l.rewrite_next_section(align, size)),
                Some(Box::new(r.rewrite_next_section(align, size))),
            ),
            Expression::Align(e, None) => {
                Expression::Align(Box::new(e.rewrite_next_section(align, size)), None)
            }
            Expression::LogicalNot(e) => {
                Expression::LogicalNot(Box::new(e.rewrite_next_section(align, size)))
            }
            Expression::BitwiseNot(e) => {
                Expression::BitwiseNot(Box::new(e.rewrite_next_section(align, size)))
            }
            Expression::Negate(e) => {
                Expression::Negate(Box::new(e.rewrite_next_section(align, size)))
            }
            Expression::Absolute(e) => {
                Expression::Absolute(Box::new(e.rewrite_next_section(align, size)))
            }
            Expression::Assert(assert_command) => Expression::Assert(AssertCommand {
                expression: Box::new(assert_command.expression.rewrite_next_section(align, size)),
                message: assert_command.message,
                remainder: assert_command.remainder,
            }),
            Expression::SegmentStart(name, default_expr) => Expression::SegmentStart(
                *name,
                Box::new(default_expr.rewrite_next_section(align, size)),
            ),
            Expression::DataSegmentAlign(l, r) => Expression::DataSegmentAlign(
                Box::new(l.rewrite_next_section(align, size)),
                Box::new(r.rewrite_next_section(align, size)),
            ),
            Expression::DataSegmentRelroEnd(l, r) => Expression::DataSegmentRelroEnd(
                Box::new(l.rewrite_next_section(align, size)),
                Box::new(r.rewrite_next_section(align, size)),
            ),
            Expression::DataSegmentEnd(e) => {
                Expression::DataSegmentEnd(Box::new(e.rewrite_next_section(align, size)))
            }
            Expression::Ternary(c, t, f) => Expression::Ternary(
                Box::new(c.rewrite_next_section(align, size)),
                Box::new(t.rewrite_next_section(align, size)),
                Box::new(f.rewrite_next_section(align, size)),
            ),
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
            | Expression::SegmentStart(..)
            | Expression::ConstantMaxPageSize
            | Expression::ConstantCommonPageSize => None,
            Expression::Add(l, r) => {
                add_dot_residual(l.relocatable_anchor(), r.relocatable_anchor())
            }
            Expression::Subtract(l, r) => {
                sub_dot_residual(l.relocatable_anchor(), r.relocatable_anchor())
            }
            Expression::Align(_, Some(value)) => value.relocatable_anchor(),
            Expression::Align(_, None) => Some(RelocatableAnchor::LocationCounter),
            Expression::DataSegmentAlign(_, _) => Some(RelocatableAnchor::LocationCounter),
            Expression::DataSegmentRelroEnd(_, exp) | Expression::DataSegmentEnd(exp) => {
                exp.relocatable_anchor()
            }
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
