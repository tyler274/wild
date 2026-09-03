//! Implements support for using linker plugins that follow the GNU Gold linker plugin API. Although
//! GNU Gold is now deprecated, this API is implemented by other linkers such as GNU ld and Mold.
//! Plugins that follow the API are provided by GCC and Clang.
//!
//! See the linker plugin API docs at https://gcc.gnu.org/wiki/whopr/driver
//!
//! Note, the lifetimes in the plugin API are a bit of a pain to deal with. The API docs don't
//! actually document lifetimes at all, so it's a case of observing what actual plugins do. We end
//! up having to make quite a bit of use of thread locals in order to get state to where it needs to
//! be.

use crate::FileSystem;
use crate::args::Input;
use crate::args::Modifiers;
use crate::args::elf::ElfArgs;
use crate::bail;
use crate::elf;
use crate::elf::Elf;
use crate::elf::ElfClass;
use crate::elf::RawSymbolName;
use crate::env;
use crate::error;
use crate::error::Context as _;
use crate::error::Error;
use crate::error::Result;
use crate::file_kind::FileKind;
use crate::input_data::FileId;
use crate::input_data::FileLoader;
use crate::input_data::InputRef;
use crate::layout_rules::LayoutRulesBuilder;
use crate::output_section_id::OutputSections;
use crate::platform::Args as _;
use crate::platform::Platform;
use crate::platform::RawSymbolName as _;
use crate::resolution::ResolutionResources;
use crate::resolution::ResolvedFile;
use crate::resolution::ResolvedGroup;
use crate::resolution::Resolver;
use crate::resolution::SymbolAttributes;
use crate::symbol::PreHashedSymbolName;
use crate::symbol::UnversionedSymbolName;
use crate::symbol_db::SymbolDb;
use crate::symbol_db::SymbolId;
use crate::symbol_db::SymbolIdRange;
use crate::timing_phase;
use crate::value_flags::FlagsForSymbol;
use crate::value_flags::PerSymbolFlags;
use crate::value_flags::ValueFlags;
use crate::verbose_timing_phase;
use bumpalo_herd::Herd;
use colosseum::sync::Arena;
use crossbeam_utils::atomic::AtomicCell;
use libloading::Library;
use rayon::Scope;
use std::cell::Cell;
use std::cell::RefCell;
use std::ffi::CStr;
use std::ffi::CString;
use std::ffi::OsStr;
use std::fs::File;
use std::ops::Not as _;
use std::os::fd::AsRawFd as _;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::path::PathBuf;

mod discover;
mod ffi;
mod lto;

#[allow(unused_imports)]
pub(crate) use ffi::*;
#[allow(unused_imports)]
pub(crate) use lto::*;

/// Set this environment variable to a directory and we'll write output files produced by the linker
/// plugin to it. Old outputs will be deleted, but only if the directory looks like one we produced.
const SAVE_VAR_NAME: &str = "WILD_SAVE_PLUGIN_OUTPUTS";

pub(crate) struct LinkerPlugin<'data> {
    store: Store<'data>,
    herd: &'data Herd,
    wrap_symbols: WrapSymbols<'data>,
    path: PathBuf,
}

enum Store<'data> {
    Unloaded(LoadInfo<'data>),
    Loaded(&'data mut LoadedPlugin),
}

struct LoadInfo<'data> {
    args: &'data ElfArgs,
    arena: &'data Arena<LoadedPlugin>,
    get_symbols_v3: GetSymbols,
}

/// Manages the lifetime of the linker plugin. Once dropped, the plugin will be deinitialised and
/// unloaded.
pub(crate) struct LoadedPlugin {
    callbacks: Callbacks,

    /// Dropping this will unload the plugin, so although we don't make use of this, we need to
    /// keep it alive until we're done.
    _lib: Library,

    version_info: Option<VersionInfo>,
}

#[derive(Debug)]
pub(crate) struct LtoInput<'data> {
    pub(crate) file_id: FileId,
    pub(crate) symbol_id_range: SymbolIdRange,
    pub(crate) section_id_range: crate::input_section_id::SectionIdRange,
    pub(crate) input_ref: InputRef<'data>,
    pub(crate) symbols: Vec<PluginSymbol<'data>>,
    /// Set to false once symbols from this object should be ignored. This is done once LTO has
    /// been performed.
    pub(crate) enabled: bool,
}

#[derive(Debug)]
pub(crate) struct LtoInputInfo<'data> {
    input_ref: InputRef<'data>,
    symbols: Vec<PluginSymbol<'data>>,
    handle: &'data FileHandle<'data>,
}

/// Stores symbol names passed to --wrap in a form that we can pass to the linker plugin if
/// requested. Note, that this appears to only be used by the LLVM plugin. See comment on call to
/// apply_wrapped_symbol_overrides.
#[derive(Clone, Copy)]
pub(crate) struct WrapSymbols<'data>(pub(crate) &'data [*const libc::c_char]);

unsafe impl Send for WrapSymbols<'_> {}
unsafe impl Sync for WrapSymbols<'_> {}

// The API version got introduced in GCC 14.
pub(crate) const API_VERSION: u32 = 1;

#[derive(Debug)]
pub(crate) struct FileHandle<'data> {
    pub(crate) data: &'data [u8],

    /// This isn't known initially because we allocate file IDs later.
    pub(crate) file_id: AtomicCell<Option<FileId>>,

    pub(crate) fd: RawFd,
    pub(crate) offset: u64,
    pub(crate) name: &'data CStr,
}

#[derive(Default)]
pub(crate) struct PluginOutputs {
    pub(crate) generated_inputs: Vec<Input>,
}

impl<'data> LinkerPlugin<'data> {
    pub(crate) fn from_args<C: ElfClass>(
        args: &'data ElfArgs,
        arena: &'data Arena<LoadedPlugin>,
        herd: &'data Herd,
    ) -> Result<Option<LinkerPlugin<'data>>> {
        let wrap_symbols = WrapSymbols::new(&args.wrap, herd)?;
        let _ = increase_file_limit();
        let path = args
            .plugin_path
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_default();
        Ok(Some(LinkerPlugin {
            path,
            store: Store::Unloaded(LoadInfo {
                args,
                arena,
                get_symbols_v3: get_symbols_v3::<C>,
            }),
            herd,
            wrap_symbols,
        }))
    }

    fn discover_plugin_path_for_kind(kind: FileKind) -> Result<PathBuf> {
        match kind {
            FileKind::LlvmIr => discover::discover_llvm_gold_plugin(),
            FileKind::GccIr => discover::discover_gcc_lto_plugin(),
            _ => crate::bail!("No linker plugin is applicable for {kind}"),
        }
    }

    pub(crate) fn process_input(
        &'_ mut self,
        input_ref: InputRef<'data>,
        file: &File,
        kind: FileKind,
    ) -> Result<Option<Box<LtoInputInfo<'data>>>> {
        verbose_timing_phase!("Linker plugin process input");

        let fd = file.as_raw_fd();

        if self.path.as_os_str().is_empty() {
            self.path = Self::discover_plugin_path_for_kind(kind)?;
        }

        if let Some(info) = self.claim_file(input_ref, fd)? {
            Ok(Some(info))
        } else {
            if input_ref.has_archive_semantics() {
                return Ok(None);
            }
            bail!(
                "Input file {input_ref} contains {kind}, \
                        but the linker plugin ({self}) didn't claim it"
            );
        }
    }

    /// Notify the plugin that all symbols have now been read. This will cause it to build
    /// additional object files that it will then pass to us for processing.
    pub(crate) fn all_symbols_read<F: FileSystem, C: ElfClass>(
        &mut self,
        symbol_db: &mut SymbolDb<'data, Elf<C>>,
        resolver: &mut Resolver<'data, Elf<C>>,
        file_loader: &mut FileLoader<'data, F>,
        per_symbol_flags: &mut PerSymbolFlags,
        output_sections: &mut OutputSections<'data, Elf<C>>,
        layout_rules_builder: &mut LayoutRulesBuilder<'data>,
    ) -> Result {
        // If no LTO files were activated, and we proceed with LTO, the GCC plugin tries to invoke
        // GCC with no input file, resulting in an error.
        if !has_loaded_lto_input(&resolver.resolved_groups) {
            return Ok(());
        }

        // Plugin codegen objects are given the command-line position of the first LTO input
        // (#1935) via `link_order`; FileIds and SymbolIds stay at the end of the ID space.

        // Mark wrapped symbol names and their __wrap_/__real_ variants as referenced by non-IR
        // code. This ensures the plugin keeps them in the LTO output rather than
        // internalising/removing them.
        mark_wrap_symbols_as_non_ir_ref(symbol_db, per_symbol_flags);

        mark_lto_symbols_for_dynamic_export(symbol_db, per_symbol_flags, &resolver.resolved_groups);

        let plugin_path = self.path.clone();
        let plugin_outputs = self
            .store
            .loaded(&plugin_path)?
            .with_callbacks(|callbacks| {
                if let Some(cb) = callbacks.all_symbols_read {
                    let ctx = AllSymbolsReadContext {
                        symbol_db,
                        resolved_groups: &resolver.resolved_groups,
                        per_symbol_flags,
                    };

                    ctx.set_current_while(|| cb().to_result("all_symbols_read"))?;
                }
                Ok(PLUGIN_OUTPUTS.take())
            })?;

        if let Ok(dir_name) = env::var(SAVE_VAR_NAME) {
            plugin_outputs.save_to(Path::new(&dir_name))?;
        }

        let plugin_loaded =
            file_loader.load_inputs(&plugin_outputs.generated_inputs, symbol_db.args, &mut None)?;

        // Temporarily restore original (pre-wrap) name mappings so that definitions of wrapped
        // names in the LTO output are registered as alternatives of the original symbols rather
        // than of `__wrap_*`.
        symbol_db.restore_wrapped_symbol_names();

        symbol_db.add_inputs(
            per_symbol_flags,
            output_sections,
            layout_rules_builder,
            plugin_loaded,
        )?;

        // Re-apply --wrap overrides so that wrapped names now map to definitions from the LTO
        // output rather than the LTO input objects.
        symbol_db.apply_wrapped_symbol_overrides();

        resolver.resolve_symbols_and_select_archive_entries(symbol_db, per_symbol_flags)?;

        symbol_db.disable_lto_inputs();

        crate::symbol_db::resolve_alternative_symbol_definitions(
            symbol_db,
            per_symbol_flags,
            &resolver.resolved_groups,
        )?;

        Ok(())
    }

    fn claim_file(
        &'_ mut self,
        input_ref: InputRef<'data>,
        fd: RawFd,
    ) -> Result<Option<Box<LtoInputInfo<'data>>>> {
        let plugin_path = self.path.clone();
        self.store
            .loaded(&plugin_path)?
            .with_callbacks(|callbacks| {
                let data = input_ref.data();
                let offset = input_ref
                    .entry
                    .as_ref()
                    .map_or(0, |entry| entry.start_offset as u64);

                let cb = callbacks
                    .claim_file_hook
                    .context("Missing claim file hook")?;

                let mut ctx = ClaimContext {
                    symbols: Vec::new(),
                    herd: self.herd,
                    wrap_symbols: self.wrap_symbols,
                };

                let name = CString::new(input_ref.file.filename.as_os_str().as_encoded_bytes())?;
                let name = CStr::from_bytes_with_nul(
                    self.herd.get().alloc_slice_copy(name.as_bytes_with_nul()),
                )
                .unwrap();

                let handle = FileHandle {
                    data,
                    name,
                    fd,
                    offset,
                    file_id: AtomicCell::new(None),
                };

                let handle = self.herd.get().alloc(handle);

                let file = LdPluginInputFile {
                    name: name.as_ptr(),
                    fd,
                    offset: offset as libc::off_t,
                    file_size: data.len() as libc::off_t,
                    // Whatever we store here needs to be valid for 'data, since the plugin might
                    // pass this back to us at a later point. e.g. get_symbols
                    // does so.
                    handle: std::ptr::from_ref::<FileHandle>(handle) as *mut libc::c_void,
                };

                let mut claimed = 0;

                ctx.set_current_while(|| {
                    unsafe { cb(&raw const file, &raw mut claimed) }.to_result("claim_file")
                })?;

                check_for_errors()?;

                if claimed != 1 {
                    return Ok(None);
                }

                Ok(Some(Box::new(LtoInputInfo {
                    input_ref,
                    symbols: ctx.symbols,
                    handle,
                })))
            })
    }

    pub(crate) fn is_initialised(&self) -> bool {
        matches!(self.store, Store::Loaded(_))
    }
}

impl<'data> WrapSymbols<'data> {
    fn new(wrap: &[String], herd: &'data Herd) -> Result<Self> {
        if wrap.is_empty() {
            return Ok(Self(&[]));
        }

        let allocator = herd.get();

        let mut wrap_args = Vec::new();
        for w in wrap {
            let w_cstring = CString::new(w.as_bytes())?;
            wrap_args.push(
                allocator
                    .alloc_slice_copy(w_cstring.as_bytes())
                    .as_ptr()
                    .cast::<libc::c_char>(),
            );
        }
        Ok(Self(&*allocator.alloc_slice_copy(wrap_args.as_slice())))
    }
}

impl LoadedPlugin {
    fn new(plugin_path: &Path, args: &ElfArgs, get_symbols_v3: GetSymbols) -> Result<LoadedPlugin> {
        timing_phase!("Load linker plugin");

        if cfg!(target_feature = "crt-static") {
            bail!(
                "Linker plugins cannot be used when Wild was built as a statically linked binary"
            );
        }

        // Safety: Truthfully, we don't control the file we're loading. The user gave it to us and
        // there's nothing we can do to guarantee that loading and running it won't trigger UB. The
        // best we can say is that we at least try to conform to the expected plugin API.
        let lib = unsafe { Library::new(plugin_path) }
            .map_err(|e| error!("{}", std::error::Error::source(&e).unwrap_or(&e)))
            .context("Failed to open linker plugin")?;

        timing_phase!("Initialise linker plugin");

        // Clear any existing state in case this thread previously made it part way through
        // initialisation.
        CALLBACKS.take();

        let onload_fn: libloading::Symbol<unsafe extern "C" fn(*mut LdPluginTv)> =
            unsafe { lib.get(b"onload") }
                .context("Failed to get `onload` function from linker plugin")?;

        let output_name = CString::new(args.common.output.as_os_str().as_encoded_bytes())?;

        let output_kind = if args.should_output_executable {
            match args.common.relocation_model {
                crate::args::RelocationModel::Fixed => OutputFileType::Exec,
                crate::args::RelocationModel::PositionIndependent => OutputFileType::Pie,
            }
        } else {
            OutputFileType::Dyn
        };

        let mut transfer_vector = Vec::new();

        // Linker plugins handle entries of this vector serially, which means the message callback
        // should be registered first. Otherwise, they won't be able to indicate the problem with
        // entries preceding the callback and, for example, silently skip invalid arguments.
        // The message callback is variadic (printf-style), so we register the C trampoline
        // directly as a raw pointer value rather than going through fn_ptr2.
        transfer_vector.push(LdPluginTv {
            tag: Tag::Message as u32,
            value: wild_plugin_message_callback as *const () as usize,
        });

        for arg in &args.plugin_args {
            transfer_vector.push(LdPluginTv::c_str(Tag::Option, arg));
        }

        transfer_vector.push(LdPluginTv::value(Tag::ApiVersion, API_VERSION as usize));
        transfer_vector.push(LdPluginTv::value(Tag::LinkerOutput, output_kind as usize));
        transfer_vector.push(LdPluginTv::c_str(Tag::OutputName, &output_name));
        transfer_vector.push(LdPluginTv::fn_ptr1(
            Tag::RegisterClaimFileHook,
            register_claim_file_hook,
        ));
        transfer_vector.push(LdPluginTv::fn_ptr1(
            Tag::RegisterCleanupHook,
            register_cleanup_hook,
        ));
        transfer_vector.push(LdPluginTv::fn_ptr1(
            Tag::RegisterAllSymbolsReadHook,
            register_all_symbols_read_hook,
        ));
        transfer_vector.push(LdPluginTv::fn_ptr6(Tag::GetApiVersion, get_api_version));
        transfer_vector.push(LdPluginTv::fn_ptr3(Tag::AddSymbols, add_symbols));
        transfer_vector.push(LdPluginTv::fn_ptr3(Tag::AddSymbolsV2, add_symbols));
        transfer_vector.push(LdPluginTv::fn_ptr3(Tag::GetSymbolsV3, get_symbols_v3));
        transfer_vector.push(LdPluginTv::fn_ptr1(Tag::AddInputFile, add_input_file));
        transfer_vector.push(LdPluginTv::fn_ptr1(Tag::AddInputLibrary, add_input_library));

        transfer_vector.push(LdPluginTv::fn_ptr0(
            Tag::GetSymbols,
            unsupported_api_version,
        ));
        transfer_vector.push(LdPluginTv::fn_ptr0(
            Tag::GetSymbolsV2,
            unsupported_api_version,
        ));

        // These don't seem to be used by the GCC plugin but are used by the clang (LLVM) plugin.
        transfer_vector.push(LdPluginTv::fn_ptr2(Tag::GetView, get_view));
        transfer_vector.push(LdPluginTv::fn_ptr2(Tag::GetWrapSymbols, get_wrap_symbols));
        transfer_vector.push(LdPluginTv::fn_ptr2(Tag::GetInputFile, get_input_file));
        transfer_vector.push(LdPluginTv::fn_ptr1(
            Tag::ReleaseInputFile,
            release_input_file,
        ));

        transfer_vector.push(LdPluginTv::value(Tag::Null, 0));

        unsafe { onload_fn(transfer_vector.as_mut_ptr()) };

        let callbacks = CALLBACKS.take();
        let version_info = VERSION_INFO.take();

        Ok(LoadedPlugin {
            _lib: lib,
            callbacks,
            version_info,
        })
    }

    /// Calls `f` with our callbacks. Checks for errors after `f` completes. We require an exclusive
    /// reference to self because the plugins callbacks aren't always threadsafe. The GCC plugin
    /// appears to be threadsafe, however the clang/LLVM plugin isn't, at least not as of clang 20.
    /// We could instead wrap the callbacks in a mutex to ensure that only one thread makes use of
    /// the callbacks at a time. In practice however, this doesn't help, since if multiple threads
    /// ask the plugin to claim files at once, then the claim order ends up non-deterministic which
    /// appears to cause the plugin to give non-deterministic output.
    fn with_callbacks<T>(&mut self, f: impl FnOnce(&mut Callbacks) -> Result<T>) -> Result<T> {
        let r = match f(&mut self.callbacks) {
            Ok(v) => v,
            Err(error) => {
                // If we encountered an error in a callback, that should take precedence over any
                // error reported by the linker plugin, since it will likely just be reporting an
                // error since we returned an error code.
                if let Some(error) = ERROR.take() {
                    return Err(error);
                }
                // If the plugin reported an error to us, then attach that as context.
                if let Some(message) = ERROR_MESSAGE.take() {
                    return Err(error).with_context(|| format!("Linker plugin error: {message}"));
                }
                return Err(error);
            }
        };
        // If the plugin reported an error to us, but then returned a successful return code, still
        // propagate the error.
        if let Some(error) = ERROR_MESSAGE.take() {
            bail!("Linker plugin error: {error}");
        }
        Ok(r)
    }
}

impl<'data> LtoInputInfo<'data> {
    pub(crate) fn num_symbols(&self) -> usize {
        self.symbols.len()
    }

    pub(crate) fn into_input_object(
        self,
        file_id: FileId,
        symbol_id_range: SymbolIdRange,
    ) -> LtoInput<'data> {
        self.handle.file_id.store(Some(file_id));

        LtoInput {
            file_id,
            symbol_id_range,
            section_id_range: crate::input_section_id::SectionIdRange::empty(),
            input_ref: self.input_ref,
            symbols: self.symbols,
            enabled: true,
        }
    }
}

impl<'data> LtoInput<'data> {
    pub(crate) fn symbol_name(
        &self,
        symbol_id: crate::symbol_db::SymbolId,
    ) -> UnversionedSymbolName<'data> {
        let local_index = self.symbol_id_range.id_to_offset(symbol_id);
        self.symbols[local_index].name
    }

    pub(crate) fn symbol_visibility(
        &self,
        symbol_id: crate::symbol_db::SymbolId,
    ) -> crate::symbol_db::Visibility {
        let local_index = self.symbol_id_range.id_to_offset(symbol_id);
        crate::elf::convert_elf_visibility(object::elf::SymbolVisibility(
            self.symbols[local_index].visibility,
        ))
    }

    pub(crate) fn symbols_iter(&self) -> impl Iterator<Item = (SymbolId, &PluginSymbol<'data>)> {
        self.symbol_id_range.into_iter().zip(self.symbols.iter())
    }

    pub(crate) fn symbol_properties_display(
        &'_ self,
        symbol_id: SymbolId,
    ) -> SymbolPropertiesDisplay<'_> {
        SymbolPropertiesDisplay(&self.symbols[self.symbol_id_range.id_to_offset(symbol_id)])
    }

    pub(crate) fn is_optional(&self) -> bool {
        self.input_ref.has_archive_semantics() && !self.input_ref.file.modifiers.whole_archive
    }
}

pub(crate) struct SymbolPropertiesDisplay<'data>(&'data PluginSymbol<'data>);

impl std::fmt::Display for SymbolPropertiesDisplay<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LTO ")?;
        if let Some(kind) = self.0.kind {
            write!(f, "{kind:?}")?;
        } else {
            write!(f, "UNKNOWN")?;
        }
        Ok(())
    }
}

impl std::fmt::Display for LtoInput<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LTO input `{}`", self.input_ref)
    }
}

impl PluginOutputs {
    pub(crate) const fn new() -> Self {
        Self {
            generated_inputs: Vec::new(),
        }
    }

    fn save_to(&self, dir_path: &Path) -> Result {
        let args_path = dir_path.join("linker-plugin-extra-args");

        if args_path.exists() {
            std::fs::remove_dir_all(dir_path)
                .with_context(|| format!("Failed to delete `{}`", dir_path.display()))?;
        } else if dir_path.exists() {
            bail!(
                "`{}` exists, but doesn't look like the right directory structure",
                dir_path.display()
            );
        }

        std::fs::create_dir_all(dir_path)
            .with_context(|| format!("Failed to create dir `{}`", dir_path.display()))?;

        let mut args = String::new();

        for input in &self.generated_inputs {
            match &input.spec {
                crate::args::InputSpec::File(path) => {
                    let dest = dir_path.join(path.file_name().context("Missing filename")?);

                    std::fs::copy(path, &dest).with_context(|| {
                        format!(
                            "Failed to copy `{}` to `{}`",
                            path.display(),
                            dest.display()
                        )
                    })?;
                }
                crate::args::InputSpec::Lib(lib_name) => {
                    args.push_str("-l");
                    args.push_str(lib_name);
                    args.push('\n');
                }
                crate::args::InputSpec::Search(search) => {
                    args.push_str("-L");
                    args.push_str(search);
                    args.push('\n');
                }
            }
        }

        std::fs::write(&args_path, args)
            .with_context(|| format!("Failed to write `{}`", args_path.display()))?;

        Ok(())
    }
}

impl std::fmt::Display for LinkerPlugin<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.store {
            Store::Unloaded(_) => write!(f, "Unloaded plugin")?,
            Store::Loaded(loaded_plugin) => {
                if let Some(version_info) = loaded_plugin.version_info.as_ref() {
                    std::fmt::Display::fmt(version_info, f)?;
                    write!(f, " ")?;
                }
            }
        }
        write!(f, "({})", self.path.display())?;
        Ok(())
    }
}

impl<'data> Store<'data> {
    fn loaded(&mut self, plugin_path: &Path) -> Result<&mut LoadedPlugin> {
        match self {
            Store::Unloaded(load_info) => {
                if plugin_path.as_os_str().is_empty() {
                    crate::bail!("No linker plugin path");
                }

                *self = Store::Loaded(
                    load_info.arena.alloc(
                        LoadedPlugin::new(plugin_path, load_info.args, load_info.get_symbols_v3)
                            .with_context(|| {
                                format!(
                                    "Failed to initialise linker plugin `{}`",
                                    plugin_path.display()
                                )
                            })?,
                    ),
                );
                let Store::Loaded(loaded) = self else {
                    unreachable!();
                };

                Ok(*loaded)
            }
            Store::Loaded(loaded_plugin) => Ok(*loaded_plugin),
        }
    }
}

/// Increase the soft file limit to whatever the hard limit is set to.
fn increase_file_limit() -> Result {
    use nix::sys::resource::Resource::RLIMIT_NOFILE;

    let (_, hard_limit) = nix::sys::resource::getrlimit(RLIMIT_NOFILE)?;

    nix::sys::resource::setrlimit(RLIMIT_NOFILE, hard_limit, hard_limit)?;

    Ok(())
}
