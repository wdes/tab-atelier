// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Allocation-counting global allocator wrapper.
//!
//! [`Counting`] wraps any [`GlobalAlloc`] and bumps relaxed atomics
//! per alloc, so `bench` can report heap allocations per payload
//! without an external profiler.
//!
//! It is installed as `#[global_allocator]` ONLY in the headless
//! binary (`src/bin/tab-atelier-headless.rs`); the gpui binary keeps
//! the system default. The overhead is two `Relaxed` atomic adds per
//! allocation — negligible, and unobservable next to the syscalls the
//! daemon already makes.

use std::alloc::{GlobalAlloc, Layout};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// Total allocations ever made (monotonic).
pub static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
/// Total bytes ever allocated (monotonic; ignores frees).
pub static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);
/// Live bytes = allocated − freed; a cheap "current heap balance".
///
/// Signed because transient underflow is possible if a free races
/// ahead of its alloc on another thread (relaxed ordering); it
/// self-corrects.
pub static LIVE_BYTES: AtomicI64 = AtomicI64::new(0);

/// Snapshot of the three counters at a point in time.
#[derive(Clone, Copy)]
pub struct Snapshot {
    pub allocations: u64,
    pub allocated_bytes: u64,
    pub live_bytes: i64,
}

/// Read the current counters.
#[must_use]
pub fn snapshot() -> Snapshot {
    Snapshot {
        allocations: ALLOCATIONS.load(Ordering::Relaxed),
        allocated_bytes: ALLOCATED_BYTES.load(Ordering::Relaxed),
        live_bytes: LIVE_BYTES.load(Ordering::Relaxed),
    }
}

impl Snapshot {
    /// `self - earlier` — allocations + bytes that happened between
    /// `earlier` and `self`. `live_bytes` is the net heap delta.
    #[must_use]
    pub const fn since(self, earlier: Self) -> Self {
        Self {
            allocations: self.allocations.wrapping_sub(earlier.allocations),
            allocated_bytes: self.allocated_bytes.wrapping_sub(earlier.allocated_bytes),
            live_bytes: self.live_bytes - earlier.live_bytes,
        }
    }
}

/// Global-allocator wrapper that counts allocations. Construct with the
/// inner allocator you want to delegate to, e.g.
/// `Counting(std::alloc::System)`.
pub struct Counting<A>(pub A);

// SAFETY: every method forwards verbatim to the wrapped allocator; the
// only added work is relaxed atomic arithmetic, which has no bearing on
// allocation correctness. Implementing GlobalAlloc requires `unsafe`;
// the crate otherwise denies unsafe_code, so allow it on this impl.
#[allow(unsafe_code)]
unsafe impl<A: GlobalAlloc> GlobalAlloc for Counting<A> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { self.0.alloc(layout) };
        if !p.is_null() {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
            LIVE_BYTES.fetch_add(layout.size() as i64, Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { self.0.dealloc(ptr, layout) };
        LIVE_BYTES.fetch_sub(layout.size() as i64, Ordering::Relaxed);
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { self.0.alloc_zeroed(layout) };
        if !p.is_null() {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
            LIVE_BYTES.fetch_add(layout.size() as i64, Ordering::Relaxed);
        }
        p
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { self.0.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            // A realloc is one allocation event; account the size delta
            // against both the byte total and the live balance.
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            let old = layout.size() as i64;
            let new = new_size as i64;
            if new > old {
                ALLOCATED_BYTES.fetch_add((new - old) as u64, Ordering::Relaxed);
            }
            LIVE_BYTES.fetch_add(new - old, Ordering::Relaxed);
        }
        p
    }
}
