// SPDX-License-Identifier: MPL-2.0

//! Round-robin pacing: spread a fan-out over time instead of one herd.
//!
//! When you have N things to act on — tabs to nudge, tabs to message — doing
//! them together is a thundering herd: they all wake at once, contend for the
//! same resources, and the load arrives as a spike. The fix is not to do fewer
//! of them; it is to pace them, one at a time, rotating through the set so every
//! item is eventually reached and none is starved.
//!
//! There are two shapes here:
//!
//! - [`pick_round_robin`] — take **one** item, advancing a cursor, so a sweep
//!   over many items is spread across ticks (one per tick) instead of done in a
//!   burst. This is what the daemon's Brain does with its nudges.
//! - [`scan_order`] — the **order** to visit all N items, starting at the cursor
//!   and wrapping, so successive sweeps do not always begin at index 0 (which
//!   would let the first item be seen far more often than the rest).
//!
//! Both were extracted from the daemon's `brain.rs`, where they pace the agent
//! nudges, and moved here so the Kiosk's broadcast can pace the same way without
//! copying the code or reaching into the daemon.

/// Round-robin pick from a slice.
///
/// Advances `cursor` mod `len()` and returns the chosen element (a reference
/// into the slice, since the caller still owns the `Vec`). `None` on empty
/// input — the caller treats that as "nothing to do this tick" and the cursor is
/// **not** advanced, so a long quiet period cannot slide the start index past
/// every possible first item of the next non-empty set and starve it.
#[must_use]
pub fn pick_round_robin<'a, T>(items: &'a [T], cursor: &mut usize) -> Option<&'a T> {
    if items.is_empty() {
        return None;
    }
    let idx = *cursor % items.len();
    *cursor = cursor.wrapping_add(1);
    items.get(idx)
}

/// The order to visit all `n` items, starting at `cursor` and wrapping.
///
/// Use this when a sweep must reach **every** item but should not always begin
/// at the first one: feed a cursor that advances past the end each sweep, so the
/// starting point rotates and no item is systematically visited first. Returns
/// an empty vector for `n == 0`.
#[must_use]
pub fn scan_order(n: usize, cursor: usize) -> Vec<usize> {
    if n == 0 {
        return Vec::new();
    }
    let start = cursor % n;
    (0..n).map(|i| (start + i) % n).collect()
}

#[cfg(test)]
mod tests {
    use super::{pick_round_robin, scan_order};

    #[test]
    fn round_robin_empty_returns_none_without_advancing_cursor() {
        // No work this tick → cursor must NOT advance, otherwise a long quiet
        // period would slide the start index past every possible "first" of the
        // next non-empty set and skip items unfairly.
        let items: [&str; 0] = [];
        let mut cursor = 7;
        assert!(pick_round_robin(&items, &mut cursor).is_none());
        assert_eq!(cursor, 7);
    }

    #[test]
    fn round_robin_single_item_always_picks_it() {
        let items = ["only-tab"];
        let mut cursor = 0;
        assert_eq!(pick_round_robin(&items, &mut cursor), Some(&"only-tab"));
        assert_eq!(pick_round_robin(&items, &mut cursor), Some(&"only-tab"));
        assert_eq!(pick_round_robin(&items, &mut cursor), Some(&"only-tab"));
    }

    #[test]
    fn round_robin_rotates_through_set() {
        // The behaviour that matters: three items fire A, B, C, A, B, C, …
        let items = ["A", "B", "C"];
        let mut cursor = 0;
        let picks: Vec<&str> = (0..7)
            .map(|_| *pick_round_robin(&items, &mut cursor).unwrap())
            .collect();
        assert_eq!(picks, vec!["A", "B", "C", "A", "B", "C", "A"]);
    }

    #[test]
    fn round_robin_starting_cursor_offsets_the_first_pick() {
        // Cursor 4 in a 3-item set hits idx 4 % 3 = 1 = "B".
        let items = ["A", "B", "C"];
        let mut cursor = 4;
        assert_eq!(pick_round_robin(&items, &mut cursor), Some(&"B"));
        assert_eq!(cursor, 5);
        assert_eq!(pick_round_robin(&items, &mut cursor), Some(&"C"));
    }

    #[test]
    fn round_robin_survives_wrap_around() {
        // wrapping_add at usize::MAX must not panic. usize::MAX % 3 == 0, so the
        // last pick is "A"; the cursor then wraps to 0 rather than overflowing.
        let items = ["A", "B", "C"];
        let mut cursor = usize::MAX;
        assert_eq!(pick_round_robin(&items, &mut cursor), Some(&"A"));
        assert_eq!(cursor, 0, "the cursor wrapped, not overflowed");
    }

    #[test]
    fn scan_order_visits_everything_once_starting_at_the_cursor() {
        assert_eq!(scan_order(4, 0), vec![0, 1, 2, 3]);
        assert_eq!(scan_order(4, 2), vec![2, 3, 0, 1], "rotated start");
        assert_eq!(scan_order(4, 6), vec![2, 3, 0, 1], "cursor wraps mod n");
        assert_eq!(scan_order(0, 3), Vec::<usize>::new(), "nothing to visit");
    }

    #[test]
    fn advancing_the_cursor_rotates_which_item_is_visited_first() {
        // A caller that wants successive sweeps to start elsewhere advances the
        // cursor by ONE each time: the first item visited cycles 0, 1, 2, 0, so
        // no item is systematically visited first.
        let firsts: Vec<usize> = (0..4).map(|step| scan_order(3, step)[0]).collect();
        assert_eq!(firsts, vec![0, 1, 2, 0]);
    }
}
