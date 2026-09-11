//! Equality bounds that pin `Platform` associated engine types to this crate's concrete types.

use crate::CommonGroupState;
use crate::DynamicLayoutState;
use crate::DynamicSymbolDefinition;
use crate::FinaliseLayoutResources;
use crate::FinaliseSizesResources;
use crate::GraphResources;
use crate::GroupState;
use crate::HeaderInfo;
use crate::Layout;
use crate::LocalWorkQueue;
use crate::ObjectLayoutState;
use crate::OutputRecordLayout;
use crate::PreludeLayoutState;
use crate::Resolution;
use crate::ResolutionWriter;
use crate::StubLibraryLayoutState;
use crate::SymbolResolutions;
use crate::grouping::Group;
use crate::grouping::SequencedLinkerScript;
use crate::layout_rules::LayoutRulesBuilder;
use crate::parsing::InternalSymDefInfo;
use crate::parsing::InternalSymbolsBuilder;
use crate::resolution::LoadedMetrics;
use crate::resolution::ResolutionResources;
use crate::resolution::ResolvedDynamic;
use crate::resolution::ResolvedObject;
use crate::resolution::ResolvedStubLibrary;
use crate::resolution::Resolver;
use crate::resolution::UnloadedSection;
use crate::symbol_db::SymbolDb;
use wild_platform::Platform;

pub trait EnginePlatform:
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
        Group<'data> = Group<'data, Self>,
        SequencedLinkerScript<'data> = SequencedLinkerScript<'data, Self>,
        LayoutRulesBuilder<'data> = LayoutRulesBuilder<'data>,
        InternalSymbolsBuilder<'data> = InternalSymbolsBuilder<'data, Self>,
        InternalSymDefInfo<'data> = InternalSymDefInfo<'data, Self>,
        LtoInput<'data> = crate::grouping::LtoInput<'data>,
        OutputSections<'data> = crate::output_section_id::OutputSections<'data, Self>,
        OutputOrder<'data> = crate::output_section_id::OutputOrder<'data>,
        LocationCounter<'data> = crate::layout_rules::LocationCounter<'data>,
        SectionOutputInfo<'data> = crate::output_section_id::SectionOutputInfo<'data, Self>,
    > + Platform<
        LocalWorkQueue = LocalWorkQueue<Self>,
        OutputRecordLayout = OutputRecordLayout,
        SymbolResolutions = SymbolResolutions<Self>,
        LayoutSection = crate::Section,
        HeaderInfo = HeaderInfo,
        Resolution = Resolution<Self>,
        UnloadedSection = UnloadedSection,
        LoadedMetrics = LoadedMetrics,
        CustomSectionIds = crate::output_section_id::CustomSectionIds,
    >
{
    /// Claim LTO IR for a linker plugin. The default reports that no plugin was supplied.
    fn process_plugin_input<'data>(
        _plugin: &mut Self::LinkerPlugin<'data>,
        input_ref: wild_args::InputRef<'data>,
        _file: &std::fs::File,
        kind: wild_platform::FileKind,
    ) -> wild_error::error::Result<Option<crate::grouping::UnsequencedLtoInput<'data>>> {
        wild_error::bail!(
            "Input file {input_ref} contains {kind}, but linker plugin was not supplied"
        )
    }
}

/// Convert concrete engine values to `Platform` GATs. Sound because every format's `Platform`
/// impl aliases these associated types to the types named here; rustc cannot be told that for
/// generic `P` without a dual-lifetime HRTB (issue 100013).
#[inline(always)]
pub fn platform_graph<'a, 'data, 'scope, P: Platform>(
    resources: &'a GraphResources<'data, 'scope, P>,
) -> &'a P::GraphResources<'data, 'scope> {
    unsafe { &*(std::ptr::from_ref(resources).cast()) }
}

#[inline(always)]
pub fn platform_finalise_layout<'a, 'scope, 'data, P: Platform>(
    resources: &'a FinaliseLayoutResources<'scope, 'data, P>,
) -> &'a P::FinaliseLayoutResources<'scope, 'data> {
    unsafe { &*(std::ptr::from_ref(resources).cast()) }
}

#[inline(always)]
pub fn platform_finalise_sizes<'a, 'data, 'scope, P: Platform>(
    resources: &'a FinaliseSizesResources<'data, 'scope, P>,
) -> &'a P::FinaliseSizesResources<'data, 'scope> {
    unsafe { &*(std::ptr::from_ref(resources).cast()) }
}

#[inline(always)]
pub fn platform_resolution_writer<'a, 'writer, 'out, P: Platform>(
    writer: &'a mut ResolutionWriter<'writer, 'out, P>,
) -> &'a mut P::ResolutionWriter<'writer, 'out> {
    unsafe { &mut *std::ptr::from_mut(writer).cast() }
}

#[cfg_attr(not(all(feature = "plugins", unix)), allow(dead_code))]
#[inline(always)]
pub fn platform_resolution<'a, 'data, 'scope, P: Platform>(
    resources: &'a ResolutionResources<'data, 'scope, P>,
) -> &'a P::ResolutionResources<'data, 'scope> {
    unsafe { &*(std::ptr::from_ref(resources).cast()) }
}

/// Dual-lifetime GAT equalities. Not folded into [`EnginePlatform`] because
/// `for<'scope, 'data: 'scope>` hits rustc issue 100013.
#[allow(dead_code)]
pub trait EngineScope<'data, 'scope>: EnginePlatform
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
pub trait EngineWriter<'writer, 'out>: EnginePlatform
where
    'out: 'writer,
    Self: Platform<ResolutionWriter<'writer, 'out> = ResolutionWriter<'writer, 'out, Self>>,
{
}

// Explicit impls live next to each format type (`Elf<C>`, `Wasm`, `MachO`). A blanket impl
// re-proves the associated-type equalities for every generic `P` and fails to unify.
