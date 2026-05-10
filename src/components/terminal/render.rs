use super::state::{CellAttributes, Color};

/// Convert a Color to a CSS color string
pub fn color_to_css(color: &Color, is_foreground: bool) -> String {
    match color {
        Color::Default => {
            if is_foreground {
                "inherit".to_string()
            } else {
                "transparent".to_string()
            }
        }
        Color::Black => "#000000".to_string(),
        Color::Red => "#cd0000".to_string(),
        Color::Green => "#00cd00".to_string(),
        Color::Yellow => "#cdcd00".to_string(),
        Color::Blue => "#0000cd".to_string(),
        Color::Magenta => "#cd00cd".to_string(),
        Color::Cyan => "#00cdcd".to_string(),
        Color::White => "#e5e5e5".to_string(),
        Color::BrightBlack => "#7f7f7f".to_string(),
        Color::BrightRed => "#ff0000".to_string(),
        Color::BrightGreen => "#00ff00".to_string(),
        Color::BrightYellow => "#ffff00".to_string(),
        Color::BrightBlue => "#5c5cff".to_string(),
        Color::BrightMagenta => "#ff00ff".to_string(),
        Color::BrightCyan => "#00ffff".to_string(),
        Color::BrightWhite => "#ffffff".to_string(),
        Color::Rgb(r, g, b) => format!("#{:02x}{:02x}{:02x}", r, g, b),
        Color::Index(idx) => index_to_css(*idx),
    }
}

/// Convert a 256-color index to CSS color
fn index_to_css(idx: u8) -> String {
    match idx {
        // Standard colors (0-15)
        0..=15 => {
            let colors = [
                "#000000", "#cd0000", "#00cd00", "#cdcd00",
                "#0000cd", "#cd00cd", "#00cdcd", "#e5e5e5",
                "#7f7f7f", "#ff0000", "#00ff00", "#ffff00",
                "#5c5cff", "#ff00ff", "#00ffff", "#ffffff",
            ];
            colors[idx as usize].to_string()
        }
        // 6x6x6 color cube (16-231)
        16..=231 => {
            let idx = idx - 16;
            let r = (idx / 36) * 51;
            let g = ((idx / 6) % 6) * 51;
            let b = (idx % 6) * 51;
            format!("#{:02x}{:02x}{:02x}", r, g, b)
        }
        // Grayscale (232-255)
        232..=255 => {
            let gray = (idx - 232) * 10 + 8;
            format!("#{:02x}{:02x}{:02x}", gray, gray, gray)
        }
    }
}

/// Generate inline style string for a cell's attributes
pub fn cell_style(attrs: &CellAttributes) -> String {
    let mut styles = Vec::new();

    // Handle reverse video
    let (fg, bg) = if attrs.reverse {
        (&attrs.bg, &attrs.fg)
    } else {
        (&attrs.fg, &attrs.bg)
    };

    // Foreground color
    let fg_css = color_to_css(fg, true);
    if fg_css != "inherit" {
        styles.push(format!("color:{}", fg_css));
    }

    // Background color
    let bg_css = color_to_css(bg, false);
    if bg_css != "transparent" {
        styles.push(format!("background-color:{}", bg_css));
    }

    // Font weight
    if attrs.bold {
        styles.push("font-weight:bold".to_string());
    }

    // Dim (reduced opacity)
    if attrs.dim {
        styles.push("opacity:0.5".to_string());
    }

    // Font style
    if attrs.italic {
        styles.push("font-style:italic".to_string());
    }

    // Text decoration
    let mut decorations = Vec::new();
    if attrs.underline {
        decorations.push("underline");
    }
    if attrs.strikethrough {
        decorations.push("line-through");
    }
    if !decorations.is_empty() {
        styles.push(format!("text-decoration:{}", decorations.join(" ")));
    }

    styles.join(";")
}
