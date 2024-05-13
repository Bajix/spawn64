use std::{
    ptr::{self, addr_of},
    sync::atomic::{AtomicU64, Ordering},
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
    usize,
};

use const_array_init::const_arr;
use once_cell::race::OnceBox;

use crate::{
    handle::TaskHandle,
    runtime::{Scheduler, RUNTIME},
};

const IDX: usize = (1 << 6) - 1;
const IDX_MASK: usize = !IDX;

#[derive(Clone, Copy)]
pub(crate) struct Index<'a> {
    pub(crate) arena: &'a TaskArena,
    idx: usize,
}

impl<'a> Index<'a> {
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

                let next = handle.next.load(Ordering::Acquire) as *const ();

                if next.is_null() {
                    let tail = RUNTIME.tail.swap(ptr as *mut (), Ordering::AcqRel);

                    if tail.is_null() {
                        Scheduler::schedule_polling(ptr as *mut ());
                    } else if tail.ne(&(ptr as *mut ())) {
                        handle.next.store(tail, Ordering::Release);
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

        let poll = match handle.task_mut() {
            Some(task) => {
                let waker = Waker::from_raw(self.raw_waker());
                let mut cx = Context::from_waker(&waker);

                Some(task.as_mut().poll(&mut cx))
            }
            None => None,
        };

        if poll.eq(&Some(Poll::Ready(()))) {
            *handle.task_mut() = None;
            self.arena
                .occupancy
                .fetch_and(!(1 << self.idx), Ordering::Release);
        }

        handle.next.swap(ptr::null_mut(), Ordering::AcqRel)
    }

    #[inline(always)]
    pub(crate) unsafe fn from_raw(ptr: *mut ()) -> Self {
        Self {
            arena: &*((ptr as usize & IDX_MASK) as *const TaskArena),
            idx: ptr as usize & IDX,
        }
    }

    #[inline(always)]
    pub(crate) fn into_raw(self) -> *mut () {
        ((addr_of!(*self.arena) as usize) | self.idx) as *mut ()
    }
}

#[repr(align(64))]
pub(crate) struct TaskArena {
    occupancy: AtomicU64,
    tasks: [TaskHandle; 64],
    next: OnceBox<TaskArena>,
}

impl TaskArena {
    pub(crate) const fn new() -> Self {
        TaskArena {
            occupancy: AtomicU64::new(0),
            tasks: const_arr!([TaskHandle; 64], |_| TaskHandle::new()),
            next: OnceBox::new(),
        }
    }
}

impl TaskArena {
    pub(crate) fn next_index(&self) -> Index {
        let mut occupancy = self.occupancy.load(Ordering::Acquire);

        let idx = loop {
            // Isolate lowest clear bit. See https://docs.rs/bitintr/latest/bitintr/trait.Blcic.html
            let least_significant_bit = !occupancy & (occupancy.wrapping_add(1));

            if least_significant_bit.ne(&0) {
                occupancy = self
                    .occupancy
                    .fetch_or(least_significant_bit, Ordering::AcqRel);

                if (occupancy & least_significant_bit).eq(&0) {
                    break least_significant_bit.trailing_zeros();
                }
            } else {
                return self
                    .next
                    .get_or_init(|| Box::new(TaskArena::new()))
                    .next_index();
            }
        };

        Index {
            arena: self,
            idx: idx as usize,
        }
    }
}
