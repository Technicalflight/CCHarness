// Stream-side extraction of provider-injected confidence markers.
//
// Some OpenAI-compatible relays append a `\confidence{NN}` marker to the
// assistant text. It is metadata, not content: parse it out, keep it out of
// the visible text AND out of the prefix history, and surface it as a field.
//
// The marker can be split across SSE deltas ("...\" + "confidence{9" + "0}"),
// so the filter holds back any suffix that could still become a marker and
// flushes it as regular text at end of stream.

const MARKER: &str = "\\confidence{";

pub struct ConfidenceFilter {
    holdback: String,
    confidence: Option<u32>,
}

impl ConfidenceFilter {
    pub fn new() -> Self {
        Self { holdback: String::new(), confidence: None }
    }

    /// Feed one delta; returns the clean text safe to emit now.
    pub fn push(&mut self, text: &str) -> String {
        self.holdback.push_str(text);
        let mut out = String::new();
        loop {
            match self.holdback.find(MARKER) {
                Some(i) => {
                    let start = i + MARKER.len();
                    match self.holdback[start..].find('}') {
                        Some(close_rel) => {
                            let raw = self.holdback[start..start + close_rel].trim();
                            let raw = raw.trim_end_matches('%').trim();
                            if let Ok(n) = raw.parse::<u32>() {
                                self.confidence = Some(n);
                            }
                            out.push_str(&self.holdback[..i]);
                            self.holdback.drain(..start + close_rel + 1);
                            continue;
                        }
                        None => {
                            // marker opened but not closed yet — hold the tail
                            out.push_str(&self.holdback[..i]);
                            self.holdback.drain(..i);
                            return out;
                        }
                    }
                }
                None => {
                    let keep = partial_marker_suffix_len(&self.holdback);
                    let emit_len = self.holdback.len() - keep;
                    out.push_str(&self.holdback[..emit_len]);
                    self.holdback.drain(..emit_len);
                    return out;
                }
            }
        }
    }

    /// Flush the residual holdback at end of stream.
    pub fn finish(&mut self) -> String {
        std::mem::take(&mut self.holdback)
    }

    pub fn confidence(&self) -> Option<u32> {
        self.confidence
    }
}

impl Default for ConfidenceFilter {
    fn default() -> Self {
        Self::new()
    }
}

/// Length of the longest suffix of `s` that is a proper prefix of MARKER
/// (i.e. the beginning of a marker that may complete in a later delta).
fn partial_marker_suffix_len(s: &str) -> usize {
    let max = MARKER.len().saturating_sub(1).min(s.len());
    for l in (1..=max).rev() {
        let at = s.len() - l;
        if s.is_char_boundary(at) && MARKER.as_bytes().starts_with(&s.as_bytes()[at..]) {
            return l;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(parts: &[&str]) -> (String, Option<u32>) {
        let mut f = ConfidenceFilter::new();
        let mut out = String::new();
        for p in parts {
            out.push_str(&f.push(p));
        }
        out.push_str(&f.finish());
        (out, f.confidence())
    }

    #[test]
    fn passes_plain_text_untouched() {
        let (out, c) = collect(&["Hello! How can I help", " you today?"]);
        assert_eq!(out, "Hello! How can I help you today?");
        assert_eq!(c, None);
    }

    #[test]
    fn strips_whole_marker_in_one_delta() {
        let (out, c) = collect(&["Hello!\n\\confidence{90}"]);
        assert_eq!(out, "Hello!\n");
        assert_eq!(c, Some(90));
    }

    #[test]
    fn strips_marker_split_across_deltas() {
        let (out, c) = collect(&["Hello ", "\\confi", "dence{8", "0} bye"]);
        assert_eq!(out, "Hello  bye");
        assert_eq!(c, Some(80));
    }

    #[test]
    fn tolerates_percent_and_spaces() {
        let (out, c) = collect(&["ok \\confidence{ 95% }"]);
        assert_eq!(out, "ok ");
        assert_eq!(c, Some(95));
    }

    #[test]
    fn incomplete_marker_at_eof_is_kept_as_text() {
        let (out, c) = collect(&["text \\confidence{9"]);
        assert_eq!(out, "text \\confidence{9");
        assert_eq!(c, None);
    }

    #[test]
    fn backslash_alone_is_flushed() {
        let (out, c) = collect(&["path C:\\ ", "more"]);
        assert_eq!(out, "path C:\\ more");
        assert_eq!(c, None);
    }

    #[test]
    fn last_marker_wins_and_text_before_is_kept() {
        let (out, c) = collect(&["a\\confidence{10}b\\confidence{88}c"]);
        assert_eq!(out, "abc");
        assert_eq!(c, Some(88));
    }
}
