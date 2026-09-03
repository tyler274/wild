#[allow(unused_imports)]
use crate::elf::abi::*;
#[allow(unused_imports)]
use crate::elf::file::*;
#[allow(unused_imports)]
use crate::elf::output::*;
#[allow(unused_imports)]
use crate::elf::types::*;
use crate::platform::FrameIndex;
use crate::platform::Relocation;
use std::mem::offset_of;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// See https://refspecs.linuxfoundation.org/LSB_1.3.0/gLSB/gLSB/ehframehdr.html
#[derive(FromBytes, IntoBytes, KnownLayout, Clone, Copy)]
#[repr(C)]
pub(crate) struct EhFrameHdr {
    pub(crate) version: u8,
    pub(crate) frame_pointer_encoding: u8,
    pub(crate) count_encoding: u8,
    pub(crate) table_encoding: u8,
    // For now we just use 32 bit pointer and count because it means that they're aligned. If we
    // need to upgrade these to u64, then we'd have to write these as unaligned fields.
    pub(crate) frame_pointer: i32,
    pub(crate) entry_count: u32,
}

pub(crate) const FRAME_POINTER_FIELD_OFFSET: usize = offset_of!(EhFrameHdr, frame_pointer);

/// The offset of the offset within the structure passed to __tls_get_addr.
#[derive(FromBytes, IntoBytes, KnownLayout, Clone, Copy)]
#[repr(C)]
pub(crate) struct EhFrameHdrEntry {
    pub(crate) frame_ptr: i32,
    pub(crate) frame_info_ptr: i32,
}

#[derive(FromBytes, Clone, Copy)]
#[repr(C)]
pub(crate) struct EhFrameEntryPrefix {
    pub(crate) length: u32,
    pub(crate) cie_id: u32,
}

pub(crate) fn is_eh_frame_terminator(data: &[u8]) -> bool {
    data.len() == size_of::<u32>() && data.iter().all(|&b| b == 0)
}

/// The offset of the pc_begin field in an FDE.
pub(crate) const FDE_PC_BEGIN_OFFSET: usize = 8;

/// A "common information entry". This is part of the .eh_frame data in ELF.
#[derive(PartialEq, Eq, Hash)]
pub(crate) struct Cie<'data> {
    pub(crate) bytes: &'data [u8],
    pub(crate) eligible_for_deduplication: bool,
}

pub(crate) struct CieAtOffset<'data> {
    // TODO: Use or remove. I think we need this when we implement deduplication of CIEs.
    /// Offset within .eh_frame
    #[allow(dead_code)]
    pub(crate) offset: u32,
    pub(crate) cie: Cie<'data>,
}

pub(crate) enum ExceptionFrames<'data, C: ElfClass> {
    Rela(Vec<ExceptionFrame<'data, ElfRela<C>>>),
    Crel(Vec<ExceptionFrame<'data, ElfCrel<C>>>),
}

impl<'data, C: ElfClass> ExceptionFrames<'data, C> {
    pub(crate) fn extend(&mut self, other: Self) {
        match (self, other) {
            (ExceptionFrames::Rela(a), ExceptionFrames::Rela(b)) => a.extend(b),
            (ExceptionFrames::Crel(a), ExceptionFrames::Crel(b)) => a.extend(b),
            (a, b) if a.is_empty() => *a = b,
            _ => panic!("Mixed exception frame relocations"),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        match self {
            ExceptionFrames::Rela(a) => a.is_empty(),
            ExceptionFrames::Crel(a) => a.is_empty(),
        }
    }
}

impl<'data, C: ElfClass> Default for ExceptionFrames<'data, C> {
    fn default() -> Self {
        ExceptionFrames::Rela(Vec::new())
    }
}

pub(crate) struct ExceptionFrame<'data, R: Relocation> {
    /// The relocations that need to be processed if we load this frame.
    pub(crate) relocations: R::Sequence<'data>,

    /// Number of bytes required to store this frame.
    pub(crate) frame_size: u32,

    /// The index of the previous frame that is for the same section.
    pub(crate) previous_frame_for_section: Option<FrameIndex>,

    pub(crate) eh_frame_section_index: object::SectionIndex,
}

pub(crate) struct EhFrameSizes {
    pub(crate) num_frames: u64,
    pub(crate) eh_frame_size: u64,
}

impl<'data, C: ElfClass> ExceptionFrames<'data, C> {
    pub(crate) fn len(&self) -> usize {
        match self {
            ExceptionFrames::Rela(f) => f.len(),
            ExceptionFrames::Crel(f) => f.len(),
        }
    }
}
