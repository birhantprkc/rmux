use std::collections::HashMap;
use std::time::{Duration, Instant};

use rmux_core::events::PaneOutputSubscriptionKey;
use rmux_proto::PaneTarget;

use crate::pane_io::PaneOutputSender;

use super::RequestHandler;

/// How long an exited, removed pane keeps its output ring available for a
/// late `Oldest` SDK subscription.
pub(in crate::handler) const EXITED_PANE_OUTPUT_RETENTION_TTL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub(in crate::handler) struct RetainedExitedPaneOutput {
    pane: PaneOutputSubscriptionKey,
    output: PaneOutputSender,
    expires_at: Instant,
}

impl RetainedExitedPaneOutput {
    fn new(
        pane: PaneOutputSubscriptionKey,
        output: PaneOutputSender,
        now: Instant,
        ttl: Duration,
    ) -> Self {
        Self {
            pane,
            output,
            expires_at: now + ttl,
        }
    }

    pub(in crate::handler) fn pane(&self) -> &PaneOutputSubscriptionKey {
        &self.pane
    }

    pub(in crate::handler) fn output(&self) -> &PaneOutputSender {
        &self.output
    }

    fn is_expired(&self, now: Instant) -> bool {
        now >= self.expires_at
    }
}

#[derive(Debug, Default)]
pub(in crate::handler) struct RetainedExitedPaneOutputs {
    by_target: HashMap<PaneTarget, RetainedExitedPaneOutput>,
}

impl RetainedExitedPaneOutputs {
    pub(in crate::handler) fn insert(
        &mut self,
        target: PaneTarget,
        pane: PaneOutputSubscriptionKey,
        output: PaneOutputSender,
        now: Instant,
        ttl: Duration,
    ) {
        self.cleanup_expired(now);
        self.by_target.insert(
            target,
            RetainedExitedPaneOutput::new(pane, output, now, ttl),
        );
    }

    pub(in crate::handler) fn get(
        &mut self,
        target: &PaneTarget,
        now: Instant,
    ) -> Option<RetainedExitedPaneOutput> {
        self.cleanup_expired(now);
        self.by_target.get(target).cloned()
    }

    pub(in crate::handler) fn cleanup_target_if_expired(
        &mut self,
        target: &PaneTarget,
        now: Instant,
    ) {
        let should_remove = self
            .by_target
            .get(target)
            .is_some_and(|retained| retained.is_expired(now));
        if should_remove {
            self.by_target.remove(target);
        }
    }

    pub(in crate::handler) fn is_empty(&mut self, now: Instant) -> bool {
        self.cleanup_expired(now);
        self.by_target.is_empty()
    }

    pub(in crate::handler) fn clear(&mut self) {
        self.by_target.clear();
    }

    fn cleanup_expired(&mut self, now: Instant) {
        self.by_target
            .retain(|_, retained| !retained.is_expired(now));
    }
}

impl RequestHandler {
    pub(in crate::handler) fn retain_exited_pane_output(
        &self,
        target: PaneTarget,
        pane: PaneOutputSubscriptionKey,
        output: PaneOutputSender,
    ) {
        let now = Instant::now();
        self.retained_exited_outputs
            .lock()
            .expect("retained exited output mutex must not be poisoned")
            .insert(
                target.clone(),
                pane,
                output,
                now,
                EXITED_PANE_OUTPUT_RETENTION_TTL,
            );
        self.watch_retained_exited_pane_output(target);
    }

    pub(in crate::handler) fn retained_exited_pane_output(
        &self,
        target: &PaneTarget,
        now: Instant,
    ) -> Option<RetainedExitedPaneOutput> {
        self.retained_exited_outputs
            .lock()
            .expect("retained exited output mutex must not be poisoned")
            .get(target, now)
    }

    fn watch_retained_exited_pane_output(&self, target: PaneTarget) {
        let handler = self.downgrade();
        tokio::spawn(async move {
            tokio::time::sleep(EXITED_PANE_OUTPUT_RETENTION_TTL).await;
            let Some(handler) = handler.upgrade() else {
                return;
            };
            handler
                .retained_exited_outputs
                .lock()
                .expect("retained exited output mutex must not be poisoned")
                .cleanup_target_if_expired(&target, Instant::now());
            let _ = handler.request_shutdown_if_pending();
        });
    }
}
