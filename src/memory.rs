use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rquickjs::allocator::{Allocator, RustAllocator};

const ALLOC_ALIGN: usize = std::mem::align_of::<u64>();
const HEADER_SIZE: usize = if std::mem::size_of::<usize>() < ALLOC_ALIGN {
    ALLOC_ALIGN
} else {
    std::mem::size_of::<usize>()
};

pub struct LimitedAllocator {
    limit: usize,
    used: usize,
    exceeded: Arc<AtomicBool>,
}

impl LimitedAllocator {
    pub fn new(limit: usize) -> (Self, Arc<AtomicBool>) {
        let exceeded = Arc::new(AtomicBool::new(false));
        let allocator = Self {
            limit,
            used: 0,
            exceeded: Arc::clone(&exceeded),
        };
        (allocator, exceeded)
    }

    fn mark_exceeded(&self) {
        self.exceeded.store(true, Ordering::Relaxed);
    }

    fn rounded_size(size: usize) -> Option<usize> {
        let rounded = size.checked_add(ALLOC_ALIGN - 1)? / ALLOC_ALIGN;
        let rounded = rounded.checked_mul(ALLOC_ALIGN)?;
        rounded.checked_add(HEADER_SIZE)?;
        Some(rounded)
    }

    fn reserve(&mut self, size: usize) -> Option<usize> {
        match self.used.checked_add(size) {
            Some(used) if used <= self.limit => Some(used),
            _ => {
                self.mark_exceeded();
                None
            }
        }
    }
}

unsafe impl Allocator for LimitedAllocator {
    fn alloc(&mut self, size: usize) -> *mut u8 {
        let Some(usable_size) = Self::rounded_size(size) else {
            self.mark_exceeded();
            return std::ptr::null_mut();
        };
        let Some(used) = self.reserve(usable_size) else {
            return std::ptr::null_mut();
        };
        let ptr = RustAllocator.alloc(size);
        if !ptr.is_null() {
            self.used = used;
        }
        ptr
    }

    fn calloc(&mut self, count: usize, size: usize) -> *mut u8 {
        let Some(total) = count.checked_mul(size) else {
            self.mark_exceeded();
            return std::ptr::null_mut();
        };
        let Some(usable_size) = Self::rounded_size(total) else {
            self.mark_exceeded();
            return std::ptr::null_mut();
        };
        let Some(used) = self.reserve(usable_size) else {
            return std::ptr::null_mut();
        };
        let ptr = if total == 0 {
            RustAllocator.alloc(0)
        } else {
            RustAllocator.calloc(count, size)
        };
        if !ptr.is_null() {
            self.used = used;
        }
        ptr
    }

    unsafe fn dealloc(&mut self, ptr: *mut u8) {
        if ptr.is_null() {
            return;
        }
        let usable_size = unsafe { RustAllocator::usable_size(ptr) };
        let Some(used) = self.used.checked_sub(usable_size) else {
            self.mark_exceeded();
            unsafe { RustAllocator.dealloc(ptr) };
            return;
        };
        self.used = used;
        unsafe { RustAllocator.dealloc(ptr) };
    }

    unsafe fn realloc(&mut self, ptr: *mut u8, new_size: usize) -> *mut u8 {
        if ptr.is_null() {
            return self.alloc(new_size);
        }
        let Some(new_usable_size) = Self::rounded_size(new_size) else {
            self.mark_exceeded();
            return std::ptr::null_mut();
        };
        let old_usable_size = unsafe { RustAllocator::usable_size(ptr) };
        let new_used = if new_usable_size >= old_usable_size {
            let growth = new_usable_size - old_usable_size;
            let Some(used) = self.reserve(growth) else {
                return std::ptr::null_mut();
            };
            used
        } else {
            let shrink = old_usable_size - new_usable_size;
            let Some(used) = self.used.checked_sub(shrink) else {
                self.mark_exceeded();
                return std::ptr::null_mut();
            };
            used
        };
        let new_ptr = unsafe { RustAllocator.realloc(ptr, new_size) };
        if !new_ptr.is_null() {
            self.used = new_used;
        }
        new_ptr
    }

    unsafe fn usable_size(ptr: *mut u8) -> usize {
        if ptr.is_null() {
            return 0;
        }
        unsafe { RustAllocator::usable_size(ptr) }
    }
}
