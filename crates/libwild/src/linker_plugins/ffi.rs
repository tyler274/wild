use super::*;
use crate::bail;
use crate::elf::Elf;
use crate::elf::ElfClass;
use crate::elf::RawSymbolName;
use crate::error;
use crate::error::Context as _;
use crate::error::Error;
use crate::error::Result;
use crate::input_data::FileId;
use crate::input_data::InputRef;
use crate::layout::EnginePlatform;
use crate::platform::Platform;
use crate::resolution::ResolvedGroup;
use crate::symbol::UnversionedSymbolName;
use crate::symbol_db::SymbolDb;
use crate::symbol_db::SymbolId;
use crate::symbol_db::SymbolIdRange;
use crate::value_flags::FlagsForSymbol;
use crate::value_flags::PerSymbolFlags;
use crate::value_flags::ValueFlags;
use bumpalo_herd::Herd;
use std::cell::Cell;
use std::cell::RefCell;
use std::ffi::CStr;
use std::ffi::CString;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::panic::AssertUnwindSafe;
use std::path::Path;

/// Checks for any errors reported by the linker plugin during a callback. Should be called after
/// each callback.
pub(crate) fn check_for_errors() -> Result {
    if let Some(message) = ERROR_MESSAGE.take() {
        bail!("Error from linker plugin: {message}");
    }
    Ok(())
}

pub(crate) type ClaimFileHook =
    unsafe extern "C" fn(*const LdPluginInputFile, *mut libc::c_int) -> Status;
pub(crate) type CleanupHook = extern "C" fn() -> Status;
pub(crate) type AllSymbolsReadHook = extern "C" fn() -> Status;
pub(crate) type GetSymbols =
    extern "C" fn(*const libc::c_void, libc::c_int, *mut RawPluginSymbol) -> Status;

#[derive(Default)]
pub(crate) struct Callbacks {
    pub(crate) claim_file_hook: Option<ClaimFileHook>,
    pub(crate) cleanup_hook: Option<CleanupHook>,
    pub(crate) all_symbols_read: Option<AllSymbolsReadHook>,
}

pub(crate) struct VersionInfo {
    identifier: Vec<u8>,
    version: Vec<u8>,
}

// Some APIs don't let us pass along a pointer to our own data, so we need to store our state in
// thread-locals.
thread_local! {
    pub(crate) static CALLBACKS: RefCell<Callbacks> = const { RefCell::new(Callbacks::new()) };
    pub(crate) static VERSION_INFO: Cell<Option<VersionInfo>> = const { Cell::new(None) };
    pub(crate) static PLUGIN_OUTPUTS: RefCell<PluginOutputs> = const { RefCell::new(PluginOutputs::new()) };
    pub(crate) static ERROR: RefCell<Option<Error>>  = const { RefCell::new(None) };
    pub(crate) static ERROR_MESSAGE: RefCell<Option<String>> = const { RefCell::new(None) };

    // Holds a ClaimContext. We store this as a void pointer since the actual type has non-static
    // lifetimes that we wouldn't be able to store here.
    pub(crate) static CLAIM_CONTEXT: Cell<*mut libc::c_void> = const { Cell::new(std::ptr::null_mut()) };

    // Same thing, but this one holds an AllSymbolsReadContext.
    pub(crate) static ALL_SYMBOLS_READ_CONTEXT: Cell<*const libc::c_void> = const { Cell::new(std::ptr::null()) };
}

#[repr(C)]
pub(crate) struct LdPluginTv {
    /// Obtained from casting a `Tag`.
    pub(crate) tag: u32,

    /// This is either a pointer or a numeric value depending on the tag.
    pub(crate) value: usize,
}

impl LdPluginTv {
    pub(crate) fn value(tag: Tag, value: usize) -> Self {
        Self {
            tag: tag as u32,
            value,
        }
    }

    pub(crate) fn c_str(tag: Tag, value: &CStr) -> Self {
        Self {
            tag: tag as u32,
            value: value.as_ptr() as usize,
        }
    }

    pub(crate) fn fn_ptr0<RET>(tag: Tag, value: extern "C" fn() -> RET) -> Self {
        Self {
            tag: tag as u32,
            value: value as *const fn() -> RET as usize,
        }
    }

    pub(crate) fn fn_ptr1<P1, RET>(tag: Tag, value: extern "C" fn(P1) -> RET) -> Self {
        Self {
            tag: tag as u32,
            value: value as *const fn(P1) -> RET as usize,
        }
    }

    pub(crate) fn fn_ptr2<P1, P2, RET>(tag: Tag, value: extern "C" fn(P1, P2) -> RET) -> Self {
        Self {
            tag: tag as u32,
            value: value as *const fn(P1, P2) -> RET as usize,
        }
    }

    pub(crate) fn fn_ptr3<P1, P2, P3, RET>(
        tag: Tag,
        value: extern "C" fn(P1, P2, P3) -> RET,
    ) -> Self {
        Self {
            tag: tag as u32,
            value: value as *const fn(P1, P2, P3) -> RET as usize,
        }
    }

    pub(crate) fn fn_ptr6<P1, P2, P3, P4, P5, P6, RET>(
        tag: Tag,
        value: extern "C" fn(P1, P2, P3, P4, P5, P6) -> RET,
    ) -> Self {
        Self {
            tag: tag as u32,
            value: value as *const fn(P1, P2, P3, P4, P5, P6) -> RET as usize,
        }
    }
}

#[allow(dead_code)]
pub(crate) enum Tag {
    Null = 0,
    ApiVersion = 1,
    GoldVersion = 2,
    LinkerOutput = 3,
    Option = 4,
    RegisterClaimFileHook = 5,
    RegisterAllSymbolsReadHook = 6,
    RegisterCleanupHook = 7,
    AddSymbols = 8,
    GetSymbols = 9,
    AddInputFile = 10,
    Message = 11,
    GetInputFile = 12,
    ReleaseInputFile = 13,
    AddInputLibrary = 14,
    OutputName = 15,
    SetExtraLibraryPath = 16,
    GnuLdVersion = 17,
    GetView = 18,
    GetInputSectionCount = 19,
    GetInputSectionType = 20,
    GetInputSectionName = 21,
    GetInputSectionContents = 22,
    UpdateSectionOrder = 23,
    AllowSectionOrdering = 24,
    GetSymbolsV2 = 25,
    AllowUniqueSegmentForSections = 26,
    UniqueSegmentForSections = 27,
    GetSymbolsV3 = 28,
    GetInputSectionAlignment = 29,
    GetInputSectionSize = 30,
    RegisterNewInputHook = 31,
    GetWrapSymbols = 32,
    AddSymbolsV2 = 33,
    GetApiVersion = 34,
    RegisterClaimFileHookV2 = 35,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MessageLevel {
    Info = 0,
    Warning = 1,
    Error = 2,
    Fatal = 3,
}

impl MessageLevel {
    pub(crate) fn from_raw(level: i32) -> Option<Self> {
        match level {
            0 => Some(MessageLevel::Info),
            1 => Some(MessageLevel::Warning),
            2 => Some(MessageLevel::Error),
            3 => Some(MessageLevel::Fatal),
            _ => None,
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, Copy)]
#[repr(C)]
pub(crate) enum Status {
    Ok = 0,
    NoSyms,
    BadHandle,
    Err,
}

#[allow(dead_code)]
pub(crate) enum OutputFileType {
    Rel = 0,
    Exec = 1,
    Dyn = 2,
    Pie = 3,
}

#[repr(C)]
pub(crate) struct LdPluginInputFile {
    pub(crate) name: *const libc::c_char,
    pub(crate) fd: libc::c_int,
    pub(crate) offset: libc::off_t,
    pub(crate) file_size: libc::off_t,
    pub(crate) handle: *mut libc::c_void,
}

#[allow(dead_code)]
#[derive(Debug)]
pub(crate) enum PluginSymbolResolution {
    Unknown = 0,
    Undef,
    PrevailingDef,
    PrevailingDefIronly,
    PreemptedReg,
    PreemptedIr,
    ResolvedIr,
    ResolvedExec,
    ResolvedDyn,
    PrevailingDefIronlyExp,
}

#[derive(Debug)]
pub(crate) struct PluginSymbol<'data> {
    pub(crate) name: UnversionedSymbolName<'data>,
    pub(crate) version: Option<&'data [u8]>,
    pub(crate) visibility: u8,
    pub(crate) kind: Option<SymbolKind>,
    pub(crate) size: u64,
}

#[repr(C)]
pub(crate) struct RawPluginSymbol {
    name: *const libc::c_char,
    version: *const libc::c_char,
    def: libc::c_char,
    symbol_type: libc::c_char,
    section_kind: libc::c_char,
    unused: libc::c_char,
    visibility: libc::c_int,
    size: u64,
    comdat_key: *const libc::c_char,
    resolution: libc::c_int,
}

unsafe impl Sync for RawPluginSymbol {}
unsafe impl Send for RawPluginSymbol {}

pub(crate) extern "C" fn register_claim_file_hook(cb: ClaimFileHook) -> Status {
    CALLBACKS.with_borrow_mut(|c| c.claim_file_hook = Some(cb));
    Status::Ok
}

pub(crate) extern "C" fn register_cleanup_hook(cb: CleanupHook) -> Status {
    CALLBACKS.with_borrow_mut(|c| c.cleanup_hook = Some(cb));
    Status::Ok
}

pub(crate) extern "C" fn register_all_symbols_read_hook(cb: AllSymbolsReadHook) -> Status {
    CALLBACKS.with_borrow_mut(|c| c.all_symbols_read = Some(cb));
    Status::Ok
}

pub(crate) extern "C" fn get_api_version(
    plugin_identifier: *const libc::c_char,
    plugin_version: *const libc::c_char,
    _minimal_version: libc::c_int,
    _maximal_version: libc::c_int,
    _linker_identifier: *mut *const libc::c_char,
    _linker_version: *mut *const libc::c_char,
) -> libc::c_int {
    if !plugin_identifier.is_null() && !plugin_version.is_null() {
        let identifier = unsafe { CStr::from_ptr(plugin_identifier) };
        let version = unsafe { CStr::from_ptr(plugin_version) };
        let version_info = VersionInfo {
            identifier: identifier.to_bytes().to_owned(),
            version: version.to_bytes().to_owned(),
        };
        VERSION_INFO.replace(Some(version_info));
    }

    API_VERSION as libc::c_int
}

pub(crate) extern "C" fn unsupported_api_version() -> Status {
    ERROR_MESSAGE.replace(Some(
        "Compiler plugin uses an older, unsupported version of the API".to_owned(),
    ));
    Status::Err
}

pub(crate) extern "C" fn add_symbols(
    _handle: *const libc::c_void,
    num_symbols: libc::c_int,
    symbols: *const RawPluginSymbol,
) -> Status {
    catch_panics(|| {
        ClaimContext::with_current(|ctx| {
            let raw_symbols = unsafe { std::slice::from_raw_parts(symbols, num_symbols as usize) };

            // Unfortunately we need to copy the symbol info that the plugin gives us because it
            // doesn't keep it alive for long enough.
            let arena = ctx.herd.get();
            ctx.symbols = raw_symbols
                .iter()
                .map(|sym| PluginSymbol {
                    name: UnversionedSymbolName::new(
                        arena.alloc_slice_copy(unsafe { CStr::from_ptr(sym.name) }.to_bytes()),
                    ),
                    version: sym.version.is_null().not().then(|| {
                        &*arena.alloc_slice_copy(unsafe { CStr::from_ptr(sym.version) }.to_bytes())
                    }),
                    kind: sym.kind(),
                    visibility: sym.visibility as u8,
                    size: sym.size,
                })
                .collect();

            Status::Ok
        })
    })
}

pub(crate) extern "C" fn get_symbols_v3<C: ElfClass>(
    handle: *const libc::c_void,
    num_symbols: libc::c_int,
    symbols: *mut RawPluginSymbol,
) -> Status {
    catch_panics(|| {
        AllSymbolsReadContext::<Elf<C>>::with_current(|ctx| {
            let handle = unsafe { &*handle.cast::<FileHandle>() };

            let Some(file_id) = handle.file_id.load() else {
                panic!("get_symbols_v3 called without first supplying FileId");
            };
            let ResolvedFile::LtoInput(file) =
                &ctx.resolved_groups[file_id.group()].files[file_id.file()]
            else {
                // An archive entry that we decided not to load.
                return Status::NoSyms;
            };

            if num_symbols == 0 {
                return Status::Ok;
            }

            let symbols = unsafe { std::slice::from_raw_parts_mut(symbols, num_symbols as usize) };

            let symbol_id_range = file.symbol_id_range;

            for sym in symbols.iter_mut() {
                let resolution = get_symbol_resolution(
                    sym,
                    ctx.symbol_db,
                    symbol_id_range,
                    ctx.per_symbol_flags,
                );

                sym.resolution = resolution as i32;
            }

            Status::Ok
        })
    })
}

pub(crate) fn get_symbol_resolution<'data, C: ElfClass>(
    sym: &mut RawPluginSymbol,
    symbol_db: &SymbolDb<'data, Elf<C>>,
    symbol_id_range: SymbolIdRange,
    per_symbol_flags: &PerSymbolFlags,
) -> PluginSymbolResolution {
    // It'd be nice if we didn't have to do hashmap lookups for all the symbols again, since we
    // effectively did that when the symbols were added. We could do that if the plugin provided us
    // with the index of the symbol in the list of symbols that it passed to add_symbols, but
    // unfortunately it doesn't give us that information.
    let name = unsafe { CStr::from_ptr(sym.name) }.to_bytes();
    let mut raw_name = RawSymbolName::parse(name);
    if !sym.version.is_null() {
        raw_name.version_name = Some(unsafe { CStr::from_ptr(sym.version) }.to_bytes());
    }

    // If the symbol was wrapped via --wrap, the name table has been modified to map the original
    // name to __wrap_<name>. We must not report the wrapped resolution to the plugin, since the
    // plugin needs to know the pre-wrap state.
    let wrap_names = symbol_db.args.symbol_names_to_wrap();
    let is_wrapped = wrap_names.iter().any(|w| w.as_bytes() == raw_name.name);

    let symbol_id = if is_wrapped {
        let real_name = format!("__real_{}", String::from_utf8_lossy(raw_name.name));
        symbol_db
            .get_unversioned(&UnversionedSymbolName::prehashed(real_name.as_bytes()))
            .map(|id| symbol_db.definition(id))
    } else {
        symbol_db
            .get(&crate::symbol::symbol_name_from_raw(&raw_name), true)
            .map(|id| symbol_db.definition(id))
    };

    let Some(symbol_id) = symbol_id else {
        return PluginSymbolResolution::Undef;
    };

    if symbol_id.is_undefined() {
        PluginSymbolResolution::Undef
    } else if sym.is_undefined() {
        // Wrapped symbols are always reported as resolved outside IR.
        if is_wrapped {
            return PluginSymbolResolution::ResolvedExec;
        }

        let defining_file = symbol_db.file(symbol_db.file_id_for_symbol(symbol_id));

        match defining_file {
            crate::grouping::SequencedInput::LtoInput(_) => PluginSymbolResolution::ResolvedIr,
            crate::grouping::SequencedInput::Object(obj) => {
                if obj.is_dynamic() {
                    PluginSymbolResolution::ResolvedDyn
                } else {
                    PluginSymbolResolution::ResolvedExec
                }
            }
            _ => PluginSymbolResolution::ResolvedExec,
        }
    } else if symbol_id_range.contains(symbol_id) {
        let flags = per_symbol_flags.flags_for_symbol(symbol_id);

        if flags.contains(ValueFlags::HAS_NON_IR_REF) {
            PluginSymbolResolution::PrevailingDef
        } else if flags.contains(ValueFlags::EXPORT_DYNAMIC) {
            PluginSymbolResolution::PrevailingDefIronlyExp
        } else {
            PluginSymbolResolution::PrevailingDefIronly
        }
    } else if is_wrapped {
        // Wrapped symbols use the regular (non-IR) preemption codes.
        PluginSymbolResolution::PreemptedReg
    } else {
        let defining_file = symbol_db.file(symbol_db.file_id_for_symbol(symbol_id));
        match defining_file {
            crate::grouping::SequencedInput::LtoInput(_) => PluginSymbolResolution::PreemptedIr,
            _ => PluginSymbolResolution::PreemptedReg,
        }
    }
}

pub(crate) extern "C" fn get_input_file(
    handle: *const libc::c_void,
    file: *mut LdPluginInputFile,
) -> Status {
    catch_panics(|| {
        if handle.is_null() || file.is_null() {
            return Status::Err;
        }
        let handle = unsafe { &*handle.cast::<FileHandle>() };
        let file = unsafe { &mut *file };

        file.fd = handle.fd;
        file.offset = handle.offset as i64;
        file.file_size = handle.data.len() as i64;
        file.name = handle.name.as_ptr();

        Status::Ok
    })
}

pub(crate) extern "C" fn release_input_file(_handle: *const libc::c_void) -> Status {
    // We don't allocate in `get_input_file`, so there's nothing to free here.
    Status::Ok
}

pub(crate) extern "C" fn get_view(
    handle: *const libc::c_void,
    view_pointer: *mut *const libc::c_void,
) -> Status {
    catch_panics(|| {
        if handle.is_null() {
            return Status::Err;
        }
        let handle = unsafe { &*handle.cast::<FileHandle>() };
        unsafe { view_pointer.write(handle.data.as_ptr().cast::<libc::c_void>()) };
        Status::Ok
    })
}

pub(crate) extern "C" fn get_wrap_symbols(
    num_symbols: *mut u64,
    wrap_symbols_list: *mut *const *const libc::c_char,
) -> Status {
    catch_panics(|| {
        ClaimContext::with_current(|ctx| {
            unsafe {
                wrap_symbols_list.write(ctx.wrap_symbols.0.as_ptr());
                num_symbols.write(ctx.wrap_symbols.0.len() as u64);
            }
            Status::Ok
        })
    })
}

pub(crate) extern "C" fn add_input_file(path: *const libc::c_char) -> Status {
    catch_panics(|| {
        let path = unsafe { CStr::from_ptr(path) };
        let path = OsStr::from_bytes(path.to_bytes());
        let path = Box::from(Path::new(path));
        PLUGIN_OUTPUTS.with_borrow_mut(|state| {
            state.generated_inputs.push(Input {
                spec: crate::args::InputSpec::File(path),
                search_first: None,
                modifiers: Modifiers {
                    temporary: true,
                    ..Default::default()
                },
            });
        });
        Status::Ok
    })
}

pub(crate) extern "C" fn add_input_library(lib_name: *const libc::c_char) -> Status {
    let lib_name = unsafe { CStr::from_ptr(lib_name) };
    let Ok(lib_name) = lib_name.to_str() else {
        ERROR.replace(Some(error!(
            "Linker plugin added library name that wasn't valid UTF-8: `{}`",
            lib_name.to_string_lossy()
        )));
        return Status::Err;
    };

    PLUGIN_OUTPUTS.with_borrow_mut(|state| {
        state.generated_inputs.push(Input {
            spec: crate::args::InputSpec::Lib(Box::from(lib_name)),
            search_first: None,
            modifiers: Modifiers {
                as_needed: true,
                ..Default::default()
            },
        });
    });

    Status::Ok
}

unsafe extern "C" {
    /// C trampoline that accepts the plugin's printf-style varargs, formats them via vsnprintf,
    /// then calls `wild_handle_plugin_message` with the resulting string. Defined in
    /// `plugin_message_shim.c`.
    pub(crate) fn wild_plugin_message_callback(level: libc::c_int, fmt: *const libc::c_char, ...);
}

/// Called by the C shim `wild_plugin_message_callback` with the already-formatted message string.
/// The `no_mangle` is required so the C shim can link against it by name.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn wild_handle_plugin_message(
    level: libc::c_int,
    message: *const libc::c_char,
) {
    let Some(level) = MessageLevel::from_raw(level) else {
        return;
    };

    let text = unsafe { CStr::from_ptr(message) }.to_string_lossy();

    eprintln!("Linker plugin {level}: {text}");

    if level == MessageLevel::Error || level == MessageLevel::Fatal {
        ERROR_MESSAGE.replace(Some(text.into_owned()));
    }
}

/// Runs `body`, catching any panics. In the case of a panic, the return status is changed to an
/// error, otherwise the return status returned by `body` is passed through. This should be called
/// from all non-trivial hooks in order to ensure that we don't try to propagate a panic back into
/// the linker-plugin which would be undefined behaviour.
pub(crate) fn catch_panics(body: impl FnOnce() -> Status) -> Status {
    std::panic::catch_unwind(AssertUnwindSafe(body)).unwrap_or_else(|_| {
        ERROR_MESSAGE.replace(Some("Panic in plugin callback".to_owned()));
        Status::Err
    })
}

pub(crate) struct ClaimContext<'data> {
    pub(crate) symbols: Vec<PluginSymbol<'data>>,
    pub(crate) herd: &'data Herd,
    pub(crate) wrap_symbols: WrapSymbols<'data>,
}

impl ClaimContext<'_> {
    pub(crate) fn with_current(cb: impl FnOnce(&mut ClaimContext) -> Status) -> Status {
        let ptr = CLAIM_CONTEXT.get();
        if ptr.is_null() {
            ERROR_MESSAGE.set(Some("Tried to obtain ClaimContext when not set".to_owned()));
            return Status::Err;
        }
        let ctx = unsafe { &mut *ptr.cast::<ClaimContext>() };
        cb(ctx)
    }

    pub(crate) fn set_current_while<R>(&mut self, cb: impl FnOnce() -> R) -> R {
        CLAIM_CONTEXT.set(std::ptr::from_mut(self).cast::<libc::c_void>());
        let r = cb();
        CLAIM_CONTEXT.take();
        r
    }
}

pub(crate) struct AllSymbolsReadContext<'scope, 'data, P: Platform> {
    pub(crate) symbol_db: &'scope SymbolDb<'data, P>,
    pub(crate) resolved_groups: &'scope [ResolvedGroup<'data, P>],
    pub(crate) per_symbol_flags: &'scope PerSymbolFlags,
}

impl<'scope, 'data, P: EnginePlatform> AllSymbolsReadContext<'scope, 'data, P> {
    pub(crate) fn with_current(
        cb: impl FnOnce(&mut AllSymbolsReadContext<'scope, 'data, P>) -> Status,
    ) -> Status {
        let ptr = ALL_SYMBOLS_READ_CONTEXT.get();
        if ptr.is_null() {
            ERROR_MESSAGE.set(Some(
                "Tried to obtain AllSymbolsReadContext when not set".to_owned(),
            ));
            return Status::Err;
        }
        let ctx = unsafe { &mut *(ptr as *mut AllSymbolsReadContext<'scope, 'data, P>) };
        cb(ctx)
    }

    pub(crate) fn set_current_while<R>(&self, cb: impl FnOnce() -> R) -> R {
        ALL_SYMBOLS_READ_CONTEXT.set(
            std::ptr::from_ref::<AllSymbolsReadContext<'scope, 'data, P>>(self)
                .cast::<libc::c_void>(),
        );
        let r = cb();
        ALL_SYMBOLS_READ_CONTEXT.take();
        r
    }
}

impl Drop for LoadedPlugin {
    fn drop(&mut self) {
        let _ = self.with_callbacks(|callbacks| {
            if let Some(hook) = callbacks.cleanup_hook {
                hook();
            }
            Ok(())
        });
    }
}

impl Status {
    pub(crate) fn to_result(self, context: &str) -> Result {
        match self {
            Status::Ok => Ok(()),
            Status::NoSyms => bail!("{context}: NoSyms"),
            Status::BadHandle => bail!("{context}: BadHandle"),
            Status::Err => bail!("{context}: Err"),
        }
    }
}

impl RawPluginSymbol {
    pub(crate) fn kind(&self) -> Option<SymbolKind> {
        match self.def {
            0 => Some(SymbolKind::Def),
            1 => Some(SymbolKind::WeakDef),
            2 => Some(SymbolKind::Undef),
            3 => Some(SymbolKind::WeakUndef),
            4 => Some(SymbolKind::Common),
            _ => None,
        }
    }

    pub(crate) fn is_undefined(&self) -> bool {
        !self.kind().is_some_and(|kind| kind.is_definition())
    }
}

impl PluginSymbol<'_> {
    pub(crate) fn is_definition(&self) -> bool {
        self.kind.is_some_and(|kind| kind.is_definition())
    }
}

impl std::fmt::Display for MessageLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            MessageLevel::Info => "message",
            MessageLevel::Warning => "warning",
            MessageLevel::Error => "error",
            MessageLevel::Fatal => "fatal error",
        };
        std::fmt::Display::fmt(message, f)
    }
}

impl Callbacks {
    const fn new() -> Self {
        Self {
            claim_file_hook: None,
            cleanup_hook: None,
            all_symbols_read: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SymbolKind {
    Def = 0,
    WeakDef = 1,
    Undef = 2,
    WeakUndef = 3,
    Common = 4,
}

impl std::fmt::Display for VersionInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} version {}",
            String::from_utf8_lossy(&self.identifier),
            String::from_utf8_lossy(&self.version)
        )
    }
}

impl SymbolKind {
    pub(crate) fn is_definition(self) -> bool {
        matches!(
            self,
            SymbolKind::Def | SymbolKind::WeakDef | SymbolKind::Common
        )
    }
}
