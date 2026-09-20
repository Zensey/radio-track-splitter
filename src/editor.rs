//! Editing state for the split-point GUI: markers, selection, playhead and the
//! visible time window. Pure logic so it can be unit-tested without a window.

pub const MIN_GAP: f64 = 0.05;
pub const MIN_SPAN: f64 = 0.5;

#[derive(Debug, Clone)]
pub struct Editor {
    pub duration: f64,
    pub marks: Vec<f64>,
    pub sel: Option<usize>,
    pub playhead: f64,
    pub view_start: f64,
    pub view_span: f64,
}

impl Editor {
    pub fn new(duration: f64) -> Self {
        let mut e = Self {
            duration,
            marks: Vec::new(),
            sel: None,
            playhead: 0.0,
            view_start: 0.0,
            view_span: 60.0,
        };
        e.clamp_view();
        e
    }

    pub fn set_marks(&mut self, mut marks: Vec<f64>) {
        marks.iter_mut().for_each(|m| *m = m.clamp(MIN_GAP, (self.duration - MIN_GAP).max(MIN_GAP)));
        marks.sort_by(|a, b| a.partial_cmp(b).unwrap());
        self.marks = marks;
        self.sel = if self.marks.is_empty() { None } else { Some(0) };
    }

    pub fn clamp_view(&mut self) {
        let max_span = self.duration.max(MIN_SPAN);
        self.view_span = self.view_span.clamp(MIN_SPAN.min(max_span), max_span);
        self.view_start = self.view_start.clamp(0.0, (self.duration - self.view_span).max(0.0));
    }

    pub fn center_on(&mut self, t: f64, span: Option<f64>) {
        if let Some(s) = span {
            self.view_span = s;
        }
        self.view_start = t - self.view_span / 2.0;
        self.clamp_view();
    }

    /// Multiply the visible span by `factor`, keeping the time under `anchor` fixed on screen.
    pub fn zoom(&mut self, factor: f64, anchor: Option<f64>) {
        let anchor = anchor.unwrap_or(self.view_start + self.view_span / 2.0);
        let frac = (anchor - self.view_start) / self.view_span;
        self.view_span *= factor;
        self.clamp_view();
        self.view_start = anchor - frac * self.view_span;
        self.clamp_view();
    }

    pub fn zoom_all(&mut self) {
        self.view_start = 0.0;
        self.view_span = self.duration;
        self.clamp_view();
    }

    pub fn pan(&mut self, dt: f64) {
        self.view_start += dt;
        self.clamp_view();
    }

    pub fn neighbor_limits(&self, i: usize) -> (f64, f64) {
        let lo = if i > 0 { self.marks[i - 1] + MIN_GAP } else { MIN_GAP };
        let hi = if i + 1 < self.marks.len() { self.marks[i + 1] - MIN_GAP } else { self.duration - MIN_GAP };
        (lo, hi)
    }

    pub fn move_marker(&mut self, i: usize, t: f64) {
        let (lo, hi) = self.neighbor_limits(i);
        self.marks[i] = t.clamp(lo, hi.max(lo));
    }

    pub fn nudge(&mut self, delta: f64) -> bool {
        match self.sel {
            Some(i) => {
                self.move_marker(i, self.marks[i] + delta);
                true
            }
            None => false,
        }
    }

    /// Adds a split (or selects the existing one within 0.2s). Returns true if a new one was added.
    pub fn add_marker(&mut self, t: f64) -> bool {
        let t = t.clamp(MIN_GAP, (self.duration - MIN_GAP).max(MIN_GAP));
        if let Some(i) = self.marks.iter().position(|m| (m - t).abs() < 0.2) {
            self.sel = Some(i);
            return false;
        }
        self.marks.push(t);
        self.marks.sort_by(|a, b| a.partial_cmp(b).unwrap());
        self.sel = self.marks.iter().position(|&m| m == t);
        true
    }

    pub fn delete(&mut self, i: usize) {
        if i >= self.marks.len() {
            return;
        }
        self.marks.remove(i);
        self.sel = if self.marks.is_empty() { None } else { Some(i.min(self.marks.len() - 1)) };
    }

    pub fn delete_selected(&mut self) {
        if let Some(i) = self.sel {
            self.delete(i);
        }
    }

    pub fn select_relative(&mut self, step: isize) {
        if self.marks.is_empty() {
            return;
        }
        let next = match self.sel {
            None => 0,
            Some(i) => (i as isize + step).clamp(0, self.marks.len() as isize - 1) as usize,
        };
        self.sel = Some(next);
        let t = self.marks[next];
        if t < self.view_start || t > self.view_start + self.view_span {
            self.center_on(t, None);
        }
    }

    /// Time boundaries of all tracks: 0, each split, duration.
    pub fn bounds(&self) -> Vec<f64> {
        let mut b = vec![0.0];
        b.extend_from_slice(&self.marks);
        b.push(self.duration);
        b
    }
}

pub fn fmt_time(t: f64) -> String {
    let t = t.max(0.0);
    let m = (t / 60.0).floor();
    format!("{}:{:06.3}", m as u64, t - m * 60.0)
}

pub fn fmt_dur(t: f64) -> String {
    let t = t.max(0.0).round() as u64;
    format!("{}:{:02}", t / 60, t % 60)
}

/// Parses "3:02.500", "1:03:02.5" or plain seconds.
pub fn parse_time(text: &str) -> Option<f64> {
    let mut secs = 0.0;
    for part in text.trim().split(':') {
        secs = secs * 60.0 + part.trim().parse::<f64>().ok()?;
    }
    secs.is_finite().then_some(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor() -> Editor {
        let mut e = Editor::new(600.0);
        e.set_marks(vec![100.0, 200.0, 400.0]);
        e
    }

    #[test]
    fn time_formatting_round_trips() {
        assert_eq!(fmt_time(182.5), "3:02.500");
        assert_eq!(parse_time("3:02.500"), Some(182.5));
        assert_eq!(parse_time("182.5"), Some(182.5));
        assert_eq!(parse_time("1:00:00"), Some(3600.0));
        assert_eq!(parse_time("abc"), None);
        assert_eq!(fmt_dur(147.0), "2:27");
    }

    #[test]
    fn markers_cannot_cross_neighbours() {
        let mut e = editor();
        e.move_marker(1, 50.0);
        assert!((e.marks[1] - (100.0 + MIN_GAP)).abs() < 1e-9);
        e.move_marker(1, 1e6);
        assert!((e.marks[1] - (400.0 - MIN_GAP)).abs() < 1e-9);
        e.move_marker(2, 1e6);
        assert!((e.marks[2] - (600.0 - MIN_GAP)).abs() < 1e-9);
    }

    #[test]
    fn nudge_and_selection() {
        let mut e = editor();
        assert!(e.nudge(0.1));
        assert!((e.marks[0] - 100.1).abs() < 1e-9);
        e.sel = None;
        assert!(!e.nudge(1.0));
        e.select_relative(1);
        assert_eq!(e.sel, Some(0));
        e.select_relative(5);
        assert_eq!(e.sel, Some(2));
    }

    #[test]
    fn add_near_existing_selects_it() {
        let mut e = editor();
        assert!(!e.add_marker(200.1));
        assert_eq!(e.sel, Some(1));
        assert!(e.add_marker(300.0));
        assert_eq!(e.marks, vec![100.0, 200.0, 300.0, 400.0]);
        assert_eq!(e.sel, Some(2));
    }

    #[test]
    fn delete_keeps_a_valid_selection() {
        let mut e = editor();
        e.sel = Some(2);
        e.delete_selected();
        assert_eq!(e.sel, Some(1));
        e.delete_selected();
        e.delete_selected();
        assert_eq!(e.sel, None);
        assert!(e.marks.is_empty());
    }

    #[test]
    fn zoom_keeps_anchor_fixed() {
        let mut e = editor();
        e.view_start = 100.0;
        e.view_span = 60.0;
        let anchor = 120.0;
        let frac = (anchor - e.view_start) / e.view_span;
        e.zoom(0.5, Some(anchor));
        assert!((e.view_span - 30.0).abs() < 1e-9);
        assert!((e.view_start + frac * e.view_span - anchor).abs() < 1e-9);
        e.zoom(1e9, None);
        assert_eq!((e.view_start, e.view_span), (0.0, 600.0));
    }

    #[test]
    fn view_stays_inside_the_file() {
        let mut e = editor();
        e.center_on(-50.0, Some(60.0));
        assert_eq!(e.view_start, 0.0);
        e.center_on(1e6, None);
        assert!((e.view_start + e.view_span - 600.0).abs() < 1e-9);
    }
}
