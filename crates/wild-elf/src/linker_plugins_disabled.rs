#![allow(dead_code)]
#![allow(clippy::unused_self)]
#![allow(clippy::unnecessary_wraps)]
#![allow(clippy::needless_pass_by_ref_mut)]

use crate::{Elf, ElfClass};
use rayon::Scope;
use std::marker::PhantomData;
use wild_args::elf::ElfArgs;
use wild_error::error::Result;
use wild_layout::grouping::LtoInput;
use wild_layout::layout_rules::LayoutRulesBuilder;
use wild_layout::output_section_id::OutputSections;
use wild_layout::resolution::Resolver;
use wild_layout::symbol_db::{LoadedInputs, SymbolDb, SymbolId};
use wild_platform::value_flags::PerSymbolFlags;

pub(crate) struct LoadedPlugin {}

pub struct LinkerPlugin<'data> {
    _phantom: PhantomData<&'data u8>,
}

pub(crate) struct LtoInputInfo<'data> {
    _phantom: PhantomData<&'data u8>,
}

impl<'data> LtoInputInfo<'data> {
    pub(crate) fn into_unsequenced(self) -> wild_layout::grouping::UnsequencedLtoInput<'data> {
        unreachable!()
    }
}

pub(crate) struct PluginOutputs {}

impl<'data> LinkerPlugin<'data> {
    pub(crate) fn process_input(
        &'_ mut self,
        _input_ref: wild_args::InputRef<'data>,
        _file: &std::fs::File,
        _kind: wild_platform::FileKind,
    ) -> Result<Option<Box<LtoInputInfo<'data>>>> {
        unreachable!();
    }

    pub(crate) fn from_args<C: ElfClass>(
        _args: &'data ElfArgs,
        _herd: &'data wild_util::arena::Herd,
    ) -> Result<Option<LinkerPlugin<'data>>> {
        Ok(None)
    }

    pub(crate) fn is_initialised(&self) -> bool {
        false
    }

    pub(crate) fn lto_codegen<C: ElfClass>(
        &mut self,
        _symbol_db: &mut SymbolDb<'data, Elf<C>>,
        _resolver: &mut Resolver<'data, Elf<C>>,
        _per_symbol_flags: &mut PerSymbolFlags,
    ) -> Result<Option<Vec<wild_args::Input>>> {
        Ok(None)
    }

    pub(crate) fn integrate_lto_objects<C: ElfClass>(
        &mut self,
        _symbol_db: &mut SymbolDb<'data, Elf<C>>,
        _resolver: &mut Resolver<'data, Elf<C>>,
        _per_symbol_flags: &mut PerSymbolFlags,
        _output_sections: &mut OutputSections<'data, Elf<C>>,
        _layout_rules_builder: &mut LayoutRulesBuilder<'data>,
        _plugin_loaded: LoadedInputs<'data, Elf<C>>,
    ) -> Result {
        Ok(())
    }
}

pub(crate) fn resolve_lto_symbols<'data, 'scope, C: ElfClass>(
    _obj: &LtoInput<'data>,
    _resources: &'scope wild_layout::resolution::ResolutionResources<'data, 'scope, Elf<C>>,
    _definitions_out: &mut [SymbolId],
    _scope: &Scope<'scope>,
) -> Result {
    Ok(())
}
