use crate::FileSystem;
use crate::args::wasm::WasmArgs;
use crate::bail;
use crate::output_section_id::OutputSectionId;
use crate::part_id::PartId;
use crate::platform::Args as _;

pub(crate) mod abi;
pub(crate) mod file;
pub(crate) mod gc;
pub(crate) mod linking;
pub(crate) mod output;
pub(crate) mod relocations;
pub(crate) mod symbols;

#[allow(unused_imports)]
pub(crate) use abi::*;
pub(crate) use file::*;
#[allow(unused_imports)]
pub(crate) use gc::*;
#[allow(unused_imports)]
pub(crate) use linking::*;
pub(crate) use output::*;
pub(crate) use relocations::*;
pub(crate) use symbols::*;

#[derive(Debug, Copy, Clone, Default)]
pub(crate) struct Wasm;

impl crate::layout::EnginePlatform for Wasm {}
impl<'data, 'scope> crate::layout::EngineScope<'data, 'scope> for Wasm where 'data: 'scope {}
impl<'writer, 'out> crate::layout::EngineWriter<'writer, 'out> for Wasm where 'out: 'writer {}

pub(crate) fn link_for_arch<'data, F: FileSystem>(
    linker: &'data crate::Linker<F>,
    args: &'data WasmArgs,
) -> crate::error::Result<crate::LinkerOutput<'data>> {
    if !(cfg!(feature = "wasm") || args.common().experimental_platforms) {
        bail!("Wasm support is still experimental. Rebuild with `--features wasm` to enable it.");
    }

    linker.link_for_arch::<Wasm, crate::wasm_wasm32::WasmWasm32>(args)
}

#[repr(u32)]
#[derive(Clone, Copy)]
pub(crate) enum SinglePartSectionId {
    WasmType = crate::output_section_id::NUM_COMMON_SINGLE_PART_SECTIONS,
    WasmImport,
    WasmFunction,
    WasmTable,
    WasmMemory,
    WasmGlobal,
    WasmExport,
    WasmStart,
    WasmElement,
    WasmDataCount,
    WasmCode,
    WasmData,
    WasmName,
    WasmTargetFeatures,

    // Must be last.
    Count,
}

pub(crate) mod part_id {
    use super::SinglePartSectionId;
    use crate::part_id::PartId;

    pub(crate) const WASM_TYPE: PartId = SinglePartSectionId::WasmType.part_id();
    pub(crate) const WASM_IMPORT: PartId = SinglePartSectionId::WasmImport.part_id();
    pub(crate) const WASM_FUNCTION: PartId = SinglePartSectionId::WasmFunction.part_id();
    pub(crate) const WASM_TABLE: PartId = SinglePartSectionId::WasmTable.part_id();
    pub(crate) const WASM_MEMORY: PartId = SinglePartSectionId::WasmMemory.part_id();
    pub(crate) const WASM_GLOBAL: PartId = SinglePartSectionId::WasmGlobal.part_id();
    pub(crate) const WASM_EXPORT: PartId = SinglePartSectionId::WasmExport.part_id();
    // TODO(wasm): Implement start-section emission.
    #[expect(dead_code)]
    pub(crate) const WASM_START: PartId = SinglePartSectionId::WasmStart.part_id();
    pub(crate) const WASM_ELEMENT: PartId = SinglePartSectionId::WasmElement.part_id();
    // TODO(wasm): Implement data-count emission.
    #[expect(dead_code)]
    pub(crate) const WASM_DATA_COUNT: PartId = SinglePartSectionId::WasmDataCount.part_id();
    pub(crate) const WASM_CODE: PartId = SinglePartSectionId::WasmCode.part_id();
    pub(crate) const WASM_DATA: PartId = SinglePartSectionId::WasmData.part_id();
    pub(crate) const WASM_NAME: PartId = SinglePartSectionId::WasmName.part_id();
    pub(crate) const WASM_TARGET_FEATURES: PartId =
        SinglePartSectionId::WasmTargetFeatures.part_id();
}

pub(crate) mod output_section_id {
    use super::SinglePartSectionId;
    use crate::output_section_id::OutputSectionId;

    pub(crate) const WASM_TYPE: OutputSectionId = SinglePartSectionId::WasmType.output_section_id();
    pub(crate) const WASM_IMPORT: OutputSectionId =
        SinglePartSectionId::WasmImport.output_section_id();
    pub(crate) const WASM_FUNCTION: OutputSectionId =
        SinglePartSectionId::WasmFunction.output_section_id();
    pub(crate) const WASM_TABLE: OutputSectionId =
        SinglePartSectionId::WasmTable.output_section_id();
    pub(crate) const WASM_MEMORY: OutputSectionId =
        SinglePartSectionId::WasmMemory.output_section_id();
    pub(crate) const WASM_GLOBAL: OutputSectionId =
        SinglePartSectionId::WasmGlobal.output_section_id();
    pub(crate) const WASM_EXPORT: OutputSectionId =
        SinglePartSectionId::WasmExport.output_section_id();
    pub(crate) const WASM_START: OutputSectionId =
        SinglePartSectionId::WasmStart.output_section_id();
    pub(crate) const WASM_ELEMENT: OutputSectionId =
        SinglePartSectionId::WasmElement.output_section_id();
    pub(crate) const WASM_DATA_COUNT: OutputSectionId =
        SinglePartSectionId::WasmDataCount.output_section_id();
    pub(crate) const WASM_CODE: OutputSectionId = SinglePartSectionId::WasmCode.output_section_id();
    pub(crate) const WASM_DATA: OutputSectionId = SinglePartSectionId::WasmData.output_section_id();
    pub(crate) const WASM_NAME: OutputSectionId = SinglePartSectionId::WasmName.output_section_id();
    pub(crate) const WASM_TARGET_FEATURES: OutputSectionId =
        SinglePartSectionId::WasmTargetFeatures.output_section_id();
}

/// Magic bytes at the start of every Wasm module.
pub(crate) const WASM_MAGIC: [u8; 4] = [0x00, b'a', b's', b'm'];

/// Supported Wasm binary format version.
pub(crate) const WASM_VERSION: u32 = 1;

pub(crate) mod section_id {
    pub(crate) const TYPE: u8 = 1;
    pub(crate) const IMPORT: u8 = 2;
    pub(crate) const FUNCTION: u8 = 3;
    pub(crate) const TABLE: u8 = 4;
    pub(crate) const MEMORY: u8 = 5;
    pub(crate) const GLOBAL: u8 = 6;
    pub(crate) const EXPORT: u8 = 7;
    pub(crate) const START: u8 = 8;
    pub(crate) const ELEMENT: u8 = 9;
    pub(crate) const CODE: u8 = 10;
    pub(crate) const DATA: u8 = 11;
    pub(crate) const DATA_COUNT: u8 = 12;
    pub(crate) const MAX: u8 = DATA_COUNT;
}

/// Size of a `[Option<u32>; _]` lookup that can be indexed by any standard section id.
pub(crate) const STANDARD_SECTION_LOOKUP_LEN: usize = section_id::MAX as usize + 1;

/// Default `__table_base` for non-PIC executables.
pub(crate) const DEFAULT_TABLE_BASE: u32 = 1;

/// The custom-section name used for the linker metadata.
pub(crate) const LINKING_SECTION_NAME: &str = "linking";

/// The prefix of every `reloc.*` custom section.
pub(crate) const RELOC_SECTION_PREFIX: &str = "reloc.";

/// The custom-section name used for the WebAssembly target features.
pub(crate) const TARGET_FEATURES_SECTION_NAME: &str = "target_features";

/// Feature is used by this object (`+` in the target_features section).
pub(crate) const TARGET_FEATURE_PREFIX_USED: u8 = b'+';
/// Feature must not appear in the output (`-` in the target_features section).
pub(crate) const TARGET_FEATURE_PREFIX_DISALLOWED: u8 = b'-';

/// Default static data base for linker-produced executables.
pub(crate) const LINKER_MEMORY_BASE: u32 = 1024;

/// Empty function body: zero locals + `end`.
pub(crate) const EMPTY_FUNCTION_BODY: &[u8] = &[0x00, 0x0b];

/// Undefined weak function stubs.
pub(crate) const UNREACHABLE_FUNCTION_BODY: &[u8] = &[0x00, 0x00, 0x0b];

/// `i32.const` body for `LINKER_MEMORY_BASE`.
pub(crate) const LINKER_MEMORY_BASE_INIT_EXPR: &[u8] = &[0x41, 0x80, 0x08];

/// `i32.const 0`. Used for immutable `__tls_base` when no TLS segment is laid out.
pub(crate) const ZERO_I32_INIT_EXPR: &[u8] = &[0x41, 0x00];

/// `i32.const 1` for `DEFAULT_TABLE_BASE`.
pub(crate) const DEFAULT_TABLE_BASE_INIT_EXPR: &[u8] = &[0x41, 0x01];

/// Sentinel for a GC'd Wasm index slot.
pub(crate) const WASM_DEAD_INDEX: u32 = u32::MAX;

impl SinglePartSectionId {
    const fn part_id(self) -> PartId {
        PartId::from_u32(self as u32)
    }

    const fn output_section_id(self) -> OutputSectionId {
        OutputSectionId::from_u32(self as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::wasm::DEFAULT_STACK_SIZE;
    use wasmparser::MemoryType;

    fn layout_input_with_features<'data>(
        file: u32,
        features: &'data [WasmTargetFeature<'data>],
    ) -> WasmObjectLayoutInput<'data> {
        WasmObjectLayoutInput {
            data: &[],
            types: Vec::new(),
            function_imports: Vec::new(),
            global_imports: Vec::new(),
            live_function_imports: Vec::new(),
            live_global_imports: Vec::new(),
            memory_imports: Vec::new(),
            table_imports: Vec::new(),
            module_functions: Vec::new(),
            globals: Vec::new(),
            exports: Vec::new(),
            function_bodies: Vec::new(),
            memories: Vec::new(),
            unsupported_output: Vec::new(),
            code_relocations: Vec::new(),
            data_segments: Vec::new(),
            data_segment_original_indices: Vec::new(),
            segment_alignments: &[],
            data_relocations: Vec::new(),
            symbols: &[],
            init_funcs: &[],
            target_features: features,
            symbol_id_range: crate::symbol_db::SymbolIdRange::empty(),
            file_id: crate::input_data::FileId::new(0, file),
            defined_function_live_ordinal: Vec::new(),
            defined_global_live_ordinal: Vec::new(),
        }
    }

    fn emitted_feature_names(section: &wasm_encoder::CustomSection<'_>) -> Vec<String> {
        let parsed = parse_target_features_payload(section.data.as_ref()).unwrap();
        assert!(
            parsed
                .iter()
                .all(|f| f.prefix == TARGET_FEATURE_PREFIX_USED),
            "output must only contain used (+) prefixes"
        );
        parsed.iter().map(|f| f.name.to_owned()).collect()
    }

    #[test]
    fn target_features_deduplicates_used_features_across_objects() {
        // Both objects use sign-ext. Only the first also uses bulk-memory.
        let features_a = [
            WasmTargetFeature {
                prefix: TARGET_FEATURE_PREFIX_USED,
                name: "sign-ext",
            },
            WasmTargetFeature {
                prefix: TARGET_FEATURE_PREFIX_USED,
                name: "bulk-memory",
            },
            WasmTargetFeature {
                prefix: TARGET_FEATURE_PREFIX_USED,
                name: "sign-ext",
            },
        ];
        let features_b = [WasmTargetFeature {
            prefix: TARGET_FEATURE_PREFIX_USED,
            name: "sign-ext",
        }];
        let inputs = [
            layout_input_with_features(1, &features_a),
            layout_input_with_features(2, &features_b),
        ];
        let section = build_target_features_section(&inputs, &[])
            .unwrap()
            .expect("expected target_features section");
        assert_eq!(emitted_feature_names(&section), ["bulk-memory", "sign-ext"]);
    }

    #[test]
    fn target_features_errors_when_used_and_disallowed_conflict() {
        let used = [WasmTargetFeature {
            prefix: TARGET_FEATURE_PREFIX_USED,
            name: "atomics",
        }];
        let disallowed = [WasmTargetFeature {
            prefix: TARGET_FEATURE_PREFIX_DISALLOWED,
            name: "atomics",
        }];
        let inputs = [
            layout_input_with_features(1, &used),
            layout_input_with_features(2, &disallowed),
        ];
        let err = build_target_features_section(&inputs, &[]).unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("atomics") && msg.contains("disallowed"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn parse_target_features_payload_used_and_disallowed() {
        // count=2, +bulk-memory, -atomics
        let payload: &[u8] = &[
            2, b'+', 11, b'b', b'u', b'l', b'k', b'-', b'm', b'e', b'm', b'o', b'r', b'y', b'-', 7,
            b'a', b't', b'o', b'm', b'i', b'c', b's',
        ];
        let features = parse_target_features_payload(payload).unwrap();
        assert_eq!(features.len(), 2);
        assert_eq!(features[0].prefix, TARGET_FEATURE_PREFIX_USED);
        assert_eq!(features[0].name, "bulk-memory");
        assert_eq!(features[1].prefix, TARGET_FEATURE_PREFIX_DISALLOWED);
        assert_eq!(features[1].name, "atomics");
    }

    #[test]
    fn linker_defined_data_symbol_addresses() {
        let data_start = 1024u32;
        let data_end = 1024u32;
        let page = wasm_page_size();
        let heap_end = heap_end_from_initial_pages(2).unwrap();
        let de = WasmLinkerSymbol::DataEnd
            .data_address(
                data_start,
                data_end,
                DEFAULT_STACK_SIZE,
                Some(heap_end),
                false,
            )
            .unwrap()
            .expect("__data_end");
        assert_eq!(de, data_end);

        let gb = WasmLinkerSymbol::GlobalBase
            .data_address(
                data_start,
                data_end,
                DEFAULT_STACK_SIZE,
                Some(heap_end),
                false,
            )
            .unwrap()
            .expect("__global_base");
        assert_eq!(gb, data_start);

        let dso = WasmLinkerSymbol::DsoHandle
            .data_address(
                data_start,
                data_end,
                DEFAULT_STACK_SIZE,
                Some(heap_end),
                false,
            )
            .unwrap()
            .expect("__dso_handle");
        assert_eq!(dso, data_start);

        let hb = WasmLinkerSymbol::HeapBase
            .data_address(
                data_start,
                data_end,
                DEFAULT_STACK_SIZE,
                Some(heap_end),
                false,
            )
            .unwrap()
            .expect("__heap_base");
        assert_eq!(
            hb,
            stack_high_after_data(data_end, DEFAULT_STACK_SIZE).unwrap()
        );

        let page_end = WasmLinkerSymbol::WasmFirstPageEnd
            .data_address(
                data_start,
                data_end,
                DEFAULT_STACK_SIZE,
                Some(heap_end),
                false,
            )
            .unwrap()
            .expect("__wasm_first_page_end");
        assert_eq!(u64::from(page_end), page);

        let he = WasmLinkerSymbol::HeapEnd
            .data_address(
                data_start,
                data_end,
                DEFAULT_STACK_SIZE,
                Some(heap_end),
                false,
            )
            .unwrap()
            .expect("__heap_end");
        assert_eq!(he, heap_end);
        assert!(he >= hb);
        assert_eq!(u64::from(he) % page, 0);

        // If there is no output memory, `__heap_end` is not synthesised.
        assert!(
            WasmLinkerSymbol::HeapEnd
                .data_address(data_start, data_end, DEFAULT_STACK_SIZE, None, false)
                .unwrap()
                .is_none()
        );
        assert!(
            WasmLinkerSymbol::WasmFirstPageEnd
                .data_address(data_start, data_end, DEFAULT_STACK_SIZE, None, false)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn stack_first_heap_base_follows_data_not_stack() {
        let data_start = 1_048_576u32;
        let data_end = 1_048_576 + 100;
        let stack_size = 1_048_576u32;
        let hb = WasmLinkerSymbol::HeapBase
            .data_address(data_start, data_end, stack_size, Some(2 * 65_536), true)
            .unwrap()
            .expect("__heap_base");
        assert_eq!(hb, heap_base_after_data(data_end).unwrap());
        assert!(hb - data_end < 16);
        let gb = WasmLinkerSymbol::GlobalBase
            .data_address(data_start, data_end, stack_size, Some(2 * 65_536), true)
            .unwrap()
            .expect("__global_base");
        assert_eq!(gb, data_start);
        let dso = WasmLinkerSymbol::DsoHandle
            .data_address(data_start, data_end, stack_size, Some(2 * 65_536), true)
            .unwrap()
            .expect("__dso_handle");
        assert_eq!(dso, data_start);
        // Without stack-first, heap would be roughly data_end + stack_size.
        let post_data = stack_high_after_data(data_end, stack_size).unwrap();
        assert!(post_data > hb + stack_size / 2);
    }

    #[test]
    fn stack_pointer_init_stack_first_is_stack_size() {
        let sp = stack_pointer_init(2_000_000, 1_048_576, true).unwrap();
        assert_eq!(sp, 1_048_576);
        let sp = stack_pointer_init(1024, DEFAULT_STACK_SIZE, false).unwrap();
        assert_eq!(sp, stack_high_after_data(1024, DEFAULT_STACK_SIZE).unwrap());
    }

    #[test]
    fn unaligned_stack_size_is_rejected() {
        assert!(ensure_stack_size_aligned(1000).is_err());
        assert!(ensure_stack_size_aligned(1024).is_ok());
        assert!(stack_pointer_init(0, 1000, true).is_err());
    }

    #[test]
    fn stack_high_is_sixteen_byte_aligned() {
        // Unaligned data_end must still yield a 16-byte-aligned stack top (wasm-ld).
        for data_end in [1u32, 2, 7, 1025, 4738] {
            let sp = stack_high_after_data(data_end, DEFAULT_STACK_SIZE).unwrap();
            assert_eq!(sp % 16, 0, "data_end={data_end} sp={sp}");
            assert!(sp >= data_end + DEFAULT_STACK_SIZE);
        }
    }

    #[test]
    fn ensure_memory_covers_stack_and_matches_heap_end() {
        let data_end = 1024u32;
        let mut layout = WasmLayout {
            data_end,
            memories: vec![MemoryType {
                memory64: false,
                shared: false,
                initial: 1,
                maximum: None,
                page_size_log2: None,
            }],
            ..Default::default()
        };

        let pages =
            ensure_memory_covers(&mut layout, DEFAULT_STACK_SIZE, true, None, None, false).unwrap();
        assert_eq!(pages, 1);
        assert_eq!(layout.memories[0].initial, 1);
        assert_eq!(layout.memories[0].maximum, None);
        assert!(!layout.memories[0].shared);

        let pages = ensure_memory_covers(&mut layout, DEFAULT_STACK_SIZE, false, None, None, false)
            .unwrap();
        let expected_pages = (u64::from(data_end) + u64::from(DEFAULT_STACK_SIZE))
            .div_ceil(wasm_page_size())
            .max(1);
        assert_eq!(pages, expected_pages);
        assert_eq!(layout.memories[0].initial, expected_pages);
        assert!(expected_pages > 1);
        assert_eq!(
            heap_end_from_initial_pages(pages).unwrap(),
            u32::try_from(pages * wasm_page_size()).unwrap()
        );
    }
}
