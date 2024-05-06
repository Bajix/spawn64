use bitintr::Blcic;
use const_array_init::const_arr;
use js_sys::{
    wasm_bindgen::{closure::Closure, JsValue},
    Promise,
};
use std::{
    cell::UnsafeCell,
    future::Future,
    pin::Pin,
    sync::atomic::AtomicU64,
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};
use std::{
    ptr::addr_of,
    sync::atomic::{AtomicUsize, Ordering},
};
struct TaskHandle {
    task: UnsafeCell<Option<Pin<Box<dyn Future<Output = ()> + 'static>>>>,
    next: AtomicUsize,
}

impl TaskHandle {
    const fn new() -> Self {
        TaskHandle {
            task: UnsafeCell::new(None),
            next: AtomicUsize::new(0),
        }
    }

    fn raw_waker(&'static self) -> RawWaker {
        unsafe fn clone(ptr: *const ()) -> RawWaker {
            RawWaker::new(ptr, &VTABLE)
        }

        unsafe fn wake(ptr: *const ()) {
            let ptr = ptr as *const TaskHandle;
            let handle = &*ptr;
            let next = handle.next.load(Ordering::Acquire) as *const TaskHandle;

            if next.is_null() {
                let tail = RUNTIME.tail.swap(ptr as usize, Ordering::AcqRel) as *const TaskHandle;

                if tail.is_null() {
                    Scheduler::schedule_tasks(ptr);
                } else if tail.ne(&ptr) {
                    handle.next.store(tail as usize, Ordering::Release);
                }
            }
        }

        unsafe fn drop(_ptr: *const ()) {}

        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake, drop);

        RawWaker::new(addr_of!(*self) as *const (), &VTABLE)
    }

    unsafe fn poll(&'static self) {
        let poll = match &mut *self.task.get() {
            Some(task) => {
                let waker = Waker::from_raw(self.raw_waker());
                let mut cx = Context::from_waker(&waker);

                Some(task.as_mut().poll(&mut cx))
            }
            None => None,
        };

        if poll.eq(&Some(Poll::Ready(()))) {
            *(&mut *self.task.get()) = None;
            let idx = addr_of!(*self).offset_from(addr_of!(RUNTIME.tasks[0])) as usize & 63;
            RUNTIME.occupancy.fetch_and(!(1 << idx), Ordering::Release);
        }
    }
}

struct Scheduler {
    promise: Promise,
    callback: Closure<dyn FnMut(JsValue)>,
}

impl Scheduler {
    fn new() -> Self {
        Scheduler {
            promise: Promise::resolve(&JsValue::undefined()),
            callback: Closure::new(|_| unsafe { RUNTIME.process_batch() }),
        }
    }

    unsafe fn schedule_tasks(head: *const TaskHandle) {
        thread_local! {
            static SCHEDULER: Scheduler = Scheduler::new();
        }

        RUNTIME.head.store(head as usize, Ordering::Release);

        SCHEDULER.with(|scheduler| {
            let _ = scheduler.promise.then(&scheduler.callback);
        });
    }
}

struct Runtime {
    occupancy: AtomicU64,
    head: AtomicUsize,
    tail: AtomicUsize,
    tasks: [TaskHandle; 64],
}

unsafe impl Sync for Runtime {}

static RUNTIME: Runtime = Runtime::new();

impl Runtime {
    const fn new() -> Self {
        Runtime {
            occupancy: AtomicU64::new(0),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            tasks: const_arr!([TaskHandle; 64], |_| TaskHandle::new()),
        }
    }

    fn next_slot(&self) -> Option<u32> {
        let occupancy = self.occupancy.load(Ordering::Acquire);

        let least_significant_bit = occupancy.blcic();

        if least_significant_bit.ne(&0) {
            self.occupancy
                .fetch_or(least_significant_bit, Ordering::Release);

            Some(least_significant_bit.trailing_zeros())
        } else {
            None
        }
    }

    unsafe fn process_batch(&'static self) {
        let mut head = self.head.swap(0, Ordering::AcqRel) as *const TaskHandle;
        self.tail.store(0, Ordering::Release);

        while !head.is_null() {
            (&*head).poll();
            head = (&*head).next.swap(0, Ordering::AcqRel) as *const TaskHandle;
        }
    }
}

#[inline]
/// Runs a Rust Future on the current thread.
///
/// # Panics
///
/// This function panics if the threshold of 64 concurrent futures is exceeded
///
pub fn spawn_local<F>(future: F)
where
    F: Future<Output = ()> + 'static,
{
    let idx = RUNTIME
        .next_slot()
        .expect("`spawn64` can only manage 64 concurrent futures. Use `wasm-bindgen-futures` instead");

    let handle = &RUNTIME.tasks[idx as usize];

    unsafe {
        *(&mut *handle.task.get()) = Some(Box::pin(future));
    }

    let ptr = addr_of!(*handle);

    let tail = RUNTIME.tail.swap(ptr as usize, Ordering::AcqRel) as *const TaskHandle;

    if !tail.is_null() {
        unsafe {
            (&*tail).next.store(ptr as usize, Ordering::Release);
        }
    } else {
        unsafe {
            Scheduler::schedule_tasks(ptr);
        }
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod tests {
    use futures_channel::oneshot;
    use js_sys::{
        wasm_bindgen::{closure::Closure, JsValue},
        Promise,
    };
    use wasm_bindgen_futures::JsFuture;

    use super::spawn_local;
    use std::ops::FnMut;
    use wasm_bindgen_test::*;

    #[wasm_bindgen_test]
    async fn spawn_local_runs() {
        let (tx, rx) = oneshot::channel::<u32>();
        spawn_local(async {
            tx.send(42).unwrap();
        });
        assert_eq!(rx.await.unwrap(), 42);
    }

    #[wasm_bindgen_test]
    async fn spawn_local_nested() {
        let (ta, mut ra) = oneshot::channel::<u32>();
        let (ts, rs) = oneshot::channel::<u32>();
        let (tx, rx) = oneshot::channel::<u32>();
        // The order in which the various promises and tasks run is important!
        // We want, on different ticks each, the following things to happen
        // 1. A promise resolves, off of which we can spawn our inbetween assertion
        // 2. The outer task runs, spawns in the inner task, and the inbetween promise, then yields
        // 3. The inbetween promise runs and asserts that the inner task hasn't run
        // 4. The inner task runs
        // This depends crucially on two facts:
        // - JsFuture schedules on ticks independently from tasks
        // - The order of ticks is the same as the code flow
        let promise = Promise::resolve(&JsValue::null());

        spawn_local(async move {
            // Create a closure that runs in between the two ticks and
            // assert that the inner task hasn't run yet
            let inbetween = Closure::wrap(Box::new(move |_| {
                assert_eq!(
                    ra.try_recv().unwrap(),
                    None,
                    "Nested task should not have run yet"
                );
            }) as Box<dyn FnMut(JsValue)>);
            let inbetween = promise.then(&inbetween);
            spawn_local(async {
                ta.send(0xdead).unwrap();
                ts.send(0xbeaf).unwrap();
            });
            JsFuture::from(inbetween).await.unwrap();
            assert_eq!(
                rs.await.unwrap(),
                0xbeaf,
                "Nested task should run eventually"
            );
            tx.send(42).unwrap();
        });

        assert_eq!(rx.await.unwrap(), 42);
    }

    #[wasm_bindgen_test]
    async fn spawn_local_err_no_exception() {
        let (tx, rx) = oneshot::channel::<u32>();
        spawn_local(async {});
        spawn_local(async {
            tx.send(42).unwrap();
        });
        let val = rx.await.unwrap();
        assert_eq!(val, 42);
    }
}
