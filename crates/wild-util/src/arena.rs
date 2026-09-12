//! Thread-local bump arenas whose allocations live as long as the [`Herd`].
//!
//! This is the layout-stack equivalent of mold's per-thread 1MiB arenas. We keep bumpalo rather
//! than mimalloc heaps (`mi_heap_new`) because these allocations are never freed individually —
//! they must borrow the linker's `'data` lifetime. mimalloc remains the optional process-wide
//! allocator on the `wild` binary (`--features mimalloc`); bump chunks then come from mimalloc's
//! own thread-local heaps automatically.
//!
//! `bumpalo-herd` always constructs `Bump::default()`, whose first chunk is 512 bytes, and takes a
//! mutex on every `get`. This herd starts each thread's bump at 1MiB and caches the bump in TLS so
//! later [`Herd::get`] calls on the same thread skip the mutex.

use bumpalo::Bump;
use std::alloc::Layout;
use std::cell::Cell;
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// Initial bump chunk per thread. Large enough that typical per-thread work (decompressed
/// merge sections, copied names) does not walk bumpalo's 512 → 1K → 2K doubling ladder.
pub const INITIAL_CHUNK_SIZE: usize = 1 << 20;

static NEXT_HERD_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy)]
struct TlsBump {
    herd_id: u64,
    bump: *mut Bump,
}

thread_local! {
    static TLS: Cell<TlsBump> = const {
        Cell::new(TlsBump {
            herd_id: 0,
            bump: std::ptr::null_mut(),
        })
    };
}

/// A group of bump allocators, one per worker thread that has called [`Herd::get`].
///
/// Allocations outlive the [`Member`] that created them and remain valid until this herd is
/// dropped.
#[derive(Debug)]
pub struct Herd {
    id: u64,
    chunk_size: usize,
    bumps: Mutex<Vec<Box<Bump>>>,
}

impl Default for Herd {
    fn default() -> Self {
        Self::new()
    }
}

impl Herd {
    #[must_use]
    pub fn new() -> Self {
        Self::with_chunk_size(INITIAL_CHUNK_SIZE)
    }

    #[must_use]
    pub fn with_chunk_size(chunk_size: usize) -> Self {
        Self {
            id: NEXT_HERD_ID.fetch_add(1, Ordering::Relaxed),
            chunk_size,
            bumps: Mutex::new(Vec::new()),
        }
    }

    /// Reset every bump. Requires exclusive access so no [`Member`] can be live.
    pub fn reset(&mut self) {
        for bump in self.bumps.get_mut().unwrap() {
            bump.reset();
        }
    }

    /// Borrow this thread's bump. Cheap after the first call on a given thread for this herd.
    #[must_use]
    pub fn get(&self) -> Member<'_> {
        TLS.with(|tls| {
            let cached = tls.get();
            if cached.herd_id == self.id && !cached.bump.is_null() {
                return Member {
                    bump: cached.bump,
                    _herd: PhantomData,
                    _not_send: PhantomData,
                };
            }

            let bump = self.new_bump();
            tls.set(TlsBump {
                herd_id: self.id,
                bump,
            });
            Member {
                bump,
                _herd: PhantomData,
                _not_send: PhantomData,
            }
        })
    }

    fn new_bump(&self) -> *mut Bump {
        let mut bumps = self.bumps.lock().unwrap();
        bumps.push(Box::new(Bump::with_capacity(self.chunk_size)));
        // The `Box` is heap-allocated; moving it inside `bumps` does not move the `Bump`.
        &raw mut **bumps.last_mut().unwrap()
    }
}

/// A thread-local bump borrowed from a [`Herd`].
///
/// Allocation methods match [`bumpalo::Bump`], except returned references are tied to the herd,
/// not to this member.
pub struct Member<'h> {
    bump: *mut Bump,
    _herd: PhantomData<&'h Herd>,
    /// `Bump` is `!Sync`. Keeping `Member` `!Send` ensures only the creating thread allocates.
    _not_send: PhantomData<*const ()>,
}

macro_rules! alloc_fn {
    ($(pub fn $name: ident<($($g: tt)*)>(&self, $($pname: ident: $pty: ty),*) -> $res: ty;)*) => {
        $(
            pub fn $name<$($g)*>(&self, $($pname: $pty),*) -> $res {
                self.extend(self.bump().$name($($pname),*))
            }
        )*
    }
}

impl<'h> Member<'h> {
    alloc_fn! {
        pub fn alloc<(T)>(&self, val: T) -> &'h mut T;
        pub fn alloc_with<(T, F: FnOnce() -> T)>(&self, f: F) -> &'h mut T;
        pub fn alloc_str<()>(&self, src: &str) -> &'h mut str;
        pub fn alloc_slice_clone<(T: Clone)>(&self, src: &[T]) -> &'h mut [T];
        pub fn alloc_slice_copy<(T: Copy)>(&self, src: &[T]) -> &'h mut [T];
        pub fn alloc_slice_fill_clone<(T: Clone)>(&self, len: usize, value: &T) -> &'h mut [T];
        pub fn alloc_slice_fill_copy<(T: Copy)>(&self, len: usize, value: T) -> &'h mut [T];
        pub fn alloc_slice_fill_default<(T: Default)>(&self, len: usize) -> &'h mut [T];
        pub fn alloc_slice_fill_with<(T, F: FnMut(usize) -> T)>(&self, len: usize, f: F)
            -> &'h mut [T];
    }

    pub fn alloc_slice_fill_iter<T, I>(&self, iter: I) -> &'h mut [T]
    where
        I: IntoIterator<Item = T>,
        I::IntoIter: ExactSizeIterator,
    {
        self.extend(self.bump().alloc_slice_fill_iter(iter))
    }

    pub fn alloc_layout(&self, layout: Layout) -> NonNull<u8> {
        self.bump().alloc_layout(layout)
    }

    fn bump(&self) -> &Bump {
        // SAFETY: `bump` points at a `Bump` owned by the `Herd` that borrowed this member. The
        // `'h` lifetime and `!Send` marker keep that `Bump` alive and exclusive to this thread.
        unsafe { &*self.bump }
    }

    fn extend<'s, T: ?Sized>(&'s self, v: &'s mut T) -> &'h mut T {
        // SAFETY: the `Bump` stays in the `Herd` for `'h`; moving the `Member` proxy does not move
        // the `Bump`. The returned reference therefore remains valid until the herd is dropped.
        unsafe { &mut *(v as *mut T) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocations_outlive_member() {
        let herd = Herd::with_chunk_size(64);
        let value = {
            let member = herd.get();
            &*member.alloc(42u32)
        };
        assert_eq!(*value, 42);
    }

    #[test]
    fn same_thread_reuses_bump() {
        let herd = Herd::with_chunk_size(64);
        let a = &*herd.get().alloc(1u32);
        let b = &*herd.get().alloc(2u32);
        assert_eq!(*a, 1);
        assert_eq!(*b, 2);
    }

    #[test]
    fn sequential_herds_do_not_share_tls_bumps() {
        let first = {
            let herd = Herd::with_chunk_size(64);
            *herd.get().alloc(1u32)
        };
        let herd = Herd::with_chunk_size(64);
        let second = *herd.get().alloc(2u32);
        assert_eq!(first, 1);
        assert_eq!(second, 2);
    }
}
