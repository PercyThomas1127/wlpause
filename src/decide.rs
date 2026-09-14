//! Turning geometry into a play/pause decision.

use crate::coverage::{covered_fraction, Rect};

/// Everything a backend must report about one output.
#[derive(Debug, Clone)]
pub struct OutputState {
    pub name: String,
    /// False when the output is DPMS-off or disabled. A wallpaper decoding to
    /// a dark panel is the most wasteful state there is, so this matters more
    /// than the geometry does.
    pub powered: bool,
    /// The wallpaper surface's own rect, or `None` if no wallpaper is present
    /// on this output.
    pub wallpaper: Option<Rect>,
    /// Windows and higher layer-shell surfaces that hide it.
    pub occluders: Vec<Rect>,
}

impl OutputState {
    /// Fraction of the wallpaper hidden, or `None` when there is no wallpaper
    /// on this output at all.
    pub fn covered(&self) -> Option<f64> {
        self.wallpaper
            .map(|w| covered_fraction(w, &self.occluders))
    }

    pub fn decision(&self, threshold: f64) -> Decision {
        match (self.powered, self.covered()) {
            (false, _) => Decision::Pause,
            (_, None) => Decision::Pause,
            (_, Some(c)) if c >= threshold => Decision::Pause,
            _ => Decision::Play,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Play,
    Pause,
}

impl Decision {
    pub fn is_paused(self) -> bool {
        self == Decision::Pause
    }
}

/// Decide for a target that spans several outputs.
///
/// `mpvpaper '*'` is a *single* mpv instance driving every output, so it must
/// keep playing while any one of them still shows wallpaper. Pausing on the
/// majority would freeze a wallpaper the user is looking at.
pub fn combine<'a, I>(states: I, threshold: f64) -> Decision
where
    I: IntoIterator<Item = &'a OutputState>,
{
    let mut seen = false;
    for s in states {
        seen = true;
        if s.decision(threshold) == Decision::Play {
            return Decision::Play;
        }
    }
    // No outputs at all: nothing can be visible.
    if !seen {
        return Decision::Pause;
    }
    Decision::Pause
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(name: &str, powered: bool, occ: Vec<Rect>) -> OutputState {
        OutputState {
            name: name.into(),
            powered,
            wallpaper: Some(Rect::new(0, 0, 1600, 1000)),
            occluders: occ,
        }
    }

    const BAR: Rect = Rect { x: 0, y: 0, w: 1600, h: 60 };

    #[test]
    fn bare_desktop_plays() {
        assert_eq!(out("a", true, vec![BAR]).decision(0.90), Decision::Play);
    }

    #[test]
    fn fully_tiled_pauses() {
        let win = Rect::new(0, 60, 1600, 940);
        assert_eq!(out("a", true, vec![BAR, win]).decision(0.90), Decision::Pause);
    }

    /// Screen off beats geometry: even a completely bare desktop must pause.
    #[test]
    fn powered_off_pauses_regardless() {
        assert_eq!(out("a", false, vec![]).decision(0.90), Decision::Pause);
    }

    #[test]
    fn no_wallpaper_on_output_pauses() {
        let s = OutputState {
            name: "a".into(),
            powered: true,
            wallpaper: None,
            occluders: vec![],
        };
        assert_eq!(s.decision(0.90), Decision::Pause);
    }

    /// A threshold of 1.0 means "only when literally nothing shows", so the
    /// gap slivers between two tiled windows must keep it playing.
    #[test]
    fn threshold_of_one_demands_total_cover() {
        let a = Rect::new(15, 75, 779, 909);
        let b = Rect::new(806, 75, 779, 909);
        let s = out("a", true, vec![BAR, a, b]);
        assert_eq!(s.decision(0.90), Decision::Pause);
        assert_eq!(s.decision(1.0), Decision::Play);
    }

    #[test]
    fn one_visible_output_keeps_a_shared_instance_playing() {
        let covered = out("a", true, vec![Rect::new(0, 0, 1600, 1000)]);
        let bare = out("b", true, vec![]);
        assert_eq!(combine([&covered, &bare], 0.90), Decision::Play);
        assert_eq!(combine([&covered], 0.90), Decision::Pause);
    }

    #[test]
    fn no_outputs_pauses() {
        assert_eq!(combine([], 0.90), Decision::Pause);
    }
}
