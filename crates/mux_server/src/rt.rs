//! §3.1 The async primitives the server uses, on whichever runtime it has.
//!
//! Natively the daemon runs on tokio and this module is a thin delegation —
//! same types, same semantics, no behaviour change. In the browser there is no
//! runtime, no thread pool and no timer wheel: the server is one cooperative
//! task pumped from JS, so `spawn` hands the future to the microtask queue and
//! `spawn_blocking` runs its closure inline. Callers get one API either way, so
//! the request handlers do not have to be written twice.

#[cfg(not(target_family = "wasm"))]
pub use native::*;
#[cfg(target_family = "wasm")]
pub use wasm::*;

#[cfg(not(target_family = "wasm"))]
mod native {
    pub use tokio::task::JoinHandle;
    use std::future::Future;
    use std::time::Duration;

    pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        tokio::spawn(future)
    }

    pub fn spawn_blocking<F, R>(body: F) -> JoinHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        tokio::task::spawn_blocking(body)
    }

    pub async fn sleep(duration: Duration) {
        tokio::time::sleep(duration).await
    }

    /// `Err` when `future` did not finish inside `duration`.
    pub async fn timeout<F: Future>(
        duration: Duration,
        future: F,
    ) -> Result<F::Output, Elapsed> {
        tokio::time::timeout(duration, future)
            .await
            .map_err(|_| Elapsed)
    }

    #[derive(Debug)]
    pub struct Elapsed;

    impl std::fmt::Display for Elapsed {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("operation timed out")
        }
    }

    impl std::error::Error for Elapsed {}
}

#[cfg(target_family = "wasm")]
mod wasm {
    use futures::channel::oneshot;
    use std::future::Future;
    use std::time::Duration;

    /// The handle a spawned task is awaited through.
    ///
    /// tokio reports a panicked or cancelled task through `JoinError`; the
    /// browser has neither unwinding to catch nor a cancellation channel, so
    /// the only failure this can report is the task's sender being dropped.
    pub struct JoinHandle<T>(oneshot::Receiver<T>);

    #[derive(Debug)]
    pub struct JoinError;

    impl std::fmt::Display for JoinError {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("the spawned task did not produce a value")
        }
    }

    impl std::error::Error for JoinError {}

    impl JoinError {
        /// Nothing cancels a browser task: `spawn` has no abort path, so the
        /// only way here is the sender being dropped.
        pub fn is_cancelled(&self) -> bool {
            false
        }
    }

    impl<T> JoinHandle<T> {
        /// Dropping the receiver is the closest thing to an abort; the task
        /// itself is already on the microtask queue and will run to completion.
        pub fn abort(&self) {}
    }

    impl<T> Future for JoinHandle<T> {
        type Output = Result<T, JoinError>;

        fn poll(
            mut self: std::pin::Pin<&mut Self>,
            context: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Self::Output> {
            std::pin::Pin::new(&mut self.0)
                .poll(context)
                .map(|result| result.map_err(|_| JoinError))
        }
    }

    /// Hands `future` to the microtask queue.
    ///
    /// There is one thread, so nothing here is `Send`; the bound is dropped
    /// rather than faked.
    pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        let (sender, receiver) = oneshot::channel();
        wasm_bindgen_futures::spawn_local(async move {
            let output = future.await;
            // The receiver is gone when the caller dropped the handle, which is
            // the ordinary "fire and forget" case rather than an error.
            let _: Result<(), _> = sender.send(output);
        });
        JoinHandle(receiver)
    }

    /// Runs `body` inline.
    ///
    /// There is no blocking pool to move it to, and every caller here is
    /// CPU-bound work that already had to finish before the response could be
    /// written.
    pub fn spawn_blocking<F, R>(body: F) -> JoinHandle<R>
    where
        F: FnOnce() -> R + 'static,
        R: 'static,
    {
        let (sender, receiver) = oneshot::channel();
        let _: Result<(), _> = sender.send(body());
        JoinHandle(receiver)
    }

    pub async fn sleep(duration: Duration) {
        gloo_timers::future::TimeoutFuture::new(duration.as_millis() as u32).await
    }

    #[derive(Debug)]
    pub struct Elapsed;

    impl std::fmt::Display for Elapsed {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("operation timed out")
        }
    }

    impl std::error::Error for Elapsed {}

    pub async fn timeout<F: Future>(
        duration: Duration,
        future: F,
    ) -> Result<F::Output, Elapsed> {
        let timer = gloo_timers::future::TimeoutFuture::new(duration.as_millis() as u32);
        futures::pin_mut!(future);
        futures::pin_mut!(timer);
        match futures::future::select(future, timer).await {
            futures::future::Either::Left((output, _)) => Ok(output),
            futures::future::Either::Right(_) => Err(Elapsed),
        }
    }
}

/// The unbounded channel the outbound envelope queue is built on.
///
/// tokio's and futures' unbounded channels differ in shape — `send` versus
/// `unbounded_send`, `recv().await` versus `Stream::next` — so the browser one
/// is wrapped to the tokio shape rather than rewriting every call site twice.
#[cfg(not(target_family = "wasm"))]
pub mod mpsc {
    pub use tokio::sync::mpsc::error;
    pub use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
}

#[cfg(target_family = "wasm")]
pub mod mpsc {
    pub mod error {
        /// Mirrors `tokio::sync::mpsc::error::SendError`: the receiver is gone.
        #[derive(Debug)]
        pub struct SendError<T>(pub T);

        impl<T> std::fmt::Display for SendError<T> {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("channel closed")
            }
        }

        impl<T: std::fmt::Debug> std::error::Error for SendError<T> {}
    }

    pub struct UnboundedSender<T>(futures::channel::mpsc::UnboundedSender<T>);

    impl<T> Clone for UnboundedSender<T> {
        fn clone(&self) -> Self {
            Self(self.0.clone())
        }
    }

    impl<T> UnboundedSender<T> {
        pub fn send(&self, value: T) -> Result<(), error::SendError<T>> {
            self.0
                .unbounded_send(value)
                .map_err(|failure| error::SendError(failure.into_inner()))
        }

        pub fn is_closed(&self) -> bool {
            self.0.is_closed()
        }
    }

    pub struct UnboundedReceiver<T>(futures::channel::mpsc::UnboundedReceiver<T>);

    impl<T> UnboundedReceiver<T> {
        pub async fn recv(&mut self) -> Option<T> {
            use futures::StreamExt as _;
            self.0.next().await
        }

        /// Takes a value only if one is already queued.
        pub fn try_recv(&mut self) -> Option<T> {
            match self.0.try_next() {
                Ok(value) => value,
                // Empty, or closed with nothing left: both mean "nothing now".
                Err(_) => None,
            }
        }
    }

    pub fn unbounded_channel<T>() -> (UnboundedSender<T>, UnboundedReceiver<T>) {
        let (sender, receiver) = futures::channel::mpsc::unbounded();
        (UnboundedSender(sender), UnboundedReceiver(receiver))
    }
}

/// One-shot reply channels, in the tokio shape.
#[cfg(not(target_family = "wasm"))]
pub mod oneshot {
    pub use tokio::sync::oneshot::{Receiver, Sender, channel};
}

#[cfg(target_family = "wasm")]
pub mod oneshot {
    pub use futures::channel::oneshot::{Receiver, Sender, channel};
}

/// A one-shot-ish wakeup, in the shape `tokio::sync::Notify` is used here.
#[cfg(not(target_family = "wasm"))]
pub use tokio::sync::Notify;

#[cfg(target_family = "wasm")]
pub use wasm_notify::Notify;

#[cfg(target_family = "wasm")]
mod wasm_notify {
    use futures::channel::oneshot;
    use parking_lot::Mutex;

    /// Only the shutdown ack uses this, and only ever with one waiter, so a
    /// list of pending senders is the whole implementation. A notify that
    /// arrives before anyone waits is remembered, matching tokio's
    /// store-one-permit behaviour for `notify_one`.
    #[derive(Default)]
    pub struct Notify {
        waiters: Mutex<Vec<oneshot::Sender<()>>>,
        permit: Mutex<bool>,
    }

    impl Notify {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn notify_one(&self) {
            if let Some(waiter) = self.waiters.lock().pop() {
                let _: Result<(), ()> = waiter.send(());
                return;
            }
            *self.permit.lock() = true;
        }

        pub async fn notified(&self) {
            {
                let mut permit = self.permit.lock();
                if *permit {
                    *permit = false;
                    return;
                }
            }
            let (sender, receiver) = oneshot::channel();
            self.waiters.lock().push(sender);
            // A dropped sender means the notifier went away; treat it as the
            // wakeup rather than hanging the caller forever.
            let _: Result<(), _> = receiver.await;
        }
    }
}
