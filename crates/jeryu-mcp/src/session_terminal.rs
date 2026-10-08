//! Terminal helpers shared by every `session.*` backend: named keys to bytes,
//! and ANSI stripping for the `text` read format.

/// Largest single input a `session.send_*` call may deliver.
pub const MAX_INPUT_BYTES: usize = 64 * 1024;
/// Longest `session.wait_for` may block one MCP call.
pub const MAX_WAIT_MS: u64 = 60_000;

/// The bytes a named key sends to a terminal in its default (normal cursor) mode.
#[must_use]
pub fn key_bytes(name: &str) -> Option<&'static [u8]> {
    Some(match name.to_ascii_lowercase().as_str() {
        "enter" | "return" => b"\r",
        "tab" => b"\t",
        "escape" | "esc" => b"\x1b",
        "backspace" => b"\x7f",
        "space" => b" ",
        "up" => b"\x1b[A",
        "down" => b"\x1b[B",
        "right" => b"\x1b[C",
        "left" => b"\x1b[D",
        "home" => b"\x1b[H",
        "end" => b"\x1b[F",
        "ctrl-c" => b"\x03",
        "ctrl-d" => b"\x04",
        "ctrl-l" => b"\x0c",
        "ctrl-z" => b"\x1a",
        _ => return None,
    })
}

/// Terminal bytes as readable text: CSI, OSC, and two-byte escape sequences
/// removed, carriage returns before newlines dropped, invalid UTF-8 replaced.
#[must_use]
pub fn strip_ansi(bytes: &[u8]) -> String {
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte != 0x1b {
            if !(byte == b'\r' && bytes.get(index + 1) == Some(&b'\n')) {
                out.push(byte);
            }
            index += 1;
            continue;
        }
        match bytes.get(index + 1) {
            // CSI: ESC [ params... final byte in 0x40..=0x7e
            Some(b'[') => {
                index += 2;
                while index < bytes.len() && !(0x40..=0x7e).contains(&bytes[index]) {
                    index += 1;
                }
                index += 1;
            }
            // OSC: ESC ] ... terminated by BEL or ESC \
            Some(b']') => {
                index += 2;
                while index < bytes.len() {
                    if bytes[index] == 0x07 {
                        index += 1;
                        break;
                    }
                    if bytes[index] == 0x1b && bytes.get(index + 1) == Some(&b'\\') {
                        index += 2;
                        break;
                    }
                    index += 1;
                }
            }
            // Other escapes: ESC, any intermediate bytes (0x20..=0x2f), then
            // one final byte, e.g. ESC ( B or ESC =.
            Some(_) => {
                index += 1;
                while index < bytes.len() && (0x20..=0x2f).contains(&bytes[index]) {
                    index += 1;
                }
                index += 1;
            }
            None => index += 1,
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_keys_map_to_terminal_bytes() {
        assert_eq!(key_bytes("Enter"), Some(&b"\r"[..]));
        assert_eq!(key_bytes("ctrl-c"), Some(&b"\x03"[..]));
        assert_eq!(key_bytes("up"), Some(&b"\x1b[A"[..]));
        assert_eq!(key_bytes("rm -rf"), None);
    }

    #[test]
    fn ansi_is_stripped_but_text_and_newlines_survive() {
        let raw = b"\x1b[1;32m$ cargo test\x1b[0m\r\n\x1b]0;title\x07PASS \xff\x1b(B\r\n";
        assert_eq!(strip_ansi(raw), "$ cargo test\nPASS \u{fffd}\n");
        assert_eq!(strip_ansi(b"\x1b[2J\x1b[H"), "");
        assert_eq!(strip_ansi(b"half\x1b["), "half");
    }
}
