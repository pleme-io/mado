use crate::ux::scroll::ScrollGesture;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Band {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl Band {
    #[must_use]
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct OverlayHits {
    pub card: Option<Band>,
    pub rows: Vec<(Band, usize)>,
}

impl OverlayHits {
    #[must_use]
    pub fn row_at(&self, x: f32, y: f32) -> Option<usize> {
        self.rows
            .iter()
            .find(|(band, _)| band.contains(x, y))
            .map(|(_, index)| *index)
    }

    #[must_use]
    pub fn contains(&self, x: f32, y: f32) -> bool {
        match self.card {
            Some(card) => card.contains(x, y),
            None => self.row_at(x, y).is_some(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct WheelSteps {
    pixels: f64,
}

impl WheelSteps {
    pub fn feed(&mut self, gesture: ScrollGesture, row_px: f64) -> i32 {
        match gesture {
            ScrollGesture::Wheel { ticks } => {
                self.pixels = 0.0;
                if ticks == 0.0 {
                    0
                } else {
                    let n = ticks.abs().ceil() as i32;
                    if ticks > 0.0 { n } else { -n }
                }
            }
            ScrollGesture::Precise { pixels } => {
                let row = row_px.max(1.0);
                self.pixels += pixels;
                let steps = (self.pixels / row).trunc();
                self.pixels -= steps * row;
                steps as i32
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn band(top: f32) -> Band {
        Band {
            left: 100.0,
            top,
            right: 500.0,
            bottom: top + 20.0,
        }
    }

    fn hits() -> OverlayHits {
        OverlayHits {
            card: Some(Band {
                left: 90.0,
                top: 50.0,
                right: 510.0,
                bottom: 200.0,
            }),
            rows: vec![(band(80.0), 0), (band(100.0), 1), (band(120.0), 2)],
        }
    }

    #[test]
    fn a_point_resolves_to_the_row_it_lands_on() {
        let h = hits();
        assert_eq!(h.row_at(200.0, 85.0), Some(0));
        assert_eq!(h.row_at(200.0, 100.0), Some(1), "band tops are inclusive");
        assert_eq!(h.row_at(200.0, 139.9), Some(2));
        assert_eq!(h.row_at(200.0, 60.0), None, "the title line is no row");
        assert_eq!(h.row_at(50.0, 85.0), None);
    }

    #[test]
    fn the_card_decides_inside_versus_outside() {
        let h = hits();
        assert!(h.contains(95.0, 60.0));
        assert!(!h.contains(20.0, 60.0));
        let bare = OverlayHits {
            card: None,
            rows: hits().rows,
        };
        assert!(bare.contains(200.0, 85.0));
        assert!(!bare.contains(200.0, 60.0));
    }

    #[test]
    fn wheel_notches_step_once_each_and_trackpads_step_per_row_height() {
        let mut w = WheelSteps::default();
        assert_eq!(w.feed(ScrollGesture::Wheel { ticks: 1.0 }, 20.0), 1);
        assert_eq!(w.feed(ScrollGesture::Wheel { ticks: -0.3 }, 20.0), -1);
        assert_eq!(w.feed(ScrollGesture::Precise { pixels: 12.0 }, 20.0), 0);
        assert_eq!(w.feed(ScrollGesture::Precise { pixels: 12.0 }, 20.0), 1);
        assert_eq!(w.feed(ScrollGesture::Precise { pixels: -45.0 }, 20.0), -2);
    }
}
