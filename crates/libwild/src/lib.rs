pub use wild_args as args;
pub use wild_args::Args;
pub(crate) mod debug_trace;
pub(crate) mod diff;
pub(crate) mod elf;
pub use wild_error::error;
pub(crate) mod file_kind;
pub(crate) mod input_data;
pub(crate) mod macho;
pub use wild_error::bail;
pub use wild_error::debug_assert_bail;
pub use wild_error::ensure;
pub use wild_error::malfunction;
pub use wild_error::malfunction_point_ret;
#[cfg(test)]
mod layout_stack_elf_tests;
pub(crate) mod output_kind;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub(crate) mod perf;
#[cfg(any(
    not(target_os = "linux"),
    all(
        target_os = "linux",
        any(
            target_arch = "riscv64",
            target_arch = "loongarch64",
            target_arch = "powerpc64"
        )
    )
))]
#[path = "perf_unsupported.rs"]
pub(crate) mod perf;
pub(crate) mod save_dir;
#[cfg(all(feature = "fork", unix))]
pub(crate) mod subprocess;
#[cfg(not(all(feature = "fork", unix)))]
#[path = "subprocess_unsupported.rs"]
pub(crate) mod subprocess;
#[cfg(all(test, not(target_family = "wasm")))]
mod tidy_tests;
pub(crate) mod timing;
pub(crate) mod wasm;

use crate::args::HasCommonArgs as _;
use crate::error::Context;
use crate::error::Result;
use colosseum::sync::Arena;
use crossbeam_utils::atomic::AtomicCell;
use error::AlreadyInitialised;
use hashbrown::HashSet;
use input_data::FileLoader;
use input_data::FileLoaderExt as _;
use input_data::InputFile as LoadedInputFile;
use std::io::BufWriter;
use std::io::IsTerminal;
use std::io::Write;
use std::path::Path;
pub use subprocess::run_in_subprocess;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
pub use wild_fs::fs::FileReplacementMode;
pub use wild_fs::fs::FileSystem;
pub use wild_fs::fs::FileType;
pub use wild_fs::fs::FileWriteMode;
pub use wild_fs::fs::InputFileData;
pub use wild_fs::fs::OsFileSystem;
pub use wild_fs::fs::OutputFileData;
pub use wild_fs::fs::OutputOptions;
pub use wild_fs::fs::make_executable;
use wild_layout::EnginePlatform;
use wild_layout::file_writer;
use wild_layout::layout_rules::LayoutRulesBuilder;
use wild_layout::output_section_id::OutputSections;
use wild_platform::Arch;
use wild_platform::Args as _;
use wild_platform::Platform;
use wild_platform::value_flags::PerSymbolFlags;
use wild_scripts::version_script::VersionScript;

/// Runs the linker in a Rayon thread pool configured from the supplied arguments or the available
/// jobserver tokens, then cleans up associated resources. Only use this function if you've OK with
/// waiting for cleanup.
pub fn run(mut args: Args) -> error::Result {
    let thread_pool = args.common_mut().build_thread_pool()?;
    thread_pool.pool.install(move || -> error::Result {
        let linker = Linker::new();
        linker.run(&args)?;
        drop(linker);
        timing::finalise_perfetto_trace()?;
        Ok(())
    })?;

    Ok(())
}

/// Sets up whatever tracing, if any, is indicated by the supplied arguments. This can only be
/// called once and only if nothing else has already set the global tracing dispatcher. Calling this
/// is optional. If it isn't called, no tracing-based features will function. e.g. --time.
pub fn setup_tracing(args: &Args) -> Result<(), AlreadyInitialised> {
    if let Some(opts) = args.common().time_phase_options.as_ref() {
        timing::init_tracing(opts)
    } else if args.common().print_allocations.is_some() {
        debug_trace::init()
    } else {
        tracing_subscriber::registry()
            .with(fmt::layer().with_ansi(std::io::stdout().is_terminal()))
            .with(EnvFilter::from_env("WILD_LOG"))
            .try_init()
            .map_err(|_| AlreadyInitialised)
    }
}

/// This is effectively a data store for use while linking. It takes ownership of all the input data
/// that we read, which allows the linking stages to borrow that data. Dropping this struct might be
/// expensive, so the caller of the linker might want to think about when best to drop it - probably
/// together with the `LinkerOutput`. Note, calling `exit` without dropping this struct is an
/// option, but likely won't save any time, since the bulk of the work done during drop (unmapping
/// pages) will still happen anyway.
pub struct Linker<F: FileSystem = OsFileSystem> {
    /// We store our input files here once we've read them.
    inputs_arena: Arena<input_data::InputFile<F::Input>>,

    /// Anything that doesn't need a custom Drop implementation can go in here. In practice, it's
    /// mostly just the decompressed copy of compressed string-merge sections.
    herd: wild_util::arena::Herd,

    /// We'll fill this in when we're done linking and start shutting down. Once this is dropped,
    /// that signals the end of shutdown for the purposes of timing measurement.
    #[allow(dyn_drop)]
    shutdown_scope: AtomicCell<Vec<Box<dyn Drop>>>,

    /// A timing scope that exists for the whole time we're linking.
    #[allow(dyn_drop)]
    _link_scope: Vec<Box<dyn Drop>>,

    // File system used for reading of the inputs and writing of the output file(s).
    file_system: std::sync::Arc<F>,
}

pub struct LinkerOutput<'layout_inputs> {
    #[allow(dyn_drop)]
    /// This is just here so that we defer its destruction. This allows us to (a) measure how long
    /// it takes to drop and (b) if we forked, signal our parent that we're done, then drop it in
    /// the background.
    layout: Option<Box<dyn Drop + 'layout_inputs>>,
}

impl Linker<OsFileSystem> {
    #[must_use]
    pub fn new() -> Self {
        Self::with_file_system(OsFileSystem::new())
    }
}

impl<F: FileSystem> Linker<F> {
    pub fn with_file_system(file_system: F) -> Self {
        let (guard_a, guard_b) = timing_guard!("Link");

        Self {
            file_system: std::sync::Arc::new(file_system),
            inputs_arena: Arena::new(),
            herd: Default::default(),
            shutdown_scope: Default::default(),
            _link_scope: vec![Box::new(guard_a), Box::new(guard_b)],
        }
    }

    /// Runs the linker. The returned value isn't useful for anything, but is somewhat expensive to
    /// drop, so we leave it up to the caller to decide when to drop it. At the point at which we
    /// return, the output file should be usable.
    ///
    /// This method runs in whatever Rayon thread pool is currently installed. If no thread pool is
    /// installed, then Rayon's default global thread pool will be created and used. In either case,
    /// the value of `--threads` in `args` has no effect. If you want a thread pool configured from
    /// the supplied arguments, use the top-level [`run`] function instead, which creates such a
    /// pool, runs the linker in it, then tears it down together with other associated resources.
    pub fn run<'layout_inputs>(
        &'layout_inputs self,
        args: &'layout_inputs Args,
    ) -> error::Result<LinkerOutput<'layout_inputs>> {
        let version = args.common().version_message();
        match args.common().version_mode {
            args::VersionMode::ExitAfterPrint => {
                let mut stdout = std::io::stdout().lock();
                writeln!(stdout, "{version}")?;
                return Ok(LinkerOutput { layout: None });
            }
            args::VersionMode::Verbose => {
                let mut stdout = std::io::stdout().lock();
                writeln!(stdout, "{version}")?;
                // Continue linking if object files are specified
                if args.common().inputs.is_empty() {
                    return Ok(LinkerOutput { layout: None });
                }
            }
            args::VersionMode::VerboseWithEmulations => {
                let mut stdout = std::io::stdout().lock();
                writeln!(stdout, "{version}")?;
                args.print_emulation_info(&mut stdout)?;
                // Continue linking if object files are specified
                if args.common().inputs.is_empty() {
                    return Ok(LinkerOutput { layout: None });
                }
            }
            args::VersionMode::None => {
                // Don't print version
            }
        }

        match args {
            Args::Coff(_) => crate::bail!("PE/COFF (Windows) support is not yet implemented"),
            Args::Elf(elf_args) => crate::elf::link_for_arch(self, elf_args),
            Args::MachO(macho_args) => crate::macho::link_for_arch(self, macho_args),
            Args::Wasm(wasm_args) => crate::wasm::link_for_arch(self, wasm_args),
        }
    }

    fn link_for_arch<'data, P, A>(
        &'data self,
        args: &'data P::Args,
    ) -> error::Result<LinkerOutput<'data>>
    where
        P: EnginePlatform
            + Platform<FileLoader<'data, F> = wild_layout::input_data::FileLoader<'data, F>>
            + Platform<FileWriterOutput<F> = file_writer::Output<F>>,
        A: Arch<Platform = P>,
        P::Args: crate::args::HasCommonArgs,
    {
        let mut file_loader = input_data::FileLoader::new(
            &self.inputs_arena,
            std::sync::Arc::clone(&self.file_system),
        );

        // Note, we propagate errors from `link_with_input_data` after we've checked if any files
        // changed. We want inputs-changed errors to take precedence over all other errors.
        let result = self.load_inputs_and_link::<P, A>(&mut file_loader, args);

        file_loader.verify_inputs_unchanged()?;

        // Write the dependency file and inputs trace after successful linking.
        if result.is_ok() {
            if let Some(dep_file_path) = &args.dependency_file() {
                write_dependency_file(
                    self.file_system.as_ref(),
                    dep_file_path,
                    args.output(),
                    &file_loader.loaded_files,
                )
                .with_context(|| {
                    format!(
                        "Failed to write dependency file `{}`",
                        dep_file_path.display()
                    )
                })?;
            }
            if args.should_write_trace_file() {
                let mut buf = BufWriter::new(std::io::stdout());
                for input in &file_loader.loaded_files {
                    writeln!(buf, "{}", input.filename.display())?;
                }
            }
        }

        result
    }

    pub fn file_system(&self) -> &F {
        self.file_system.as_ref()
    }

    fn load_inputs_and_link<'data, P, A>(
        &'data self,
        file_loader: &mut FileLoader<'data, F>,
        args: &'data P::Args,
    ) -> error::Result<LinkerOutput<'data>>
    where
        P: EnginePlatform
            + Platform<FileLoader<'data, F> = wild_layout::input_data::FileLoader<'data, F>>
            + Platform<FileWriterOutput<F> = file_writer::Output<F>>,
        A: Arch<Platform = P>,
        P::Args: crate::args::HasCommonArgs,
    {
        let mut plugin = P::maybe_init_linker_plugin(args, &self.herd)?;

        let loaded = file_loader.load_inputs::<P>(&args.common().inputs, args, &mut plugin);

        crate::save_dir::SaveDir::from_common(args.common())?.finish(file_loader, args)?;

        let loaded = loaded?;

        for script in loaded.linker_scripts.iter().rev() {
            if let Some(name) = script.script.output_filename() {
                args.common().apply_script_output(name);
                break;
            }
        }

        if loaded
            .linker_scripts
            .iter()
            .any(|script| script.script.force_common_allocation())
        {
            args.apply_force_common_allocation();
        }

        let output_kind = crate::output_kind::new(args, file_loader);

        let mut output = file_writer::Output::new::<P>(args, output_kind, self.file_system.clone());

        let mut output_sections = OutputSections::with_base_address(
            args.image_base()
                .unwrap_or_else(|| A::start_memory_address(output_kind)),
            output_kind,
        );
        if let Some(base) = args.image_base() {
            let page_size = args.loadable_segment_alignment().value();
            if base % page_size != 0 {
                bail!("--image-base: address isn't multiple of page size: {base:#x}");
            }
        }
        output_sections.set_rosegment(args.rosegment());

        let mut layout_rules_builder = LayoutRulesBuilder::default();

        let auxiliary =
            input_data::load_auxiliary_files(args, &self.inputs_arena, self.file_system.as_ref())?;

        let mut symbol_db = wild_layout::symbol_db::SymbolDb::new(
            args,
            output_kind,
            auxiliary.version_script_data,
            auxiliary.export_list_data,
            &self.herd,
        )?;
        let mut per_symbol_flags = PerSymbolFlags::new();

        symbol_db.add_inputs(
            &mut per_symbol_flags,
            &mut output_sections,
            &mut layout_rules_builder,
            loaded,
        )?;

        symbol_db.apply_wrapped_symbol_overrides();

        let mut resolver = wild_layout::resolution::Resolver::default();

        resolver
            .resolve_symbols_and_select_archive_entries(&mut symbol_db, &mut per_symbol_flags)?;

        // Now that we know which archive entries are being loaded, we can resolve alternative
        // symbol definitions.
        wild_layout::symbol_db::resolve_alternative_symbol_definitions(
            &mut symbol_db,
            &mut per_symbol_flags,
            &resolver.resolved_groups,
        )?;

        if let Some(plugin) = plugin.as_mut()
            && P::plugin_is_initialised(plugin)
            && let Some(generated) =
                P::plugin_lto_codegen(plugin, &mut symbol_db, &mut resolver, &mut per_symbol_flags)?
        {
            let plugin_loaded = file_loader.load_inputs(&generated, args, &mut None)?;
            P::plugin_integrate_lto_objects(
                plugin,
                &mut symbol_db,
                &mut resolver,
                &mut per_symbol_flags,
                &mut output_sections,
                &mut layout_rules_builder,
                plugin_loaded,
            )?;
        }

        // If it's a rust version script, apply the global symbol visibility now.
        // We previously downgraded all symbols to local visibility.
        if let VersionScript::Rust(rust_vscript) = &symbol_db.version_script {
            symbol_db.handle_rust_version_script(rust_vscript, &mut per_symbol_flags);
        }

        let layout_rules = layout_rules_builder.build::<P>(args);

        let resolved = resolver.resolve_sections_and_canonicalise_undefined(
            &mut symbol_db,
            &mut per_symbol_flags,
            &mut output_sections,
            &layout_rules,
        )?;

        let mut layout =
            wild_layout::compute::<P, A>(symbol_db, per_symbol_flags, resolved, output_sections)?;

        output.set_size(wild_layout::compute_total_file_size(
            &layout.section_layouts,
        ));
        wild_layout::gc_stats::maybe_write_gc_stats(&layout.group_layouts, &layout.symbol_db)?;

        let plugin_active = plugin.as_ref().is_some_and(P::plugin_is_initialised);
        let mut incremental_session = if args.incremental() {
            wild_layout::incremental::IncrementalSession::from_args(args)
        } else {
            None
        };
        if let Some(session) = incremental_session.as_mut() {
            if let Some(reason) =
                wild_layout::incremental::fallback_for_plugin_or_gc::<P>(args, plugin_active)
            {
                session.record_fallback(reason);
            }
            let (sections, has_strict_order_sections) = incremental_section_snapshot(&layout);
            if has_strict_order_sections {
                session.record_fallback("strict-order .init/.fini");
            }
            let records = layout.incremental_file_records();
            let input_paths: Vec<&Path> = file_loader
                .loaded_files
                .iter()
                .map(|f| f.filename.as_path())
                .collect();
            layout.incremental_atoms = session.bind_files(&records);
            layout.incremental_skip_payloads =
                session.plan_in_place_update(&sections, &records, &input_paths);
            if !layout.incremental_skip_payloads.is_empty() {
                if let (Some(old_resolutions), Some(reverse_relocs)) = (
                    session.previous_resolutions.take(),
                    session.previous_reverse_relocs.take(),
                ) {
                    layout.incremental_patch =
                        Some(wild_layout::incremental::IncrementalPatchJob {
                            old_resolutions,
                            reverse_relocs,
                        });
                }
            }
        }

        P::write_output_file::<A, F>(&output, &layout)?;
        diff::maybe_diff()?;

        if let Some(session) = incremental_session {
            let (sections, has_strict_order_sections) = incremental_section_snapshot(&layout);
            let records = layout.incremental_file_records();
            let resolutions = layout.incremental_resolutions();
            let reverse_relocs = layout.take_reverse_relocs();
            session.finish(
                &file_loader
                    .loaded_files
                    .iter()
                    .map(|f| f.filename.as_path())
                    .collect::<Vec<_>>(),
                plugin_active,
                has_strict_order_sections,
                &sections,
                &resolutions,
                &reverse_relocs,
                &records,
            )?;
        }

        // We've finished linking. We consider everything from this point onwards as shutdown.
        let (g1, g2) = timing_guard!("Shutdown");
        self.shutdown_scope.store(vec![Box::new(g1), Box::new(g2)]);

        Ok(LinkerOutput {
            layout: Some(Box::new(layout)),
        })
    }
}

impl Default for Linker<OsFileSystem> {
    fn default() -> Self {
        Self::new()
    }
}

impl<F: FileSystem> Drop for Linker<F> {
    fn drop(&mut self) {
        timing_phase!("Drop inputs");
        self.inputs_arena = Arena::new();
        self.herd = Default::default();
    }
}

impl Drop for LinkerOutput<'_> {
    fn drop(&mut self) {
        timing_phase!("Drop layout");
        self.layout.take();
    }
}

fn incremental_section_snapshot<P: EnginePlatform>(
    layout: &wild_layout::Layout<P>,
) -> (Vec<wild_layout::incremental::PersistedSection>, bool) {
    let mut sections = Vec::new();
    let mut has_strict_order_sections = false;
    layout.section_layouts.for_each(|id, rec| {
        let name = layout
            .output_sections
            .name(id)
            .map(|n| String::from_utf8_lossy(n.bytes()).into_owned())
            .unwrap_or_default();
        if rec.mem_size > 0 && (name == ".init" || name == ".fini") {
            has_strict_order_sections = true;
        }
        sections.push(wild_layout::incremental::PersistedSection {
            name,
            file_offset: rec.file_offset,
            file_size: rec.file_size,
            mem_size: rec.mem_size,
        });
    });
    (sections, has_strict_order_sections)
}

/// Writes a dependency file in Makefile format.
fn write_dependency_file<I: InputFileData>(
    file_system: &impl FileSystem,
    dep_file_path: &Path,
    output_path: &Path,
    loaded_files: &[&LoadedInputFile<I>],
) -> Result<()> {
    timing_phase!("Write dependency file");

    let mut writer = Vec::new();

    // Collect unique dependency paths
    let mut seen = HashSet::new();
    let mut deps = Vec::new();
    for input_file in loaded_files {
        // Skip temporary files. e.g. those generated by linker plugins.
        if input_file.modifiers.temporary {
            continue;
        }

        let path_str = input_file.filename.display().to_string();
        if seen.insert(path_str.clone()) {
            deps.push(path_str);
        }
    }

    write!(writer, "{}:", output_path.display())?;

    for dep in &deps {
        write!(writer, " {dep}")?;
    }

    writeln!(writer)?;

    for dep in &deps {
        writeln!(writer, "\n{dep}:")?;
    }

    file_system.write_auxiliary(dep_file_path, &writer)?;
    Ok(())
}

/// Possibly initialise timing if a timing-related environment variable is active and it was enabled
/// in the build, otherwise, do nothing. See `BENCHMARKING.md` for details.
pub fn init_timing() -> Result {
    timing::setup()
}

pub fn should_fork(args: &Args) -> bool {
    args.common().should_fork()
}
