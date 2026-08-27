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
//   - OSC 9 `z3rm-download;` / `z3rm-copy;`: BEL/ST-terminated typed action
//     sequences. Emitted as a `PaneAction` (DOWNLOAD / COPY) and consumed
//     (removed from the grid stream). Ordinary OSC 9 stays in the grid.
//   - OSC 52 clipboard: preserved byte-for-byte in `grid_bytes` for
//     alacritty's existing `ClipboardStore` path (→ clipboard hook →
//     ServerClipboard) which keeps working unchanged. No PaneAction is
//     emitted — the clipboard hook is the sole copy path.
//
// The scanner is bounded and incremental: a real PTY splits sequences at
// arbitrary byte boundaries, so state lives across `feed` calls, and every
// control-sequence buffer is capped by `MAX_CONTROL_SEQUENCE_BYTES`. On
// overflow the sequence is dropped, the parse error is logged, and the
// scanner enters a discard-until-terminator state that persists across feeds
// so that residue of the oversized sequence is not emitted as text.


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
    /// Control events in their original arrival order. The public vectors are
    /// retained for API compatibility; Pane uses this index/offset ledger to
    /// merge media/actions with OSC 133 boundaries without guessing.
    pub(crate) events: Vec<ScanEvent>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ScanEvent {
    /// A complete media payload. `placement_from_pending` means the cursor
    /// was captured when an earlier `m=1,a=T` chunk started this transfer.
    Media {
        index: usize,
        image_id: u32,
        grid_offset: usize,
        action: Option<u8>,
        placement_from_pending: bool,
    },
    /// Internal cursor boundary for the first `m=1,a=T` chunk. No media is
    /// emitted until the matching final chunk arrives.
    Placement {
        image_id: u32,
        grid_offset: usize,
        action: Option<u8>,
    },
    Action {
        index: usize,
        grid_offset: usize,
    },
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
    /// Discarding bytes until a BEL or ST (ESC \) terminator after an
    /// overflow. Persists across feed boundaries.
    Discard,
    /// In Discard, after an ESC — waiting for `\` to confirm ST.
    DiscardEsc,
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
    mode: Option<u32>,
    delete_mode: Option<u8>,
}

/// Reassembled Kitty image awaiting its final chunk.
struct PendingMedia {
    format: u32,
    columns: u32,
    rows: u32,
    data: Vec<u8>,
    /// Placement metadata belongs to the first chunk. A final continuation
    /// may omit `a`, but `a=T` still names the cursor event to reuse.
    placement_action: Option<u8>,
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
    /// Active Kitty transfer, if any (protocol chunks do not interleave).
    pending: Option<(u32, PendingMedia)>,
    /// Grid-byte offset at which the currently buffered control sequence
    /// began. Offsets are relative to the current `feed`; a sequence spanning
    /// feeds is therefore anchored at offset zero in the completing feed.
    sequence_grid_offset: usize,
}


impl Default for TerminalMediaScanner {
    fn default() -> Self {
        Self {
            state: ScanState::Ground,
            buffer: Vec::new(),
            osc_number: 0,
            osc_digits: 0,
            pending: None,
            sequence_grid_offset: 0,
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
        if !matches!(self.state, ScanState::Ground | ScanState::Discard | ScanState::DiscardEsc) {
            // A control sequence split across feeds has no grid bytes before
            // its completion in this feed, so its local event offset is zero.
            self.sequence_grid_offset = 0;
        }
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
                    0x1b => {
                        // Preserve the first ESC immediately and keep the
                        // second one pending in case it starts a sequence.
                        output.grid_bytes.push(0x1b);
                        self.state = ScanState::Escape;
                    }
                    b'_' => {
                        self.start_sequence(b'_', output.grid_bytes.len());
                        self.state = ScanState::ApcIntro;
                    }
                    b']' => {
                        self.start_sequence(b']', output.grid_bytes.len());
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
                ScanState::Discard | ScanState::DiscardEsc => {
                    self.consume_discard_byte(byte);
                }
                ScanState::ApcIntro => {
                    if !self.buffer_byte(byte, "APC") {
                        continue;
                    }
                    match byte {
                        b'G' => self.state = ScanState::ApcParams,
                        0x1b => self.state = ScanState::ApcPassthroughEscape,
                        0x07 => self.flush_passthrough(&mut output),
                        _ => self.state = ScanState::ApcPassthrough,
                    }
                }
                ScanState::ApcParams => {
                    if byte == 0x1b {
                        // Possible ST start; keep the ESC out of Kitty data.
                        self.state = ScanState::ApcEscape;
                    } else if byte == 0x07 {
                        // BEL terminates the APC, but is not payload.
                        self.finish_kitty(&mut output);
                    } else if self.buffer_byte(byte, "Kitty APC") {
                        if byte == b';' {
                            self.state = ScanState::ApcData;
                        }
                    }
                }
                ScanState::ApcData => {
                    if byte == 0x1b {
                        self.state = ScanState::ApcEscape;
                    } else if byte == 0x07 {
                        // BEL terminates the APC, but is not payload.
                        self.finish_kitty(&mut output);
                    } else {
                        let _ = self.buffer_byte(byte, "Kitty APC");
                    }
                }
                ScanState::ApcEscape => {
                    if byte == b'\\' {
                        self.finish_kitty(&mut output);
                    } else {
                        // A malformed Kitty APC is dropped. Re-dispatch the
                        // byte after its ESC so a new sequence can begin.
                        self.state = ScanState::Ground;
                        self.buffer.clear();
                        index -= 1;
                    }
                }
                ScanState::ApcPassthrough => {
                    if !self.buffer_byte(byte, "APC") {
                        continue;
                    }
                    match byte {
                        0x1b => self.state = ScanState::ApcPassthroughEscape,
                        0x07 => self.flush_passthrough(&mut output),
                        _ => {}
                    }
                }
                ScanState::ApcPassthroughEscape => {
                    if !self.buffer_byte_after_escape(byte, "APC") {
                        continue;
                    }
                    match byte {
                        b'\\' | 0x07 => {
                            // Both ST and BEL terminate a pass-through APC.
                            self.flush_passthrough(&mut output);
                        }
                        0x1b => {
                            // The newest ESC is now the possible ST start;
                            // retain both consecutive ESC bytes.
                            self.state = ScanState::ApcPassthroughEscape;
                        }
                        _ => self.state = ScanState::ApcPassthrough,
                    }
                }
                ScanState::OscNumber => {
                    if !self.buffer_byte(byte, "OSC") {
                        continue;
                    }
                    match byte {
                        b'0'..=b'9' if self.osc_digits < 5 => {
                            self.osc_number =
                                self.osc_number.saturating_mul(10) + u32::from(byte - b'0');
                            self.osc_digits += 1;
                        }
                        b';' => {
                            if self.osc_number == 9 || self.osc_number == 52 {
                                self.state = ScanState::OscPayload;
                            } else {
                                // OSC 8 and all other OSCs are passed through.
                                self.state = ScanState::OscPassthrough;
                            }
                        }
                        0x1b => self.state = ScanState::OscPassthroughEscape,
                        0x07 => self.flush_passthrough(&mut output),
                        _ => self.state = ScanState::OscPassthrough,
                    }
                }
                ScanState::OscPayload => {
                    if !self.buffer_byte(byte, "OSC") {
                        continue;
                    }
                    match byte {
                        0x1b => self.state = ScanState::OscPayloadEscape,
                        0x07 => self.finish_osc(&mut output),
                        _ => {}
                    }
                }
                ScanState::OscPayloadEscape => {
                    if !self.buffer_byte_after_escape(byte, "OSC") {
                        continue;
                    }
                    match byte {
                        b'\\' => self.finish_osc(&mut output),
                        0x07 => self.finish_osc(&mut output),
                        0x1b => self.state = ScanState::OscPayloadEscape,
                        _ => {
                            // Not ST: the typed OSC was aborted by its ESC.
                            // Drop the partial payload and re-dispatch this
                            // byte from escape state so a new sequence (CSI,
                            // APC, OSC) can begin.
                            self.state = ScanState::Escape;
                            self.buffer.clear();
                            index -= 1;
                        }
                    }
                }
                ScanState::OscPassthrough => {
                    if !self.buffer_byte(byte, "OSC") {
                        continue;
                    }
                    match byte {
                        0x1b => self.state = ScanState::OscPassthroughEscape,
                        0x07 => self.flush_passthrough(&mut output),
                        _ => {}
                    }
                }
                ScanState::OscPassthroughEscape => {
                    if !self.buffer_byte_after_escape(byte, "OSC") {
                        continue;
                    }
                    match byte {
                        b'\\' | 0x07 => {
                            self.flush_passthrough(&mut output);
                        }
                        0x1b => {
                            self.state = ScanState::OscPassthroughEscape;
                        }
                        _ => self.state = ScanState::OscPassthrough,
                    }
                }
            }
        }
        output
    }

    /// Begin buffering a control sequence introduced by `_` or `]`. The ESC
    /// was already consumed by the caller and is replayed here.
    fn start_sequence(&mut self, introducer: u8, grid_offset: usize) {
        self.buffer.clear();
        self.buffer.push(0x1b);
        self.buffer.push(introducer);
        self.sequence_grid_offset = grid_offset;
    }
    /// Append a control byte without ever growing the buffer beyond the cap.
    /// If the cap is already full, the current byte is consumed as part of
    /// discard recovery so a terminator on this byte still resumes parsing.
    fn buffer_byte(&mut self, byte: u8, what: &str) -> bool {
        if self.buffer.len() >= MAX_CONTROL_SEQUENCE_BYTES {
            self.drop_overflow(what);
            self.consume_discard_byte(byte);
            false
        } else {
            self.buffer.push(byte);
            true
        }
    }

    /// As [`buffer_byte`], but the existing buffer ends in an ESC that was
    /// already stored. If the current byte is `\\`, it must still close ST
    /// even though the overflowing sequence itself is discarded.
    fn buffer_byte_after_escape(&mut self, byte: u8, what: &str) -> bool {
        if self.buffer.len() >= MAX_CONTROL_SEQUENCE_BYTES {
            self.drop_overflow(what);
            self.state = ScanState::DiscardEsc;
            self.consume_discard_byte(byte);
            false
        } else {
            self.buffer.push(byte);
            true
        }
    }

    fn drop_overflow(&mut self, what: &str) {
        tracing::warn!(
            "{what} control sequence exceeded {MAX_CONTROL_SEQUENCE_BYTES} bytes; dropped"
        );
        self.buffer.clear();
        self.state = ScanState::Discard;
    }

    /// Consume bytes after an overflow until BEL or ST. This state is
    /// intentionally independent of the original APC/OSC kind and persists
    /// across calls to `feed`.
    fn consume_discard_byte(&mut self, byte: u8) {
        match self.state {
            ScanState::Discard => {
                if byte == 0x07 {
                    self.state = ScanState::Ground;
                } else if byte == 0x1b {
                    self.state = ScanState::DiscardEsc;
                }
            }
            ScanState::DiscardEsc => {
                if byte == b'\\' || byte == 0x07 {
                    self.state = ScanState::Ground;
                } else if byte == 0x1b {
                    self.state = ScanState::DiscardEsc;
                } else {
                    self.state = ScanState::Discard;
                }
            }
            _ => {}
        }
    }

    fn flush_passthrough(&mut self, output: &mut ScanOutput) {
        output.grid_bytes.append(&mut self.buffer);
        self.state = ScanState::Ground;
    }

    /// A complete Kitty APC is buffered: `ESC _ G params ; data`.
    fn finish_kitty(&mut self, output: &mut ScanOutput) {
        // Buffer layout: ESC _ G <params> ; <data> (ST/BEL already consumed).
        let event_offset = self.sequence_grid_offset;
        let body = std::mem::take(&mut self.buffer);
        self.state = ScanState::Ground;
        let params = &body[3..];
        let (params, data) = match params.iter().position(|&b| b == b';') {
            Some(split) => (&params[..split], &params[split + 1..]),
            None => (params, &[][..]),
        };
        let mut parsed = KittyParams::default();
        if let Err(message) = parse_kitty_params(params, &mut parsed) {
            // The `d=i` delete mode encodes a character, not a u32; the
            // parser accepts either.
            tracing::warn!("terminal media: malformed Kitty params: {message}");
            return;
        }

        // `a=d` (or `d=1`) deletes a previously published image.
        // If `image_id` is omitted but a pending transfer exists, inherit
        // its image id.
        let image_id = parsed.image_id.or_else(|| {
            self.pending.as_ref().map(|(id, _)| *id)
        });
        let Some(image_id) = image_id else {
            tracing::warn!("terminal media: Kitty APC without image id; dropped");
            return;
        };

        let delete = parsed.action == Some(b'd')
            || (parsed.action == Some(b'q') && parsed.delete_mode == Some(b'i'));
        if delete {
            let index = output.media.len();
            output.media.push(PaneMedia {
                pane_id: String::new(),
                sequence: 0,
                image_id,
                format: parsed.format.unwrap_or(0),
                row: 0,
                column: 0,
                columns: parsed.columns.unwrap_or(0),
                rows: parsed.rows.unwrap_or(0),
                data: Vec::new(),
                final_chunk: false,
                delete: true,
            });
            output.events.push(ScanEvent::Media {
                index,
                image_id,
                grid_offset: event_offset,
                action: parsed.action,
                placement_from_pending: false,
            });
            self.pending = None;
            return;
        }

        // The payload is always base64-encoded (Kitty `q` is response
        // suppression, not encoding).
        let payload = match base64::engine::general_purpose::STANDARD.decode(data) {
            Ok(payload) => payload,
            Err(_) => {
                tracing::warn!("terminal media: Kitty data is not valid base64; dropped");
                self.pending = None;
                return;
            }
        };

        let is_continuation = parsed.mode == Some(1);
        if is_continuation {
            // Accumulate; publish only on the final chunk (m=0/absent).
            // Only one active transfer is tracked (protocol chunks do not
            // interleave); if a new image id arrives, replace the pending.
            let new_transfer = match self.pending.as_ref() {
                Some((pending_id, _)) => *pending_id != image_id,
                None => true,
            };
            let entry = self.pending.get_or_insert_with(|| (image_id, PendingMedia {
                format: parsed.format.unwrap_or(0),
                columns: parsed.columns.unwrap_or(0),
                rows: parsed.rows.unwrap_or(0),
                data: Vec::new(),
                placement_action: parsed.action,
            }));
            if entry.0 != image_id {
                *entry = (image_id, PendingMedia {
                    format: parsed.format.unwrap_or(0),
                    columns: parsed.columns.unwrap_or(0),
                    rows: parsed.rows.unwrap_or(0),
                    data: Vec::new(),
                    placement_action: parsed.action,
                });
            }
            // A later continuation may carry the placement action if the
            // initial chunk omitted it; preserve the first `a=T` boundary.
            let new_placement = entry.1.placement_action != Some(b'T')
                && parsed.action == Some(b'T');
            if new_placement {
                entry.1.placement_action = parsed.action;
            }
            // Preflight data size with checked_add.
            let accepted = match entry.1.data.len().checked_add(payload.len()) {
                Some(new_len) if new_len <= MAX_REASSEMBLED_MEDIA_BYTES => {
                    entry.1.data.extend_from_slice(&payload);
                    true
                }
                _ => {
                    tracing::warn!(
                        "terminal media: image {image_id} exceeded {MAX_REASSEMBLED_MEDIA_BYTES} bytes; dropped"
                    );
                    self.pending = None;
                    false
                }
            };
            if accepted && (new_transfer || new_placement) && parsed.action == Some(b'T') {
                output.events.push(ScanEvent::Placement {
                    image_id,
                    grid_offset: event_offset,
                    action: Some(b'T'),
                });
            }
            return;
        }

        // Final chunk: complete a pending image or publish a single chunk.
        let mut placement_action = parsed.action;
        let mut placement_from_pending = false;
        let mut media = if let Some((pid, entry)) = self.pending.take() {
            if pid != image_id {
                // Image id mismatch — start fresh with this chunk.
                PaneMedia {
                    pane_id: String::new(),
                    sequence: 0,
                    image_id,
                    format: parsed.format.unwrap_or(0),
                    row: 0,
                    column: 0,
                    columns: parsed.columns.unwrap_or(0),
                    rows: parsed.rows.unwrap_or(0),
                    data: Vec::new(),
                    final_chunk: true,
                    delete: false,
                }
            } else {
                placement_action = entry.placement_action.or(parsed.action);
                placement_from_pending = entry.placement_action == Some(b'T');
                PaneMedia {
                    pane_id: String::new(),
                    sequence: 0,
                    image_id,
                    format: entry.format,
                    row: 0,
                    column: 0,
                    columns: entry.columns,
                    rows: entry.rows,
                    data: entry.data,
                    final_chunk: true,
                    delete: false,
                }
            }
        } else {
            PaneMedia {
                pane_id: String::new(),
                sequence: 0,
                image_id,
                format: parsed.format.unwrap_or(0),
                row: 0,
                column: 0,
                columns: parsed.columns.unwrap_or(0),
                rows: parsed.rows.unwrap_or(0),
                data: Vec::new(),
                final_chunk: true,
                delete: false,
            }
        };
        // Preflight final append.
        match media.data.len().checked_add(payload.len()) {
            Some(new_len) if new_len <= MAX_REASSEMBLED_MEDIA_BYTES => {
                media.data.extend_from_slice(&payload);
            }
            _ => {
                tracing::warn!(
                    "terminal media: image {image_id} exceeded {MAX_REASSEMBLED_MEDIA_BYTES} bytes; dropped"
                );
                return;
            }
        }
        let index = output.media.len();
        output.media.push(media);
        output.events.push(ScanEvent::Media {
            index,
            image_id,
            grid_offset: event_offset,
            action: placement_action,
            placement_from_pending,
        });
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
            // OSC 52 stays in the grid for alacritty's clipboard hook.
            // No PaneAction is emitted — the clipboard hook is the sole
            // copy path (per design/ledger ruling).
            output.grid_bytes.append(&mut self.buffer);
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
        // Wire format: `OSC 9;z3rm-download;<uri>` and
        // `OSC 9;z3rm-copy;<base64>` (semicolon-delimited).
        let (kind, value) = match payload {
            p if p.starts_with(b"z3rm-download;") => (
                Some(PaneActionKind::Download),
                String::from_utf8(p[b"z3rm-download;".len()..].to_vec()).ok(),
            ),
            p if p.starts_with(b"z3rm-copy;") => (
                Some(PaneActionKind::Copy),
                String::from_utf8(p[b"z3rm-copy;".len()..].to_vec()).ok(),
            ),
            _ => (None, None),
        };
        match kind {
            Some(PaneActionKind::Download) => {
                if let Some(value) = value {
                    let index = output.actions.len();
                    output.actions.push(PaneAction {
                        pane_id: String::new(),
                        sequence: 0,
                        kind: PaneActionKind::Download as i32,
                        value,
                    });
                    output.events.push(ScanEvent::Action {
                        index,
                        grid_offset: self.sequence_grid_offset,
                    });
                } else {
                    tracing::warn!("terminal media: OSC 9 z3rm-download value is not valid UTF-8");
                }
            }
            Some(PaneActionKind::Copy) => {
                if let Some(value) = value {
                    match base64::engine::general_purpose::STANDARD.decode(value.as_bytes()) {
                        Ok(text) => match String::from_utf8(text) {
                            Ok(text) => {
                                let index = output.actions.len();
                                output.actions.push(PaneAction {
                                    pane_id: String::new(),
                                    sequence: 0,
                                    kind: PaneActionKind::Copy as i32,
                                    value: text,
                                });
                                output.events.push(ScanEvent::Action {
                                    index,
                                    grid_offset: self.sequence_grid_offset,
                                });
                            }
                            Err(_) => {
                                tracing::warn!(
                                    "terminal media: OSC 9 z3rm-copy decoded text is not valid UTF-8"
                                );
                            }
                        },
                        Err(_) => {
                            tracing::warn!("terminal media: OSC 9 z3rm-copy is not valid base64");
                        }
                    }
                } else {
                    tracing::warn!("terminal media: OSC 9 z3rm-copy value is not valid UTF-8");
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
/// Parse the comma-separated `key=value` parameter list of a Kitty chunk.
///
/// `q` is response suppression (not encoding); the payload is always
/// base64. `x` and `y` are crop offsets, not terminal cell coordinates;
/// they are ignored here (Pane wiring supplies cursor-cell placement).
/// `d` is a character deletion selector (e.g. `d=i` for delete by image
/// id), not a numeric value.
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
            // `q` is response suppression; the payload is always base64.
            // Parsed for completeness but not used for encoding.
            "q" => {}
            // `d` is a character deletion selector, not numeric.
            "d" => {
                out.delete_mode = value.as_bytes().first().copied();
            }
            // `x` and `y` are crop offsets, not terminal cell coordinates.
            // Ignored here; Pane wiring supplies cursor-cell placement.
            "x" | "y" => {}
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
    fn kitty_continuation_without_image_id_uses_pending() {
        // Final chunk omits `i`; inherits image_id from the pending transfer.
        let mut scanner = TerminalMediaScanner::new();
        let first = scanner.feed(b"\x1b_Ga=T,f=100,i=7,c=2,r=1,m=1;SGVsbG8=\x1b\\");
        assert!(first.media.is_empty());
        let second = scanner.feed(b"\x1b_Ga=T,m=0;IQ==\x1b\\");
        assert_eq!(second.media.len(), 1);
        assert_eq!(second.media[0].image_id, 7);
        assert_eq!(second.media[0].data, b"Hello!");
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
        assert!(!media.final_chunk);
    }

    #[test]
    fn kitty_delete_with_d_i_selector() {
        // The `d` parameter is a character deletion selector; `d=i` means
        // delete by image id.
        let mut scanner = TerminalMediaScanner::new();
        let output = scanner.feed(b"\x1b_Ga=d,d=i,i=9\x1b\\");
        assert!(output.grid_bytes.is_empty());
        assert_eq!(output.media.len(), 1);
        let media = &output.media[0];
        assert_eq!(media.image_id, 9);
        assert!(media.delete);
        assert!(media.data.is_empty());
        assert!(!media.final_chunk);
    }

    #[test]
    fn kitty_q_is_response_suppression_not_encoding() {
        // `q=0` and `q=1` are response suppression modes; the payload is
        // always base64 regardless of `q`. All three values decode.
        for q in [b"0", b"1", b"2"] {
            let mut scanner = TerminalMediaScanner::new();
            let q_s = std::str::from_utf8(q).unwrap();
            let input = format!(
                "\x1b_Ga=T,f=100,i=7,q={};SGVsbG8=\x1b\\",
                q_s
            );
            let output = scanner.feed(input.as_bytes());
            assert_eq!(output.media.len(), 1, "q={}", q_s);
            assert_eq!(
                output.media[0].data,
                b"Hello",
                "q={}",
                q_s
            );
        }
    }

    #[test]
    fn download_and_copy_actions_are_bounded_and_decoded() {
        let mut scanner = TerminalMediaScanner::new();
        // OSC 9 z3rm-download; consumed, typed DOWNLOAD, no grid residue.
        let output = scanner.feed(b"a\x1b]9;z3rm-download;https://example.com/f.bin\x07b");
        assert_eq!(output.grid_bytes, b"ab");
        assert_eq!(output.actions.len(), 1);
        assert_eq!(output.actions[0].kind, PaneActionKind::Download as i32);
        assert_eq!(output.actions[0].value, "https://example.com/f.bin");

        // OSC 9 z3rm-copy; base64-decoded typed COPY.
        let mut scanner = TerminalMediaScanner::new();
        let output = scanner.feed(b"\x1b]9;z3rm-copy;Y2FyZ28gaW5zdGFsbCB6M3Jt\x1b\\");
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
    fn osc52_preserves_grid_and_emits_no_action() {
        // OSC 52 is preserved byte-for-byte for the alacritty clipboard
        // hook. No PaneAction is emitted (per design/ledger ruling).
        let mut scanner = TerminalMediaScanner::new();
        let output = scanner.feed(b"\x1b]52;c;SGVsbG8=\x1b\\");
        assert_eq!(output.grid_bytes, b"\x1b]52;c;SGVsbG8=\x1b\\");
        assert!(output.actions.is_empty());
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
    fn overflow_discards_until_terminator_then_resumes() {
        // Overflow: enter discard-until-terminator state. The sequence
        // is dropped; text after the terminator in the same feed is
        // preserved.
        let mut scanner = TerminalMediaScanner::new();
        let mut buf = Vec::new();
        buf.extend_from_slice(b"\x1b_Ga=T,i=1,q=2;");
        buf.resize(MAX_CONTROL_SEQUENCE_BYTES + 64, b'A');
        // ST terminates the discarded sequence, followed by ordinary text.
        buf.extend_from_slice(b"\x1b\\after");
        let output = scanner.feed(&buf);
        assert!(output.media.is_empty());
        assert_eq!(
            output.grid_bytes,
            b"after",
            "text after ST terminator is preserved"
        );

        // Next feed continues normally.
        let output = scanner.feed(b" more");
        assert_eq!(output.grid_bytes, b" more");
        assert!(output.media.is_empty());
    }

    #[test]
    fn overflow_cross_feed_discard_then_resume() {
        // Overflow spans multiple feeds; the discard state persists until
        // a terminator arrives.
        let mut scanner = TerminalMediaScanner::new();
        let mut buf = Vec::new();
        buf.extend_from_slice(b"\x1b_Ga=T,i=1,q=2;");
        buf.resize(MAX_CONTROL_SEQUENCE_BYTES + 64, b'A');
        // No terminator in this feed — overflow leads to Discard state.
        let output = scanner.feed(&buf);
        assert!(output.media.is_empty());
        assert!(output.grid_bytes.is_empty());

        // Next feed still has no terminator; still discarding.
        let output = scanner.feed(b"still no terminator here");
        assert!(output.grid_bytes.is_empty());

        // Terminator arrives in this feed; resume after it.
        let output = scanner.feed(b"\x1b\\after");
        assert_eq!(output.grid_bytes, b"after");
        assert!(output.media.is_empty());
    }

    #[test]
    fn overflow_bel_terminator_resumes() {
        // BEL can also terminate a discarded sequence.
        let mut scanner = TerminalMediaScanner::new();
        let mut buf = Vec::new();
        buf.extend_from_slice(b"\x1b_Ga=T,i=1,q=2;");
        buf.resize(MAX_CONTROL_SEQUENCE_BYTES + 64, b'A');
        buf.extend_from_slice(b"\x07after");
        let output = scanner.feed(&buf);
        assert!(output.media.is_empty());
        assert_eq!(output.grid_bytes, b"after");
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
        assert!(matches!(
            second.events.as_slice(),
            [ScanEvent::Media { grid_offset: 0, .. }]
        ));
    }

    #[test]
    fn consecutive_esc_bytes_are_preserved() {
        // The second ESC must not be collapsed into the first. The trailing
        // ordinary byte resolves the second ESC as an unrecognized sequence,
        // so both input ESC bytes reach the grid parser.
        let mut scanner = TerminalMediaScanner::new();
        let output = scanner.feed(b"\x1b\x1btext");
        assert_eq!(output.grid_bytes, b"\x1b\x1btext");
        assert!(output.media.is_empty());
        assert!(output.actions.is_empty());
    }

    #[test]
    fn kitty_pending_payload_budget_is_checked_before_final_append() {
        let mut scanner = TerminalMediaScanner::new();
        let first_data = vec![b'A'; MAX_REASSEMBLED_MEDIA_BYTES / 2];
        let first_encoded = base64::engine::general_purpose::STANDARD.encode(&first_data);
        let first = format!("\x1b_Ga=T,i=1,m=1;{first_encoded}\x1b\\");
        assert!(first.len() <= MAX_CONTROL_SEQUENCE_BYTES);
        let output = scanner.feed(first.as_bytes());
        assert!(output.media.is_empty());

        // The decoded aggregate would exceed the scanner-wide 4 MiB budget;
        // the final append is rejected rather than growing the pending Vec.
        let second_data = vec![b'B'; MAX_REASSEMBLED_MEDIA_BYTES / 2 + 1];
        let second_encoded = base64::engine::general_purpose::STANDARD.encode(&second_data);
        let second = format!("\x1b_Ga=T,m=0;{second_encoded}\x1b\\");
        assert!(second.len() <= MAX_CONTROL_SEQUENCE_BYTES);
        let output = scanner.feed(second.as_bytes());
        assert!(output.media.is_empty());
        assert!(output.grid_bytes.is_empty());
    }

    #[test]
    fn apc_passthrough_bel_after_non_st_esc_terminates() {
        let mut scanner = TerminalMediaScanner::new();
        let output = scanner.feed(b"a\x1b_Xhello\x1b\x07after");
        assert_eq!(output.grid_bytes, b"a\x1b_Xhello\x1b\x07after");
        assert!(output.media.is_empty());
        assert!(output.actions.is_empty());
    }

    #[test]
    fn overflow_osc_cross_feed_discards_until_terminator() {
        let mut scanner = TerminalMediaScanner::new();
        let mut prefix = Vec::new();
        prefix.extend_from_slice(b"\x1b]9;z3rm-copy;");
        prefix.resize(MAX_CONTROL_SEQUENCE_BYTES + 1, b'A');
        let output = scanner.feed(&prefix);
        assert!(output.grid_bytes.is_empty());
        assert!(output.actions.is_empty());

        let output = scanner.feed(b"residue");
        assert!(output.grid_bytes.is_empty());
        assert!(output.actions.is_empty());

        let output = scanner.feed(b"\x07after");
        assert_eq!(output.grid_bytes, b"after");
        assert!(output.media.is_empty());
        assert!(output.actions.is_empty());
    }

    #[test]
    fn kitty_ignores_x_y_crop_offsets() {
        // `x` and `y` are crop offsets, not terminal cell coordinates.
        // They are ignored; row/column default to 0.
        let mut scanner = TerminalMediaScanner::new();
        let output = scanner.feed(
            b"\x1b_Ga=T,f=100,i=7,c=2,r=1,x=10,y=20;SGVsbG8=\x1b\\",
        );
        assert_eq!(output.media.len(), 1);
        assert_eq!(output.media[0].row, 0);
        assert_eq!(output.media[0].column, 0);
        assert_eq!(output.media[0].data, b"Hello");
    }

    #[test]
    fn kitty_bel_terminated_does_not_include_bel_in_data() {
        // BEL termination must not enter the base64 data.
        let mut scanner = TerminalMediaScanner::new();
        let output = scanner.feed(b"\x1b_Ga=T,f=100,i=7,c=2,r=1,q=2;SGVsbG8=\x07");
        assert_eq!(output.media.len(), 1);
        assert_eq!(output.media[0].data, b"Hello");
    }

    #[test]
    fn apc_passthrough_non_st_preserves_byte() {
        // APC passthrough with ESC followed by non-`\` preserves the
        // byte and stays in passthrough.
        let mut scanner = TerminalMediaScanner::new();
        let output = scanner.feed(b"a\x1b_Xhello\x1bXworld\x1b\\b");
        assert_eq!(output.grid_bytes, b"a\x1b_Xhello\x1bXworld\x1b\\b");
        assert!(output.media.is_empty());
        assert!(output.actions.is_empty());
    }

    #[test]
    fn malformed_osc_payload_aborts_on_non_st_esc_and_resumes() {
        // A truncated OSC 9 followed by a normal CSI must not swallow the
        // CSI or subsequent text. The malformed OSC is dropped, and the new
        // escape sequence resumes from its ESC.
        let mut scanner = TerminalMediaScanner::new();
        let output = scanner.feed(b"\x1b]9;z3rm-copy;AAAA\x1b[31mhello");
        assert!(output.actions.is_empty());
        assert_eq!(output.grid_bytes, b"\x1b[31mhello");
    }
}