/// Convert a keyboard event to bytes to send to the PTY
pub fn key_to_escape_sequence(key: &str, ctrl: bool, alt: bool, shift: bool) -> Option<Vec<u8>> {
    // Handle Ctrl+key combinations
    if ctrl {
        match key.to_lowercase().as_str() {
            "a" => return Some(vec![0x01]),
            "b" => return Some(vec![0x02]),
            "c" => return Some(vec![0x03]),
            "d" => return Some(vec![0x04]),
            "e" => return Some(vec![0x05]),
            "f" => return Some(vec![0x06]),
            "g" => return Some(vec![0x07]),
            "h" => return Some(vec![0x08]),
            "i" => return Some(vec![0x09]),
            "j" => return Some(vec![0x0A]),
            "k" => return Some(vec![0x0B]),
            "l" => return Some(vec![0x0C]),
            "m" => return Some(vec![0x0D]),
            "n" => return Some(vec![0x0E]),
            "o" => return Some(vec![0x0F]),
            "p" => return Some(vec![0x10]),
            "q" => return Some(vec![0x11]),
            "r" => return Some(vec![0x12]),
            "s" => return Some(vec![0x13]),
            "t" => return Some(vec![0x14]),
            "u" => return Some(vec![0x15]),
            "v" => return Some(vec![0x16]),
            "w" => return Some(vec![0x17]),
            "x" => return Some(vec![0x18]),
            "y" => return Some(vec![0x19]),
            "z" => return Some(vec![0x1A]),
            "[" => return Some(vec![0x1B]),
            "\\" => return Some(vec![0x1C]),
            "]" => return Some(vec![0x1D]),
            "^" | "6" => return Some(vec![0x1E]),
            "_" | "-" => return Some(vec![0x1F]),
            _ => {}
        }
    }

    // Handle Alt+key (send ESC prefix)
    if alt && key.len() == 1 {
        let c = key.chars().next().unwrap();
        return Some(vec![0x1B, c as u8]);
    }

    // Handle special keys
    match key {
        // Arrow keys
        "ArrowUp" => Some(b"\x1b[A".to_vec()),
        "ArrowDown" => Some(b"\x1b[B".to_vec()),
        "ArrowRight" => Some(b"\x1b[C".to_vec()),
        "ArrowLeft" => Some(b"\x1b[D".to_vec()),
        
        // Function keys
        "F1" => Some(b"\x1bOP".to_vec()),
        "F2" => Some(b"\x1bOQ".to_vec()),
        "F3" => Some(b"\x1bOR".to_vec()),
        "F4" => Some(b"\x1bOS".to_vec()),
        "F5" => Some(b"\x1b[15~".to_vec()),
        "F6" => Some(b"\x1b[17~".to_vec()),
        "F7" => Some(b"\x1b[18~".to_vec()),
        "F8" => Some(b"\x1b[19~".to_vec()),
        "F9" => Some(b"\x1b[20~".to_vec()),
        "F10" => Some(b"\x1b[21~".to_vec()),
        "F11" => Some(b"\x1b[23~".to_vec()),
        "F12" => Some(b"\x1b[24~".to_vec()),
        
        // Navigation keys
        "Home" => Some(b"\x1b[H".to_vec()),
        "End" => Some(b"\x1b[F".to_vec()),
        "Insert" => Some(b"\x1b[2~".to_vec()),
        "Delete" => Some(b"\x1b[3~".to_vec()),
        "PageUp" => Some(b"\x1b[5~".to_vec()),
        "PageDown" => Some(b"\x1b[6~".to_vec()),
        
        // Control keys
        "Enter" => Some(vec![0x0D]),
        "Escape" => Some(vec![0x1B]),
        "Tab" => {
            if shift {
                Some(b"\x1b[Z".to_vec())
            } else {
                Some(vec![0x09])
            }
        }
        "Backspace" => Some(vec![0x7F]),
        "Space" => Some(vec![0x20]),
        
        // Regular characters (single char keys)
        _ if key.len() == 1 => {
            let c = key.chars().next().unwrap();
            if shift && c.is_ascii_alphabetic() {
                Some(vec![c.to_ascii_uppercase() as u8])
            } else {
                Some(vec![c as u8])
            }
        }
        
        // Ignore modifier-only keys and unhandled keys
        "Shift" | "Control" | "Alt" | "Meta" | "CapsLock" | "NumLock" | "ScrollLock" => None,
        _ => None,
    }
}
