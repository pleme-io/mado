//! Tab management — multiple terminal sessions in a single window.
//!
//! Each tab owns an independent terminal + PTY pair. The tab manager
//! tracks the active tab and provides navigation.

/// Unique identifier for a tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TabId(pub usize);

/// Metadata for a single tab.
#[derive(Debug, Clone)]
pub struct Tab {
    pub id: TabId,
    pub title: String,
}

impl Tab {
    fn new(id: usize) -> Self {
        Self {
            id: TabId(id),
            title: format!("Tab {}", id + 1),
        }
    }
}

/// Tab manager state.
pub struct TabManager {
    tabs: Vec<Tab>,
    active: usize,
    next_id: usize,
}

impl TabManager {
    /// Create a new tab manager with one initial tab.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tabs: vec![Tab::new(0)],
            active: 0,
            next_id: 1,
        }
    }

    /// Add a new tab and make it active. Returns the new tab's ID.
    pub fn add(&mut self) -> TabId {
        let id = self.next_id;
        self.next_id += 1;
        let tab = Tab::new(id);
        let tab_id = tab.id;
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        tab_id
    }

    /// Close the tab at the given index. Returns the closed tab's ID,
    /// or None if it's the last tab (can't close the last one).
    pub fn close(&mut self, index: usize) -> Option<TabId> {
        if self.tabs.len() <= 1 || index >= self.tabs.len() {
            return None;
        }
        let removed = self.tabs.remove(index);
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        } else if self.active > index {
            self.active -= 1;
        }
        Some(removed.id)
    }

    /// Close the active tab.
    pub fn close_active(&mut self) -> Option<TabId> {
        self.close(self.active)
    }

    /// Switch to the next tab (wraps around).
    pub fn next(&mut self) {
        if !self.tabs.is_empty() {
            self.active = (self.active + 1) % self.tabs.len();
        }
    }

    /// Switch to the previous tab (wraps around).
    pub fn prev(&mut self) {
        if !self.tabs.is_empty() {
            self.active = if self.active == 0 {
                self.tabs.len() - 1
            } else {
                self.active - 1
            };
        }
    }

    /// Switch to tab at the given index.
    pub fn select(&mut self, index: usize) {
        if index < self.tabs.len() {
            self.active = index;
        }
    }

    /// Get the active tab.
    #[must_use]
    pub fn active_tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    /// Get the active tab index.
    #[must_use]
    pub fn active_index(&self) -> usize {
        self.active
    }

    /// All tabs.
    #[must_use]
    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    /// Number of tabs.
    #[must_use]
    pub fn count(&self) -> usize {
        self.tabs.len()
    }

    /// Set the title of a tab by ID.
    pub fn set_title(&mut self, id: TabId, title: String) {
        if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) {
            tab.title = title;
        }
    }
}

impl Default for TabManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_has_one_tab() {
        let mgr = TabManager::new();
        assert_eq!(mgr.count(), 1);
        assert_eq!(mgr.active_index(), 0);
    }

    #[test]
    fn add_tab() {
        let mut mgr = TabManager::new();
        let id = mgr.add();
        assert_eq!(mgr.count(), 2);
        assert_eq!(mgr.active_index(), 1);
        assert_eq!(mgr.active_tab().id, id);
    }

    #[test]
    fn close_tab() {
        let mut mgr = TabManager::new();
        mgr.add();
        mgr.add();
        assert_eq!(mgr.count(), 3);

        mgr.select(1);
        let closed = mgr.close(1);
        assert!(closed.is_some());
        assert_eq!(mgr.count(), 2);
    }

    #[test]
    fn cannot_close_last_tab() {
        let mut mgr = TabManager::new();
        assert!(mgr.close(0).is_none());
        assert_eq!(mgr.count(), 1);
    }

    #[test]
    fn next_prev_wrap() {
        let mut mgr = TabManager::new();
        mgr.add();
        mgr.add();
        mgr.select(0);

        mgr.next();
        assert_eq!(mgr.active_index(), 1);
        mgr.next();
        assert_eq!(mgr.active_index(), 2);
        mgr.next();
        assert_eq!(mgr.active_index(), 0); // wraps

        mgr.prev();
        assert_eq!(mgr.active_index(), 2); // wraps back
    }

    #[test]
    fn set_title() {
        let mut mgr = TabManager::new();
        let id = mgr.active_tab().id;
        mgr.set_title(id, "My Shell".to_string());
        assert_eq!(mgr.active_tab().title, "My Shell");
    }

    #[test]
    fn close_active() {
        let mut mgr = TabManager::new();
        mgr.add();
        mgr.select(0);
        let closed = mgr.close_active();
        assert!(closed.is_some());
        assert_eq!(mgr.count(), 1);
    }

    #[test]
    fn close_adjusts_active_index() {
        let mut mgr = TabManager::new();
        mgr.add();
        mgr.add();
        mgr.select(2);
        mgr.close(0);
        // Active was at 2, removed index 0, so active should shift to 1
        assert_eq!(mgr.active_index(), 1);
    }
}
