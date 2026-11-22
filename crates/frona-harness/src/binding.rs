//! Single-use host bindings for SDK runs.
use crate::{BudgetLimits, Harness, Incomplete, Run, Structured, Text};
use futures::{FutureExt, TryFutureExt, future::BoxFuture};
use std::future::{Future, IntoFuture};

type Initialize<'a, E> =
    Box<dyn FnOnce(Run<'static>, String) -> BoxFuture<'a, Result<Run<'static>, E>> + Send + 'a>;

pub struct RunBinding<'a, E> {
    user_id: String,
    chat_id: Option<String>,
    initialize: Initialize<'a, E>,
}

impl Harness<'static> {
    /// Bind one run to an owner and optional chat. The initializer is polled
    /// only when the run is driven, and can attach host tools and history.
    pub fn bind<'a, E, F, Fut>(
        self,
        user_id: String,
        chat_id: Option<String>,
        initialize: F,
    ) -> Harness<'static, RunBinding<'a, E>>
    where
        F: FnOnce(Run<'static>, String) -> Fut + Send + 'a,
        Fut: Future<Output = Result<Run<'static>, E>> + Send + 'a,
    {
        Harness {
            model: self.model,
            binding: RunBinding {
                user_id,
                chat_id,
                initialize: Box::new(move |run, input| initialize(run, input).boxed()),
            },
        }
    }
}

impl<'a, E: Send + 'a> Harness<'static, RunBinding<'a, E>> {
    pub fn user_id(&self) -> &str {
        &self.binding.user_id
    }
    pub fn chat_id(&self) -> Option<&str> {
        self.binding.chat_id.as_deref()
    }

    /// Calling `run` consumes the bound harness, even if the run is never awaited.
    ///
    /// ```compile_fail,E0382
    /// use frona_harness::{Harness, RunBinding};
    ///
    /// fn run_twice(harness: Harness<'static, RunBinding<'_, ()>>) {
    ///     let first = harness.run("First prompt");
    ///     let second = harness.run("Second prompt"); // use of moved value
    /// }
    /// ```
    pub fn run(self, input: impl Into<String>) -> BoundRun<'a, E> {
        let input = input.into();
        let run = Harness::new(self.model).run(input.clone());
        let initialize = self.binding.initialize;
        BoundRun {
            prepare: async move { initialize(run, input).await }.boxed(),
        }
    }
}

/// An SDK run with deferred host initialization. Configuration and drop perform no I/O.
pub struct BoundRun<'a, E, Mode = Text> {
    prepare: BoxFuture<'a, Result<Run<'static, Mode>, E>>,
}

impl<'a, E: Send + 'a, Mode: 'static> BoundRun<'a, E, Mode> {
    /// Apply SDK configuration after host initialization.
    pub fn configure<Next: 'static>(
        self,
        configure: impl FnOnce(Run<'static, Mode>) -> Run<'static, Next> + Send + 'a,
    ) -> BoundRun<'a, E, Next> {
        BoundRun {
            prepare: self.prepare.map_ok(configure).boxed(),
        }
    }

    pub fn budget_limits(self, limits: BudgetLimits) -> Self {
        self.configure(move |run| run.budget_limits(limits))
    }

    pub fn structured<T: schemars::JsonSchema + 'static>(self) -> BoundRun<'a, E, Structured<T>> {
        self.configure(|run| run.structured::<T>())
    }

    /// Access the prepared SDK run for manual driving, inspection, or progress subscriptions.
    pub async fn start(self) -> Result<Run<'static, Mode>, E> {
        self.prepare.await
    }
}

impl<'a, E, T: schemars::JsonSchema + serde::de::DeserializeOwned + 'static>
    BoundRun<'a, E, Structured<T>>
where
    E: Send + From<Incomplete<T>> + 'a,
{
    pub fn submission_attempts(self, maximum: usize) -> Self {
        self.configure(move |run| run.submission_attempts(maximum))
    }

    pub async fn verify<V: std::fmt::Debug>(
        self,
        check: impl AsyncFnMut(&T) -> Result<Result<(), String>, V>,
    ) -> Result<T, E> {
        self.prepare.await?.verify(check).await.map_err(E::from)
    }
}

impl<'a, E: Send + From<Incomplete> + 'a, Mode: 'static, T> IntoFuture for BoundRun<'a, E, Mode>
where
    for<'run> Run<'run, Mode>: IntoFuture<Output = Result<T, Incomplete>, IntoFuture: Send>,
{
    type Output = Result<T, E>;
    type IntoFuture = BoxFuture<'a, Self::Output>;

    fn into_future(self) -> Self::IntoFuture {
        async move { self.prepare.await?.await.map_err(E::from) }.boxed()
    }
}
