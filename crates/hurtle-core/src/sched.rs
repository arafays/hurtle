//! Scheduling decisions for segmented downloads, kept pure so they can be
//! unit-tested: which segment an idle connection should help with, where to
//! cut it, and how many connections are worth running.

use std::time::Duration;

/// Speed assumed for a segment nobody has measured yet ("treat as slow").
pub const UNKNOWN_SPEED: u64 = 64 * 1024;

/// What the scheduler sees of one segment.
#[derive(Clone, Copy, Debug)]
pub struct SegView {
    pub remaining: u64,
    pub owned: bool,
    /// Smoothed bytes/s of the connection working on it; 0 = unknown.
    pub speed: u64,
    /// Time since the owner last delivered data.
    pub idle: Duration,
}

#[derive(Clone, Copy, Debug)]
pub struct Tuning {
    /// Neither half of a split may be smaller than this.
    pub min_segment: u64,
    /// Don't help a segment whose owner will be done within this many seconds.
    pub min_eta: f64,
    /// An owner silent for this long is considered stalled.
    pub stall: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plan {
    /// Work on an unowned segment as it is.
    Take(usize),
    /// The owner is stalled: take over the whole remainder.
    Takeover(usize),
    /// Split segment `idx`; the new worker starts `at` bytes after its `pos`.
    Split { idx: usize, at: u64 },
}

/// Estimated seconds until a segment completes.
pub fn eta(remaining: u64, speed: u64) -> f64 {
    remaining as f64 / speed.max(if speed == 0 { UNKNOWN_SPEED } else { 1 }) as f64
}

/// Where to cut a segment with `remaining` bytes left so the owner (at
/// `owner` B/s) and the helper (at `helper` B/s) finish together. Returns the
/// number of bytes the owner keeps. Unknown speeds split at the midpoint.
pub fn split_point(remaining: u64, owner: u64, helper: u64, min_segment: u64) -> u64 {
    let share = if owner > 0 && helper > 0 {
        owner as f64 / (owner + helper) as f64
    } else {
        0.5
    };
    let keep = (remaining as f64 * share) as u64;
    let lo = min_segment.min(remaining / 2);
    keep.clamp(lo, remaining - lo)
}

/// Decide what an idle connection (measured at `helper` B/s, 0 = unknown)
/// should do next. Orphans first, then stalled owners, then the segment with
/// the longest time left, not merely the biggest.
pub fn plan(segs: &[SegView], helper: u64, t: &Tuning) -> Option<Plan> {
    if let Some(i) = segs.iter().position(|s| !s.owned && s.remaining > 0) {
        return Some(Plan::Take(i));
    }
    let stalled = segs
        .iter()
        .enumerate()
        .filter(|(_, s)| s.owned && s.remaining > 0 && s.idle >= t.stall)
        .max_by_key(|(_, s)| s.remaining);
    if let Some((i, _)) = stalled {
        return Some(Plan::Takeover(i));
    }
    let (idx, s) = segs
        .iter()
        .enumerate()
        .filter(|(_, s)| s.owned && s.remaining >= 2 * t.min_segment)
        .max_by(|(_, a), (_, b)| {
            eta(a.remaining, a.speed)
                .partial_cmp(&eta(b.remaining, b.speed))
                .unwrap_or(std::cmp::Ordering::Equal)
        })?;
    // A known-fast owner that is nearly done doesn't need help.
    if s.speed > 0 && eta(s.remaining, s.speed) < t.min_eta {
        return None;
    }
    let keep = split_point(s.remaining, s.speed, helper, t.min_segment);
    Some(Plan::Split { idx, at: keep })
}

/// Exponentially weighted speed update.
pub fn ewma(old: u64, sample: u64) -> u64 {
    if old == 0 {
        sample
    } else {
        (old as f64 * 0.6 + sample as f64 * 0.4) as u64
    }
}

/// Adaptive connection ramp: decides after each measuring window whether to
/// add connections.
#[derive(Debug)]
pub struct Ramp {
    prev: f64,
    added: usize,
    pub done: bool,
}

impl Ramp {
    pub fn new() -> Self {
        Self {
            prev: 0.0,
            added: 0,
            done: false,
        }
    }

    /// `tput` is the aggregate bytes/s of the window that just ended, `live`
    /// the current connections and `cap` the allowed maximum. Returns how many
    /// to add now.
    pub fn step(&mut self, tput: f64, live: usize, cap: usize) -> usize {
        if self.done || live >= cap {
            self.prev = tput;
            return 0;
        }
        // Adding `added` connections should have bought ~5% each.
        let wanted = self.prev * (1.0 + 0.05 * self.added as f64);
        if self.prev > 0.0 && tput < wanted {
            self.done = true; // more connections stopped helping
            self.prev = tput;
            return 0;
        }
        let add = 2.min(cap - live);
        self.added = add;
        self.prev = tput;
        add
    }

    pub fn stop(&mut self) {
        self.done = true;
    }
}

impl Default for Ramp {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Tuning = Tuning {
        min_segment: 512 * 1024,
        min_eta: 2.0,
        stall: Duration::from_secs(8),
    };

    fn seg(remaining: u64, owned: bool, speed: u64) -> SegView {
        SegView {
            remaining,
            owned,
            speed,
            idle: Duration::ZERO,
        }
    }

    const MIB: u64 = 1 << 20;

    #[test]
    fn orphans_come_first() {
        let v = [seg(50 * MIB, true, 0), seg(MIB, false, 0)];
        assert_eq!(plan(&v, 0, &T), Some(Plan::Take(1)));
    }

    #[test]
    fn longest_eta_beats_biggest() {
        // Big but fast (2 s left) vs small but crawling (40 s left).
        let v = [
            seg(20 * MIB, true, 10 * MIB),
            seg(4 * MIB, true, 100 * 1024),
        ];
        match plan(&v, 1024 * 1024, &T) {
            Some(Plan::Split { idx, .. }) => assert_eq!(idx, 1),
            p => panic!("{p:?}"),
        }
    }

    #[test]
    fn unknown_speed_counts_as_slow() {
        let v = [seg(10 * MIB, true, 5 * MIB), seg(6 * MIB, true, 0)];
        match plan(&v, 0, &T) {
            Some(Plan::Split { idx, .. }) => assert_eq!(idx, 1),
            p => panic!("{p:?}"),
        }
    }

    #[test]
    fn split_is_proportional_to_speeds() {
        // Owner 100 KiB/s, helper 300 KiB/s: owner keeps a quarter.
        let keep = split_point(8 * MIB, 100, 300, 512 * 1024);
        assert_eq!(keep, 2 * MIB);
        // Equal / unknown: midpoint.
        assert_eq!(split_point(8 * MIB, 0, 300, 512 * 1024), 4 * MIB);
        assert_eq!(split_point(8 * MIB, 200, 200, 512 * 1024), 4 * MIB);
    }

    #[test]
    fn split_respects_minimum() {
        let keep = split_point(2 * MIB, 1, 1000, 512 * 1024);
        assert_eq!(keep, 512 * 1024);
        let keep = split_point(2 * MIB, 1000, 1, 512 * 1024);
        assert_eq!(keep, 2 * MIB - 512 * 1024);
    }

    #[test]
    fn no_split_when_small_or_nearly_done() {
        // Under 2x min_segment left.
        assert_eq!(plan(&[seg(MIB - 1, true, 1000)], 0, &T), None);
        // Fast owner, ~1 s left.
        assert_eq!(plan(&[seg(10 * MIB, true, 10 * MIB)], 0, &T), None);
    }

    #[test]
    fn stalled_owner_is_taken_over_whole() {
        let mut s = seg(300 * 1024, true, 5 * MIB);
        s.idle = Duration::from_secs(10);
        let v = [seg(30 * MIB, true, 100 * 1024), s];
        assert_eq!(plan(&v, 0, &T), Some(Plan::Takeover(1)));
    }

    #[test]
    fn ramp_adds_until_gain_stops() {
        let mut r = Ramp::new();
        assert_eq!(r.step(1.0e6, 4, 8), 2);
        assert_eq!(r.step(1.5e6, 6, 8), 2); // improved
        assert_eq!(r.step(1.52e6, 8, 8), 0); // at cap
        let mut r = Ramp::new();
        assert_eq!(r.step(1.0e6, 4, 8), 2);
        assert_eq!(r.step(1.01e6, 6, 8), 0); // no real gain: stop
        assert!(r.done);
        assert_eq!(r.step(9.0e6, 6, 8), 0);
    }

    #[test]
    fn ramp_never_exceeds_cap() {
        let mut r = Ramp::new();
        assert_eq!(r.step(1.0e6, 7, 8), 1);
    }
}
