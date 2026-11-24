use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rquickjs::allocator::{Allocator, RustAllocator};

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

    fn reserve(&mut self, size: usize) -> bool {
        match self.used.checked_add(size) {
            Some(used) if used <= self.limit => true,
            _ => {
                self.exceeded.store(true, Ordering::Relaxed);
                false
            }
        }
    }
}

unsafe impl Allocator for LimitedAllocator {
    fn alloc(&mut self, size: usize) -> *mut u8 {
        if !self.reserve(size) {
            return std::ptr::null_mut();
        }
        let ptr = RustAllocator.alloc(size);
        if !ptr.is_null() {
            self.used += unsafe { RustAllocator::usable_size(ptr) };
        }
        ptr
    }

    fn calloc(&mut self, count: usize, size: usize) -> *mut u8 {
        let Some(total) = count.checked_mul(size) else {
            return std::ptr::null_mut();
        };
        if !self.reserve(total) {
            return std::ptr::null_mut();
        }
        let ptr = if total == 0 {
            RustAllocator.alloc(0)
        } else {
            RustAllocator.calloc(count, size)
        };
        if !ptr.is_null() {
            self.used += unsafe { RustAllocator::usable_size(ptr) };
        }
        ptr
    }

    unsafe fn dealloc(&mut self, ptr: *mut u8) {
        self.used -= unsafe { RustAllocator::usable_size(ptr) };
        unsafe { RustAllocator.dealloc(ptr) };
    }

    unsafe fn realloc(&mut self, ptr: *mut u8, new_size: usize) -> *mut u8 {
        let old_size = unsafe { RustAllocator::usable_size(ptr) };
        if !self.reserve(new_size.saturating_sub(old_size)) {
            return std::ptr::null_mut();
        }
        let new_ptr = unsafe { RustAllocator.realloc(ptr, new_size) };
        if !new_ptr.is_null() {
            self.used = self.used - old_size + unsafe { RustAllocator::usable_size(new_ptr) };
        }
        new_ptr
    }

    unsafe fn usable_size(ptr: *mut u8) -> usize {
        unsafe { RustAllocator::usable_size(ptr) }
    }
}
