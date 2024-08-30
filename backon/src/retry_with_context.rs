use std::future::Future;
use std::pin::Pin;
use std::task::ready;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;

use crate::backoff::BackoffBuilder;
use crate::sleep::MaybeSleeper;
use crate::Backoff;
use crate::DefaultSleeper;
use crate::Sleeper;

/// `RetryableWithContext` adds retry support for functions that produce futures with results
/// and context.
///
/// This means all types implementing `FnMut(Ctx) -> impl Future<Output = (Ctx, Result<T, E>)>`
/// can use `retry`.
///
/// Users must provide context to the function and can receive it back after the retry is completed.
///
/// # Example
///
/// Without context, we might encounter errors such as the following:
///
/// ```shell
/// error: captured variable cannot escape `FnMut` closure body
///    --> src/retry.rs:404:27
///     |
/// 400 |         let mut test = Test;
///     |             -------- variable defined here
/// ...
/// 404 |         let result = { || async { test.hello().await } }
///     |                         - ^^^^^^^^----^^^^^^^^^^^^^^^^
///     |                         | |       |
///     |                         | |       variable captured here
///     |                         | returns an `async` block that contains a reference to a captured variable, which then escapes the closure body
///     |                         inferred to be a `FnMut` closure
///     |
///     = note: `FnMut` closures only have access to their captured variables while they are executing...
///     = note: ...therefore, they cannot allow references to captured variables to escape
/// ```
///
/// However, with context support, we can implement it this way:
///
/// ```no_run
/// use anyhow::anyhow;
/// use anyhow::Result;
/// use backon::ExponentialBuilder;
/// use backon::RetryableWithContext;
///
/// struct Test;
///
/// impl Test {
///     async fn hello(&mut self) -> Result<usize> {
///         Err(anyhow!("not retryable"))
///     }
/// }
///
/// #[tokio::main(flavor = "current_thread")]
/// async fn main() -> Result<()> {
///     let mut test = Test;
///
///     // (Test, Result<usize>)
///     let (_, result) = {
///         |mut v: Test| async {
///             let res = v.hello().await;
///             (v, res)
///         }
///     }
///     .retry(ExponentialBuilder::default())
///     .context(test)
///     .await;
///
///     Ok(())
/// }
/// ```
pub trait RetryableWithContext<
    B: BackoffBuilder,
    T,
    E,
    Ctx,
    Fut: Future<Output = (Ctx, Result<T, E>)>,
    FutureFn: FnMut(Ctx) -> Fut,
>
{
    /// Generate a new retry
    fn retry(self, builder: B) -> RetryWithContext<B::Backoff, T, E, Ctx, Fut, FutureFn>;
}

impl<B, T, E, Ctx, Fut, FutureFn> RetryableWithContext<B, T, E, Ctx, Fut, FutureFn> for FutureFn
where
    B: BackoffBuilder,
    Fut: Future<Output = (Ctx, Result<T, E>)>,
    FutureFn: FnMut(Ctx) -> Fut,
{
    fn retry(self, builder: B) -> RetryWithContext<B::Backoff, T, E, Ctx, Fut, FutureFn> {
        RetryWithContext::new(self, builder.build())
    }
}

/// Retry struct generated by [`RetryableWithContext`].
pub struct RetryWithContext<
    B: Backoff,
    T,
    E,
    Ctx,
    Fut: Future<Output = (Ctx, Result<T, E>)>,
    FutureFn: FnMut(Ctx) -> Fut,
    SF: MaybeSleeper = DefaultSleeper,
    RF = fn(&E) -> bool,
    NF = fn(&E, Duration),
> {
    backoff: B,
    retryable: RF,
    notify: NF,
    future_fn: FutureFn,
    sleep_fn: SF,

    state: State<T, E, Ctx, Fut, SF::Sleep>,
}

impl<B, T, E, Ctx, Fut, FutureFn> RetryWithContext<B, T, E, Ctx, Fut, FutureFn>
where
    B: Backoff,
    Fut: Future<Output = (Ctx, Result<T, E>)>,
    FutureFn: FnMut(Ctx) -> Fut,
{
    /// Create a new retry.
    fn new(future_fn: FutureFn, backoff: B) -> Self {
        RetryWithContext {
            backoff,
            retryable: |_: &E| true,
            notify: |_: &E, _: Duration| {},
            future_fn,
            sleep_fn: DefaultSleeper::default(),
            state: State::Idle(None),
        }
    }
}

impl<B, T, E, Ctx, Fut, FutureFn, SF, RF, NF>
    RetryWithContext<B, T, E, Ctx, Fut, FutureFn, SF, RF, NF>
where
    B: Backoff,
    Fut: Future<Output = (Ctx, Result<T, E>)>,
    FutureFn: FnMut(Ctx) -> Fut,
    SF: Sleeper,
    RF: FnMut(&E) -> bool,
    NF: FnMut(&E, Duration),
{
    /// Set the sleeper for retrying.
    ///
    /// The sleeper should implement the [`Sleeper`] trait. The simplest way is to use a closure that returns a `Future<Output=()>`.
    ///
    /// If not specified, we use the [`DefaultSleeper`].
    pub fn sleep<SN: Sleeper>(
        self,
        sleep_fn: SN,
    ) -> RetryWithContext<B, T, E, Ctx, Fut, FutureFn, SN, RF, NF> {
        assert!(
            matches!(self.state, State::Idle(None)),
            "sleep must be set before context"
        );

        RetryWithContext {
            backoff: self.backoff,
            retryable: self.retryable,
            notify: self.notify,
            future_fn: self.future_fn,
            sleep_fn,
            state: State::Idle(None),
        }
    }

    /// Set the context for retrying.
    ///
    /// Context is used to capture ownership manually to prevent lifetime issues.
    pub fn context(
        self,
        context: Ctx,
    ) -> RetryWithContext<B, T, E, Ctx, Fut, FutureFn, SF, RF, NF> {
        RetryWithContext {
            backoff: self.backoff,
            retryable: self.retryable,
            notify: self.notify,
            future_fn: self.future_fn,
            sleep_fn: self.sleep_fn,
            state: State::Idle(Some(context)),
        }
    }

    /// Set the conditions for retrying.
    ///
    /// If not specified, all errors are considered retryable.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use anyhow::Result;
    /// use backon::ExponentialBuilder;
    /// use backon::Retryable;
    ///
    /// async fn fetch() -> Result<String> {
    ///     Ok(reqwest::get("https://www.rust-lang.org")
    ///         .await?
    ///         .text()
    ///         .await?)
    /// }
    ///
    /// #[tokio::main(flavor = "current_thread")]
    /// async fn main() -> Result<()> {
    ///     let content = fetch
    ///         .retry(ExponentialBuilder::default())
    ///         .when(|e| e.to_string() == "EOF")
    ///         .await?;
    ///     println!("fetch succeeded: {}", content);
    ///
    ///     Ok(())
    /// }
    /// ```
    pub fn when<RN: FnMut(&E) -> bool>(
        self,
        retryable: RN,
    ) -> RetryWithContext<B, T, E, Ctx, Fut, FutureFn, SF, RN, NF> {
        RetryWithContext {
            backoff: self.backoff,
            retryable,
            notify: self.notify,
            future_fn: self.future_fn,
            sleep_fn: self.sleep_fn,
            state: self.state,
        }
    }

    /// Set to notify for all retry attempts.
    ///
    /// When a retry happens, the input function will be invoked with the error and the sleep duration before pausing.
    ///
    /// If not specified, this operation does nothing.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::time::Duration;
    ///
    /// use anyhow::Result;
    /// use backon::ExponentialBuilder;
    /// use backon::Retryable;
    ///
    /// async fn fetch() -> Result<String> {
    ///     Ok(reqwest::get("https://www.rust-lang.org")
    ///         .await?
    ///         .text()
    ///         .await?)
    /// }
    ///
    /// #[tokio::main(flavor = "current_thread")]
    /// async fn main() -> Result<()> {
    ///     let content = fetch
    ///         .retry(ExponentialBuilder::default())
    ///         .notify(|err: &anyhow::Error, dur: Duration| {
    ///             println!("retrying error {:?} with sleeping {:?}", err, dur);
    ///         })
    ///         .await?;
    ///     println!("fetch succeeded: {}", content);
    ///
    ///     Ok(())
    /// }
    /// ```
    pub fn notify<NN: FnMut(&E, Duration)>(
        self,
        notify: NN,
    ) -> RetryWithContext<B, T, E, Ctx, Fut, FutureFn, SF, RF, NN> {
        RetryWithContext {
            backoff: self.backoff,
            retryable: self.retryable,
            notify,
            future_fn: self.future_fn,
            sleep_fn: self.sleep_fn,
            state: self.state,
        }
    }
}

/// State maintains internal state of retry.
enum State<T, E, Ctx, Fut: Future<Output = (Ctx, Result<T, E>)>, SleepFut: Future<Output = ()>> {
    Idle(Option<Ctx>),
    Polling(Fut),
    Sleeping((Option<Ctx>, SleepFut)),
}

impl<B, T, E, Ctx, Fut, FutureFn, SF, RF, NF> Future
    for RetryWithContext<B, T, E, Ctx, Fut, FutureFn, SF, RF, NF>
where
    B: Backoff,
    Fut: Future<Output = (Ctx, Result<T, E>)>,
    FutureFn: FnMut(Ctx) -> Fut,
    SF: Sleeper,
    RF: FnMut(&E) -> bool,
    NF: FnMut(&E, Duration),
{
    type Output = (Ctx, Result<T, E>);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Safety: This is safe because we don't move the `Retry` struct itself,
        // only its internal state.
        //
        // We do the exactly same thing like `pin_project` but without depending on it directly.
        let this = unsafe { self.get_unchecked_mut() };

        loop {
            match &mut this.state {
                State::Idle(ctx) => {
                    let ctx = ctx.take().expect("context must be valid");
                    let fut = (this.future_fn)(ctx);
                    this.state = State::Polling(fut);
                    continue;
                }
                State::Polling(fut) => {
                    // Safety: This is safe because we don't move the `Retry` struct and this fut,
                    // only its internal state.
                    //
                    // We do the exactly same thing like `pin_project` but without depending on it directly.
                    let mut fut = unsafe { Pin::new_unchecked(fut) };

                    let (ctx, res) = ready!(fut.as_mut().poll(cx));
                    match res {
                        Ok(v) => return Poll::Ready((ctx, Ok(v))),
                        Err(err) => {
                            // If input error is not retryable, return error directly.
                            if !(this.retryable)(&err) {
                                return Poll::Ready((ctx, Err(err)));
                            }
                            match this.backoff.next() {
                                None => return Poll::Ready((ctx, Err(err))),
                                Some(dur) => {
                                    (this.notify)(&err, dur);
                                    this.state =
                                        State::Sleeping((Some(ctx), this.sleep_fn.sleep(dur)));
                                    continue;
                                }
                            }
                        }
                    }
                }
                State::Sleeping((ctx, sl)) => {
                    // Safety: This is safe because we don't move the `Retry` struct and this fut,
                    // only its internal state.
                    //
                    // We do the exactly same thing like `pin_project` but without depending on it directly.
                    let mut sl = unsafe { Pin::new_unchecked(sl) };

                    ready!(sl.as_mut().poll(cx));
                    let ctx = ctx.take().expect("context must be valid");
                    this.state = State::Idle(Some(ctx));
                    continue;
                }
            }
        }
    }
}

#[cfg(test)]
#[cfg(any(feature = "tokio-sleep", feature = "gloo-timers-sleep"))]
mod tests {
    use std::time::Duration;

    use anyhow::{anyhow, Result};
    use tokio::sync::Mutex;

    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    #[cfg(not(target_arch = "wasm32"))]
    use tokio::test;

    use super::*;
    use crate::ExponentialBuilder;

    struct Test;

    impl Test {
        async fn hello(&mut self) -> Result<usize> {
            Err(anyhow!("not retryable"))
        }
    }

    #[test]
    async fn test_retry_with_not_retryable_error() -> Result<()> {
        let error_times = Mutex::new(0);

        let test = Test;

        let backoff = ExponentialBuilder::default().with_min_delay(Duration::from_millis(1));

        let (_, result) = {
            |mut v: Test| async {
                let mut x = error_times.lock().await;
                *x += 1;

                let res = v.hello().await;
                (v, res)
            }
        }
        .retry(backoff)
        .context(test)
        // Only retry If error message is `retryable`
        .when(|e| e.to_string() == "retryable")
        .await;

        assert!(result.is_err());
        assert_eq!("not retryable", result.unwrap_err().to_string());
        // `f` always returns error "not retryable", so it should be executed
        // only once.
        assert_eq!(*error_times.lock().await, 1);
        Ok(())
    }
}
