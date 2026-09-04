use super::file::*;
use super::symbols::*;
use crate::bail;
use crate::ensure;
use crate::error::Result;
use std::ops::Range;
use wasmparser::BinaryReader;
use wasmparser::RelocationEntry;
use wasmparser::RelocationType;

#[derive(Debug, Clone)]
pub(crate) struct WasmRelocSection {
    /// Index (into [`File::sections`]) of the section that the relocations apply to.
    pub(crate) target_section_index: u32,
    /// Byte range of the section's contents (after the section name) within the module bytes.
    pub(crate) payload_range: Range<u32>,
}

impl WasmRelocSection {
    pub(crate) fn decode_entries(&self, data: &[u8]) -> Result<Vec<WasmRelocation>> {
        let payload = data
            .get(self.payload_range.start as usize..self.payload_range.end as usize)
            .ok_or_else(|| crate::error!("Wasm reloc section payload range out of bounds"))?;
        let reader = wasmparser::RelocSectionReader::new(BinaryReader::new(
            payload,
            u64::from(self.payload_range.start),
        ))?;
        reader
            .entries()
            .into_iter()
            .map(|entry| Ok(WasmRelocation::from_entry(entry?)))
            .collect()
    }
}

#[derive(Debug, Copy, Clone)]
pub(crate) struct WasmRelocation {
    /// Wasm relocation type.
    pub(crate) ty: RelocationType,
    /// Byte offset within the target section's payload.
    pub(crate) offset: u32,
    /// Symbol or type index.
    pub(crate) index: u32,
    pub(crate) addend: i64,
}

macro_rules! define_relocation_type_to_string {
    ($($variant:ident),* $(,)?) => {
        pub(crate) const fn relocation_type_to_string(ty: RelocationType) -> &'static str {
            match ty {
                $(RelocationType::$variant => stringify!($variant),)*
            }
        }
    };
}

define_relocation_type_to_string!(
    FunctionIndexLeb,
    TableIndexSleb,
    TableIndexI32,
    MemoryAddrLeb,
    MemoryAddrSleb,
    MemoryAddrI32,
    TypeIndexLeb,
    GlobalIndexLeb,
    FunctionOffsetI32,
    SectionOffsetI32,
    EventIndexLeb,
    MemoryAddrRelSleb,
    TableIndexRelSleb,
    GlobalIndexI32,
    MemoryAddrLeb64,
    MemoryAddrSleb64,
    MemoryAddrI64,
    MemoryAddrRelSleb64,
    TableIndexSleb64,
    TableIndexI64,
    TableNumberLeb,
    MemoryAddrTlsSleb,
    FunctionOffsetI64,
    MemoryAddrLocrelI32,
    TableIndexRelSleb64,
    MemoryAddrTlsSleb64,
    FunctionIndexI32,
);

impl WasmRelocation {
    pub(crate) fn from_entry(entry: RelocationEntry) -> Self {
        Self {
            ty: entry.ty,
            offset: entry.offset,
            index: entry.index,
            addend: entry.addend,
        }
    }

    /// Width in bytes of the slot this relocation overwrites.
    pub(crate) fn slot_size(&self) -> usize {
        match self.ty {
            RelocationType::FunctionIndexLeb
            | RelocationType::TableIndexSleb
            | RelocationType::TableIndexRelSleb
            | RelocationType::MemoryAddrLeb
            | RelocationType::MemoryAddrSleb
            | RelocationType::MemoryAddrRelSleb
            | RelocationType::TypeIndexLeb
            | RelocationType::GlobalIndexLeb
            | RelocationType::EventIndexLeb
            | RelocationType::TableNumberLeb => 5,
            RelocationType::TableIndexI32
            | RelocationType::MemoryAddrI32
            | RelocationType::FunctionOffsetI32
            | RelocationType::SectionOffsetI32
            | RelocationType::GlobalIndexI32
            | RelocationType::FunctionIndexI32 => 4,
            _ => 0,
        }
    }
}

/// Write `value` as an unsigned LEB128 into `buf`, returning the number of bytes written.
pub(crate) fn write_uleb128(buf: &mut [u8], value: u64) -> usize {
    let mut writable = &mut *buf;
    leb128::write::unsigned(&mut writable, value).unwrap()
}

/// Write `value` as a signed LEB128 into `buf`, returning the number of bytes written.
pub(crate) fn write_sleb128(buf: &mut [u8], value: i64) -> usize {
    let mut writable = &mut *buf;
    leb128::write::signed(&mut writable, value).unwrap()
}

/// Write `value` as a 5-byte fixed-width unsigned LEB128. Used for wasm reloc slots that reserve
/// exactly 5 bytes regardless of the encoded value.
pub(crate) fn write_uleb128_5(buf: &mut [u8; 5], value: u32) {
    buf[0] = (value as u8 & 0x7f) | 0x80;
    buf[1] = ((value >> 7) as u8 & 0x7f) | 0x80;
    buf[2] = ((value >> 14) as u8 & 0x7f) | 0x80;
    buf[3] = ((value >> 21) as u8 & 0x7f) | 0x80;
    buf[4] = (value >> 28) as u8 & 0x0f;
}

/// Write `value` as a 5-byte fixed-width signed LEB128. The high three bits of the final byte are
/// sign-extended so the encoded form is canonical for any `i32`.
pub(crate) fn write_sleb128_5(buf: &mut [u8; 5], value: i32) {
    let v = value as u32;
    buf[0] = (v as u8 & 0x7f) | 0x80;
    buf[1] = ((v >> 7) as u8 & 0x7f) | 0x80;
    buf[2] = ((v >> 14) as u8 & 0x7f) | 0x80;
    buf[3] = ((v >> 21) as u8 & 0x7f) | 0x80;
    let last = (v >> 28) as u8 & 0x0f;
    let sign_ext = if value < 0 { 0x70 } else { 0x00 };
    buf[4] = last | sign_ext;
}

pub(crate) fn apply_relocation(
    bytes: &mut [u8],
    reloc: &WasmRelocation,
    value: u32,
) -> crate::error::Result<()> {
    let offset = reloc.offset as usize;
    let size = reloc.slot_size();
    let end = offset
        .checked_add(size)
        .ok_or_else(|| crate::error!("Wasm relocation offset overflow"))?;
    let slot = bytes
        .get_mut(offset..end)
        .ok_or_else(|| crate::error!("Wasm relocation slot out of range"))?;
    match reloc.ty {
        RelocationType::FunctionIndexLeb
        | RelocationType::MemoryAddrLeb
        | RelocationType::TypeIndexLeb
        | RelocationType::GlobalIndexLeb
        | RelocationType::EventIndexLeb
        | RelocationType::TableNumberLeb => {
            let buf: &mut [u8; 5] = slot.try_into().expect("slot_size returned 5");
            write_uleb128_5(buf, value);
        }
        RelocationType::TableIndexSleb
        | RelocationType::TableIndexRelSleb
        | RelocationType::MemoryAddrSleb
        | RelocationType::MemoryAddrRelSleb => {
            let buf: &mut [u8; 5] = slot.try_into().expect("slot_size returned 5");
            write_sleb128_5(buf, value as i32);
        }
        RelocationType::TableIndexI32
        | RelocationType::MemoryAddrI32
        | RelocationType::FunctionOffsetI32
        | RelocationType::SectionOffsetI32
        | RelocationType::GlobalIndexI32
        | RelocationType::FunctionIndexI32 => {
            slot.copy_from_slice(&value.to_le_bytes());
        }
        other => bail!(
            "unsupported Wasm relocation type {}",
            relocation_type_to_string(other)
        ),
    }
    Ok(())
}

pub(crate) fn is_memory_addr_relocation(ty: RelocationType) -> bool {
    matches!(
        ty,
        RelocationType::MemoryAddrLeb
            | RelocationType::MemoryAddrSleb
            | RelocationType::MemoryAddrI32
            | RelocationType::MemoryAddrRelSleb
    )
}

pub(crate) fn is_supported_data_relocation(ty: RelocationType) -> bool {
    is_memory_addr_relocation(ty)
        || matches!(
            ty,
            RelocationType::FunctionIndexI32
                | RelocationType::FunctionIndexLeb
                | RelocationType::TableIndexI32
                | RelocationType::TableIndexSleb
                | RelocationType::TableNumberLeb
                | RelocationType::GlobalIndexI32
                | RelocationType::GlobalIndexLeb
                | RelocationType::TypeIndexLeb
        )
}

pub(crate) fn data_relocations_are_supported(relocs: &[WasmRelocation]) -> bool {
    relocs
        .iter()
        .all(|reloc| is_supported_data_relocation(reloc.ty))
}

pub(crate) fn reloc_value_with_addend(base: u32, addend: i64) -> Result<u32> {
    let value = i64::from(base)
        .checked_add(addend)
        .ok_or_else(|| crate::error!("Wasm relocation value overflow"))?;
    u32::try_from(value).map_err(|_| crate::error!("Wasm relocation value out of range"))
}

/// Apply addend policy. Relative table/memory bases already include the addend.
pub(crate) fn finalize_reloc_value(reloc: &WasmRelocation, base: u32) -> Result<u32> {
    if matches!(
        reloc.ty,
        RelocationType::MemoryAddrRelSleb | RelocationType::TableIndexRelSleb
    ) {
        Ok(base)
    } else {
        reloc_value_with_addend(base, reloc.addend)
    }
}

pub(crate) fn data_segment_memory_offsets_by_original_index(
    object_data_layout: &[WasmDataSegmentLayout<'_>],
) -> Vec<Option<u32>> {
    let max_index = object_data_layout
        .iter()
        .map(|s| s.segment_index)
        .max()
        .map_or(0, |i| i as usize);
    let mut by_original = vec![None; max_index.saturating_add(1)];
    for segment in object_data_layout {
        let idx = segment.segment_index as usize;
        by_original[idx] = Some(segment.output_memory_offset);
    }
    by_original
}

/// Address of a defined data symbol, or `None` when its segment was GC'd.
pub(crate) fn try_data_symbol_memory_address(
    segment_memory_offsets: &[Option<u32>],
    sym: &WasmSymbol,
) -> Result<Option<u32>> {
    ensure!(
        sym.kind == WasmSymbolKind::Data,
        "memory address relocation references non-data symbol"
    );
    let Some(Some(segment_base)) = segment_memory_offsets.get(sym.index as usize) else {
        return Ok(None);
    };
    Ok(Some(segment_base.checked_add(sym.offset).ok_or_else(
        || crate::error!("Wasm data symbol address overflow"),
    )?))
}
