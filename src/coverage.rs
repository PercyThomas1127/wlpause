//! Geometric occlusion: how much of a wallpaper surface is actually visible?
//!
//! This is the whole point of the tool, and it is deliberately a pure function
//! of plain rectangles so that it can be tested without a compositor running.
//! Every backend's job is reduced to "produce a target rect and a list of
//! occluder rects"; the decision is made here.

/// A rectangle in the compositor's *logical* coordinate space.
///
/// Logical, not physical: Hyprland reports monitors in physical pixels but
/// window and layer geometry in logical units, so a 2560x1600 panel at
/// scale 1.6 is a 1600x1000 coordinate space. Mixing the two silently breaks
/// every coverage calculation, so backends must convert before constructing
/// a `Rect`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Rect { x, y, w, h }
    }

    pub fn right(&self) -> i32 {
        self.x.saturating_add(self.w)
    }

    pub fn bottom(&self) -> i32 {
        self.y.saturating_add(self.h)
    }

    pub fn area(&self) -> i64 {
        if self.w <= 0 || self.h <= 0 {
            0
        } else {
            self.w as i64 * self.h as i64
        }
    }

    pub fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    /// The overlapping region, or `None` when they do not touch.
    ///
    /// This is what makes off-screen surfaces harmless: Hyprland happily
    /// reports layers positioned outside the monitor (a notification parked at
    /// x=1684 on a 1600-wide output), and they must contribute nothing.
    pub fn intersection(&self, other: &Rect) -> Option<Rect> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let r = self.right().min(other.right());
        let b = self.bottom().min(other.bottom());
        if r > x && b > y {
            Some(Rect::new(x, y, r - x, b - y))
        } else {
            None
        }
    }
}

/// Remove `cut` from `base`, returning the pieces that survive (up to four).
///
/// Splitting into disjoint pieces rather than accumulating areas is what keeps
/// overlapping occluders from being counted twice -- the classic bug in this
/// kind of code, and the reason two identical fullscreen windows must still
/// report exactly 100% coverage rather than 200%.
pub fn subtract(base: Rect, cut: Rect) -> Vec<Rect> {
    let Some(c) = base.intersection(&cut) else {
        return vec![base];
    };

    let mut out = Vec::with_capacity(4);
    // Full-width band above the cut.
    if c.y > base.y {
        out.push(Rect::new(base.x, base.y, base.w, c.y - base.y));
    }
    // Full-width band below the cut.
    if c.bottom() < base.bottom() {
        out.push(Rect::new(base.x, c.bottom(), base.w, base.bottom() - c.bottom()));
    }
    // Left and right slivers, limited to the cut's vertical span so they do
    // not overlap the bands above.
    if c.x > base.x {
        out.push(Rect::new(base.x, c.y, c.x - base.x, c.h));
    }
    if c.right() < base.right() {
        out.push(Rect::new(c.right(), c.y, base.right() - c.right(), c.h));
    }
    out
}

/// Worst-case guard on fragment count.
///
/// Each subtraction can turn one piece into four, so a pathological window
/// layout could blow up. On overflow we stop early and report what is left,
/// which *over*-estimates the visible area -- the safe direction, since it
/// errs towards leaving the wallpaper playing rather than freezing a visible
/// one.
const MAX_PIECES: usize = 2048;

/// Area of `target` not covered by any occluder.
pub fn uncovered_area(target: Rect, occluders: &[Rect]) -> i64 {
    if target.is_empty() {
        return 0;
    }
    let mut pieces = vec![target];
    for occ in occluders {
        if pieces.is_empty() {
            break;
        }
        if occ.is_empty() {
            continue;
        }
        let mut next = Vec::with_capacity(pieces.len());
        for p in pieces.drain(..) {
            next.extend(subtract(p, *occ));
        }
        pieces = next;
        if pieces.len() > MAX_PIECES {
            break;
        }
    }
    pieces.iter().map(|r| r.area()).sum()
}

/// Fraction of `target` hidden by `occluders`, in `0.0..=1.0`.
///
/// A degenerate target counts as fully covered: there is no wallpaper to see.
pub fn covered_fraction(target: Rect, occluders: &[Rect]) -> f64 {
    let total = target.area();
    if total <= 0 {
        return 1.0;
    }
    let visible = uncovered_area(target, occluders);
    1.0 - (visible as f64 / total as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The logical size of the machine this was written on: 2560x1600 @ 1.6.
    const OUT: Rect = Rect { x: 0, y: 0, w: 1600, h: 1000 };
    /// waybar, opaque, with a 60px exclusive zone.
    const BAR: Rect = Rect { x: 0, y: 0, w: 1600, h: 60 };

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    #[test]
    fn nothing_covers_nothing() {
        approx(covered_fraction(OUT, &[]), 0.0);
    }

    #[test]
    fn exact_cover() {
        approx(covered_fraction(OUT, &[OUT]), 1.0);
    }

    #[test]
    fn oversized_occluder_is_clipped_not_overcounted() {
        let huge = Rect::new(-500, -500, 5000, 5000);
        approx(covered_fraction(OUT, &[huge]), 1.0);
    }

    /// The real single-window case: one tiled window plus the bar's exclusive
    /// zone tile the output exactly. Smart gaps mean no border or gap here.
    /// If this ever stops being 1.0 the tool stops pausing in its most common
    /// situation, which is the entire reason it exists.
    #[test]
    fn bar_plus_one_tiled_window_is_total() {
        let win = Rect::new(0, 60, 1600, 940);
        approx(covered_fraction(OUT, &[BAR, win]), 1.0);
    }

    /// Two tiled windows with gaps_in 11.8 / gaps_out 15.5 leave visible
    /// slivers. Must be high but strictly below 1.0, which is what forces the
    /// threshold to be configurable rather than an equality test.
    #[test]
    fn two_tiled_windows_leave_gap_slivers() {
        let a = Rect::new(15, 75, 779, 909);
        let b = Rect::new(806, 75, 779, 909);
        let c = covered_fraction(OUT, &[BAR, a, b]);
        assert!(c > 0.90 && c < 1.0, "expected a high but partial cover, got {c}");
    }

    /// A small floating window must NOT read as covered. This is the case
    /// mpvpaper-stop gets wrong by counting windows instead of measuring them.
    #[test]
    fn small_floating_window_is_not_cover() {
        let float = Rect::new(480, 320, 640, 360);
        let c = covered_fraction(OUT, &[BAR, float]);
        assert!(c < 0.30, "a 640x360 float should not hide the wallpaper, got {c}");
    }

    #[test]
    fn overlapping_occluders_are_not_double_counted() {
        approx(covered_fraction(OUT, &[OUT, OUT, OUT]), 1.0);
        // Two half-screen windows overlapping by half their width.
        let a = Rect::new(0, 0, 1000, 1000);
        let b = Rect::new(600, 0, 1000, 1000);
        approx(covered_fraction(OUT, &[a, b]), 1.0);
    }

    #[test]
    fn offscreen_occluder_contributes_nothing() {
        let off = Rect::new(1684, 70, 306, 120);
        approx(covered_fraction(OUT, &[off]), 0.0);
    }

    #[test]
    fn partially_offscreen_occluder_counts_only_the_visible_part() {
        // Half of a 200-wide box hangs off the right edge.
        let half_off = Rect::new(1500, 0, 200, 1000);
        let expected = 100.0 * 1000.0 / (1600.0 * 1000.0);
        approx(covered_fraction(OUT, &[half_off]), expected);
    }

    #[test]
    fn hole_in_the_middle() {
        // Cover everything, then verify a central hole is reported exactly.
        let ring = [
            Rect::new(0, 0, 1600, 400),
            Rect::new(0, 600, 1600, 400),
            Rect::new(0, 400, 700, 200),
            Rect::new(900, 400, 700, 200),
        ];
        let hole = 200.0 * 200.0;
        approx(covered_fraction(OUT, &ring), 1.0 - hole / (1600.0 * 1000.0));
    }

    #[test]
    fn degenerate_rects_are_safe() {
        approx(covered_fraction(Rect::new(0, 0, 0, 0), &[]), 1.0);
        approx(covered_fraction(OUT, &[Rect::new(10, 10, 0, 500)]), 0.0);
        approx(covered_fraction(OUT, &[Rect::new(10, 10, -5, 500)]), 0.0);
    }

    #[test]
    fn subtract_of_disjoint_is_identity() {
        let base = Rect::new(0, 0, 100, 100);
        assert_eq!(subtract(base, Rect::new(200, 200, 10, 10)), vec![base]);
    }

    /// Pieces must always stay disjoint; verify by summing areas.
    #[test]
    fn subtract_pieces_partition_the_remainder() {
        let base = Rect::new(0, 0, 100, 100);
        let cut = Rect::new(25, 25, 50, 50);
        let pieces = subtract(base, cut);
        let total: i64 = pieces.iter().map(|r| r.area()).sum();
        assert_eq!(total, 100 * 100 - 50 * 50);
    }
}
