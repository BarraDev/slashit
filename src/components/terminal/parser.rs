use super::state::{CellAttributes, Color, TerminalState};
use vte::{Params, Perform};

/// VTE Performer that updates TerminalState
pub struct TerminalParser<'a> {
    pub state: &'a mut TerminalState,
}

impl<'a> TerminalParser<'a> {
    pub fn new(state: &'a mut TerminalState) -> Self {
        Self { state }
    }
}

impl<'a> Perform for TerminalParser<'a> {
    fn print(&mut self, c: char) {
        self.state.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            // Bell
            0x07 => {}
            // Backspace
            0x08 => self.state.backspace(),
            // Tab
            0x09 => self.state.tab(),
            // Line feed, vertical tab, form feed
            0x0A..=0x0C => self.state.linefeed(),
            // Carriage return
            0x0D => self.state.carriage_return(),
            _ => {}
        }
    }

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: char) {}

    fn put(&mut self, _byte: u8) {}

    fn unhook(&mut self) {}

    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {}

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        let params: Vec<u16> = params.iter().map(|p| p.first().copied().unwrap_or(0)).collect();

        match action {
            // Cursor Up
            'A' => {
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                self.state.cursor_up(n);
            }
            // Cursor Down
            'B' => {
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                self.state.cursor_down(n);
            }
            // Cursor Forward
            'C' => {
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                self.state.cursor_forward(n);
            }
            // Cursor Back
            'D' => {
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                self.state.cursor_backward(n);
            }
            // Cursor Next Line
            'E' => {
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                self.state.cursor_down(n);
                self.state.carriage_return();
            }
            // Cursor Previous Line
            'F' => {
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                self.state.cursor_up(n);
                self.state.carriage_return();
            }
            // Cursor Horizontal Absolute
            'G' => {
                let col = params.first().copied().unwrap_or(1).max(1) as usize;
                self.state.cursor_x = (col - 1).min(self.state.cols - 1);
            }
            // Cursor Position
            'H' | 'f' => {
                let row = params.first().copied().unwrap_or(1).max(1) as usize;
                let col = params.get(1).copied().unwrap_or(1).max(1) as usize;
                self.state.move_cursor(row, col);
            }
            // Erase in Display
            'J' => {
                let mode = params.first().copied().unwrap_or(0);
                match mode {
                    0 => self.state.erase_screen_to_end(),
                    1 => self.state.erase_screen_to_start(),
                    2 | 3 => self.state.erase_screen(),
                    _ => {}
                }
            }
            // Erase in Line
            'K' => {
                let mode = params.first().copied().unwrap_or(0);
                match mode {
                    0 => self.state.erase_line_to_end(),
                    1 => self.state.erase_line_to_start(),
                    2 => self.state.erase_line(),
                    _ => {}
                }
            }
            // Insert Lines
            'L' => {
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                self.state.insert_lines(n);
            }
            // Delete Lines
            'M' => {
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                self.state.delete_lines(n);
            }
            // Delete Characters
            'P' => {
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                self.state.delete_chars(n);
            }
            // Scroll Up
            'S' => {
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                for _ in 0..n {
                    self.state.scroll_up();
                }
            }
            // Scroll Down
            'T' => {
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                for _ in 0..n {
                    self.state.scroll_down();
                }
            }
            // Insert Characters
            '@' => {
                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                self.state.insert_chars(n);
            }
            // SGR (Select Graphic Rendition)
            'm' => {
                self.handle_sgr(&params);
            }
            // Cursor Vertical Position
            'd' => {
                let row = params.first().copied().unwrap_or(1).max(1) as usize;
                self.state.cursor_y = (row - 1).min(self.state.rows - 1);
            }
            // Set scrolling region
            'r' => {
                let top = params.first().copied().unwrap_or(1).max(1) as usize;
                let bottom = params.get(1).copied().unwrap_or(self.state.rows as u16) as usize;
                self.state.scroll_top = (top - 1).min(self.state.rows - 1);
                self.state.scroll_bottom = (bottom - 1).min(self.state.rows - 1).max(self.state.scroll_top);
            }
            // Save cursor position (DECSC)
            's' => {
                if intermediates.is_empty() {
                    self.state.save_cursor();
                }
            }
            // Restore cursor position (DECRC)
            'u' => {
                self.state.restore_cursor();
            }
            // Device Status Report (DSR)
            'n' => {}
            // DECSET/DECRST
            'h' | 'l' => {}
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        match (intermediates, byte) {
            // Save cursor (DECSC)
            ([], b'7') => self.state.save_cursor(),
            // Restore cursor (DECRC)
            ([], b'8') => self.state.restore_cursor(),
            // Reset (RIS)
            ([], b'c') => self.state.reset(),
            // Index - move cursor down, scroll if needed
            ([], b'D') => self.state.linefeed(),
            // Reverse Index - move cursor up, scroll if needed
            ([], b'M') => {
                if self.state.cursor_y == self.state.scroll_top {
                    self.state.scroll_down();
                } else if self.state.cursor_y > 0 {
                    self.state.cursor_y -= 1;
                }
            }
            // Next Line
            ([], b'E') => {
                self.state.linefeed();
                self.state.carriage_return();
            }
            _ => {}
        }
    }
}

impl<'a> TerminalParser<'a> {
    fn handle_sgr(&mut self, params: &[u16]) {
        if params.is_empty() {
            self.state.current_attrs = CellAttributes::default();
            return;
        }

        let mut i = 0;
        while i < params.len() {
            match params[i] {
                0 => self.state.current_attrs = CellAttributes::default(),
                1 => self.state.current_attrs.bold = true,
                2 => self.state.current_attrs.dim = true,
                3 => self.state.current_attrs.italic = true,
                4 => self.state.current_attrs.underline = true,
                7 => self.state.current_attrs.reverse = true,
                9 => self.state.current_attrs.strikethrough = true,
                21 => self.state.current_attrs.bold = false,
                22 => {
                    self.state.current_attrs.bold = false;
                    self.state.current_attrs.dim = false;
                }
                23 => self.state.current_attrs.italic = false,
                24 => self.state.current_attrs.underline = false,
                27 => self.state.current_attrs.reverse = false,
                29 => self.state.current_attrs.strikethrough = false,
                // Foreground colors
                30 => self.state.current_attrs.fg = Color::Black,
                31 => self.state.current_attrs.fg = Color::Red,
                32 => self.state.current_attrs.fg = Color::Green,
                33 => self.state.current_attrs.fg = Color::Yellow,
                34 => self.state.current_attrs.fg = Color::Blue,
                35 => self.state.current_attrs.fg = Color::Magenta,
                36 => self.state.current_attrs.fg = Color::Cyan,
                37 => self.state.current_attrs.fg = Color::White,
                38 => {
                    if i + 2 < params.len() && params[i + 1] == 5 {
                        // 256 color mode
                        self.state.current_attrs.fg = Color::Index(params[i + 2] as u8);
                        i += 2;
                    } else if i + 4 < params.len() && params[i + 1] == 2 {
                        // True color mode
                        let r = params[i + 2] as u8;
                        let g = params[i + 3] as u8;
                        let b = params[i + 4] as u8;
                        self.state.current_attrs.fg = Color::Rgb(r, g, b);
                        i += 4;
                    }
                }
                39 => self.state.current_attrs.fg = Color::Default,
                // Background colors
                40 => self.state.current_attrs.bg = Color::Black,
                41 => self.state.current_attrs.bg = Color::Red,
                42 => self.state.current_attrs.bg = Color::Green,
                43 => self.state.current_attrs.bg = Color::Yellow,
                44 => self.state.current_attrs.bg = Color::Blue,
                45 => self.state.current_attrs.bg = Color::Magenta,
                46 => self.state.current_attrs.bg = Color::Cyan,
                47 => self.state.current_attrs.bg = Color::White,
                48 => {
                    if i + 2 < params.len() && params[i + 1] == 5 {
                        // 256 color mode
                        self.state.current_attrs.bg = Color::Index(params[i + 2] as u8);
                        i += 2;
                    } else if i + 4 < params.len() && params[i + 1] == 2 {
                        // True color mode
                        let r = params[i + 2] as u8;
                        let g = params[i + 3] as u8;
                        let b = params[i + 4] as u8;
                        self.state.current_attrs.bg = Color::Rgb(r, g, b);
                        i += 4;
                    }
                }
                49 => self.state.current_attrs.bg = Color::Default,
                // Bright foreground colors
                90 => self.state.current_attrs.fg = Color::BrightBlack,
                91 => self.state.current_attrs.fg = Color::BrightRed,
                92 => self.state.current_attrs.fg = Color::BrightGreen,
                93 => self.state.current_attrs.fg = Color::BrightYellow,
                94 => self.state.current_attrs.fg = Color::BrightBlue,
                95 => self.state.current_attrs.fg = Color::BrightMagenta,
                96 => self.state.current_attrs.fg = Color::BrightCyan,
                97 => self.state.current_attrs.fg = Color::BrightWhite,
                // Bright background colors
                100 => self.state.current_attrs.bg = Color::BrightBlack,
                101 => self.state.current_attrs.bg = Color::BrightRed,
                102 => self.state.current_attrs.bg = Color::BrightGreen,
                103 => self.state.current_attrs.bg = Color::BrightYellow,
                104 => self.state.current_attrs.bg = Color::BrightBlue,
                105 => self.state.current_attrs.bg = Color::BrightMagenta,
                106 => self.state.current_attrs.bg = Color::BrightCyan,
                107 => self.state.current_attrs.bg = Color::BrightWhite,
                _ => {}
            }
            i += 1;
        }
    }
}

/// Parse bytes into terminal state using VTE
/// Uses the persistent VTE parser stored in TerminalState to properly handle
/// escape sequences that may span multiple calls
pub fn parse_bytes(state: &mut TerminalState, bytes: &[u8]) {
    // Temporarily take the parser out to avoid borrow checker issues
    // This is safe and allows us to use the persistent parser
    let mut parser = std::mem::replace(&mut state.vte_parser, vte::Parser::new());
    let mut performer = TerminalParser::new(state);
    
    for byte in bytes {
        parser.advance(&mut performer, *byte);
    }
    
    // Put the parser back
    state.vte_parser = parser;
}
