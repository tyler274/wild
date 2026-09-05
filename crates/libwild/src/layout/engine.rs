//! Equality bounds that pin `Platform` associated engine types to this crate's concrete types.

use crate::grouping::Group;
use crate::grouping::SequencedLinkerScript;
use crate::layout::CommonGroupState;
use crate::layout::DynamicLayoutState;
use crate::layout::DynamicSymbolDefinition;
use crate::layout::FinaliseLayoutResources;
use crate::layout::FinaliseSizesResources;
use crate::layout::GraphResources;
use crate::layout::GroupState;
use crate::layout::HeaderInfo;
use crate::layout::Layout;
use crate::layout::LocalWorkQueue;
use crate::layout::ObjectLayoutState;
use crate::layout::OutputRecordLayout;
use crate::layout::PreludeLayoutState;
use crate::layout::Resolution;
use crate::layout::ResolutionWriter;
use crate::layout::StubLibraryLayoutState;
use crate::layout::SymbolResolutions;
use crate::layout_rules::LayoutRulesBuilder;
use crate::linker_plugins::LinkerPlugin;
use crate::linker_plugins::LoadedPlugin;
use crate::linker_plugins::LtoInput;
use crate::parsing::InternalSymDefInfo;
use crate::parsing::InternalSymbolsBuilder;
use crate::platform::Platform;
use crate::resolution::LoadedMetrics;
use crate::resolution::ResolutionResources;
use crate::resolution::ResolvedDynamic;
use crate::resolution::ResolvedObject;
use crate::resolution::ResolvedStubLibrary;
use crate::resolution::Resolver;
use crate::resolution::UnloadedSection;
use crate::symbol_db::SymbolDb;

pub(crate) trait EnginePlatform:
    for<'data> Platform<
        Layout<'data> = Layout<'data, Self>,
        SymbolDb<'data> = SymbolDb<'data, Self>,
        Resolver<'data> = Resolver<'data, Self>,
        ObjectLayoutState<'data> = ObjectLayoutState<'data, Self>,
        CommonGroupState<'data> = CommonGroupState<'data, Self>,
        GroupState<'data> = GroupState<'data, Self>,
        DynamicLayoutState<'data> = DynamicLayoutState<'data, Self>,
        PreludeLayoutState<'data> = PreludeLayoutState<'data, Self>,
        StubLibraryLayoutState<'data> = StubLibraryLayoutState<'data, Self>,
        DynamicSymbolDefinition<'data> = DynamicSymbolDefinition<'data, Self>,
        ResolvedObject<'data> = ResolvedObject<'data, Self>,
        ResolvedDynamic<'data> = ResolvedDynamic<'data, Self>,
        ResolvedStubLibrary<'data> = ResolvedStubLibrary<'data>,
        LinkerPlugin<'data> = LinkerPlugin<'data>,
        LtoInput<'data> = LtoInput<'data>,
        Group<'data> = Group<'data, Self>,
        SequencedLinkerScript<'data> = SequencedLinkerScript<'data, Self>,
        LayoutRulesBuilder<'data> = LayoutRulesBuilder<'data>,
        InternalSymbolsBuilder<'data> = InternalSymbolsBuilder<'data, Self>,
        InternalSymDefInfo<'data> = InternalSymDefInfo<'data, Self>,
        OutputSections<'data> = crate::output_section_id::OutputSections<'data, Self>,
        OutputOrder<'data> = crate::output_section_id::OutputOrder<'data>,
        LocationCounter<'data> = crate::layout_rules::LocationCounter<'data>,
        SectionOutputInfo<'data> = crate::output_section_id::SectionOutputInfo<'data, Self>,
    > + Platform<
        LocalWorkQueue = LocalWorkQueue<Self>,
        OutputRecordLayout = OutputRecordLayout,
        SymbolResolutions = SymbolResolutions<Self>,
        LayoutSection = crate::layout::Section,
        HeaderInfo = HeaderInfo,
        Resolution = Resolution<Self>,
        UnloadedSection = UnloadedSection,
        LoadedMetrics = LoadedMetrics,
        LoadedPlugin = LoadedPlugin,
        CustomSectionIds = crate::output_section_id::CustomSectionIds,
        FileKind = crate::file_kind::FileKind,
    >
{
}

/// Convert concrete engine values to `Platform` GATs. Sound because every format's `Platform`
/// impl aliases these associated types to the types named here; rustc cannot be told that for
/// generic `P` without a dual-lifetime HRTB (issue 100013).
#[inline(always)]
pub(crate) fn platform_graph<'a, 'data, 'scope, P: Platform>(
    resources: &'a GraphResources<'data, 'scope, P>,
) -> &'a P::GraphResources<'data, 'scope> {
    unsafe { &*(std::ptr::from_ref(resources).cast()) }
}

#[inline(always)]
pub(crate) fn platform_finalise_layout<'a, 'scope, 'data, P: Platform>(
    resources: &'a FinaliseLayoutResources<'scope, 'data, P>,
) -> &'a P::FinaliseLayoutResources<'scope, 'data> {
    unsafe { &*(std::ptr::from_ref(resources).cast()) }
}

#[inline(always)]
pub(crate) fn platform_finalise_sizes<'a, 'data, 'scope, P: Platform>(
    resources: &'a FinaliseSizesResources<'data, 'scope, P>,
) -> &'a P::FinaliseSizesResources<'data, 'scope> {
    unsafe { &*(std::ptr::from_ref(resources).cast()) }
}

#[inline(always)]
pub(crate) fn platform_resolution_writer<'a, 'writer, 'out, P: Platform>(
    writer: &'a mut ResolutionWriter<'writer, 'out, P>,
) -> &'a mut P::ResolutionWriter<'writer, 'out> {
    unsafe { &mut *std::ptr::from_mut(writer).cast() }
}

#[cfg_attr(not(all(feature = "plugins", unix)), allow(dead_code))]
#[inline(always)]
pub(crate) fn platform_resolution<'a, 'data, 'scope, P: Platform>(
    resources: &'a ResolutionResources<'data, 'scope, P>,
) -> &'a P::ResolutionResources<'data, 'scope> {
    unsafe { &*(std::ptr::from_ref(resources).cast()) }
}

/// Dual-lifetime GAT equalities. Not folded into [`EnginePlatform`] because
/// `for<'scope, 'data: 'scope>` hits rustc issue 100013.
#[allow(dead_code)]
pub(crate) trait EngineScope<'data, 'scope>: EnginePlatform
where
    'data: 'scope,
    Self: Platform<
            GraphResources<'data, 'scope> = GraphResources<'data, 'scope, Self>,
            FinaliseLayoutResources<'scope, 'data> = FinaliseLayoutResources<'scope, 'data, Self>,
            FinaliseSizesResources<'data, 'scope> = FinaliseSizesResources<'data, 'scope, Self>,
            ResolutionResources<'data, 'scope> = ResolutionResources<'data, 'scope, Self>,
        >,
{
}

/// `ResolutionWriter` uses a different lifetime pair than [`EngineScope`].
#[allow(dead_code)]
pub(crate) trait EngineWriter<'writer, 'out>: EnginePlatform
where
    'out: 'writer,
    Self: Platform<ResolutionWriter<'writer, 'out> = ResolutionWriter<'writer, 'out, Self>>,
{
}

// Explicit impls live next to each format type (`Elf<C>`, `Wasm`, `MachO`). A blanket impl
// re-proves the associated-type equalities for every generic `P` and fails to unify.
