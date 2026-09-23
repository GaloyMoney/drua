use rquickjs::allocator::{Allocator, RustAllocator};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::{
    cell::Cell,
    future::{poll_fn, Future},
};

thread_local! { static STACK_TOP: Cell<usize> = const { Cell::new(0) }; }

pub(crate) async fn track_stack<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    poll_fn(|cx| {
        let marker = 0u8;
        let previous = STACK_TOP.replace((&marker as *const u8) as usize);
        struct Restore(usize);
        impl Drop for Restore {
            fn drop(&mut self) {
                STACK_TOP.set(self.0);
            }
        }
        let _restore = Restore(previous);
        future.as_mut().poll(cx)
    })
    .await
}

pub(crate) struct LimitedAllocator {
    used: usize,
    limit: usize,
    exhausted: Arc<AtomicBool>,
    stack_limit: usize,
    stack_exhausted: Arc<AtomicBool>,
}

impl LimitedAllocator {
    pub(crate) fn new(
        limit: usize,
        exhausted: Arc<AtomicBool>,
        stack_limit: usize,
        stack_exhausted: Arc<AtomicBool>,
    ) -> Self {
        Self {
            used: 0,
            limit: if limit == 0 { usize::MAX } else { limit },
            exhausted,
            stack_limit,
            stack_exhausted,
        }
    }

    fn charge(size: usize) -> Option<usize> {
        // rquickjs 0.9 RustAllocator rounds to u64 alignment and stores a usize header.
        let align = std::mem::align_of::<u64>();
        size.checked_add(align - 1)?
            .checked_div(align)?
            .checked_mul(align)?
            .checked_add(std::mem::size_of::<usize>().max(align))
    }

    fn permits(&self, old: usize, size: usize) -> bool {
        // Native stack errors can be caught in JS. Their exception allocation
        // happens before unwinding, so remember exhaustion independently.
        let marker = 0u8;
        let here = (&marker as *const u8) as usize;
        let top = STACK_TOP.get();
        if top != 0
            && self.stack_limit != 0
            && top.abs_diff(here) >= self.stack_limit.saturating_sub(32 * 1024)
        {
            self.stack_exhausted.store(true, Ordering::Relaxed);
        }
        // QuickJS 0.9 allocates while building an OOM backtrace. Keep a bounded
        // reserve for unwinding after the first failure; execution still terminates.
        let limit = if self.exhausted.load(Ordering::Relaxed) {
            self.limit
        } else {
            self.limit.saturating_sub((self.limit / 8).min(64 * 1024))
        };
        let allowed = Self::charge(size)
            .and_then(|new| self.used.checked_sub(old)?.checked_add(new))
            .is_some_and(|total| total <= limit);
        if !allowed {
            self.exhausted.store(true, Ordering::Relaxed);
        }
        allowed
    }

    fn failed(&self, ptr: *mut u8) {
        if ptr.is_null() {
            self.exhausted.store(true, Ordering::Relaxed);
        }
    }
}

// SAFETY: Allocation, alignment, reallocation and freeing are delegated to RustAllocator.
// Accounting never changes pointers or layouts, and failed reallocations retain the old charge.
unsafe impl Allocator for LimitedAllocator {
    fn alloc(&mut self, size: usize) -> *mut u8 {
        if !self.permits(0, size) {
            return std::ptr::null_mut();
        }
        let ptr = RustAllocator.alloc(size);
        self.failed(ptr);
        if !ptr.is_null() {
            self.used += Self::charge(size).unwrap();
        }
        ptr
    }

    fn calloc(&mut self, count: usize, size: usize) -> *mut u8 {
        let Some(bytes) = count.checked_mul(size) else {
            self.exhausted.store(true, Ordering::Relaxed);
            return std::ptr::null_mut();
        };
        if bytes == 0 || !self.permits(0, bytes) {
            return std::ptr::null_mut();
        }
        let ptr = RustAllocator.calloc(count, size);
        self.failed(ptr);
        if !ptr.is_null() {
            self.used += Self::charge(bytes).unwrap();
        }
        ptr
    }

    unsafe fn dealloc(&mut self, ptr: *mut u8) {
        if ptr.is_null() {
            return;
        }
        // SAFETY: The trait caller supplies a live allocation from this allocator.
        unsafe {
            self.used -= Self::charge(RustAllocator::usable_size(ptr)).unwrap();
            RustAllocator.dealloc(ptr);
        }
    }

    unsafe fn realloc(&mut self, ptr: *mut u8, size: usize) -> *mut u8 {
        if ptr.is_null() {
            return self.alloc(size);
        }
        if size == 0 {
            // SAFETY: The trait caller supplies a live allocation from this allocator.
            unsafe {
                self.dealloc(ptr);
            }
            return std::ptr::null_mut();
        }
        // SAFETY: The pointer was allocated through RustAllocator and is still live.
        let old = Self::charge(unsafe { RustAllocator::usable_size(ptr) }).unwrap();
        if !self.permits(old, size) {
            return std::ptr::null_mut();
        }
        // SAFETY: The pointer and its layout belong to the delegated allocator.
        let result = unsafe { RustAllocator.realloc(ptr, size) };
        self.failed(result);
        if !result.is_null() {
            self.used = self.used - old + Self::charge(size).unwrap();
        }
        result
    }

    unsafe fn usable_size(ptr: *mut u8) -> usize {
        // SAFETY: The trait caller supplies an allocation made by the delegated allocator.
        unsafe { RustAllocator::usable_size(ptr) }
    }
}
