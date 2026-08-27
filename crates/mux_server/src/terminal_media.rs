// §16.13 Server-side terminal media & action scanner.
//
// The guest TUI (or any PTY process) can push three kinds of control
// sequences through its output stream that the mux server must intercept at
// its own boundary, before the emulator grid parser sees them:
//
//   - Kitty Graphics APC (`ESC _ G … ST`): stripped from the grid stream and
//     emitted as a typed `PaneMedia` notification so clients can render the
//     image without parsing raw bytes. Continuation chunks (`m=1` … `m=0`)
//     sharing one image id are reassembled per pane and published as one
//     final `PaneMedia`.
//   - OSC 8 hyperlinks (including `z3rm-download:` URIs): these are ordinary
//     grid content and stay in `grid_bytes` untouched — a visible link never
//     triggers a download merely by being rendered (the client decides on
//     click). No action is emitted.
//   - OSC 9 `z3rm-download:` / `z3rm-copy:`: BEL/ST-terminated typed action
//     sequences. Emitted as a `PaneAction` (DOWNLOAD / COPY) and consumed
//     (removed from the grid stream). Ordinary OSC 9 stays in the grid.
//   - OSC 52 clipboard: emitted as a typed COPY `PaneAction` after base64
//     decoding AND left in `grid_bytes`, so alacritty's existing
//     `ClipboardStore` path (→ clipboard hook → ServerClipboard) keeps
//     working unchanged and grid semantics are preserved.
//
// The scanner is bounded and incremental: a real PTY splits sequences at
// arbitrary byte boundaries, so state lives across `feed` calls, and every
// control-sequence buffer is capped by `MAX_CONTROL_SEQUENCE_BYTES`. On
// overflow the sequence is dropped, the parse error is logged, and scanning
// resumes at the next ground-state byte without dropping the text after it.

use std::collections::HashMap;

use base64::Engine;
use mux_protocol::proto::{PaneAction, PaneActionKind, PaneMedia};

/// Upper bound on the bytes buffered for one control sequence (APC or OSC).
/// A hostile or malformed PTY stream cannot grow the scanner past this: the
/// sequence is dropped and parsing resumes at ground state.
pub const MAX_CONTROL_SEQUENCE_BYTES: usize = 4 * 1024 * 1024;

/// Upper bound on the reassembled payload of one Kitty image.
const MAX_REASSEMBLED_MEDIA_BYTES: usize = MAX_CONTROL_SEQUENCE_BYTES;

/// Output of one `feed`: the bytes that may reach the grid parser, plus the
/// typed media and actions the client consumes directly.
#[derive(Debug, Default)]
pub struct ScanOutput {
    /// Bytes safe to hand to the emulator. Kitty APC and consumed OSC 9
    /// action sequences are absent; OSC 8 hyperlinks, OSC 52, and ordinary
    /// bytes are preserved in order.
    pub grid_bytes: Vec<u8>,
    /// Complete Kitty media, in the order their final chunk arrived.
    pub media: Vec<PaneMedia>,
    /// Typed DOWNLOAD/COPY actions, in arrival order.
    pub actions: Vec<PaneAction>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScanState {
    Ground,
    /// After a lone `ESC`, waiting for the introducer.
    Escape,
    /// After `ESC _`: deciding whether this is a Kitty APC (`G`).
    ApcIntro,
    /// In a Kitty APC, before the `;` that separates params from data.
    ApcParams,
    /// In a Kitty APC data payload.
    ApcData,
    /// In a Kitty APC after an `ESC` (checking for ST `ESC \`).
    ApcEscape,
    /// In an APC that is not Kitty (`ESC _ X`): pass through until ST/BEL.
    ApcPassthrough,
    /// Pass-through APC after an `ESC`.
    ApcPassthroughEscape,
    /// Reading an OSC number after `ESC ]`.
    OscNumber,
    /// In an OSC 9 / OSC 52 payload (buffered for decode).
    OscPayload,
    /// Buffered OSC after an `ESC`.
    OscPayloadEscape,
    /// In an OSC that is not 8/9/52: pass through until ST/BEL.
    OscPassthrough,
    /// Pass-through OSC after an `ESC`.
    OscPassthroughEscape,
}

/// A parsed Kitty chunk's parameter list. Optional fields let a continuation
/// chunk inherit values from the chunk that started the image.
#[derive(Debug, Default)]
struct KittyParams {
    action: Option<u8>,
    format: Option<u32>,
    image_id: Option<u32>,
    columns: Option<u32>,
    rows: Option<u32>,
    row: Option<i32>,
    column: Option<u32>,
    mode: Option<u32>,
    encoding: Option<u32>,
    delete: Option<u32>,
}

/// Reassembled Kitty image awaiting its final chunk.
struct PendingMedia {
    format: u32,
    row: i32,
    column: u32,
    columns: u32,
    rows: u32,
    data: Vec<u8>,
}

/// Incremental, bounded scanner for the control sequences above.
///
/// `feed` may be called once per PTY read; the scanner keeps its state so a
/// sequence split across reads is recognized and consumed whole.
pub struct TerminalMediaScanner {
    state: ScanState,
    /// Raw bytes of the control sequence currently being scanned. Bounded by
    /// `MAX_CONTROL_SEQUENCE_BYTES`; pass-through sequences are flushed to
    /// `grid_bytes` from here.
    buffer: Vec<u8>,
    /// OSC number being read after `ESC ]`.
    osc_number: u32,
    /// Digits consumed so far for `osc_number`.
    osc_digits: u32,
    /// Kitty continuation state per image id.
    pending: HashMap<u32, PendingMedia>,
}

impl Default for TerminalMediaScanner {
    fn default() -> Self {
        Self {
            state: ScanState::Ground,
            buffer: Vec::new(),
            osc_number: 0,
            osc_digits: 0,
            pending: HashMap::new(),
        }
    }
}

impl TerminalMediaScanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Scan one PTY byte batch, returning the grid-safe bytes and any
    /// complete media/actions. State persists for the next batch.
    pub fn feed(&mut self, bytes: &[u8]) -> ScanOutput {
        let mut output = ScanOutput {
            grid_bytes: Vec::with_capacity(bytes.len()),
            ..ScanOutput::default()
        };
        let mut index = 0;
        while index < bytes.len() {
            let byte = bytes[index];
            index += 1;
            match self.state {
                ScanState::Ground => {
                    if byte == 0x1b {
                        self.state = ScanState::Escape;
                    } else {
                        output.grid_bytes.push(byte);
                    }
                }
                ScanState::Escape => match byte {
                    0x1b => {} // double ESC: still escaping
                    b'_' => {
                        self.start_sequence(b'_');
                        self.state = ScanState::ApcIntro;
                    }
                    b']' => {
                        self.start_sequence(b']');
                        self.osc_number = 0;
                        self.osc_digits = 0;
                        self.state = ScanState::OscNumber;
                    }
                    _ => {
                        output.grid_bytes.push(0x1b);
                        output.grid_bytes.push(byte);
                        self.state = ScanState::Ground;
                    }
                },
                ScanState::ApcIntro => {
                    self.buffer.push(byte);
                    if byte == b'G' {
                        self.state = ScanState::ApcParams;
                    } else {
                        self.state = ScanState::ApcPassthrough;
                    }
                }
                ScanState::ApcParams => {
                    self.buffer.push(byte);
                    if byte == b';' {
                        self.state = ScanState::ApcData;
                    } else if byte == 0x1b {
                        // Possible ST start; do not push ESC into buffer yet.
                        self.buffer.pop();
                        self.state = ScanState::ApcEscape;
                    } else if byte == 0x07 {
                        // BEL terminates a Kitty APC too.
                        self.finish_kitty(&mut output);
                    }
                    if self.buffer.len() > MAX_CONTROL_SEQUENCE_BYTES {
                        self.drop_overflow("Kitty APC");
                        break;
                    }
                }
                ScanState::ApcData => {
                    self.buffer.push(byte);
                    if byte == 0x1b {
                        self.buffer.pop();
                        self.state = ScanState::ApcEscape;
                    } else if byte == 0x07 {
                        self.finish_kitty(&mut output);
                    }
                    if self.buffer.len() > MAX_CONTROL_SEQUENCE_BYTES {
                        self.drop_overflow("Kitty APC");
                        break;
                    }
                }
                ScanState::ApcEscape => {
                    if byte == b'\\' {
                        self.finish_kitty(&mut output);
                    } else {
                        // An ESC that is not ST aborts the APC; re-dispatch
                        // this byte from ground state.
                        self.state = ScanState::Ground;
                        self.buffer.clear();
                        index -= 1;
                    }
                }
                ScanState::ApcPassthrough => {
                    self.buffer.push(byte);
                    if byte == 0x1b {
                        self.state = ScanState::ApcPassthroughEscape;
                    } else if byte == 0x07 {
                        self.flush_passthrough(&mut output);
                    }
                    if self.buffer.len() > MAX_CONTROL_SEQUENCE_BYTES {
                        self.drop_overflow("APC");
                        break;
                    }
                }
                ScanState::ApcPassthroughEscape => {
                    if byte == b'\\' {
                        self.buffer.push(byte);
                        self.flush_passthrough(&mut output);
                    } else {
                        // Not ST: the ESC is data in the pass-through text.
                        self.state = ScanState::ApcPassthrough;
                    }
                }
                ScanState::OscNumber => {
                    self.buffer.push(byte);
                    match byte {
                        b'0'..=b'9' if self.osc_digits < 5 => {
                            self.osc_number =
                                self.osc_number.saturating_mul(10) + u32::from(byte - b'0');
                            self.osc_digits += 1;
                        }
                        b';' => {
                            if self.osc_number == 8 {
                                // OSC 8 hyperlink: ordinary grid content.
                                self.state = ScanState::OscPassthrough;
                            } else if self.osc_number == 9 || self.osc_number == 52 {
                                self.state = ScanState::OscPayload;
                            } else {
                                self.state = ScanState::OscPassthrough;
                            }
                        }
                        0x1b => {
                            self.state = ScanState::Escape;
                            index -= 1;
                        }
                        _ => self.state = ScanState::OscPassthrough,
                    }
                    if self.buffer.len() > MAX_CONTROL_SEQUENCE_BYTES {
                        self.drop_overflow("OSC");
                        break;
                    }
                }
                ScanState::OscPayload => {
                    self.buffer.push(byte);
                    if byte == 0x1b {
                        self.state = ScanState::OscPayloadEscape;
                    } else if byte == 0x07 {
                        self.finish_osc(&mut output);
                    }
                    if self.buffer.len() > MAX_CONTROL_SEQUENCE_BYTES {
                        self.drop_overflow("OSC");
                        break;
                    }
                }
                ScanState::OscPayloadEscape => {
                    if byte == b'\\' {
                        // ST terminates the OSC; keep the backslash so
                        // preserved sequences round-trip unchanged.
                        self.buffer.push(byte);
                        self.finish_osc(&mut output);
                    } else {
                        // Not ST: the OSC was aborted by its ESC; drop the
                        // partial payload and re-dispatch this byte from
                        // escape state.
                        self.state = ScanState::Escape;
                        self.buffer.clear();
                        index -= 1;
                    }
                }
                ScanState::OscPassthrough => {
                    self.buffer.push(byte);
                    if byte == 0x1b {
                        self.state = ScanState::OscPassthroughEscape;
                    } else if byte == 0x07 {
                        self.flush_passthrough(&mut output);
                    }
                    if self.buffer.len() > MAX_CONTROL_SEQUENCE_BYTES {
                        self.drop_overflow("OSC");
                        break;
                    }
                }
                ScanState::OscPassthroughEscape => {
                    if byte == b'\\' {
                        self.buffer.push(byte);
                        self.flush_passthrough(&mut output);
                    } else {
                        // Not ST: the OSC was aborted by its ESC; emit its
                        // buffer and re-dispatch this byte from escape state.
                        self.state = ScanState::Escape;
                        self.buffer.clear();
                        index -= 1;
                    }
                }
            }
        }
        output
    }

    /// Begin buffering a control sequence introduced by `_` or `]`. The ESC
    /// was already consumed by the caller and is replayed here.
    fn start_sequence(&mut self, introducer: u8) {
        self.buffer.clear();
        self.buffer.push(0x1b);
        self.buffer.push(introducer);
    }

    fn drop_overflow(&mut self, what: &str) {
        tracing::warn!(
            "{what} control sequence exceeded {MAX_CONTROL_SEQUENCE_BYTES} bytes; dropped"
        );
        self.buffer.clear();
        self.state = ScanState::Ground;
    }

    fn flush_passthrough(&mut self, output: &mut ScanOutput) {
        output.grid_bytes.append(&mut self.buffer);
        self.state = ScanState::Ground;
    }

    /// A complete Kitty APC is buffered: `ESC _ G params ; data`.
    fn finish_kitty(&mut self, output: &mut ScanOutput) {
        // Buffer layout: ESC _ G <params> ; <data> (ST/BEL already consumed).
        let body = std::mem::take(&mut self.buffer);
        self.state = ScanState::Ground;
        let params = &body[3..];
        let (params, data) = match params.iter().position(|&b| b == b';') {
            Some(split) => (&params[..split], &params[split + 1..]),
            None => (params, &[][..]),
        };
        let mut parsed = KittyParams::default();
        if let Err(message) = parse_kitty_params(params, &mut parsed) {
            tracing::warn!("terminal media: malformed Kitty params: {message}");
            return;
        }
        let Some(image_id) = parsed.image_id else {
            tracing::warn!("terminal media: Kitty APC without image id; dropped");
            return;
        };

        // `a=d` (or `d=1`) deletes a previously published image.
        let delete = parsed.action == Some(b'd') || parsed.delete == Some(1);
        if delete {
            output.media.push(PaneMedia {
                pane_id: String::new(),
                sequence: 0,
                image_id,
                format: parsed.format.unwrap_or(0),
                row: parsed.row.unwrap_or(0),
                column: parsed.column.unwrap_or(0),
                columns: parsed.columns.unwrap_or(0),
                rows: parsed.rows.unwrap_or(0),
                data: Vec::new(),
                final_chunk: true,
                delete: true,
            });
            self.pending.remove(&image_id);
            return;
        }

        let payload = match decode_kitty_data(data, parsed.encoding) {
            Some(payload) => payload,
            None => {
                tracing::warn!("terminal media: Kitty data is not valid base64; dropped");
                self.pending.remove(&image_id);
                return;
            }
        };

        let is_continuation = parsed.mode == Some(1);
        if is_continuation {
            // Accumulate; publish only on the final chunk (m=0/absent).
            let entry = self.pending.entry(image_id).or_insert_with(|| PendingMedia {
                format: parsed.format.unwrap_or(0),
                row: parsed.row.unwrap_or(0),
                column: parsed.column.unwrap_or(0),
                columns: parsed.columns.unwrap_or(0),
                rows: parsed.rows.unwrap_or(0),
                data: Vec::new(),
            });
            entry.data.extend_from_slice(&payload);
            if entry.data.len() > MAX_REASSEMBLED_MEDIA_BYTES {
                tracing::warn!(
                    "terminal media: image {image_id} exceeded {MAX_REASSEMBLED_MEDIA_BYTES} bytes; dropped"
                );
                self.pending.remove(&image_id);
            }
            return;
        }

        // Final chunk: complete a pending image or publish a single chunk.
        let mut media = if let Some(entry) = self.pending.remove(&image_id) {
            PaneMedia {
                pane_id: String::new(),
                sequence: 0,
                image_id,
                format: entry.format,
                row: entry.row,
                column: entry.column,
                columns: entry.columns,
                rows: entry.rows,
                data: entry.data,
                final_chunk: true,
                delete: false,
            }
        } else {
            PaneMedia {
                pane_id: String::new(),
                sequence: 0,
                image_id,
                format: parsed.format.unwrap_or(0),
                row: parsed.row.unwrap_or(0),
                column: parsed.column.unwrap_or(0),
                columns: parsed.columns.unwrap_or(0),
                rows: parsed.rows.unwrap_or(0),
                data: Vec::new(),
                final_chunk: true,
                delete: false,
            }
        };
        media.data.extend_from_slice(&payload);
        output.media.push(media);
    }

    /// A complete OSC payload is buffered: `ESC ] <number> ; <payload>`.
    fn finish_osc(&mut self, output: &mut ScanOutput) {
        let number = self.osc_number;
        // Everything after the first `;` is the payload; a trailing BEL or
        // ST (`ESC \`) is the terminator, not data.
        let payload_start = self.buffer.iter().position(|&b| b == b';').map_or(0, |s| s + 1);
        let mut payload_end = self.buffer.len();
        if payload_end > payload_start {
            match self.buffer[payload_end - 1] {
                0x07 => payload_end -= 1, // BEL
                b'\\' if payload_end >= 2 && self.buffer[payload_end - 2] == 0x1b => {
                    payload_end -= 2 // ST: ESC \
                }
                _ => {}
            }
        }
        if number == 52 {
            // OSC 52 stays in the grid so alacritty's ClipboardStore path
            // (clipboard hook → ServerClipboard) keeps working; the typed
            // COPY action is an additive signal for the guest TUI client.
            let payload = &self.buffer[payload_start..payload_end];
            let action = match decode_osc52(payload) {
                Some(text) => Some(PaneAction {
                    pane_id: String::new(),
                    sequence: 0,
                    kind: PaneActionKind::Copy as i32,
                    value: text,
                }),
                None => {
                    tracing::warn!("terminal media: OSC 52 payload is not valid base64; dropped");
                    None
                }
            };
            output.grid_bytes.append(&mut self.buffer);
            if let Some(action) = action {
                output.actions.push(action);
            }
            self.state = ScanState::Ground;
            return;
        }
        if number != 9 {
            // OSC 8 and other OSCs are ordinary grid content.
            output.grid_bytes.append(&mut self.buffer);
            self.state = ScanState::Ground;
            return;
        }
        let payload = &self.buffer[payload_start..payload_end];
        // OSC 9: only the z3rm action prefixes are consumed.
        let (kind, value) = match payload {
            p if p.starts_with(b"z3rm-download:") => (
                Some(PaneActionKind::Download),
                String::from_utf8_lossy(&p[b"z3rm-download:".len()..]).into_owned(),
            ),
            p if p.starts_with(b"z3rm-copy:") => (
                Some(PaneActionKind::Copy),
                String::from_utf8_lossy(&p[b"z3rm-copy:".len()..]).into_owned(),
            ),
            _ => (None, String::new()),
        };
        match kind {
            Some(PaneActionKind::Download) => output.actions.push(PaneAction {
                pane_id: String::new(),
                sequence: 0,
                kind: PaneActionKind::Download as i32,
                value,
            }),
            Some(PaneActionKind::Copy) => {
                let decoded = base64::engine::general_purpose::STANDARD.decode(value.as_bytes());
                match decoded {
                    Ok(text) => output.actions.push(PaneAction {
                        pane_id: String::new(),
                        sequence: 0,
                        kind: PaneActionKind::Copy as i32,
                        value: String::from_utf8_lossy(&text).into_owned(),
                    }),
                    Err(_) => {
                        tracing::warn!("terminal media: OSC 9 z3rm-copy is not valid base64");
                    }
                }
            }
            Some(PaneActionKind::Unspecified) | None => {
                // Ordinary OSC 9 (e.g. a desktop notification): grid content.
                output.grid_bytes.append(&mut self.buffer);
            }
        }
        self.state = ScanState::Ground;
    }
}

/// Decode a Kitty chunk's payload per its `q` encoding: `2` = base64,
/// absent = base64, `0` = raw bytes.
fn decode_kitty_data(data: &[u8], encoding: Option<u32>) -> Option<Vec<u8>> {
    match encoding {
        Some(2) | None => base64::engine::general_purpose::STANDARD.decode(data).ok(),
        Some(0) => Some(data.to_vec()),
        Some(other) => {
            tracing::warn!("terminal media: unsupported Kitty encoding q={other}");
            None
        }
    }
}

/// OSC 52 payload is `[selection;]base64`. Return the decoded text.
fn decode_osc52(payload: &[u8]) -> Option<String> {
    let base64_part = payload
        .iter()
        .position(|&b| b == b';')
        .map(|split| &payload[split + 1..])
        .unwrap_or(payload);
    let decoded = base64::engine::general_purpose::STANDARD.decode(base64_part).ok()?;
    Some(String::from_utf8_lossy(&decoded).into_owned())
}

/// Parse the comma-separated `key=value` parameter list of a Kitty chunk.
fn parse_kitty_params(params: &[u8], out: &mut KittyParams) -> Result<(), String> {
    for field in params.split(|&b| b == b',') {
        if field.is_empty() {
            continue;
        }
        let Some(split) = field.iter().position(|&b| b == b'=') else {
            return Err(format!(
                "parameter `{}` lacks `=`",
                String::from_utf8_lossy(field)
            ));
        };
        let key = std::str::from_utf8(&field[..split]).unwrap_or("");
        let value = std::str::from_utf8(&field[split + 1..]).unwrap_or("");
        match key {
            "a" => out.action = Some(value.as_bytes().first().copied().unwrap_or(0)),
            "f" => out.format = Some(parse_u32(key, value)?),
            "i" => out.image_id = Some(parse_u32(key, value)?),
            "c" => out.columns = Some(parse_u32(key, value)?),
            "r" => out.rows = Some(parse_u32(key, value)?),
            "m" => out.mode = Some(parse_u32(key, value)?),
            "q" => out.encoding = Some(parse_u32(key, value)?),
            "d" => out.delete = Some(parse_u32(key, value)?),
            // `y` is the placement row, `x` the placement column.
            "y" => out.row = Some(parse_i32(key, value)?),
            "x" => out.column = Some(parse_u32(key, value)?),
            _ => {} // unsupported keys are ignored
        }
    }
    Ok(())
}

fn parse_u32(key: &str, value: &str) -> Result<u32, String> {
    value
        .parse::<u32>()
        .map_err(|_| format!("parameter `{key}` has non-numeric value `{value}`"))
}

fn parse_i32(key: &str, value: &str) -> Result<i32, String> {
    value
        .parse::<i32>()
        .map_err(|_| format!("parameter `{key}` has non-numeric value `{value}`"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kitty_transmit_and_display_emits_media_and_keeps_text() {
        let mut scanner = TerminalMediaScanner::new();
        let output = scanner.feed(b"before\x1b_Ga=T,f=100,i=7,c=2,r=1,q=2;SGVsbG8=\x1b\\after");
        assert_eq!(output.grid_bytes, b"beforeafter");
        assert_eq!(output.media.len(), 1);
        let media = &output.media[0];
        assert_eq!(media.image_id, 7);
        assert_eq!(media.format, 100);
        assert_eq!(media.columns, 2);
        assert_eq!(media.rows, 1);
        assert_eq!(media.data, b"Hello");
        assert!(media.final_chunk);
        assert!(!media.delete);
        assert!(output.actions.is_empty());
    }

    #[test]
    fn kitty_continuation_chunks_are_reassembled_in_order() {
        let mut scanner = TerminalMediaScanner::new();
        // First chunk: m=1 (more chunks follow).
        let first = scanner.feed(b"\x1b_Ga=T,f=100,i=7,c=2,r=1,m=1;SGVsbG8=\x1b\\");
        assert!(first.media.is_empty());
        assert!(first.grid_bytes.is_empty());
        // Final chunk: m=0 completes the image.
        let second = scanner.feed(b"\x1b_Ga=T,i=7,m=0;IQ==\x1b\\");
        assert!(second.grid_bytes.is_empty());
        assert_eq!(second.media.len(), 1, "one final media payload");
        let media = &second.media[0];
        assert_eq!(media.image_id, 7);
        assert_eq!(media.format, 100);
        assert_eq!(media.columns, 2);
        assert_eq!(media.rows, 1);
        assert_eq!(media.data, b"Hello!");
        assert!(media.final_chunk);
        assert!(!media.delete);
    }

    #[test]
    fn kitty_delete_emits_delete_media() {
        let mut scanner = TerminalMediaScanner::new();
        let output = scanner.feed(b"\x1b_Ga=d,i=9\x1b\\");
        assert!(output.grid_bytes.is_empty());
        assert_eq!(output.media.len(), 1);
        let media = &output.media[0];
        assert_eq!(media.image_id, 9);
        assert!(media.delete);
        assert!(media.data.is_empty());
    }

    #[test]
    fn download_and_copy_actions_are_bounded_and_decoded() {
        let mut scanner = TerminalMediaScanner::new();
        // OSC 9 z3rm-download: consumed, typed DOWNLOAD, no grid residue.
        let output = scanner.feed(b"a\x1b]9;z3rm-download:https://example.com/f.bin\x07b");
        assert_eq!(output.grid_bytes, b"ab");
        assert_eq!(output.actions.len(), 1);
        assert_eq!(output.actions[0].kind, PaneActionKind::Download as i32);
        assert_eq!(output.actions[0].value, "https://example.com/f.bin");

        // OSC 9 z3rm-copy: base64-decoded typed COPY.
        let mut scanner = TerminalMediaScanner::new();
        let output = scanner.feed(b"\x1b]9;z3rm-copy:Y2FyZ28gaW5zdGFsbCB6M3Jt\x1b\\");
        assert!(output.grid_bytes.is_empty());
        assert_eq!(output.actions.len(), 1);
        assert_eq!(output.actions[0].kind, PaneActionKind::Copy as i32);
        assert_eq!(output.actions[0].value, "cargo install z3rm");
    }

    #[test]
    fn osc8_hyperlink_stays_in_grid_and_emits_no_action() {
        let mut scanner = TerminalMediaScanner::new();
        let output = scanner.feed(b"a\x1b]8;;https://example.com\x1b\\b");
        assert_eq!(output.grid_bytes, b"a\x1b]8;;https://example.com\x1b\\b");
        assert!(output.actions.is_empty());
        assert!(output.media.is_empty());
    }

    #[test]
    fn osc8_z3rm_download_uri_is_grid_content_only() {
        // A visible `z3rm-download:` OSC 8 link must remain an ordinary grid
        // hyperlink and must not emit a download action on render.
        let mut scanner = TerminalMediaScanner::new();
        let output = scanner.feed(
            b"\x1b]8;;z3rm-download:https://example.com/x.png\x1b\\download\x1b]8;;\x1b\\",
        );
        assert_eq!(
            output.grid_bytes,
            b"\x1b]8;;z3rm-download:https://example.com/x.png\x1b\\download\x1b]8;;\x1b\\"
        );
        assert!(output.actions.is_empty());
        assert!(output.media.is_empty());
    }

    #[test]
    fn osc52_emits_copy_and_preserves_grid_semantics() {
        let mut scanner = TerminalMediaScanner::new();
        let output = scanner.feed(b"\x1b]52;c;SGVsbG8=\x1b\\");
        // OSC 52 stays in the grid so the emulator clipboard hook still fires.
        assert_eq!(output.grid_bytes, b"\x1b]52;c;SGVsbG8=\x1b\\");
        assert_eq!(output.actions.len(), 1);
        assert_eq!(output.actions[0].kind, PaneActionKind::Copy as i32);
        assert_eq!(output.actions[0].value, "Hello");
    }

    #[test]
    fn ordinary_osc_stays_in_grid() {
        let mut scanner = TerminalMediaScanner::new();
        let output = scanner.feed(b"\x1b]0;title\x07\x1b]133;A\x07");
        assert_eq!(output.grid_bytes, b"\x1b]0;title\x07\x1b]133;A\x07");
        assert!(output.actions.is_empty());
        assert!(output.media.is_empty());
    }

    #[test]
    fn unterminated_control_sequence_is_bounded_and_does_not_drop_future_text() {
        let mut scanner = TerminalMediaScanner::new();
        let mut huge = Vec::new();
        huge.extend_from_slice(b"\x1b_Ga=T,i=1,q=2;");
        huge.resize(MAX_CONTROL_SEQUENCE_BYTES + 64, b'A');
        let output = scanner.feed(&huge);
        assert!(output.media.is_empty());
        assert!(output.grid_bytes.is_empty());
        // Future text after the overflow resumes at ground state.
        let output = scanner.feed(b"after");
        assert_eq!(output.grid_bytes, b"after");
        assert!(output.media.is_empty());
    }

    #[test]
    fn split_across_feed_boundaries_is_reassembled() {
        let mut scanner = TerminalMediaScanner::new();
        let first = scanner.feed(b"pre\x1b_Ga=T,f=100,i=7,c=2,r=1,q=2;SGVsbG8=");
        assert_eq!(first.grid_bytes, b"pre");
        assert!(first.media.is_empty());
        let second = scanner.feed(b"\x1b\\post");
        assert_eq!(second.grid_bytes, b"post");
        assert_eq!(second.media.len(), 1);
        assert_eq!(second.media[0].data, b"Hello");
    }
}