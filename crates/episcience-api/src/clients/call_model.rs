//! The model that answered ONE LLM call, carried from the provider that made
//! the call to the synthesis attempt that asked for it.
//!
//! The kernel's `LlmProvider` trait returns only the answer
//! (`serde_json::Value`) and reports a model through `model_name()`, a
//! property of the provider. The worker holds ONE provider for every job in
//! the process, so a model the provider remembers is the last model ANY job's
//! call resolved, not the one this job's calls did. Instead, a provider that
//! learns the model per call (`claude -p`'s envelope names it) [`report`]s it,
//! and the synthesis attempt's wrapper (`jobs::ArcLlm`) runs each call inside
//! [`capture`] and keeps the model with the attempt.
//!
//! The channel is a tokio task-local scoped to the one call's future, so two
//! attempts whose calls run at the same time, on one task (`join!`) or on
//! many, each see only their own call's model. A provider that awaits its
//! work in the calling future (as `ClaudeCliProvider` does) reports into the
//! right scope; one that spawned the work onto another task and reported
//! there would report nothing (a no-op), never into another call's scope.

use std::cell::Cell;
use std::future::Future;

tokio::task_local! {
    static CALL_MODEL: Cell<Option<&'static str>>;
}

/// Run `call`, one provider call, and return its output together with the
/// model the provider [`report`]ed for it (`None` when it reported none: a
/// provider that does not report, a failed call, or an answer whose model
/// the provider could not tell). The last report within the call wins.
pub async fn capture<F: Future>(call: F) -> (F::Output, Option<&'static str>) {
    CALL_MODEL
        .scope(Cell::new(None), async move {
            let out = call.await;
            (out, CALL_MODEL.with(Cell::get))
        })
        .await
}

/// Name the model that produced the answer of the call in progress. A no-op
/// outside [`capture`] (a caller that does not record a per-call model).
pub fn report(model: &'static str) {
    let _ = CALL_MODEL.try_with(|slot| slot.set(Some(model)));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each capture sees the report made inside it, and only that one: two
    /// calls interleaved on one task (the second reports while the first,
    /// which reported already, is still pending) keep their own models; a
    /// report outside any capture changes nothing; a call that reports
    /// nothing captures `None`. Kills: one slot shared by every call (the
    /// first call would read the second's model).
    #[tokio::test]
    async fn each_capture_sees_only_its_own_calls_report() {
        report("outside");
        let (gate_tx, gate_rx) = tokio::sync::oneshot::channel::<()>();
        let slow = async move {
            report("slow-model");
            gate_rx.await.expect("gate");
            1
        };
        let fast = async move {
            report("fast-model");
            gate_tx.send(()).expect("open the gate");
            2
        };
        let ((a, ma), (b, mb)) = tokio::join!(capture(slow), capture(fast));
        assert_eq!((a, ma), (1, Some("slow-model")));
        assert_eq!((b, mb), (2, Some("fast-model")));
        let (c, mc) = capture(async { 3 }).await;
        assert_eq!((c, mc), (3, None));
    }
}
