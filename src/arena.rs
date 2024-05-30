use std::{
    cell::UnsafeCell,
    future::Future,
    pin::Pin,
    ptr::{self, addr_of},
    sync::atomic::{AtomicPtr, AtomicU64, Ordering},
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};

use const_array_init::const_arr;

use crate::runtime::{Scheduler, RUNTIME};

const IDX: usize = (1 << 6) - 1;
const IDX_MASK: usize = !IDX;

#[derive(Clone, Copy)]
pub(crate) struct Index<'a> {
    pub(crate) arena: &'a TaskArena,
    idx: usize,
}

impl<'a> Index<'a> {
    #[inline(always)]
    pub(crate) fn new(arena: &'a TaskArena, idx: usize) -> Self {
        Self { arena, idx }
    }

    #[inline(always)]
    pub(crate) fn handle(&self) -> &TaskHandle {
        &self.arena.tasks[self.idx]
    }

    #[inline(always)]
    fn has_task(&self) -> bool {
        (self.arena.occupancy.load(Ordering::Acquire) & (1 << self.idx)).ne(&0)
    }

    #[inline(always)]
    fn raw_waker(&self) -> RawWaker {
        unsafe fn clone(ptr: *const ()) -> RawWaker {
            RawWaker::new(ptr, &VTABLE)
        }

        unsafe fn wake(ptr: *const ()) {
            let slot = Index::from_raw(ptr as *mut ());

            if slot.has_task() {
                let handle = slot.handle();

                let next = handle.next_enqueued.load(Ordering::Acquire) as *const ();

                if next.is_null() {
                    let tail = RUNTIME.poll_tail.swap(ptr as *mut (), Ordering::AcqRel);

                    if tail.is_null() {
                        Scheduler::schedule_polling(ptr as *mut ());
                    } else if tail.ne(&(ptr as *mut ())) {
                        handle.next_enqueued.store(tail, Ordering::Release);
                    }
                }
            }
        }

        unsafe fn drop(_ptr: *const ()) {}

        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake, drop);

        RawWaker::new(self.into_raw(), &VTABLE)
    }

    #[inline(always)]
    pub(crate) unsafe fn poll(&self) -> *mut () {
        let handle = self.handle();

        let poll = match &mut *handle.task.get() {
            Some(task) => {
                let waker = Waker::from_raw(self.raw_waker());
                let mut cx = Context::from_waker(&waker);

                Some(task.as_mut().poll(&mut cx))
            }
            None => None,
        };

        if poll.eq(&Some(Poll::Ready(()))) {
            *handle.task.get() = None;
            self.release_occupancy();
        }

        handle.next_enqueued.swap(ptr::null_mut(), Ordering::AcqRel)
    }

    #[inline(always)]
    #[cfg(not(feature = "strict_provenance"))]
    pub(crate) unsafe fn from_raw(ptr: *mut ()) -> Self {
        Self {
            arena: &*((ptr as usize & IDX_MASK) as *const _),
            idx: ptr as usize & IDX,
        }
    }

    #[inline(always)]
    #[cfg(feature = "strict_provenance")]
    pub(crate) unsafe fn from_raw(ptr: *mut ()) -> Self {
        Self {
            arena: &*(ptr.map_addr(|addr| addr & IDX_MASK) as *const _),
            idx: ptr as usize & IDX,
        }
    }

    #[inline(always)]
    #[cfg(not(feature = "strict_provenance"))]
    pub(crate) fn into_raw(&self) -> *mut () {
        ((addr_of!(*self.arena) as usize) | self.idx) as *mut ()
    }

    #[inline(always)]
    #[cfg(feature = "strict_provenance")]
    pub(crate) fn into_raw(&self) -> *mut () {
        addr_of!(*self.arena).map_addr(|addr| addr | self.idx) as *mut ()
    }

    pub(crate) fn set_as_occupied(&self) -> u64 {
        let occupancy_bit = 1 << self.idx;

        self.arena
            .occupancy
            .fetch_or(occupancy_bit, Ordering::AcqRel)
            | occupancy_bit
    }

    pub(crate) fn release_occupancy(&self) {
        let occupancy = self
            .arena
            .occupancy
            .fetch_add(!(1 << self.idx), Ordering::AcqRel);

        if occupancy.eq(&u64::MAX) {
            let ptr = self.into_raw();
            let tail = RUNTIME.free_tail.swap(ptr, Ordering::AcqRel);

            if !tail.is_null() {
                unsafe {
                    Index::from_raw(tail)
                        .arena
                        .next
                        .store(ptr, Ordering::Release);
                }
            } else {
                let next = RUNTIME.next.load(Ordering::Acquire);

                unsafe {
                    Index::from_raw(next)
                        .arena
                        .next
                        .store(ptr, Ordering::Release);
                }
            }
        }
    }

    pub(crate) fn next_index(&'a self, occupancy: u64) -> Index<'a> {
        let low_bit = !occupancy & (occupancy.wrapping_add(1));

        if low_bit.ne(&0) {
            let idx = low_bit.trailing_zeros() as usize;
            Index::new(&self.arena, idx)
        } else {
            let next = self.arena.next.swap(ptr::null_mut(), Ordering::AcqRel);

            if !next.is_null() {
                unsafe { Index::from_raw(next) }
            } else {
                let arena: &'static TaskArena = Box::leak(Box::new(TaskArena::new()));
                Index::new(arena, 0)
            }
        }
    }
}

pub(crate) struct TaskHandle {
    pub(crate) task: UnsafeCell<Option<Pin<Box<dyn Future<Output = ()> + 'static>>>>,
    pub(crate) next_enqueued: AtomicPtr<()>,
}

impl TaskHandle {
    pub(crate) const fn new() -> Self {
        TaskHandle {
            task: UnsafeCell::new(None),
            next_enqueued: AtomicPtr::new(ptr::null_mut()),
        }
    }
}

#[repr(align(64))]
pub(crate) struct TaskArena {
    occupancy: AtomicU64,
    tasks: [TaskHandle; 64],
    next: AtomicPtr<()>,
}

impl TaskArena {
    pub(crate) const fn new() -> Self {
        TaskArena {
            occupancy: AtomicU64::new(0),
            #[allow(clippy::declare_interior_mutable_const)]
            tasks: const_arr!([TaskHandle; 64], |_| TaskHandle::new()),
            next: AtomicPtr::new(ptr::null_mut()),
        }
    }
}
