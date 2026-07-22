# Terminal Architecture

z3rm_terminal crate provides VT100/ANSI terminal emulation with a cell grid, scrollback buffer, and PTY process management.

## Core Components

```
z3rm_terminal/
├── parser/          # VT100/ANSI parser (state machine)
├── grid/            # Cell grid (fixed rows × cols, scrollback ring)
├── pty/             # PTY process spawn, resize, I/O
├── selection/       # Text selection, copy/paste
├── search/          # Scrollback search (regex, case-insensitive)
├── hyperlink/       # OSC 8 hyperlink detection
├── image/           # Kitty/Sixel image protocol support
└── render/          # Damage tracking, dirty rects for GPU upload
```

## Grid Model

```
┌─────────────────────────────────────────────────────────────┐
│                      Grid (cols × rows)                     │
│  ┌─────────────────────────────────────────────────────┐   │
│  │  Visible Region (viewport)                          │   │
│  │  ┌───────────────────────────────────────────────┐  │   │
│  │  │ Cell[0,0]  Cell[1,0]  ...  Cell[cols-1,0]    │  │   │
│  │  │ Cell[0,1]  Cell[1,1]  ...  Cell[cols-1,1]    │  │   │
│  │  │   ...                                      │  │   │
│  │  │ Cell[0,rows-1] ... Cell[cols-1,rows-1]      │  │   │
│  │  └───────────────────────────────────────────────┘  │   │
│  └─────────────────────────────────────────────────────┘   │
│  ┌─────────────────────────────────────────────────────┐   │
│  │  Scrollback Ring Buffer (capacity = scrollback_lines)│   │
│  │  Ring[0] ... Ring[capacity-1]  (each = row of cells) │   │
│  └─────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────┘
```

### Cell Structure
```rust
pub struct Cell {
    pub char: char,                    // Unicode codepoint (or '\0' for empty)
    pub attrs: CellAttrs,              // fg/bg color, bold, italic, underline, etc.
    pub hyperlink: Option<HyperlinkId>,// OSC 8 hyperlink
    pub image: Option<ImageFragment>,  // Kitty/Sixel image fragment
    pub width: u8,                     // 0 (combining), 1, or 2 (wide)
}
```

### Scrollback
- Ring buffer of rows, capacity configurable (default 10,000 lines)
- Each row = `Vec<Cell>` of length `cols`
- Logical line mapping: `visible_row = (cursor_row - scrollback_offset) % capacity`
- Search indexes scrollback via suffix array (incremental update on new lines)

## Parser (VT100/ANSI)

State machine per [VT100 spec](https://vt100.net/docs/vt100-ug/chapter3.html) + common extensions:
- **C0/C1 controls:** BEL, BS, HT, LF, VT, FF, CR, ESC, CSI, OSC, DCS, PM, APC
- **CSI sequences:** Cursor movement, scroll region, SGR (colors), DEC modes, etc.
- **OSC sequences:** Title (0/2), hyperlink (8), clipboard (52), color palette (4/10/11/12/104/105/106/107/108/109/110/111/112/113/114/115/116/117/118/119)
- **DCS:** Sixel, DECRQSS, soft font
- **Kitty graphics:** `ESC_G` + payload + `ESC\\`
- **DEC private modes:** DECCKM, DECOM, DECAWM, DECTCEM, etc.

Parser outputs `Action` enum applied to `Grid`:
```rust
enum Action {
    Print(char),
    Execute(C0Control),
    Csi(CsiSequence),
    Osc(OscSequence),
    Dcs(DcsSequence),
    Esc(EscSequence),
}
```

## PTY Management

```rust
pub struct PtyManager {
    // Spawns PTY with given shell, cwd, env, size
    pub fn spawn(&self, config: PtyConfig) -> Result<PtyHandle>;
    // Resize PTY (ioctl TIOCSWINSZ)
    pub fn resize(&self, handle: &PtyHandle, size: PtySize) -> Result<()>;
    // Write to stdin
    pub fn write(&self, handle: &PtyHandle, data: &[u8]) -> Result<()>;
    // Read stdout (async stream)
    pub fn read(&self, handle: &PtyHandle) -> impl Stream<Item = Vec<u8>>;
    // Wait for exit
    pub fn wait(&self, handle: &PtyHandle) -> Result<ExitStatus>;
}
```

Platform abstraction:
- **Unix:** `posix_openpt`, `grantpt`, `unlockpt`, `ptsname`, `forkpty` / `openpty`
- **Windows:** ConPTY API (Windows 10 1809+)

## Damage Tracking (GPUI Render)

Terminal view renders via GPUI texture atlas. Damage tracking minimizes GPU uploads:

```rust
pub struct DamageTracker {
    dirty_rects: Vec<Rect>,          // union of dirty regions this frame
    prev_grid: Arc<Grid>,            // previous frame grid (for diff)
}

impl DamageTracker {
    pub fn mark_dirty(&mut self, row: usize, col_start: usize, col_end: usize);
    pub fn mark_row_dirty(&mut self, row: usize);
    pub fn mark_all_dirty(&mut self);
    pub fn compute_damage(&mut self, current: &Grid) -> Vec<Rect>;
}
```

Only changed cells generate texture uploads. Scroll = viewport offset change (no texture upload).

## Selection & Search

- **Selection:** Block or stream mode. Mouse drag = stream; Shift+click extends. Double-click = word; triple-click = line.
- **Search:** Incremental regex search in scrollback. Matches highlighted in grid. Uses Aho-Corasick for multi-pattern.

## Image Protocol Support

- **Kitty graphics protocol:** Full support (transmission, placement, animation, deletion)
- **Sixel:** Decode to RGBA, render as texture
- **iTerm2 inline images:** Not supported (proprietary)

## Configuration

```toml
[terminal]
scrollback_lines = 10000
bell = "visual"        # "visual" | "audio" | "none"
cursor_blink = true
cursor_shape = "block" # "block" | "underline" | "bar"
font_size = 14.0
line_height = 1.2
ligatures = true
```

## Integration Points

- `z3rm_mux::pty::PtyManager` wraps `z3rm_terminal::pty::PtyManager`
- `z3rm_terminal_view::TerminalView` holds `Arc<Grid>`, subscribes to PTY output stream
- `z3rm_mux::grid::GridManager` owns `Grid` per pane, applies parser actions
- Scrollback search exposed via `z3rm_chrome` command palette