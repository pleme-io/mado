#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerJump {
    First,
    Last,
    PageUp,
    PageDown,
}

impl PickerJump {
    #[must_use]
    pub fn target(self, selected: usize, len: usize, page: usize) -> usize {
        let Some(last) = len.checked_sub(1) else {
            return 0;
        };
        let selected = selected.min(last);
        let page = page.max(1);
        match self {
            Self::First => 0,
            Self::Last => last,
            Self::PageUp if selected == 0 => last,
            Self::PageUp => selected.saturating_sub(page),
            Self::PageDown if selected == last => 0,
            Self::PageDown => selected.saturating_add(page).min(last),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PickerJump::{self, First, Last, PageDown, PageUp};

    #[test]
    fn home_and_end_reach_the_ends_from_anywhere() {
        for selected in 0..7 {
            assert_eq!(First.target(selected, 7, 3), 0);
            assert_eq!(Last.target(selected, 7, 3), 6);
        }
    }

    #[test]
    fn a_page_moves_by_the_visible_rows_and_stops_at_the_edge() {
        assert_eq!(PageDown.target(0, 10, 4), 4);
        assert_eq!(PageDown.target(8, 10, 4), 9);
        assert_eq!(PageUp.target(9, 10, 4), 5);
        assert_eq!(PageUp.target(2, 10, 4), 0);
    }

    #[test]
    fn a_page_past_an_edge_wraps_so_navigation_never_dead_ends() {
        assert_eq!(PageDown.target(9, 10, 4), 0);
        assert_eq!(PageUp.target(0, 10, 4), 9);
    }

    #[test]
    fn every_jump_lands_inside_the_list_and_an_empty_list_stays_at_zero() {
        for jump in [First, Last, PageUp, PageDown] {
            assert_eq!(jump.target(5, 0, 3), 0, "{jump:?}");
            for len in 1..6 {
                for selected in 0..8 {
                    for page in 0..4 {
                        let t: usize = PickerJump::target(jump, selected, len, page);
                        assert!(t < len, "{jump:?} {selected} {len} {page} -> {t}");
                    }
                }
            }
        }
    }
}
