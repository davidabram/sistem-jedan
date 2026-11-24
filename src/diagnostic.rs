use std::fmt::{self, Write};

pub(super) const MAX_BYTES: usize = 8 * 1024;
const TRUNCATED: &str = "… [truncated]";

pub(super) fn message(value: impl fmt::Display) -> String {
    let mut output = Diagnostic(String::new());
    let _ = write!(output, "{value}");
    output.0
}

pub(super) fn truncate(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let end = value.floor_char_boundary(max_bytes - TRUNCATED.len());
    let mut output = String::with_capacity(end + TRUNCATED.len());
    output.push_str(&value[..end]);
    output.push_str(TRUNCATED);
    output
}

struct Diagnostic(String);

impl Write for Diagnostic {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if text.len() <= MAX_BYTES - self.0.len() {
            self.0.push_str(text);
            return Ok(());
        }
        let prefix = MAX_BYTES - TRUNCATED.len();
        self.0.truncate(self.0.floor_char_boundary(prefix));
        self.0
            .push_str(&text[..text.floor_char_boundary(prefix - self.0.len())]);
        self.0.push_str(TRUNCATED);
        Err(fmt::Error)
    }
}
