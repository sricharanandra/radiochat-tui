/// Vim mode system for RadioChat TUI
/// Provides Normal, Insert, and Visual modes with standard vim keybindings

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VimMode {
    Normal,
    Insert,
    Visual,
}

impl VimMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            VimMode::Normal => "-- NORMAL --",
            VimMode::Insert => "-- INSERT --",
            VimMode::Visual => "-- VISUAL --",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct VimState {
    pub mode: VimMode,
    pub pending_command: Option<char>,
    pub count: Option<usize>,
}

impl Default for VimState {
    fn default() -> Self {
        Self {
            mode: VimMode::Normal,
            pending_command: None,
            count: None,
        }
    }
}

impl VimState {
    pub fn reset(&mut self) {
        self.pending_command = None;
        self.count = None;
    }

    pub fn enter_insert_mode(&mut self) {
        self.mode = VimMode::Insert;
        self.reset();
    }

    pub fn enter_normal_mode(&mut self) {
        self.mode = VimMode::Normal;
        self.reset();
    }

    pub fn enter_visual_mode(&mut self) {
        self.mode = VimMode::Visual;
        self.reset();
    }
}
