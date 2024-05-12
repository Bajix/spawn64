use std::{cell::UnsafeCell, future::Future, pin::Pin, ptr, sync::atomic::AtomicPtr};

pub(crate) struct TaskHandle {
    task: UnsafeCell<Option<Pin<Box<dyn Future<Output = ()> + 'static>>>>,
    pub(crate) next: AtomicPtr<()>,
}

impl TaskHandle {
    pub(crate) const fn new() -> Self {
        TaskHandle {
            task: UnsafeCell::new(None),
            next: AtomicPtr::new(ptr::null_mut()),
        }
    }

    pub(crate) unsafe fn task_mut(
        &self,
    ) -> &mut Option<Pin<Box<dyn Future<Output = ()> + 'static>>> {
        &mut *self.task.get()
    }
}
