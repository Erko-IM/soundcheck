//! Memory for the whole program on macOS: the system's, except that a block
//! of a megabyte or more is mapped from the kernel when it is asked for and
//! handed back when it is freed.
//!
//! macOS keeps the large blocks a program frees for it to use again, but
//! uses one again only for a request of about the same size, and keeps
//! them in the program's memory meanwhile. Buffers sized by the file being
//! read and by the window, as a spectrogram's are, come in a new size each
//! time, so it would hold on to every one: hundreds of megabytes after a
//! few dozen files, and gigabytes after a while.

use std::alloc::{GlobalAlloc, Layout, System};

/// Blocks this big or bigger are mapped.
const BIG: usize = 1 << 20;

pub struct Allocator;

// SAFETY: every block is handed back the way it was handed out, told
// apart by its layout, which the caller gives back unchanged. Each is
// called, as the system's are, rather than copied into every place that
// allocates, which makes the program over half a megabyte bigger.
unsafe impl GlobalAlloc for Allocator {
    #[inline(never)]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.size() < BIG {
            // SAFETY: passed on as given.
            return unsafe { System.alloc(layout) };
        }
        // SAFETY: as for `GlobalAlloc::alloc`.
        unsafe { take(layout, false) }
    }

    #[inline(never)]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if layout.size() < BIG {
            // SAFETY: passed on as given.
            return unsafe { System.alloc_zeroed(layout) };
        }
        // SAFETY: as for `GlobalAlloc::alloc_zeroed`.
        unsafe { take(layout, true) }
    }

    #[inline(never)]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if layout.size() < BIG {
            // SAFETY: passed on as given.
            return unsafe { System.dealloc(ptr, layout) };
        }
        // SAFETY: as for `GlobalAlloc::dealloc`.
        unsafe { give(ptr, layout) }
    }

    #[inline(never)]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if layout.size() < BIG && size < BIG {
            // SAFETY: passed on as given.
            return unsafe { System.realloc(ptr, layout, size) };
        }
        // SAFETY: as for `GlobalAlloc::realloc`.
        unsafe { resize(ptr, layout, size) }
    }
}

/// Whether a block of `layout` is mapped: one big enough, whose alignment
/// the start of a page serves.
fn mapped(layout: Layout) -> bool {
    layout.size() >= BIG && layout.align() <= page()
}

fn page() -> usize {
    // SAFETY: sysconf only reads a constant of the system.
    usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap_or(4096)
}

/// A new big block, zeroed if asked: mapped pages come zeroed.
unsafe fn take(layout: Layout, zeroed: bool) -> *mut u8 {
    if !mapped(layout) {
        // SAFETY: passed on as given.
        return unsafe {
            if zeroed {
                System.alloc_zeroed(layout)
            } else {
                System.alloc(layout)
            }
        };
    }
    // SAFETY: an anonymous private mapping touches no existing memory.
    let pages = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            layout.size(),
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        )
    };
    if pages == libc::MAP_FAILED {
        std::ptr::null_mut()
    } else {
        pages.cast()
    }
}

/// Hands back a block `take` or `System` gave for `layout`.
unsafe fn give(ptr: *mut u8, layout: Layout) {
    if mapped(layout) {
        // SAFETY: `ptr` is a mapping of this size, from `take`.
        unsafe { libc::munmap(ptr.cast(), layout.size()) };
    } else {
        // SAFETY: passed on as given.
        unsafe { System.dealloc(ptr, layout) }
    }
}

/// `GlobalAlloc::realloc` where either side is big.
unsafe fn resize(ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
    // SAFETY: the caller keeps `size` within what a layout allows.
    let new = unsafe { Layout::from_size_align_unchecked(size, layout.align()) };
    if !mapped(layout) && !mapped(new) {
        // SAFETY: passed on as given.
        return unsafe { System.realloc(ptr, layout, size) };
    }
    let pages = |bytes: usize| bytes.div_ceil(page());
    if mapped(layout) && mapped(new) && pages(layout.size()) == pages(size) {
        return ptr;
    }
    // SAFETY: a block is taken, the old one copied into it and handed back,
    // as `GlobalAlloc::realloc` does by default.
    unsafe {
        let moved = take(new, false);
        if !moved.is_null() {
            std::ptr::copy_nonoverlapping(ptr, moved, layout.size().min(size));
            give(ptr, layout);
        }
        moved
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn blocks_keep_their_contents_as_they_grow_and_shrink_across_the_size_that_maps_them() {
        let mut bytes: Vec<u8> = (0..1000).map(|i| i as u8).collect();
        for size in [
            super::BIG / 2,
            super::BIG * 3,
            super::BIG * 3 + 7,
            super::BIG * 9,
        ] {
            let known = bytes.len();
            bytes.resize(size, 0xAB);
            assert!(bytes[..1000].iter().enumerate().all(|(i, &b)| b == i as u8));
            assert!(bytes[known..].iter().all(|&b| b == 0xAB));
        }
        bytes.truncate(super::BIG + 5);
        bytes.shrink_to_fit();
        assert_eq!(bytes[super::BIG + 4], 0xAB);
        bytes.truncate(10);
        bytes.shrink_to_fit();
        assert_eq!(bytes, (0..10).collect::<Vec<u8>>());
        let zeroed = vec![0u64; super::BIG];
        assert!(zeroed.iter().all(|&v| v == 0));
    }
}
