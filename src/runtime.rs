use std::{
    future::Future,
    ptr::{self, addr_of},
    sync::atomic::{AtomicPtr, Ordering},
};

use js_sys::{
    wasm_bindgen::{closure::Closure, JsValue},
    Promise,
};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen]
    fn queueMicrotask(closure: &Closure<dyn FnMut(JsValue)>);

    type Global;

    #[wasm_bindgen(method, getter, js_name = queueMicrotask)]
    fn hasQueueMicrotask(this: &Global) -> JsValue;
}

use crate::arena::{Index, TaskArena};

pub(crate) enum Scheduler {
    Microtask {
        poll_enqueued: Closure<dyn FnMut(JsValue)>,
    },
    Promise {
        promise: Promise,
        poll_enqueued: Closure<dyn FnMut(JsValue)>,
    },
}

impl Scheduler {
    fn new() -> Self {
        let has_queue_microtask = js_sys::global()
            .unchecked_into::<Global>()
            .hasQueueMicrotask()
            .is_function();

        let poll_enqueued = Closure::new(|_| unsafe { RUNTIME.poll_enqueued() });

        if has_queue_microtask {
            Scheduler::Microtask { poll_enqueued }
        } else {
            Scheduler::Promise {
                promise: Promise::resolve(&JsValue::undefined()),
                poll_enqueued,
            }
        }
    }

    pub(crate) unsafe fn schedule_polling(head: *mut ()) {
        thread_local! {
            static SCHEDULER: Scheduler = Scheduler::new();
        }

        RUNTIME.poll_head.store(head, Ordering::Release);

        SCHEDULER.with(|scheduler| match scheduler {
            Scheduler::Microtask { poll_enqueued } => {
                queueMicrotask(poll_enqueued);
            }
            Scheduler::Promise {
                promise,
                poll_enqueued,
            } => {
                let _ = promise.then(poll_enqueued);
            }
        });
    }
}

pub(crate) struct Runtime {
    pub(crate) poll_head: AtomicPtr<()>,
    pub(crate) poll_tail: AtomicPtr<()>,
    pub(crate) free_tail: AtomicPtr<()>,
    pub(crate) next: AtomicPtr<()>,
    arena: TaskArena,
}

unsafe impl Sync for Runtime {}

pub(crate) static RUNTIME: Runtime = Runtime::new();

impl Runtime {
    const fn new() -> Self {
        Runtime {
            poll_head: AtomicPtr::new(ptr::null_mut()),
            poll_tail: AtomicPtr::new(ptr::null_mut()),
            free_tail: AtomicPtr::new(ptr::null_mut()),
            next: AtomicPtr::new(ptr::null_mut()),
            arena: TaskArena::new(),
        }
    }

    unsafe fn poll_enqueued(&'static self) {
        let mut head = self.poll_head.swap(ptr::null_mut(), Ordering::AcqRel);

        self.poll_tail.store(ptr::null_mut(), Ordering::Release);

        while !head.is_null() {
            head = Index::from_raw(head).poll();
        }
    }
}

/// Runs a Rust Future on the current thread.
pub fn spawn_local<F>(future: F)
where
    F: Future<Output = ()> + 'static,
{
    let (ptr, index) = {
        let ptr = RUNTIME.next.load(Ordering::Acquire);

        if ptr.is_null() {
            (
                addr_of!(RUNTIME.arena) as *mut (),
                Index::new(&RUNTIME.arena, 0),
            )
        } else {
            unsafe { (ptr, Index::from_raw(ptr)) }
        }
    };

    let occupancy = index.set_as_occupied();

    RUNTIME
        .next
        .store(index.next_index(occupancy).into_raw(), Ordering::Release);

    unsafe {
        *index.handle().task.get() = Some(Box::pin(future));
    }

    let tail = RUNTIME.poll_tail.swap(ptr, Ordering::AcqRel);

    if !tail.is_null() {
        unsafe {
            Index::from_raw(tail)
                .handle()
                .next_enqueued
                .store(ptr, Ordering::Release);
        }
    } else {
        unsafe {
            Scheduler::schedule_polling(ptr);
        }
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod tests {
    use std::ops::FnMut;

    use futures_channel::oneshot;
    use js_sys::{
        wasm_bindgen::{closure::Closure, JsValue},
        Promise,
    };
    use wasm_bindgen_futures::JsFuture;
    use wasm_bindgen_test::*;

    use super::spawn_local;

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
