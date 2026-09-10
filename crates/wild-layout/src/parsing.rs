use crate::EnginePlatform;
use crate::OutputSections;
use crate::layout_rules::LayoutRulesBuilder;
use crate::layout_rules::LocationCounter;
use crate::output_section_id::LocationCounterIndex;
use crate::output_section_id::OutputSectionId;
use crate::symbol::UnversionedSymbolName;
use crate::symbol_db::SymbolId;
use crate::symbol_db::SymbolIdRange;
use crate::timing_phase;
use crate::verbose_timing_phase;
use wild_args::InputLinkerScript;
use wild_args::InputRef;
use wild_args::Modifiers;
use wild_error::error::Context as _;
use wild_error::error::Result;
use wild_platform::Args;
use wild_platform::FileId;
use wild_platform::ObjectFile;
use wild_platform::OutputKind;
use wild_platform::Platform;
use wild_platform::Symbol;
use wild_scripts::linker_script::Expression;

pub fn process_linker_scripts<'data, P: EnginePlatform>(
    linker_scripts_in: &[InputLinkerScript<'data>],
    output_sections: &mut OutputSections<'data, P>,
    layout_rules_builder: &mut LayoutRulesBuilder<'data>,
    args: &P::Args,
) -> Result<Vec<ProcessedLinkerScript<'data, P>>> {
    timing_phase!("Process linker scripts");

    linker_scripts_in
        .iter()
        .map(|script| layout_rules_builder.process_linker_script(script, output_sections, args))
        .collect::<Result<Vec<ProcessedLinkerScript<P>>>>()
}

#[derive(Debug)]
pub struct Prelude<'data, P: Platform> {
    pub symbol_definitions: Vec<InternalSymDefInfo<'data, P>>,
}

#[derive(Debug)]
pub struct ParsedInputObject<'data, P: Platform> {
    pub input: InputRef<'data>,
    pub object: P::File<'data>,
    pub modifiers: Modifiers,
}

#[derive(Debug)]
pub struct ProcessedLinkerScript<'data, P: Platform> {
    pub input: InputRef<'data>,
    pub symbol_defs: Vec<InternalSymDefInfo<'data, P>>,
    pub memory_regions: Vec<wild_scripts::linker_script::MemoryRegion<'data>>,
    pub program_headers: Vec<wild_scripts::linker_script::Phdr<'data>>,
    pub location_counters: Vec<LocationCounter<'data>>,
    pub ordered_sections: Vec<OutputSectionId>,
}

#[derive(Debug)]
pub struct SyntheticSymbols {
    pub file_id: FileId,
    pub symbol_id_range: SymbolIdRange,
}

#[derive(Clone, derive_more::Debug)]
pub struct InternalSymDefInfo<'data, P: Platform> {
    pub symbol: P::SymtabEntry,
    pub placement: SymbolPlacement<'data, P>,
    #[debug("{:?}", String::from_utf8_lossy(name))]
    pub name: &'data [u8],
    /// `PROVIDE` / `PROVIDE_HIDDEN`. Unused PROVIDE is ignored, including when the
    /// right-hand side is an undefined symbol (GNU ld).
    pub is_provide: bool,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SymbolPlacement<'data, P: Platform> {
    /// Symbol 0 - the undefined symbol.
    Undefined,

    /// Defines a symbol that points to the start of a section.
    SectionStart(OutputSectionId),

    /// Defines a symbol that points at the non-inclusive end of the section. i.e. 1 byte past the
    /// last byte of the section.
    SectionEnd(OutputSectionId),

    /// Where secondary sections are merged into a primary section, this causes our symbol to point
    /// to the non-inclusive end of the last section merged into the specified primary.
    SectionGroupEnd(OutputSectionId),

    /// An undefined symbol supplied by the user, e.g. via `--undefined=symbol-name`.
    ForceUndefined,

    /// A symbol that redirects to some other symbol.
    Redirect(Redirect<'data>),

    /// Symbol will point to the start of the first loadable segment.
    LoadBaseAddress,

    /// Platform-specific linker symbol.
    PlatformSpecific(P::PlatformSpecificSymbol),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SymbolLoc {
    SectionStartRelative(OutputSectionId),
    SectionEndRelative(OutputSectionId),
    SectionEnd(OutputSectionId),
    FirstSection,
    LocationCounter(LocationCounterIndex, Option<OutputSectionId>),
    None,
}

impl SymbolLoc {
    pub fn section_id(&self) -> Option<OutputSectionId> {
        match self {
            SymbolLoc::SectionStartRelative(id)
            | SymbolLoc::SectionEndRelative(id)
            | SymbolLoc::SectionEnd(id) => Some(*id),
            SymbolLoc::LocationCounter(_, section_id) => *section_id,
            SymbolLoc::FirstSection | SymbolLoc::None => None,
        }
    }

    pub fn relative_section_id(&self) -> Option<OutputSectionId> {
        match self {
            SymbolLoc::SectionStartRelative(id)
            | SymbolLoc::SectionEndRelative(id)
            | SymbolLoc::LocationCounter(_, Some(id)) => Some(*id),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect<'data> {
    pub kind: RedirectKind,
    pub expression: Expression<'data>,
    pub loc: SymbolLoc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectKind {
    DefSym,
    Script,
}

impl<'data, P: Platform> InternalSymDefInfo<'data, P> {
    pub fn new(placement: SymbolPlacement<'data, P>, name: &'data [u8]) -> Self {
        Self {
            placement,
            name,
            symbol: P::default_symtab_entry(),
            is_provide: false,
        }
    }

    pub fn with_provide(self) -> Self {
        Self {
            is_provide: true,
            ..self
        }
    }

    pub fn with_hidden(self, hidden: bool) -> Self {
        Self {
            symbol: self.symbol.with_hidden(hidden),
            ..self
        }
    }

    pub fn hide(&mut self) -> &mut Self {
        self.symbol = self.symbol.with_hidden(true);
        self
    }

    pub fn set_hidden(&mut self, hidden: bool) -> &mut Self {
        self.symbol = self.symbol.with_hidden(hidden);
        self
    }
}

impl<'data, P: Platform> ParsedInputObject<'data, P> {
    pub fn new(
        input: InputRef<'data>,
        data: &'data [u8],
        modifiers: Modifiers,
        is_dynamic: bool,
        args: &P::Args,
    ) -> Result<Box<Self>> {
        verbose_timing_phase!("Parse file");

        let object = P::File::parse(data, is_dynamic, args)
            .with_context(|| format!("Failed to parse object file `{input}`"))?;

        Ok(Box::new(Self {
            input,
            object,
            modifiers,
        }))
    }

    pub fn is_dynamic(&self) -> bool {
        self.object.is_dynamic()
    }

    pub fn num_symbols(&self) -> usize {
        self.object.num_symbols()
    }
}

impl<'data, P: EnginePlatform> Prelude<'data, P> {
    pub fn new(args: &'data P::Args, output_kind: OutputKind) -> Result<Self> {
        verbose_timing_phase!("Construct prelude");

        let mut symbols = InternalSymbolsBuilder::default();

        P::create_linker_defined_symbols(&mut symbols, output_kind, args);

        args.force_undefined_symbol_names().iter().for_each(|name| {
            symbols.add_symbol(InternalSymDefInfo::new(
                SymbolPlacement::ForceUndefined,
                name.as_bytes(),
            ));
        });

        // Add symbols defined via the command line.
        args.defsym()
            .iter()
            .try_for_each(|(name, value)| -> Result<()> {
                let mut value = winnow::BStr::new(value);
                let expr = wild_scripts::linker_script::parse_expression(&mut value)
                    .with_context(|| format!("Failed to parse --defsym {name}={value}"))?;

                let placement = SymbolPlacement::Redirect(Redirect {
                    kind: RedirectKind::DefSym,
                    expression: expr,
                    loc: SymbolLoc::None,
                });
                symbols.add_symbol(InternalSymDefInfo::new(placement, name.as_bytes()));
                Ok(())
            })?;

        Ok(Self {
            symbol_definitions: symbols.symbol_definitions,
        })
    }
}

impl<'data, P: Platform> Prelude<'data, P> {
    pub fn symbol_name(&self, symbol_id: SymbolId) -> UnversionedSymbolName<'data> {
        let def = &self.symbol_definitions[symbol_id.as_usize()];
        UnversionedSymbolName::new(def.name)
    }

    pub fn symbol_def(&self, symbol_id: SymbolId) -> &InternalSymDefInfo<'data, P> {
        &self.symbol_definitions[symbol_id.as_usize()]
    }
}

#[derive(Default)]
pub struct InternalSymbolsBuilder<'data, P: Platform> {
    symbol_definitions: Vec<InternalSymDefInfo<'data, P>>,
}

impl<'data, P: Platform> InternalSymbolsBuilder<'data, P> {
    pub fn add_symbol(
        &mut self,
        def: InternalSymDefInfo<'data, P>,
    ) -> &mut InternalSymDefInfo<'data, P> {
        let index = self.symbol_definitions.len();
        self.symbol_definitions.push(def);
        &mut self.symbol_definitions[index]
    }

    pub fn section_start(
        &mut self,
        section_id: OutputSectionId,
        name: &'static str,
    ) -> &mut InternalSymDefInfo<'data, P> {
        self.add_symbol(InternalSymDefInfo::new(
            SymbolPlacement::SectionStart(section_id),
            name.as_bytes(),
        ))
    }

    pub fn section_end(
        &mut self,
        section_id: OutputSectionId,
        name: &'static str,
    ) -> &mut InternalSymDefInfo<'data, P> {
        self.add_symbol(InternalSymDefInfo::new(
            SymbolPlacement::SectionEnd(section_id),
            name.as_bytes(),
        ))
    }

    pub fn section_group_end(
        &mut self,
        section_id: OutputSectionId,
        name: &'static str,
    ) -> &mut InternalSymDefInfo<'data, P> {
        self.add_symbol(InternalSymDefInfo::new(
            SymbolPlacement::SectionGroupEnd(section_id),
            name.as_bytes(),
        ))
    }

    pub fn platform_specific(
        &mut self,
        name: &'static [u8],
        specific: P::PlatformSpecificSymbol,
    ) -> &mut InternalSymDefInfo<'data, P> {
        self.add_symbol(InternalSymDefInfo::new(
            SymbolPlacement::PlatformSpecific(specific),
            name,
        ))
    }
}

impl<'data, P: Platform> ProcessedLinkerScript<'data, P> {
    pub fn num_symbols(&self) -> usize {
        self.symbol_defs.len()
    }
}

impl<'data, P: Platform> std::fmt::Display for ParsedInputObject<'data, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.input, f)
    }
}

impl<'data, P: Platform> std::fmt::Display for ProcessedLinkerScript<'data, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.input, f)
    }
}

impl Redirect<'_> {
    pub fn missing_target(&self, target_name: &[u8]) -> wild_error::error::Error {
        wild_error::error!(
            "Symbol '{name}' referenced by {kind} does not exist",
            name = String::from_utf8_lossy(target_name),
            kind = self.kind.message_text(),
        )
    }

    pub fn missing_resolution(&self, target_name: &[u8]) -> wild_error::error::Error {
        wild_error::error!(
            "Symbol '{name}' referenced by {kind} has no resolution.",
            name = String::from_utf8_lossy(target_name),
            kind = self.kind.message_text(),
        )
    }
}

impl RedirectKind {
    fn message_text(self) -> &'static str {
        match self {
            RedirectKind::DefSym => "--defsym",
            RedirectKind::Script => "linker script",
        }
    }
}
