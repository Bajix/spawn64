use js_sys::{
    wasm_bindgen::{closure::Closure, JsValue},
    Promise,
};
use std::sync::atomic::Ordering;
use std::{future::Future, ptr, sync::atomic::AtomicPtr};

use crate::arena::{Index, TaskArena};
pub(crate) struct Scheduler {
    promise: Promise,
    callback: Closure<dyn FnMut(JsValue)>,
}

impl Scheduler {
    fn new() -> Self {
        Scheduler {
            promise: Promise::resolve(&JsValue::undefined()),
            callback: Closure::new(|_| unsafe { RUNTIME.poll_enqueued() }),
        }
    }

    pub(crate) unsafe fn schedule_polling(head: *mut ()) {
        thread_local! {
            static SCHEDULER: Scheduler = Scheduler::new();
        }

        RUNTIME.head.store(head, Ordering::Release);

        SCHEDULER.with(|scheduler| {
            let _ = scheduler.promise.then(&scheduler.callback);
        });
    }
}

pub(crate) struct Runtime {
    pub(crate) head: AtomicPtr<()>,
    pub(crate) tail: AtomicPtr<()>,
    arena: TaskArena,
}

unsafe impl Sync for Runtime {}

pub(crate) static RUNTIME: Runtime = Runtime::new();

impl Runtime {
    const fn new() -> Self {
        Runtime {
            head: AtomicPtr::new(ptr::null_mut()),
            tail: AtomicPtr::new(ptr::null_mut()),
            arena: TaskArena::new(),
        }
    }

    unsafe fn poll_enqueued(&'static self) {
        let mut head = self.head.swap(ptr::null_mut(), Ordering::AcqRel);

        self.tail.store(ptr::null_mut(), Ordering::Release);

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
    let index = RUNTIME.arena.next_index();

    unsafe {
        *index.handle().task_mut() = Some(Box::pin(future));
    }

    let ptr = index.into_raw();

    let tail = RUNTIME.tail.swap(ptr, Ordering::AcqRel);

    if !tail.is_null() {
        unsafe {
            Index::from_raw(tail)
                .handle()
                .next
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
