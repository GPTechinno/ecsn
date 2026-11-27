//! The system allocator, counted: the bytes in use and their peak, and an optional limit beyond
//! which an allocation fails, as it does on a board whose heap is its RAM.

use std::alloc::{GlobalAlloc, Layout, System};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

pub struct Counted {
    used: AtomicUsize,
    peak: AtomicUsize,
    limit: AtomicUsize,
}

impl Counted {
    pub const fn new() -> Self {
        Self {
            used: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            limit: AtomicUsize::new(usize::MAX),
        }
    }

    /// Bytes in use.
    pub fn used(&self) -> usize {
        self.used.load(Relaxed)
    }

    /// The most bytes in use at once so far.
    pub fn peak(&self) -> usize {
        self.peak.load(Relaxed)
    }

    /// Bytes left under the limit.
    pub fn free(&self) -> usize {
        self.limit.load(Relaxed).saturating_sub(self.used())
    }

    /// Fails the allocations that would take the bytes in use beyond `limit`.
    pub fn set_limit(&self, limit: usize) {
        self.limit.store(limit, Relaxed);
    }

    /// Counts `size` more bytes, unless they would go beyond the limit.
    fn take(&self, size: usize) -> bool {
        let limit = self.limit.load(Relaxed);
        let mut used = self.used.load(Relaxed);
        loop {
            let Some(total) = used.checked_add(size).filter(|total| *total <= limit) else {
                return false;
            };
            match self
                .used
                .compare_exchange_weak(used, total, Relaxed, Relaxed)
            {
                Ok(_) => {
                    self.peak.fetch_max(total, Relaxed);
                    return true;
                }
                Err(now) => used = now,
            }
        }
    }
}

unsafe impl GlobalAlloc for Counted {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if !self.take(layout.size()) {
            return null_mut();
        }
        let ptr = unsafe { System.alloc(layout) };
        if ptr.is_null() {
            self.used.fetch_sub(layout.size(), Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        self.used.fetch_sub(layout.size(), Relaxed);
    }

    // `realloc` is left to its default: a new block, a copy, then the old one freed, so that
    // the old and the new block count together while both exist, as on a heap that cannot grow
    // a block in place.
}
