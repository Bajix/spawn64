use std::{
    cell::UnsafeCell,
    future::Future,
    mem::{self},
    pin::Pin,
    ptr::{self, addr_of},
    sync::atomic::{AtomicPtr, AtomicU64, Ordering},
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
    usize,
};

use array_init::array_init;
use const_array_init::const_arr;

use crate::runtime::{Scheduler, RUNTIME};

const IDX: usize = (1 << 6) - 1;
const IDX_MASK: usize = !IDX;

#[derive(Clone, Copy)]
pub(crate) struct Index<'a, T> {
    pub(crate) list: &'a T,
    pub(crate) idx: usize,
}

impl<'a> Index<'a, TaskArena> {
    pub(crate) fn handle(&self) -> &TaskHandle {
        &self.list.tasks[self.idx]
    }

    #[inline(always)]
    fn has_task(&self) -> bool {
        (self.list.occupancy.load(Ordering::Acquire) & (1 << self.idx)).ne(&0)
    }

    #[inline(always)]
    fn raw_waker(&self) -> RawWaker {
        unsafe fn clone(ptr: *const ()) -> RawWaker {
            RawWaker::new(ptr, &VTABLE)
        }

        unsafe fn wake(ptr: *const ()) {
            let slot: Index<TaskArena> = Index::from_raw(ptr as *mut ());

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

        handle.next.swap(ptr::null_mut(), Ordering::AcqRel)
    }

    #[inline(always)]
    pub(crate) fn set_as_occupied(&self) -> u64 {
        let occupancy_bit = 1 << self.idx;

        let occupied = self
            .list
            .occupancy
            .fetch_or(occupancy_bit, Ordering::AcqRel)
            | occupancy_bit;

        if occupied.eq(&u64::MAX) {
            self.list.free_list_index().set_as_full();
        }

        occupied
    }

    pub fn release_occupancy(&self) {
        let occupancy = self
            .list
            .occupancy
            .fetch_and(!(1 << self.idx), Ordering::AcqRel);

        if occupancy.eq(&u64::MAX) {
            self.list.free_list_index().set_as_available();
        }
    }

    pub(crate) fn next_index(&'a self, occupancy: Option<u64>) -> Index<'a, TaskArena> {
        let occupancy = occupancy.unwrap_or_else(|| self.list.occupancy.load(Ordering::Acquire));

        // Isolate lowest clear bit.
        let low_bit = !occupancy & (occupancy.wrapping_add(1));

        if low_bit.ne(&0) {
            let idx = low_bit.trailing_zeros() as usize;
            Index::new(&self.list, idx)
        } else {
            let free_list_index = self.list.free_list_index();
            let full = free_list_index.set_as_full();
            let index = free_list_index.next_index(Some(full));

            unsafe { mem::transmute::<_, Index<'a, TaskArena>>(index) }
        }
    }
}

impl<'a, T> Index<'a, T> {
    pub(crate) fn new(list: &'a T, idx: usize) -> Self {
        Self { list, idx }
    }

    #[inline(always)]
    #[cfg(not(feature = "strict_provenance"))]
    pub(crate) unsafe fn from_raw(ptr: *mut ()) -> Self {
        Self {
            list: &*((ptr as usize & IDX_MASK) as *const _),
            idx: ptr as usize & IDX,
        }
    }

    #[inline(always)]
    #[cfg(feature = "strict_provenance")]
    pub(crate) unsafe fn from_raw(ptr: *mut ()) -> Self {
        Self {
            list: &*(ptr.map_addr(|addr| addr & IDX_MASK) as *const _),
            idx: ptr as usize & IDX,
        }
    }

    #[inline(always)]
    #[cfg(not(feature = "strict_provenance"))]
    pub(crate) fn into_raw(&self) -> *mut () {
        ((addr_of!(*self.list) as usize) | self.idx) as *mut ()
    }

    #[inline(always)]
    #[cfg(feature = "strict_provenance")]
    pub(crate) fn into_raw(&self) -> *mut () {
        addr_of!(*self.list).map_addr(|addr| addr | self.idx) as *mut ()
    }
}

impl<'a, T> Index<'a, FreeList<T>>
where
    T: for<'b> From<&'b Index<'b, FreeList<T>>>,
{
    #[inline(always)]
    fn set_as_full(&self) -> u64 {
        let full_bit = 1 << self.idx;
        let full = self.list.full.fetch_or(full_bit, Ordering::AcqRel) | full_bit;

        if full.eq(&u64::MAX) {
            if let Some(index) = self.list.owning_index() {
                index.set_as_full();
            }
        }

        full
    }

    fn set_as_available(&self) {
        let was_full = self.list.full.fetch_and(!(1 << self.idx), Ordering::AcqRel);

        if was_full.eq(&u64::MAX) {
            if let Some(index) = self.list.owning_index() {
                index.set_as_available();
            }
        }
    }

    fn next_index(&'a self, full: Option<u64>) -> Index<'a, T> {
        let full = full.unwrap_or_else(|| self.list.full.load(Ordering::Acquire));

        // Isolate lowest clear bit.
        let low_bit = !full & (full.wrapping_add(1));

        if low_bit.ne(&0) {
            let idx = low_bit.trailing_zeros() as usize;
            Index::new(self.list.get_or_init_index(idx), idx)
        } else {
            // The list has no clear bits; it is full

            if let Some(index) = self.list.owning_index() {
                let full = index.set_as_full();
                let freelist_index = index.next_index(Some(full));
                let index = freelist_index.next_index(None);

                // Unify `a lifetime
                unsafe { mem::transmute::<Index<'_, T>, Index<'a, T>>(index) }
            } else {
                // Create owning list for the current full list and a sibling
                let mut parent: Box<FreeList<FreeList<T>>> = Box::new(FreeList::default());

                // Add as first list but mark as full
                *parent.full.get_mut() = 1;
                *parent.slots[0].get_mut() = addr_of!(*self.list);

                let mut sibling: Box<FreeList<T>> = Box::new(FreeList::default());
                *sibling.parent.get_mut() = Index::new(&parent, 1).into_raw();

                let index = Index::new(sibling.get_or_init_index(0), 0);

                // Unify `a lifetime
                let index = unsafe { mem::transmute::<Index<'_, T>, Index<'a, T>>(index) };

                *parent.slots[1].get_mut() = Box::into_raw(sibling) as *const FreeList<T>;

                self.list
                    .parent
                    .store(Box::into_raw(parent) as *mut (), Ordering::Release);

                index
            }
        }
    }
}

impl<'a, T> From<&Index<'a, FreeList<FreeList<T>>>> for FreeList<T>
where
    T: From<&'a Index<'a, FreeList<T>>>,
{
    fn from(index: &Index<'a, FreeList<FreeList<T>>>) -> Self {
        FreeList {
            full: AtomicU64::new(0),
            slots: array_init(|_| UnsafeCell::new(ptr::null())),
            parent: AtomicPtr::new(index.into_raw()),
        }
    }
}

pub(crate) struct TaskHandle {
    pub(crate) task: UnsafeCell<Option<Pin<Box<dyn Future<Output = ()> + 'static>>>>,
    pub(crate) next: AtomicPtr<()>,
}

impl TaskHandle {
    pub(crate) const fn new() -> Self {
        TaskHandle {
            task: UnsafeCell::new(None),
            next: AtomicPtr::new(ptr::null_mut()),
        }
    }
}

pub(crate) struct FreeList<T> {
    pub(crate) full: AtomicU64,
    pub(crate) slots: [UnsafeCell<*const T>; 64],
    pub(crate) parent: AtomicPtr<()>,
}

impl<T> Default for FreeList<T> {
    fn default() -> Self {
        FreeList {
            full: AtomicU64::new(0),
            slots: array_init(|_| UnsafeCell::new(ptr::null())),
            parent: AtomicPtr::new(ptr::null_mut()),
        }
    }
}

impl<T> FreeList<T> {
    /// Return index within parent list
    fn owning_index<'a>(&'a self) -> Option<Index<'a, FreeList<FreeList<T>>>> {
        let ptr = self.parent.load(Ordering::Acquire);

        if !ptr.is_null() {
            Some(unsafe { Index::from_raw(ptr) })
        } else {
            None
        }
    }

    fn get_or_init_index(&self, idx: usize) -> &T
    where
        T: for<'a> From<&'a Index<'a, FreeList<T>>>,
    {
        let ptr = self.slots[idx].get();

        unsafe {
            if (*ptr).is_null() {
                *ptr = Box::into_raw(Box::new(T::from(&Index::new(self, idx)))) as *const T;
            }
            &**ptr
        }
    }
}

impl<'a, T> From<&Index<'a, FreeList<FreeList<T>>>> for FreeList<FreeList<T>>
where
    FreeList<T>: for<'b> From<&'b Index<'b, FreeList<T>>>,
{
    fn from(index: &Index<'a, FreeList<FreeList<T>>>) -> Self {
        FreeList {
            full: AtomicU64::new(0),
            slots: array_init(|_| UnsafeCell::new(ptr::null())),
            parent: AtomicPtr::new(index.into_raw()),
        }
    }
}

impl FreeList<TaskArena> {
    pub(crate) const fn new() -> Self {
        FreeList {
            full: AtomicU64::new(0),
            slots: const_arr!([UnsafeCell<*const TaskArena>; 64], |_| UnsafeCell::new(
                ptr::null()
            )),
            parent: AtomicPtr::new(ptr::null_mut()),
        }
    }
}

/// An index tagged pointer to an `TaskArena` in a `FreeList`
pub(crate) struct RawIndex(*const FreeList<TaskArena>);

impl<'a> From<&Index<'a, FreeList<TaskArena>>> for RawIndex {
    fn from(index: &Index<'a, FreeList<TaskArena>>) -> Self {
        RawIndex(index.into_raw() as *const FreeList<TaskArena>)
    }
}

impl RawIndex {
    fn as_index<'a>(&'a self) -> Index<'a, FreeList<TaskArena>> {
        if self.0.is_null() {
            // Only the statically allocated arena will be without a free list pointer
            Index::new(&RUNTIME.free_list, 0)
        } else {
            unsafe { Index::from_raw(self.0 as *mut ()) }
        }
    }
}

#[repr(align(64))]
pub(crate) struct TaskArena {
    pub(crate) occupancy: AtomicU64,
    pub(crate) tasks: [TaskHandle; 64],
    pub(crate) free_list: RawIndex,
}

impl TaskArena {
    pub(crate) const fn new() -> Self {
        TaskArena {
            occupancy: AtomicU64::new(0),
            #[allow(clippy::declare_interior_mutable_const)]
            tasks: const_arr!([TaskHandle; 64], |_| TaskHandle::new()),
            free_list: RawIndex(ptr::null()),
        }
    }

    /// Index within free list
    fn free_list_index<'a>(&'a self) -> Index<'a, FreeList<TaskArena>> {
        self.free_list.as_index()
    }
}

impl<'a> From<&Index<'a, FreeList<TaskArena>>> for TaskArena {
    fn from(index: &Index<'a, FreeList<TaskArena>>) -> Self {
        TaskArena {
            occupancy: AtomicU64::new(0),
            #[allow(clippy::declare_interior_mutable_const)]
            tasks: const_arr!([TaskHandle; 64], |_| TaskHandle::new()),
            free_list: index.into(),
        }
    }
}
