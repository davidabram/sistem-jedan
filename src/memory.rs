use std::alloc::{Layout, alloc, alloc_zeroed, dealloc, realloc};
use std::ptr::{self, NonNull};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rquickjs::allocator::Allocator;

#[repr(C, align(8))]
struct Header(Layout);

pub(super) struct LimitedAllocator {
    limit: usize,
    used: usize,
    failed: Arc<AtomicBool>,
}

impl LimitedAllocator {
    pub(super) fn new(limit: usize) -> (Self, Arc<AtomicBool>) {
        let failed = Arc::new(AtomicBool::new(false));
        let allocator = Self {
            limit,
            used: 0,
            failed: Arc::clone(&failed),
        };
        (allocator, failed)
    }

    fn fail(&self) -> *mut u8 {
        self.failed.store(true, Ordering::Relaxed);
        ptr::null_mut()
    }

    fn layout(size: usize) -> Option<Layout> {
        Layout::from_size_align(size.checked_add(size_of::<Header>())?, align_of::<Header>()).ok()
    }

    fn reserve(&self, used: usize, layout: Layout) -> Option<usize> {
        used.checked_add(layout.size())
            .filter(|&used| used <= self.limit)
    }

    fn without(&self, layout: Layout) -> usize {
        let Some(used) = self.used.checked_sub(layout.size()) else {
            std::process::abort();
        };
        used
    }

    unsafe fn init_header(base: NonNull<u8>, layout: Layout) -> *mut u8 {
        unsafe {
            base.cast::<Header>().write(Header(layout));
            base.add(size_of::<Header>()).as_ptr()
        }
    }

    unsafe fn read_header(payload: NonNull<u8>) -> (NonNull<u8>, Layout) {
        let base = unsafe { payload.sub(size_of::<Header>()) };
        (base, unsafe { base.cast::<Header>().read().0 })
    }

    fn allocate(&mut self, size: usize, zeroed: bool) -> *mut u8 {
        if size == 0 {
            return ptr::null_mut();
        }
        let Some(layout) = Self::layout(size) else {
            return self.fail();
        };
        let Some(used) = self.reserve(self.used, layout) else {
            return self.fail();
        };
        let raw = unsafe {
            if zeroed {
                alloc_zeroed(layout)
            } else {
                alloc(layout)
            }
        };
        let Some(base) = NonNull::new(raw) else {
            return self.fail();
        };
        self.used = used;
        unsafe { Self::init_header(base, layout) }
    }
}

unsafe impl Allocator for LimitedAllocator {
    fn alloc(&mut self, size: usize) -> *mut u8 {
        self.allocate(size, false)
    }

    fn calloc(&mut self, count: usize, size: usize) -> *mut u8 {
        match count.checked_mul(size) {
            Some(total) => self.allocate(total, true),
            None => self.fail(),
        }
    }

    unsafe fn dealloc(&mut self, raw: *mut u8) {
        if let Some(payload) = NonNull::new(raw) {
            let (base, layout) = unsafe { Self::read_header(payload) };
            self.used = self.without(layout);
            unsafe { dealloc(base.as_ptr(), layout) };
        }
    }

    unsafe fn realloc(&mut self, raw: *mut u8, size: usize) -> *mut u8 {
        let Some(payload) = NonNull::new(raw) else {
            return self.alloc(size);
        };
        if size == 0 {
            unsafe { self.dealloc(raw) };
            return ptr::null_mut();
        }
        let Some(layout) = Self::layout(size) else {
            return self.fail();
        };
        let (base, old_layout) = unsafe { Self::read_header(payload) };
        let Some(used) = self.reserve(self.without(old_layout), layout) else {
            return self.fail();
        };
        let resized = unsafe { realloc(base.as_ptr(), old_layout, layout.size()) };
        let Some(base) = NonNull::new(resized) else {
            return self.fail();
        };
        self.used = used;
        unsafe { Self::init_header(base, layout) }
    }

    unsafe fn usable_size(raw: *mut u8) -> usize {
        NonNull::new(raw).map_or(0, |payload| {
            let (_, layout) = unsafe { Self::read_header(payload) };
            layout.size() - size_of::<Header>()
        })
    }
}

/// Returns memory freed by a finished execution to the OS. glibc otherwise
/// keeps it cached in per-thread arenas, holding up to roughly one
/// execution's worth of RSS per thread that ran one.
pub(super) fn release_free_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: malloc_trim takes no pointers and is safe to call at any time.
    unsafe {
        libc::malloc_trim(0);
    }
}
