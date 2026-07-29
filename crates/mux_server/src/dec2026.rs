//! Incremental recognizer for DEC synchronized-output private mode 2026.

use std::time::{Duration, Instant};

const SYNC_TIMEOUT: Duration = Duration::from_millis(100);
const SYNC_PREFIX: &[u8] = b"\x1b[?2026";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Dec2026Transitions {
    began: bool,
    ended: bool,
}

impl Dec2026Transitions {
    pub fn began(self) -> bool {
        self.began
    }

    pub fn ended(self) -> bool {
        self.ended
    }
}

pub struct Dec2026Parser {
    in_sync: bool,
    begin_time: Option<Instant>,
    matched_prefix_bytes: usize,
}

impl Dec2026Parser {
    pub fn new() -> Self {
        Self {
            in_sync: false,
            begin_time: None,
            matched_prefix_bytes: 0,
        }
    }

    /// Observe bytes without consuming or rewriting them. The caller must still
    /// forward the original slice to the terminal parser exactly once.
    pub fn parse(&mut self, bytes: &[u8]) -> Dec2026Transitions {
        let mut transitions = Dec2026Transitions::default();

        for &byte in bytes {
            if self.matched_prefix_bytes < SYNC_PREFIX.len() {
                if byte == SYNC_PREFIX[self.matched_prefix_bytes] {
                    self.matched_prefix_bytes += 1;
                } else {
                    self.matched_prefix_bytes = usize::from(byte == SYNC_PREFIX[0]);
                }
                continue;
            }

            self.matched_prefix_bytes = usize::from(byte == SYNC_PREFIX[0]);
            match byte {
                b'h' => {
                    transitions.began |= !self.in_sync;
                    self.in_sync = true;
                    self.begin_time = Some(Instant::now());
                }
                b'l' if self.in_sync => {
                    self.in_sync = false;
                    self.begin_time = None;
                    transitions.ended = true;
                }
                _ => {}
            }
        }

        transitions
    }

    /// Close an unpaired synchronized-output window even when the PTY becomes
    /// quiet, so one malformed application cannot freeze rendering forever.
    pub fn check_timeout(&mut self) -> bool {
        if self
            .begin_time
            .is_some_and(|begin_time| begin_time.elapsed() >= SYNC_TIMEOUT)
        {
            self.in_sync = false;
            self.begin_time = None;
            return true;
        }
        false
    }

    pub fn is_in_sync(&self) -> bool {
        self.in_sync
    }

    pub fn reset(&mut self) {
        self.in_sync = false;
        self.begin_time = None;
        self.matched_prefix_bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BEGIN: &[u8] = b"\x1b[?2026h";
    const END: &[u8] = b"\x1b[?2026l";

    #[test]
    fn recognizes_private_mode_pair() {
        let mut parser = Dec2026Parser::new();
        assert_eq!(
            parser.parse(BEGIN),
            Dec2026Transitions {
                began: true,
                ended: false,
            }
        );
        assert!(parser.is_in_sync());
        assert_eq!(
            parser.parse(END),
            Dec2026Transitions {
                began: false,
                ended: true,
            }
        );
        assert!(!parser.is_in_sync());
    }

    #[test]
    fn recognizes_every_read_split() {
        for sequence in [BEGIN, END] {
            for split in 0..=sequence.len() {
                let mut parser = Dec2026Parser::new();
                if sequence == END {
                    parser.parse(BEGIN);
                }
                let first = parser.parse(&sequence[..split]);
                let second = parser.parse(&sequence[split..]);
                assert_eq!(first.began() || second.began(), sequence == BEGIN);
                assert_eq!(first.ended() || second.ended(), sequence == END);
                assert_eq!(parser.is_in_sync(), sequence == BEGIN);
            }
        }
    }

    #[test]
    fn recognizes_byte_at_a_time_with_noise() {
        let mut parser = Dec2026Parser::new();
        let mut began = false;
        let mut ended = false;
        for &byte in b"text\x1b[?2026hupdated\x1b[?2026lmore" {
            let transitions = parser.parse(&[byte]);
            began |= transitions.began();
            ended |= transitions.ended();
        }
        assert!(began);
        assert!(ended);
        assert!(!parser.is_in_sync());
    }

    #[test]
    fn ignores_legacy_bracketed_paste_like_markers() {
        let mut parser = Dec2026Parser::new();
        let transitions = parser.parse(b"\x1b201~text\x1b202~");
        assert_eq!(transitions, Dec2026Transitions::default());
        assert!(!parser.is_in_sync());
    }

    #[test]
    fn timeout_closes_unpaired_begin_without_sleeping() {
        let mut parser = Dec2026Parser::new();
        parser.parse(BEGIN);
        parser.begin_time = Some(Instant::now() - SYNC_TIMEOUT);
        assert!(parser.check_timeout());
        assert!(!parser.is_in_sync());
        assert!(!parser.check_timeout());
    }
}
