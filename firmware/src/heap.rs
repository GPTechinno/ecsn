//! The heap: a TLSF allocator (rlsf) over the SDRAM, counting the bytes allocated and their peak,
//! and telling its free bytes and its largest free block. The free space read between two steps
//! of the executor misses what a step allocates and frees on its way, such as a block's
//! validation.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::RefCell;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use critical_section::Mutex;
use rlsf::Tlsf;

type Tlsf32 = Tlsf<'static, usize, usize, { usize::BITS as usize }, { usize::BITS as usize }>;

struct Inner {
    tlsf: Tlsf32,
    /// The memory the allocator covers, which its list of blocks walks: what it was given, its
    /// start aligned and its length rounded down to its granularity.
    pool: Option<NonNull<[u8]>>,
}

// the pool is only reached through the allocator, in critical sections
unsafe impl Send for Inner {}

pub struct Heap {
    inner: Mutex<RefCell<Inner>>,
    allocated: AtomicUsize,
    peak: AtomicUsize,
}

impl Heap {
    pub const fn empty() -> Self {
        Self {
            inner: Mutex::new(RefCell::new(Inner {
                tlsf: Tlsf::new(),
                pool: None,
            })),
            allocated: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        }
    }

    /// Gives the allocator `size` bytes at `start`, aligned to 16 bytes (the allocator's
    /// granularity).
    ///
    /// # Safety
    ///
    /// Once only, before the first allocation, with memory nothing else uses.
    pub unsafe fn init(&self, start: usize, size: usize) {
        assert_eq!(start % 16, 0, "the heap's start is aligned to 16 bytes");
        critical_section::with(|cs| {
            let mut inner = self.inner.borrow_ref_mut(cs);
            // SAFETY: the caller gives memory nothing else uses
            let given = unsafe {
                NonNull::slice_from_raw_parts(NonNull::new_unchecked(start as *mut u8), size)
            };
            let covered = unsafe { inner.tlsf.insert_free_block_ptr(given) };
            inner.pool = covered.map(|len| {
                // SAFETY: the start of what was given, aligned already
                NonNull::slice_from_raw_parts(
                    unsafe { NonNull::new_unchecked(start as *mut u8) },
                    len.get(),
                )
            });
        });
    }

    /// The bytes the allocator has free (its own overhead counted as taken), and the largest of
    /// its free blocks: the most it can allocate at once.
    pub fn free_and_largest(&self) -> (usize, usize) {
        critical_section::with(|cs| {
            let inner = self.inner.borrow_ref(cs);
            let Some(pool) = inner.pool else {
                return (0, 0);
            };
            let mut free = 0;
            let mut largest = 0;
            // SAFETY: the pool the allocator was given
            for block in unsafe { inner.tlsf.iter_blocks(pool) } {
                if !block.is_occupied() {
                    free += block.max_payload_size();
                    largest = largest.max(block.max_payload_size());
                }
            }
            (free, largest)
        })
    }

    /// The bytes the allocator has free, its own overhead counted as taken.
    pub fn free(&self) -> usize {
        self.free_and_largest().0
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
        let ptr = critical_section::with(|cs| self.inner.borrow_ref_mut(cs).tlsf.allocate(layout));
        match ptr {
            Some(ptr) => {
                let allocated = self.allocated.fetch_add(layout.size(), Relaxed) + layout.size();
                self.peak.fetch_max(allocated, Relaxed);
                ptr.as_ptr()
            }
            None => core::ptr::null_mut(),
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        critical_section::with(|cs| {
            // SAFETY: `ptr` came from `alloc` with this layout
            unsafe {
                self.inner
                    .borrow_ref_mut(cs)
                    .tlsf
                    .deallocate(NonNull::new_unchecked(ptr), layout.align())
            }
        });
        self.allocated.fetch_sub(layout.size(), Relaxed);
    }
}
