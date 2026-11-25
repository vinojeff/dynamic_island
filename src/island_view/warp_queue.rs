//! The glass warp queue: one in-flight warp, one pending slot, and a
//! deterministic-failure marker, guarded by a ticket.
//!
//! Pure on purpose — the view owns installation and GPUI. The two-slot shape
//! matters: a single slot would let the 32 ms poller invalidate a long warp
//! forever.

use std::sync::Arc;

use crate::glass_optics::GlassShape;
use crate::wallpaper::RawGlass;

/// Which view slot a warp result may install into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WarpDest {
    /// The live capture slot: guarded by the newest frame's generation.
    Live,
    /// The static fallback slot: guarded by the build epoch.
    Static,
}

/// One queued warp: a raw crop, the shape to bake, and where it may land.
#[derive(Clone)]
pub(crate) struct WarpRequest {
    /// The crop to refract.
    pub raw: Arc<RawGlass>,
    /// The logical shape to bake the bevel for.
    pub shape: GlassShape,
    /// Where the result may install.
    pub dest: WarpDest,
    /// The raw's generation at request time.
    pub generation: u64,
    /// Monotonic queue ticket; only the newest may install.
    pub ticket: u64,
}

impl WarpRequest {
    fn identity(&self) -> (u64, GlassShape, WarpDest) {
        (self.generation, self.shape, self.dest)
    }
}

#[derive(Default)]
pub(crate) struct WarpQueue {
    pending: Option<WarpRequest>,
    in_flight: Option<WarpRequest>,
    failed: Option<(u64, GlassShape, WarpDest)>,
    ticket: u64,
}

impl WarpQueue {
    /// Queues one warp and returns it, or `None` when the same
    /// `(generation, shape, dest)` is already pending, in flight, or known
    /// to fail. A *different* request supersedes the pending slot.
    pub(crate) fn request(
        &mut self,
        raw: Arc<RawGlass>,
        shape: GlassShape,
        dest: WarpDest,
    ) -> Option<WarpRequest> {
        let identity = (raw.generation, shape, dest);
        if self.pending.as_ref().is_some_and(|r| r.identity() == identity) {
            return None;
        }
        if self.in_flight.as_ref().is_some_and(|r| r.identity() == identity) {
            return None;
        }
        if self.failed.as_ref().is_some_and(|f| *f == identity) {
            return None;
        }
        self.ticket = self.ticket.wrapping_add(1);
        let request = WarpRequest { raw, shape, dest, generation: identity.0, ticket: self.ticket };
        self.pending = Some(request.clone());
        Some(request)
    }

    /// Marks a request as started, clearing the pending slot only if it is
    /// still this request.
    pub(crate) fn mark_started(&mut self, request: &WarpRequest) {
        if self.pending.as_ref().is_some_and(|p| p.ticket == request.ticket) {
            self.pending = None;
        }
        self.in_flight = Some(request.clone());
    }

    /// Clears the in-flight slot. Returns `true` when this warp is still the
    /// newest one and may install.
    pub(crate) fn finish(&mut self, request: &WarpRequest) -> bool {
        self.in_flight = None;
        request.ticket == self.ticket
    }

    /// Records a deterministic failure; the poller stops re-requesting it
    /// until the raw generation or the shape changes.
    pub(crate) fn record_failed(&mut self, request: &WarpRequest) {
        self.failed = Some(request.identity());
    }

    /// A success clears the failure marker.
    pub(crate) fn record_success(&mut self) {
        self.failed = None;
    }

    /// Drops the pending slot (source switch). The in-flight warp is left to
    /// its own completion, which clears `in_flight` itself.
    pub(crate) fn clear_pending(&mut self) {
        self.pending = None;
    }

    /// Drops only the pending warps for one destination — used when a static
    /// build fails and its queued re-warp must not run against a dead raw.
    /// An in-flight warp for that destination still finishes; its install is
    /// blocked by the generation check.
    pub(crate) fn clear_dest(&mut self, dest: WarpDest) {
        if self.pending.as_ref().is_some_and(|r| r.dest == dest) {
            self.pending = None;
        }
    }

    /// Takes the pending request for the next start.
    pub(crate) fn take_pending(&mut self) -> Option<WarpRequest> {
        self.pending.take()
    }

    /// Whether a warp is running.
    pub(crate) fn busy(&self) -> bool {
        self.in_flight.is_some()
    }

    /// The in-flight generation, for tests.
    #[cfg(test)]
    pub(crate) fn in_flight_generation(&self) -> Option<u64> {
        self.in_flight.as_ref().map(|r| r.generation)
    }

    /// The current ticket, for tests.
    #[cfg(test)]
    pub(crate) fn ticket(&self) -> u64 {
        self.ticket
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests assert the precondition themselves")]
mod tests {
    use super::*;
    use crate::glass_optics::Rect;

    fn raw(generation: u64) -> Arc<RawGlass> {
        Arc::new(RawGlass {
            rgba: Vec::new(),
            width: 0,
            height: 0,
            crop: Rect::ZERO,
            image_scale: 1.0,
            stage_width: 420.0,
            page: (1600.0, 900.0),
            luma: 0.5,
            generation,
        })
    }

    fn shape() -> GlassShape {
        GlassShape { width: 420.0, height: 288.0, radius: 24.0 }
    }

    #[test]
    fn a_duplicate_of_pending_or_in_flight_is_skipped_without_a_new_ticket() {
        let mut queue = WarpQueue::default();
        let first = queue.request(raw(1), shape(), WarpDest::Live).unwrap();
        let ticket = queue.ticket();
        queue.mark_started(&first);
        assert!(queue.request(raw(1), shape(), WarpDest::Live).is_none());
        assert_eq!(queue.ticket(), ticket, "a duplicate must not invalidate the in-flight warp");
    }

    #[test]
    fn a_newer_request_supersedes_the_pending_one() {
        let mut queue = WarpQueue::default();
        let first = queue.request(raw(1), shape(), WarpDest::Live).unwrap();
        queue.mark_started(&first);
        let second = queue.request(raw(1), shape(), WarpDest::Static).unwrap();
        assert!(!queue.finish(&first), "the superseded warp must not install");
        assert_eq!(queue.take_pending().unwrap().ticket, second.ticket);
    }

    #[test]
    fn a_failed_request_is_not_reissued_until_it_changes() {
        let mut queue = WarpQueue::default();
        let request = queue.request(raw(1), shape(), WarpDest::Live).unwrap();
        queue.mark_started(&request);
        assert!(queue.finish(&request));
        queue.record_failed(&request);
        assert!(queue.request(raw(1), shape(), WarpDest::Live).is_none());
        assert!(queue.request(raw(2), shape(), WarpDest::Live).is_some());
        queue.record_success();
        assert!(queue.request(raw(1), shape(), WarpDest::Live).is_some());
    }

    #[test]
    fn clear_pending_drops_only_the_pending_slot() {
        let mut queue = WarpQueue::default();
        let first = queue.request(raw(1), shape(), WarpDest::Live).unwrap();
        queue.mark_started(&first);
        let _second = queue.request(raw(2), shape(), WarpDest::Live).unwrap();
        queue.clear_pending();
        assert!(queue.take_pending().is_none());
        assert!(!queue.finish(&first));
        assert_eq!(queue.in_flight_generation(), None);
    }
}
