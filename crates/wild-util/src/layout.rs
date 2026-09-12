//! Layout helpers that Kani can prove without compiling `wild-layout`.

use crate::alignment::Alignment;

/// GNU default LMA when a section does not set `AT` / `AT>`.
///
/// Later sections in a VMA region whose previous section had LMA ≠ VMA
/// (typically `AT>`) continue from that section's LMA end.
pub fn gnu_default_lma(
    mem_offset: u64,
    region_lma_end: Option<u64>,
    keep_running_lma: bool,
) -> u64 {
    if keep_running_lma {
        region_lma_end.unwrap_or(mem_offset)
    } else {
        mem_offset
    }
}

/// Align VMA, and LMA according to GNU `ALIGN_WITH_INPUT` / default `AT>` freeze.
pub fn align_vma_lma(
    mem_offset: u64,
    lma_offset: u64,
    alignment: Alignment,
    align_with_input: bool,
    freeze_lma: bool,
) -> (u64, u64) {
    let new_mem = alignment.align_up(mem_offset);
    let new_lma = if freeze_lma && !align_with_input {
        lma_offset
    } else if align_with_input {
        lma_offset + (new_mem - mem_offset)
    } else {
        alignment.align_up(lma_offset)
    };
    (new_mem, new_lma)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gnu_default_lma_keeps_region_when_running() {
        assert_eq!(gnu_default_lma(0x1000, Some(0x2000), true), 0x2000);
        assert_eq!(gnu_default_lma(0x1000, None, true), 0x1000);
        assert_eq!(gnu_default_lma(0x1000, Some(0x2000), false), 0x1000);
    }

    #[test]
    fn freeze_lma_does_not_follow_vma_gap() {
        let align = Alignment { exponent: 4 };
        let (vma, lma) = align_vma_lma(0x1, 0x1001, align, false, true);
        assert_eq!(vma, 0x10);
        assert_eq!(lma, 0x1001);
    }

    #[test]
    fn align_with_input_preserves_delta() {
        let align = Alignment { exponent: 4 };
        let (vma, lma) = align_vma_lma(0x1, 0x1001, align, true, false);
        assert_eq!(vma, 0x10);
        assert_eq!(lma, 0x1010);
    }
}

#[cfg(kani)]
mod verify {
    use super::*;
    use crate::alignment::Alignment;

    #[kani::proof]
    fn gnu_default_lma_matches_cases() {
        let mem: u64 = kani::any();
        let region: Option<u64> = kani::any();
        let keep: bool = kani::any();
        let got = gnu_default_lma(mem, region, keep);
        if keep {
            assert_eq!(got, region.unwrap_or(mem));
        } else {
            assert_eq!(got, mem);
        }
    }

    #[kani::proof]
    fn freeze_lma_preserves_lma() {
        let mem: u64 = kani::any();
        let lma: u64 = kani::any();
        let exp: u8 = kani::any();
        kani::assume(exp <= 21);
        let alignment = Alignment { exponent: exp };
        let align = alignment.value();
        kani::assume(mem <= u64::MAX - (align - 1));
        let (new_mem, new_lma) = align_vma_lma(mem, lma, alignment, false, true);
        assert_eq!(new_lma, lma);
        assert_eq!(new_mem, alignment.align_up(mem));
    }
}
