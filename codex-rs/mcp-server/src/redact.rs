use std::io;

/// Lightweight redactor for sensitive substrings in log lines.
/// Currently masks any occurrence of a Bearer token (case-insensitive)
/// after the substring "Bearer ". Example:
/// - "Authorization: Bearer abc.def.ghi" -> "Authorization: Bearer ****"
pub(crate) fn redact_sensitive(input: &str) -> String {
    fn redact_once(s: &str, needle: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut start = 0usize;
        let mut i = 0usize;
        let ln = s.len();
        while i < ln {
            if let Some(idx) = s[i..].find(needle) {
                let hit = i + idx;
                // write segment before hit
                out.push_str(&s[start..hit]);
                // write the marker itself
                out.push_str(needle);
                // skip token chars (non-whitespace, non-quote)
                let mut j = hit + needle.len();
                while j < ln {
                    let ch = s.as_bytes()[j];
                    if ch == b'\n'
                        || ch == b'\r'
                        || ch == b'\t'
                        || ch == b' '
                        || ch == b'"'
                        || ch == b'\''
                    {
                        break;
                    }
                    j += 1;
                }
                out.push_str("****");
                i = j;
                start = j;
            } else {
                break;
            }
        }
        if start < ln {
            out.push_str(&s[start..]);
        }
        out
    }

    // Perform redaction for common casing variants.
    let s1 = redact_once(input, "Bearer ");
    redact_once(&s1, "bearer ")
}

/// Writer wrapper that redacts sensitive substrings (e.g., Bearer tokens)
/// from log output before writing to the underlying writer.
pub(crate) struct SanitizingWriter<W: io::Write> {
    inner: W,
}

impl<W: io::Write> SanitizingWriter<W> {
    pub(crate) fn new(inner: W) -> Self {
        Self { inner }
    }
}

impl<W: io::Write> io::Write for SanitizingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Convert bytes to UTF-8 lossily, redact, and write.
        let s = String::from_utf8_lossy(buf);
        let redacted = redact_sensitive(&s);
        self.inner.write_all(redacted.as_bytes())?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
