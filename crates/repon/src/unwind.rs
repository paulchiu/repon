//! Escape's unwind stack: one handler, [`unwind_one`], that cancels exactly one level per
//! press, in the fixed innermost-first order
//! [keybindings.md](../../../../docs/spec/keybindings.md#esc) fixes: an in-flight fan-out,
//! then a range anchor, then the detail pane, then a committed Filter.
//!
//! Only the range anchor level exists yet ([`Selection`](crate::selection::Selection)'s own
//! `unwind` impl below); the other three are later tickets' work, and each slots in by
//! implementing [`UnwindLevel`] and taking its place in the level list `unwind_one` is called
//! with, in the order above. Esc never quits, at any depth, and there is no press-twice-to
//! -force gesture: the reason (the timing measurement that would justify one does not hold
//! over a remote connection) is already recorded in keybindings.md's "Esc" section, so it is
//! not restated here.

/// One level of the Escape stack: something a single Escape press can cancel.
pub(crate) trait UnwindLevel {
    /// Cancels this level, if it has anything live to cancel. Returns whether it did.
    fn unwind(&mut self) -> bool;
}

/// Tries each level in innermost-first order and stops at the first one that actually
/// unwinds something, so one Escape press never touches more than one level. Returns
/// whether any level unwound; a press over an empty stack is inert rather than escalating,
/// since nothing here ever reaches for `Message::Quit`.
pub(crate) fn unwind_one(levels: &mut [&mut dyn UnwindLevel]) -> bool {
    levels.iter_mut().any(|level| level.unwind())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A level that can be told in advance whether it has something to cancel, so tests can
    /// prove ordering and short-circuiting without depending on `Selection`'s own semantics.
    struct StubLevel {
        has_something_live: bool,
        was_asked_to_unwind: bool,
    }

    impl StubLevel {
        fn armed() -> Self {
            Self {
                has_something_live: true,
                was_asked_to_unwind: false,
            }
        }

        fn empty() -> Self {
            Self {
                has_something_live: false,
                was_asked_to_unwind: false,
            }
        }
    }

    impl UnwindLevel for StubLevel {
        fn unwind(&mut self) -> bool {
            self.was_asked_to_unwind = true;
            if self.has_something_live {
                self.has_something_live = false;
                true
            } else {
                false
            }
        }
    }

    #[test]
    fn unwind_one_with_nothing_live_at_any_level_is_inert() {
        let mut a = StubLevel::empty();
        let mut b = StubLevel::empty();

        let unwound = unwind_one(&mut [&mut a, &mut b]);

        assert!(!unwound);
    }

    #[test]
    fn unwind_one_cancels_only_the_innermost_live_level_and_stops_there() {
        let mut innermost = StubLevel::armed();
        let mut outer = StubLevel::armed();

        let unwound = unwind_one(&mut [&mut innermost, &mut outer]);

        assert!(unwound);
        assert!(
            !innermost.has_something_live,
            "the innermost level must have been the one cancelled"
        );
        assert!(
            outer.has_something_live,
            "a single press must not also unwind the next level in the same call"
        );
        assert!(
            !outer.was_asked_to_unwind,
            "the outer level must not even be tried once an earlier level has unwound"
        );
    }

    #[test]
    fn unwind_one_falls_through_to_a_later_level_only_when_the_earlier_one_is_already_empty() {
        let mut innermost = StubLevel::empty();
        let mut outer = StubLevel::armed();

        let unwound = unwind_one(&mut [&mut innermost, &mut outer]);

        assert!(unwound);
        assert!(!outer.has_something_live);
    }
}
