//! The one ceiling every response shares.
//!
//! A request may raise or lower it. A request cannot remove it — there is no "unlimited"
//! value, because the whole argument for this server is that a caller's context is a
//! budget somebody has to spend. Output that hits the ceiling says so; it never simply
//! stops.

/// Applied when a caller names no ceiling of its own.
pub const DEFAULT_MAX_BYTES: usize = 40_000;

/// The most a single response may render, however large a number was asked for.
pub const MAX_MAX_BYTES: usize = 400_000;

/// A shrinking allowance of rendered bytes, plus a tally of what it refused.
#[derive(Debug)]
pub struct Budget {
    remaining: usize,
    skipped: usize,
}

impl Budget {
    /// Clamp a requested ceiling into `[0, MAX_MAX_BYTES]`.
    pub fn new(requested: Option<usize>) -> Self {
        Self {
            remaining: requested.unwrap_or(DEFAULT_MAX_BYTES).min(MAX_MAX_BYTES),
            skipped: 0,
        }
    }

    pub fn remaining(&self) -> usize {
        self.remaining
    }

    pub fn is_exhausted(&self) -> bool {
        self.remaining == 0
    }

    /// Spend `text`'s bytes, all or nothing. A partial write would cut a line in half and
    /// leave the caller unable to tell truncation from content.
    pub fn try_spend(&mut self, text: &str) -> bool {
        if text.len() > self.remaining {
            self.skipped += 1;
            return false;
        }
        self.remaining -= text.len();
        true
    }

    /// Record something left out for a reason other than the ceiling, so one counter
    /// carries everything the footer has to admit to.
    pub fn note_skipped(&mut self) {
        self.skipped += 1;
    }

    /// How many pieces were refused. Non-zero means the footer owes the caller a note.
    pub fn skipped(&self) -> usize {
        self.skipped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_cannot_opt_out_of_the_budget() {
        assert_eq!(Budget::new(None).remaining(), DEFAULT_MAX_BYTES);
        assert_eq!(Budget::new(Some(usize::MAX)).remaining(), MAX_MAX_BYTES);
        assert_eq!(Budget::new(Some(0)).remaining(), 0);
    }

    #[test]
    fn try_spend_refuses_rather_than_overshooting() {
        let mut budget = Budget::new(Some(10));
        assert!(budget.try_spend("0123456789"));
        assert!(budget.is_exhausted());
        assert!(!budget.try_spend("x"));
        assert_eq!(budget.skipped(), 1);
    }
}
