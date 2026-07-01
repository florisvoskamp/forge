//! Sandboxed JS workflow-script engine for Forge's mesh-routed multi-agent orchestration
//! (docs/rfcs/forge-workflow.md). This module is currently a spike (PR0 of that plan): it proves
//! the async host-function → JS `await` bridge works end-to-end on rquickjs before any real
//! `agent()`/`pipeline()`/`parallel()` host functions are built on top of it.
//!
//! The bridge: an `async fn` wrapped in `rquickjs::prelude::Async` and registered as a JS global
//! becomes directly `await`-able from script code, returning a real JS Promise
//! (`Async<F>`'s `IntoJsFunc` impl wraps the future in `Promised`, which `ctx.spawn`s it onto the
//! runtime's own executor). That executor only makes progress while something polls it — for a
//! session-lived runtime the right pattern is `tokio::spawn(rt.drive())` once, kept running in
//! the background for the runtime's whole lifetime, rather than manually polling `rt.idle()`
//! around every call.

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use rquickjs::prelude::Async;
    use rquickjs::{AsyncContext, AsyncRuntime, CatchResultExt, Function, Promise};

    /// A stand-in for a real subagent turn (which takes 10-60s of real LLM latency in the
    /// eventual feature) — sleeps, then returns a string, proving the future genuinely suspends
    /// the JS `await` rather than blocking synchronously.
    async fn spike_agent(label: String) -> rquickjs::Result<String> {
        tokio::time::sleep(Duration::from_millis(30)).await;
        Ok(format!("agent done: {label}"))
    }

    /// Evaluates `(async () => { ... })()` (an async-IIFE, since a bare top-level `await` isn't
    /// valid outside a module) with one global `agent(label)` host function registered, returning
    /// whatever the script's promise resolves to.
    async fn eval_async(rt: &AsyncRuntime, script: &str) -> String {
        let ctx = AsyncContext::full(rt).await.expect("create context");
        ctx.async_with(async |ctx| {
            let agent_fn = Function::new(ctx.clone(), Async(spike_agent))
                .unwrap()
                .with_name("agent")
                .unwrap();
            ctx.globals().set("agent", agent_fn).unwrap();

            let iife: Function = ctx.eval(script).catch(&ctx).unwrap();
            let promise: Promise = iife.call(()).catch(&ctx).unwrap();
            promise.into_future::<String>().await.catch(&ctx).unwrap()
        })
        .await
    }

    /// Proves the core bridge: a script's `await agent(...)` genuinely suspends until the
    /// underlying Rust future (a real `tokio::time::sleep`) completes, and the resolved value
    /// round-trips back into JS and out again as a Rust `String`.
    #[tokio::test]
    async fn await_agent_resolves_after_the_real_sleep_completes() {
        let rt = AsyncRuntime::new().expect("create runtime");
        tokio::spawn(rt.drive());

        let out = eval_async(
            &rt,
            r#"
            (async () => {
                const result = await agent("hello");
                return result + "!";
            })
            "#,
        )
        .await;

        assert_eq!(out, "agent done: hello!");
    }

    /// Proves real concurrency, not serialization: two `agent()` calls run via `Promise.all`
    /// should take roughly ONE sleep's worth of wall-clock time, not the sum of both — this is
    /// the property `parallel()` will depend on in the real feature.
    #[tokio::test]
    async fn concurrent_agent_calls_via_promise_all_run_in_parallel_not_serially() {
        let rt = AsyncRuntime::new().expect("create runtime");
        tokio::spawn(rt.drive());

        let start = Instant::now();
        let out = eval_async(
            &rt,
            r#"
            (async () => {
                const [a, b] = await Promise.all([agent("a"), agent("b")]);
                return a + " / " + b;
            })
            "#,
        )
        .await;
        let elapsed = start.elapsed();

        assert_eq!(out, "agent done: a / agent done: b");
        // Each sleep is 30ms; serialized execution would take ~60ms+. Generous upper bound
        // (45ms) to absorb scheduler jitter in CI while still failing if it's truly serialized.
        assert!(
            elapsed < Duration::from_millis(45),
            "expected concurrent execution (~30ms), took {elapsed:?} — looks serialized"
        );
    }
}
