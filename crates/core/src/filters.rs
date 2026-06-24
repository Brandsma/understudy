//! Strip <think>…</think> reasoning blocks from local reasoning models (qwen3,
//! deepseek-r1, …) so the comprehension answer stays clean.

use regex::Regex;
use std::sync::OnceLock;

const OPEN: &str = "<think>";
const CLOSE: &str = "</think>";

fn think_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)<think>.*?</think>").unwrap())
}

/// Remove complete <think>…</think> blocks from a finished string.
pub fn strip_think(text: &str) -> String {
    think_re().replace_all(text, "").trim().to_string()
}

/// Streaming filter: feed deltas, get back only the non-think visible text.
/// Handles tags split across chunk boundaries by holding back a tail that could
/// be the start of an opening/closing tag.
#[derive(Default)]
pub struct ThinkFilter {
    buf: String,
    in_think: bool,
}

impl ThinkFilter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, delta: &str) -> String {
        self.buf.push_str(delta);
        let mut out = String::new();
        loop {
            if !self.in_think {
                if let Some(idx) = self.buf.find(OPEN) {
                    out.push_str(&self.buf[..idx]);
                    self.buf.drain(..idx + OPEN.len());
                    self.in_think = true;
                    continue;
                }
                let safe = safe_len(&self.buf, OPEN);
                out.push_str(&self.buf[..safe]);
                self.buf.drain(..safe);
                break;
            } else if let Some(idx) = self.buf.find(CLOSE) {
                self.buf.drain(..idx + CLOSE.len());
                self.in_think = false;
                continue;
            } else {
                let safe = safe_len(&self.buf, CLOSE);
                self.buf.drain(..safe); // drop reasoning text
                break;
            }
        }
        out
    }

    pub fn flush(&mut self) -> String {
        if self.in_think {
            self.buf.clear();
            String::new()
        } else {
            std::mem::take(&mut self.buf)
        }
    }
}

/// Leading bytes that cannot be the start of `tag` (keep the rest as a held tail).
fn safe_len(buf: &str, tag: &str) -> usize {
    let max_keep = buf.len().min(tag.len() - 1);
    for keep in (1..=max_keep).rev() {
        let cut = buf.len() - keep;
        if buf.is_char_boundary(cut) && &buf[cut..] == &tag[..keep] {
            return cut;
        }
    }
    buf.len()
}
