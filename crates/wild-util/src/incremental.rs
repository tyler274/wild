//! Incremental-link predicates that Kani can prove without `hashbrown` or `wild-layout`.

/// Generation 0 is never issued. Wrapping from `u32::MAX` resumes at 1.
pub fn next_generation(generation: u32) -> u32 {
    match generation.wrapping_add(1) {
        0 => 1,
        next => next,
    }
}

/// Skip an object's section payloads when it is skippable, its atom was reused,
/// and the input path did not change.
pub fn should_skip_payload(skippable: bool, reused: bool, path_changed: bool) -> bool {
    skippable && reused && !path_changed
}

/// Incremental + plugin/GC is a full padded link, not an in-place update.
pub fn fallback_reason_for_plugin_or_gc(
    plugin_active: bool,
    gc_and_incremental: bool,
) -> Option<&'static str> {
    if plugin_active {
        return Some("LTO/plugin inputs");
    }
    if gc_and_incremental {
        return Some("--gc-sections is ignored for incremental links");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_generation_skips_zero() {
        assert_eq!(next_generation(0), 1);
        assert_eq!(next_generation(1), 2);
        assert_eq!(next_generation(u32::MAX), 1);
    }

    #[test]
    fn skip_payload_requires_all_three() {
        assert!(should_skip_payload(true, true, false));
        assert!(!should_skip_payload(true, true, true));
        assert!(!should_skip_payload(false, true, false));
        assert!(!should_skip_payload(true, false, false));
    }

    #[test]
    fn plugin_or_gc_forces_fallback() {
        assert_eq!(
            fallback_reason_for_plugin_or_gc(true, false),
            Some("LTO/plugin inputs")
        );
        assert_eq!(
            fallback_reason_for_plugin_or_gc(true, true),
            Some("LTO/plugin inputs")
        );
        assert_eq!(
            fallback_reason_for_plugin_or_gc(false, true),
            Some("--gc-sections is ignored for incremental links")
        );
        assert_eq!(fallback_reason_for_plugin_or_gc(false, false), None);
    }
}

#[cfg(kani)]
mod verify {
    use super::*;

    #[kani::proof]
    fn generation_never_zero() {
        let generation: u32 = kani::any();
        let next = next_generation(generation);
        assert!(next != 0);
        if generation == u32::MAX {
            assert_eq!(next, 1);
        } else {
            assert_eq!(next, generation + 1);
        }
    }

    #[kani::proof]
    fn skip_payload_iff_skippable_reused_unchanged() {
        let skippable: bool = kani::any();
        let reused: bool = kani::any();
        let path_changed: bool = kani::any();
        let skip = should_skip_payload(skippable, reused, path_changed);
        assert_eq!(skip, skippable && reused && !path_changed);
    }

    #[kani::proof]
    fn plugin_fallback_beats_gc() {
        let plugin_active: bool = kani::any();
        let gc_and_incremental: bool = kani::any();
        let reason = fallback_reason_for_plugin_or_gc(plugin_active, gc_and_incremental);
        if plugin_active {
            assert_eq!(reason, Some("LTO/plugin inputs"));
        } else if gc_and_incremental {
            assert_eq!(
                reason,
                Some("--gc-sections is ignored for incremental links")
            );
        } else {
            assert_eq!(reason, None);
        }
    }
}
