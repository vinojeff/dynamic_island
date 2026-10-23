//! Which toast is on screen, and for how long. No thread of its own.
//!
//! The GPUI thread owns the [`Queue`] and mutates it in place. Display shape
//! stays in `island`; this module only answers "what content wins".
//!
//! Follow one arrival through [`Queue::push`]:
//!
//! - A duplicate (same app, title, and body inside 5s) is dropped.
//! - `panel_open` (the card) appends and raises [`Queue::badge`]. The card
//!   keeps showing its current head.
//! - Otherwise the toast is inserted at index 0, `pinned` and `badge` clear,
//!   and the caller restarts the [`NOTICE_HOLD_MS`] hold for that id.
//!
//! [`Queue::content`] is the head, or [`Content::Media`] when the queue is
//! empty. [`Queue::expire`] removes that head only when it is not pinned and
//! not [`Queue::set_suspended`]. [`Queue::pin`] (a body click on a capsule
//! that is showing a toast) holds it until [`Queue::dismiss`].
//! [`Queue::retain_ids`] drops ids the listener stopped reporting, except
//! `local` notices.

/// How long an unclicked toast stays on the capsule before media returns.
pub const NOTICE_HOLD_MS: u64 = 6_000;

/// One toast, already parsed. `id` dedupes the listener snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Notice {
    /// `UserNotification` id.
    pub id: u32,
    /// Source application name.
    pub app: String,
    /// First text node.
    pub title: String,
    /// Second text node.
    pub body: String,
    /// Milliseconds since an arbitrary clock. The caller owns the clock.
    pub arrival_ms: u64,
    /// Injected locally. The system poll must not drop it for being absent.
    pub local: bool,
}

/// What the capsule and the card are showing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Content {
    /// SMTC / demo media.
    Media,
    /// The head of the queue.
    Notice(Notice),
}

/// Queue plus the open-detail flag.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Queue {
    items: Vec<Notice>,
    /// The user opened the head. It stays until dismissed.
    pinned: bool,
    /// A toast arrived while the card was open. The view paints a dot.
    badge: bool,
    /// Exclusive fullscreen is up. Arrivals stay queued and the hold does not run.
    suspended: bool,
}

/// What a poll found relative to the previous snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Arrival {
    /// Ids present now.
    pub ids: Vec<u32>,
    /// Toasts that were not in the previous snapshot, newest last.
    pub fresh: Vec<Notice>,
}

impl Queue {
    /// Empty queue, showing media.
    #[must_use]
    pub const fn new() -> Self {
        Self { items: Vec::new(), pinned: false, badge: false, suspended: false }
    }

    /// Park the queue while a game owns the screen. Clearing it lets the head show.
    pub const fn set_suspended(&mut self, suspended: bool) {
        self.suspended = suspended;
    }

    /// The dock is hidden for exclusive fullscreen, so nothing should surface.
    #[must_use]
    pub const fn suspended(&self) -> bool {
        self.suspended
    }

    /// The content that should be on screen.
    #[must_use]
    pub fn content(&self) -> Content {
        match self.items.first() {
            Some(notice) => Content::Notice(notice.clone()),
            None => Content::Media,
        }
    }

    /// A toast arrived while the panel was open.
    #[must_use]
    pub const fn badge(&self) -> bool {
        self.badge
    }

    /// The user is reading the head, so the hold timer must not expire it.
    #[must_use]
    pub const fn pinned(&self) -> bool {
        self.pinned
    }

    /// The id on screen, if a toast is showing.
    #[must_use]
    pub fn head_id(&self) -> Option<u32> {
        self.items.first().map(|item| item.id)
    }

    /// Push a toast. A newer toast replaces the one on screen and restarts the hold.
    ///
    /// `panel_open` queues the toast without changing what the card shows, and
    /// raises the badge instead.
    pub fn push(&mut self, notice: Notice, panel_open: bool) {
        let duplicate = self.items.iter().any(|item| {
            item.app == notice.app
                && item.title == notice.title
                && item.body == notice.body
                && notice.arrival_ms.saturating_sub(item.arrival_ms) < 5_000
        });
        if duplicate {
            return;
        }
        self.items.retain(|item| item.id != notice.id);
        if panel_open {
            self.items.push(notice);
            self.badge = true;
            return;
        }
        self.items.insert(0, notice);
        self.pinned = false;
        self.badge = false;
    }

    /// Drop ids the listener no longer reports. The user handled them elsewhere.
    pub fn retain_ids(&mut self, live: &[u32]) {
        self.items.retain(|item| item.local || live.contains(&item.id));
        if self.items.is_empty() {
            self.pinned = false;
        }
    }

    /// The user opened the detail. The head stays until [`Self::dismiss`].
    pub fn pin(&mut self) {
        if !self.items.is_empty() {
            self.pinned = true;
            self.badge = false;
        }
    }

    /// Drop one toast by id. Returns whether it was the one on screen.
    pub fn remove_id(&mut self, id: u32) -> bool {
        let was_head = self.head_id() == Some(id);
        self.items.retain(|item| item.id != id);
        if was_head {
            self.pinned = false;
        }
        was_head
    }

    /// Drop the head. The next queued toast, if any, becomes current.
    pub fn dismiss(&mut self) {
        if !self.items.is_empty() {
            let _removed = self.items.remove(0);
        }
        self.pinned = false;
        self.badge = false;
    }

    /// Expire the head when its hold elapses and the user has not opened it.
    ///
    /// Returns whether the head was removed.
    pub fn expire(&mut self, now_ms: u64) -> bool {
        let Some(head) = self.items.first() else {
            return false;
        };
        if self.suspended || self.pinned || now_ms.saturating_sub(head.arrival_ms) < NOTICE_HOLD_MS {
            return false;
        }
        let _removed = self.items.remove(0);
        true
    }

    /// Clear the badge once the panel has closed and the head is on screen.
    pub fn clear_badge(&mut self) {
        self.badge = false;
    }
}

/// Single-line capsule copy: `【app】title：body`.
#[must_use]
pub fn marquee_text(notice: &Notice) -> String {
    let mut text = String::new();
    text.push('【');
    text.push_str(&notice.app);
    text.push('】');
    text.push_str(&notice.title);
    if !notice.body.is_empty() {
        text.push('：');
        text.push_str(&notice.body);
    }
    text
}

/// Ids that appeared since the previous snapshot.
#[must_use]
pub fn fresh_ids(previous: &[u32], current: &[u32]) -> Vec<u32> {
    current.iter().copied().filter(|id| !previous.contains(id)).collect()
}

#[cfg(test)]
mod tests {
    use super::{Content, Notice, Queue, fresh_ids, marquee_text, NOTICE_HOLD_MS};

    fn notice(id: u32, arrival_ms: u64) -> Notice {
        Notice {
            id,
            app: "提醒".to_owned(),
            title: format!("标题{id}"),
            body: "正文".to_owned(),
            arrival_ms,
            local: false,
        }
    }

    #[test]
    fn a_new_toast_replaces_the_head_and_restarts_the_hold() {
        let mut queue = Queue::new();
        queue.push(notice(1, 0), false);
        queue.push(notice(2, 1_000), false);
        assert!(matches!(queue.content(), super::Content::Notice(item) if item.id == 2));
        assert!(!queue.expire(1_000 + NOTICE_HOLD_MS - 1));
        assert!(queue.expire(1_000 + NOTICE_HOLD_MS));
        assert!(matches!(queue.content(), super::Content::Notice(item) if item.id == 1));
    }

    #[test]
    fn an_open_panel_queues_the_toast_and_raises_the_badge() {
        let mut queue = Queue::new();
        queue.push(notice(1, 0), false);
        queue.push(notice(2, 100), true);
        assert!(matches!(queue.content(), super::Content::Notice(item) if item.id == 1));
        assert!(queue.badge());
        assert!(queue.expire(NOTICE_HOLD_MS));
        assert!(matches!(queue.content(), super::Content::Notice(item) if item.id == 2));
    }

    #[test]
    fn pinning_holds_the_head_until_dismiss_and_then_media_returns() {
        let mut queue = Queue::new();
        queue.push(notice(1, 0), false);
        queue.pin();
        assert!(!queue.expire(NOTICE_HOLD_MS + 5));
        queue.dismiss();
        assert!(matches!(queue.content(), super::Content::Media));
    }

    #[test]
    fn a_removed_id_leaves_the_queue() {
        let mut queue = Queue::new();
        queue.push(notice(1, 0), false);
        queue.push(notice(2, 10), false);
        queue.retain_ids(&[1]);
        assert!(matches!(queue.content(), super::Content::Notice(item) if item.id == 1));
    }

    #[test]
    fn a_suspended_queue_holds_the_head_past_the_hold_and_shows_it_after() {
        let mut queue = Queue::new();
        queue.push(notice(1, 0), false);
        queue.set_suspended(true);
        assert_eq!(queue.head_id(), Some(1));
        assert!(!queue.expire(NOTICE_HOLD_MS + 1));
        queue.set_suspended(false);
        assert_eq!(queue.head_id(), Some(1));
        assert!(matches!(queue.content(), Content::Notice(_)));
    }

    #[test]
    fn marquee_joins_the_fields_and_fresh_ids_ignore_known_ones() {
        let text = marquee_text(&notice(3, 0));
        assert!(text.contains("提醒"));
        assert!(text.contains("标题3"));
        assert_eq!(fresh_ids(&[1, 2], &[2, 3, 4]), vec![3, 4]);
    }
}
