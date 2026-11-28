//! What Bitcoin Core's C++ (the script interpreter, with the `scripts` feature) needs from the
//! firmware, in place of a hosted C library's: `malloc` and its kin on the heap, the static
//! constructors run at the start (cortex-m-rt runs none), `abort`, and the registration of static
//! destructors (never run: the firmware does not exit).

use core::alloc::{GlobalAlloc, Layout};
use core::ffi::{c_int, c_void};
use core::ptr;

use crate::HEAP;

/// Each block starts with its whole size, ahead of what the caller gets: 8 bytes, which keeps
/// what it gets aligned to 8 as C requires of `malloc`.
const HEADER: usize = 8;

fn layout(total: usize) -> Option<Layout> {
    Layout::from_size_align(total, HEADER).ok()
}

#[no_mangle]
pub unsafe extern "C" fn malloc(size: usize) -> *mut c_void {
    let Some(total) = size.checked_add(HEADER) else {
        return ptr::null_mut();
    };
    let Some(layout) = layout(total) else {
        return ptr::null_mut();
    };
    // SAFETY: a layout of a non-zero size
    let base = unsafe { HEAP.alloc(layout) };
    if base.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: the block holds the header and `size` bytes after it
    unsafe {
        base.cast::<usize>().write(total);
        base.add(HEADER).cast()
    }
}

#[no_mangle]
pub unsafe extern "C" fn free(p: *mut c_void) {
    if p.is_null() {
        return;
    }
    // SAFETY: `p` came from `malloc`, its header just before it
    unsafe {
        let base = p.cast::<u8>().sub(HEADER);
        let total = base.cast::<usize>().read();
        HEAP.dealloc(base, Layout::from_size_align_unchecked(total, HEADER));
    }
}

#[no_mangle]
pub unsafe extern "C" fn calloc(count: usize, size: usize) -> *mut c_void {
    let Some(bytes) = count.checked_mul(size) else {
        return ptr::null_mut();
    };
    // SAFETY: a block of `bytes` bytes, zeroed
    unsafe {
        let p = malloc(bytes);
        if !p.is_null() {
            p.cast::<u8>().write_bytes(0, bytes);
        }
        p
    }
}

#[no_mangle]
pub unsafe extern "C" fn realloc(p: *mut c_void, size: usize) -> *mut c_void {
    if p.is_null() {
        // SAFETY: as `malloc`
        return unsafe { malloc(size) };
    }
    if size == 0 {
        // SAFETY: `p` came from `malloc`
        unsafe { free(p) };
        return ptr::null_mut();
    }
    let Some(total) = size.checked_add(HEADER) else {
        return ptr::null_mut();
    };
    if layout(total).is_none() {
        return ptr::null_mut();
    }
    // SAFETY: `p` came from `malloc`, its header just before it
    unsafe {
        let base = p.cast::<u8>().sub(HEADER);
        let old = base.cast::<usize>().read();
        let moved = HEAP.realloc(base, Layout::from_size_align_unchecked(old, HEADER), total);
        if moved.is_null() {
            return ptr::null_mut();
        }
        moved.cast::<usize>().write(total);
        moved.add(HEADER).cast()
    }
}

#[no_mangle]
pub extern "C" fn abort() -> ! {
    panic!("abort from C++");
}

/// A static destructor to register: never run, the firmware does not exit.
#[no_mangle]
pub extern "C" fn __aeabi_atexit(
    _object: *mut c_void,
    _destructor: extern "C" fn(*mut c_void),
    _dso: *mut c_void,
) -> c_int {
    0
}

#[no_mangle]
pub extern "C" fn __cxa_atexit(
    _destructor: extern "C" fn(*mut c_void),
    _object: *mut c_void,
    _dso: *mut c_void,
) -> c_int {
    0
}

/// The handle of the static destructors of this program, the one shared object.
#[no_mangle]
pub static __dso_handle: u8 = 0;

/// Runs the static constructors of the C++ code (`.init_array`, which `cxx.x` keeps).
///
/// # Safety
///
/// Once only, once the heap is ready, before the C++ code runs.
pub unsafe fn run_constructors() {
    extern "C" {
        static __init_array_start: extern "C" fn();
        static __init_array_end: extern "C" fn();
    }
    let mut constructor = ptr::addr_of!(__init_array_start);
    let end = ptr::addr_of!(__init_array_end);
    while constructor < end {
        // SAFETY: the linker's table of constructors
        unsafe {
            (*constructor)();
            constructor = constructor.add(1);
        }
    }
}

// The system calls newlib's stdio reaches, which C++ uses only on its way to `abort` (the message
// of an uncaught exception): none is available. `_sbrk` gives no memory, so that newlib's own
// allocator never takes any; `malloc` above serves the C++ code.

#[no_mangle]
pub extern "C" fn _read(_fd: c_int, _buf: *mut u8, _len: c_int) -> c_int {
    -1
}

#[no_mangle]
pub extern "C" fn _write(_fd: c_int, _buf: *const u8, _len: c_int) -> c_int {
    -1
}

#[no_mangle]
pub extern "C" fn _lseek(_fd: c_int, _offset: c_int, _whence: c_int) -> c_int {
    -1
}

#[no_mangle]
pub extern "C" fn _isatty(_fd: c_int) -> c_int {
    0
}

#[no_mangle]
pub extern "C" fn _fstat(_fd: c_int, _stat: *mut c_void) -> c_int {
    -1
}

#[no_mangle]
pub extern "C" fn _close(_fd: c_int) -> c_int {
    -1
}

#[no_mangle]
pub extern "C" fn _sbrk(_increment: isize) -> *mut c_void {
    usize::MAX as *mut c_void
}
