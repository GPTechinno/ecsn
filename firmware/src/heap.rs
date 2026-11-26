//! The heap: the TLSF allocator over the SDRAM, counting the bytes allocated and their peak. The
//! allocator's free space, read between two steps of the executor, misses what a step allocates
//! and frees on its way, such as a block's validation.

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use embedded_alloc::TlsfHeap;

pub struct Heap {
    tlsf: TlsfHeap,
    allocated: AtomicUsize,
    peak: AtomicUsize,
}

impl Heap {
    pub const fn empty() -> Self {
        Self {
            tlsf: TlsfHeap::empty(),
            allocated: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        }
    }

    /// Gives the allocator `size` bytes at `start`.
    ///
    /// # Safety
    ///
    /// Once only, before the first allocation, with memory nothing else uses.
    pub unsafe fn init(&self, start: usize, size: usize) {
        unsafe { self.tlsf.init(start, size) }
    }

    /// The bytes the allocator has free, its own overhead counted as taken.
    pub fn free(&self) -> usize {
        self.tlsf.free()
    }

    /// The bytes allocated.
    pub fn allocated(&self) -> usize {
        self.allocated.load(Relaxed)
    }

    /// The most bytes allocated at once so far.
    pub fn peak(&self) -> usize {
        self.peak.load(Relaxed)
    }
}

unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { self.tlsf.alloc(layout) };
        if !ptr.is_null() {
            let allocated = self.allocated.fetch_add(layout.size(), Relaxed) + layout.size();
            self.peak.fetch_max(allocated, Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { self.tlsf.dealloc(ptr, layout) };
        self.allocated.fetch_sub(layout.size(), Relaxed);
    }
}
