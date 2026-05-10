/// ANSI color representation
#[derive(Clone, Copy, Debug, PartialEq)]
#[derive(Default)]
pub enum Color {
    #[default]
    Default,
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    BrightBlack,
    BrightRed,
    BrightGreen,
    BrightYellow,
    BrightBlue,
    BrightMagenta,
    BrightCyan,
    BrightWhite,
    Rgb(u8, u8, u8),
    Index(u8),
}


/// Cell attributes (bold, italic, etc.)
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CellAttributes {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub dim: bool,
    pub reverse: bool,
    pub fg: Color,
    pub bg: Color,
}

/// A single terminal cell
#[derive(Clone, Debug)]
pub struct Cell {
    pub char: char,
    pub attrs: CellAttributes,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            char: ' ',
            attrs: CellAttributes::default(),
        }
    }
}

/// Maximum scrollback lines to keep
const MAX_SCROLLBACK: usize = 5000;

/// Terminal state including the character grid and cursor
pub struct TerminalState {
    pub cols: usize,
    pub rows: usize,
    pub grid: Vec<Vec<Cell>>,
    pub cursor_x: usize,
    pub cursor_y: usize,
    pub current_attrs: CellAttributes,
    pub scroll_top: usize,
    pub scroll_bottom: usize,
    pub saved_cursor: Option<(usize, usize)>,
    /// Scrollback buffer - lines that have scrolled off the top
    pub scrollback: Vec<Vec<Cell>>,
    /// Current scroll offset (0 = at bottom, >0 = scrolled up into history)
    pub scroll_offset: usize,
    /// Persistent VTE parser to handle escape sequences spanning multiple calls
    pub vte_parser: vte::Parser,
}

impl TerminalState {
    pub fn new(cols: usize, rows: usize) -> Self {
        let grid = (0..rows)
            .map(|_| (0..cols).map(|_| Cell::default()).collect())
            .collect();

        Self {
            cols,
            rows,
            grid,
            cursor_x: 0,
            cursor_y: 0,
            current_attrs: CellAttributes::default(),
            scroll_top: 0,
            scroll_bottom: rows - 1,
            saved_cursor: None,
            scrollback: Vec::new(),
            scroll_offset: 0,
            vte_parser: vte::Parser::new(),
        }
    }
    
    /// Get maximum scroll offset
    pub fn max_scroll_offset(&self) -> usize {
        self.scrollback.len()
    }
    
    /// Scroll view by delta lines (positive = up into history, negative = down)
    pub fn scroll_view(&mut self, delta: i32) {
        let max_offset = self.max_scroll_offset();
        if delta > 0 {
            // Scroll up (into history)
            self.scroll_offset = (self.scroll_offset + delta as usize).min(max_offset);
        } else {
            // Scroll down (towards present)
            let abs_delta = (-delta) as usize;
            self.scroll_offset = self.scroll_offset.saturating_sub(abs_delta);
        }
    }
    
    /// Reset scroll to bottom (latest output)
    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
    }
    
    /// Get visible rows for rendering (accounting for scroll offset)
    pub fn visible_rows(&self) -> Vec<&Vec<Cell>> {
        if self.scroll_offset == 0 {
            // At bottom - show current grid
            self.grid.iter().collect()
        } else {
            // Scrolled up - mix scrollback and grid
            let scrollback_start = self.scrollback.len().saturating_sub(self.scroll_offset);
            let scrollback_rows = self.scroll_offset.min(self.scrollback.len()).min(self.rows);
            let grid_rows = self.rows.saturating_sub(scrollback_rows);
            
            let mut result: Vec<&Vec<Cell>> = Vec::with_capacity(self.rows);
            
            // Add scrollback rows
            for i in 0..scrollback_rows {
                if scrollback_start + i < self.scrollback.len() {
                    result.push(&self.scrollback[scrollback_start + i]);
                }
            }
            
            // Add grid rows from the top
            for i in 0..grid_rows {
                if i < self.grid.len() {
                    result.push(&self.grid[i]);
                }
            }
            
            result
        }
    }

    /// Put a character at the current cursor position
    pub fn put_char(&mut self, c: char) {
        if self.cursor_x >= self.cols {
            self.cursor_x = 0;
            self.linefeed();
        }

        if self.cursor_y < self.rows && self.cursor_x < self.cols {
            self.grid[self.cursor_y][self.cursor_x] = Cell {
                char: c,
                attrs: self.current_attrs,
            };
            self.cursor_x += 1;
        }
    }

    /// Move cursor to next line
    pub fn linefeed(&mut self) {
        if self.cursor_y >= self.scroll_bottom {
            self.scroll_up();
        } else {
            self.cursor_y += 1;
        }
    }

    /// Carriage return - move cursor to beginning of line
    pub fn carriage_return(&mut self) {
        self.cursor_x = 0;
    }

    /// Scroll the terminal up by one line
    pub fn scroll_up(&mut self) {
        if self.scroll_top < self.scroll_bottom {
            // Save the top line to scrollback before removing
            let top_line = self.grid.remove(self.scroll_top);
            self.scrollback.push(top_line);
            
            // Trim scrollback if too large
            while self.scrollback.len() > MAX_SCROLLBACK {
                self.scrollback.remove(0);
            }
            
            self.grid.insert(
                self.scroll_bottom,
                (0..self.cols).map(|_| Cell::default()).collect(),
            );
            
            // Reset scroll to bottom when new content arrives
            self.scroll_offset = 0;
        }
    }

    /// Scroll the terminal down by one line
    pub fn scroll_down(&mut self) {
        if self.scroll_top < self.scroll_bottom {
            self.grid.remove(self.scroll_bottom);
            self.grid.insert(
                self.scroll_top,
                (0..self.cols).map(|_| Cell::default()).collect(),
            );
        }
    }

    /// Move cursor to position (1-indexed from terminal perspective, 0-indexed internally)
    pub fn move_cursor(&mut self, row: usize, col: usize) {
        self.cursor_y = row.saturating_sub(1).min(self.rows - 1);
        self.cursor_x = col.saturating_sub(1).min(self.cols - 1);
    }

    /// Move cursor up by n lines
    pub fn cursor_up(&mut self, n: usize) {
        self.cursor_y = self.cursor_y.saturating_sub(n);
    }

    /// Move cursor down by n lines
    pub fn cursor_down(&mut self, n: usize) {
        self.cursor_y = (self.cursor_y + n).min(self.rows - 1);
    }

    /// Move cursor forward by n columns
    pub fn cursor_forward(&mut self, n: usize) {
        self.cursor_x = (self.cursor_x + n).min(self.cols - 1);
    }

    /// Move cursor backward by n columns
    pub fn cursor_backward(&mut self, n: usize) {
        self.cursor_x = self.cursor_x.saturating_sub(n);
    }

    /// Erase from cursor to end of line
    pub fn erase_line_to_end(&mut self) {
        if self.cursor_y < self.rows {
            for x in self.cursor_x..self.cols {
                self.grid[self.cursor_y][x] = Cell::default();
            }
        }
    }

    /// Erase from start of line to cursor
    pub fn erase_line_to_start(&mut self) {
        if self.cursor_y < self.rows {
            for x in 0..=self.cursor_x.min(self.cols - 1) {
                self.grid[self.cursor_y][x] = Cell::default();
            }
        }
    }

    /// Erase entire line
    pub fn erase_line(&mut self) {
        if self.cursor_y < self.rows {
            self.grid[self.cursor_y] = (0..self.cols).map(|_| Cell::default()).collect();
        }
    }

    /// Erase from cursor to end of screen
    pub fn erase_screen_to_end(&mut self) {
        self.erase_line_to_end();
        for y in (self.cursor_y + 1)..self.rows {
            self.grid[y] = (0..self.cols).map(|_| Cell::default()).collect();
        }
    }

    /// Erase from start of screen to cursor
    pub fn erase_screen_to_start(&mut self) {
        self.erase_line_to_start();
        for y in 0..self.cursor_y {
            self.grid[y] = (0..self.cols).map(|_| Cell::default()).collect();
        }
    }

    /// Erase entire screen
    pub fn erase_screen(&mut self) {
        for y in 0..self.rows {
            self.grid[y] = (0..self.cols).map(|_| Cell::default()).collect();
        }
        self.cursor_x = 0;
        self.cursor_y = 0;
    }

    /// Tab - move to next tab stop (every 8 columns)
    pub fn tab(&mut self) {
        let next_tab = ((self.cursor_x / 8) + 1) * 8;
        self.cursor_x = next_tab.min(self.cols - 1);
    }

    /// Backspace - move cursor back one position
    pub fn backspace(&mut self) {
        if self.cursor_x > 0 {
            self.cursor_x -= 1;
        }
    }

    /// Save cursor position
    pub fn save_cursor(&mut self) {
        self.saved_cursor = Some((self.cursor_x, self.cursor_y));
    }

    /// Restore cursor position
    pub fn restore_cursor(&mut self) {
        if let Some((x, y)) = self.saved_cursor {
            self.cursor_x = x;
            self.cursor_y = y;
        }
    }

    /// Delete n characters at cursor position
    pub fn delete_chars(&mut self, n: usize) {
        if self.cursor_y < self.rows {
            let row = &mut self.grid[self.cursor_y];
            for _ in 0..n {
                if self.cursor_x < row.len() {
                    row.remove(self.cursor_x);
                    row.push(Cell::default());
                }
            }
        }
    }

    /// Insert n blank characters at cursor position
    pub fn insert_chars(&mut self, n: usize) {
        if self.cursor_y < self.rows {
            let row = &mut self.grid[self.cursor_y];
            for _ in 0..n {
                if row.len() >= self.cols {
                    row.pop();
                }
                row.insert(self.cursor_x, Cell::default());
            }
        }
    }

    /// Delete n lines at cursor position
    pub fn delete_lines(&mut self, n: usize) {
        for _ in 0..n {
            if self.cursor_y < self.rows {
                self.grid.remove(self.cursor_y);
                self.grid.insert(
                    self.scroll_bottom.min(self.rows - 1),
                    (0..self.cols).map(|_| Cell::default()).collect(),
                );
            }
        }
    }

    /// Insert n blank lines at cursor position
    pub fn insert_lines(&mut self, n: usize) {
        for _ in 0..n {
            if self.cursor_y <= self.scroll_bottom {
                self.grid.remove(self.scroll_bottom.min(self.rows - 1));
                self.grid.insert(
                    self.cursor_y,
                    (0..self.cols).map(|_| Cell::default()).collect(),
                );
            }
        }
    }

    /// Reset terminal state
    pub fn reset(&mut self) {
        self.cursor_x = 0;
        self.cursor_y = 0;
        self.current_attrs = CellAttributes::default();
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
        self.saved_cursor = None;
        self.scrollback.clear();
        self.scroll_offset = 0;
        self.vte_parser = vte::Parser::new();
        self.erase_screen();
    }

    /// Resize the terminal
    pub fn resize(&mut self, new_cols: usize, new_rows: usize) {
        if new_cols == self.cols && new_rows == self.rows {
            return;
        }
        
        // Adjust rows
        while self.grid.len() < new_rows {
            self.grid.push((0..self.cols).map(|_| Cell::default()).collect());
        }
        while self.grid.len() > new_rows {
            // Move excess rows to scrollback
            if !self.grid.is_empty() {
                let row = self.grid.remove(0);
                self.scrollback.push(row);
            }
        }

        // Adjust columns in grid
        for row in &mut self.grid {
            while row.len() < new_cols {
                row.push(Cell::default());
            }
            while row.len() > new_cols {
                row.pop();
            }
        }
        
        // Adjust columns in scrollback too
        for row in &mut self.scrollback {
            while row.len() < new_cols {
                row.push(Cell::default());
            }
            while row.len() > new_cols {
                row.pop();
            }
        }
        
        // Trim scrollback if too large
        while self.scrollback.len() > MAX_SCROLLBACK {
            self.scrollback.remove(0);
        }

        self.cols = new_cols;
        self.rows = new_rows;
        self.scroll_bottom = new_rows - 1;

        // Clamp cursor position
        self.cursor_x = self.cursor_x.min(new_cols.saturating_sub(1));
        self.cursor_y = self.cursor_y.min(new_rows.saturating_sub(1));
        
        // Reset scroll offset on resize
        self.scroll_offset = 0;
    }
}
