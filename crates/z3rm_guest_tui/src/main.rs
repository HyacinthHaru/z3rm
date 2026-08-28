use std::ffi::CString;
use std::io;
use std::mem::MaybeUninit;
use std::process;

const STDIN_FD: libc::c_int = 0;
const STDOUT_FD: libc::c_int = 1;
const CONTENT_TOP: usize = 4;
const FOOTER_ROWS: usize = 2;
const PAGE_HEIGHT: usize = 46;
const SCROLL_STEP: usize = 3;
const IMAGE_PAGE_ROW: usize = 17;
const IMAGE_PATH: &str = "/mnt/z3rm-terminal-grid.png";
const DOWNLOAD_ROOT: &str = "/z3rm-server";
const COPY_TEXT: &str = "cargo install z3rm";
const COPY_BASE64: &str = "Y2FyZ28gaW5zdGFsbCB6M3Jt";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Rect {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

impl Rect {
    const fn new(x: usize, y: usize, width: usize, height: usize) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    fn contains(self, x: usize, y: usize) -> bool {
        x >= self.x
            && x < self.x.saturating_add(self.width)
            && y >= self.y
            && y < self.y.saturating_add(self.height)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Layout {
    columns: usize,
    rows: usize,
    content_top: usize,
    viewport_height: usize,
    page_height: usize,
    download: Rect,
    copy: Rect,
}

impl Layout {
    fn new(columns: usize, rows: usize) -> Self {
        Self {
            columns: columns.max(1),
            rows: rows.max(1),
            content_top: CONTENT_TOP,
            viewport_height: rows.saturating_sub(CONTENT_TOP + FOOTER_ROWS).max(1),
            page_height: PAGE_HEIGHT,
            download: Rect::new(4, 11, 26, 3),
            copy: Rect::new(33, 11, 29, 3),
        }
    }

    fn max_offset(self) -> usize {
        self.page_height.saturating_sub(self.viewport_height)
    }

    fn clamp_offset(self, offset: usize) -> usize {
        offset.min(self.max_offset())
    }

    fn action_at(self, x: usize, y: usize, offset: usize) -> Option<Action> {
        if y < self.content_top {
            return None;
        }
        let page_y = self.clamp_offset(offset).saturating_add(y - self.content_top);
        if self.download.contains(x, page_y) {
            Some(Action::Download)
        } else if self.copy.contains(x, page_y) {
            Some(Action::Copy)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Action {
    Download,
    Copy,
}

#[derive(Debug)]
struct InputResult {
    offset: usize,
    action: Option<Action>,
    output: String,
    quit: bool,
    redraw: bool,
}

impl InputResult {
    fn new(offset: usize) -> Self {
        Self {
            offset,
            action: None,
            output: String::new(),
            quit: false,
            redraw: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MouseEvent {
    button: usize,
    x: usize,
    y: usize,
    pressed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MouseParse {
    Complete(MouseEvent, usize),
    Incomplete,
    Invalid,
}

#[derive(Default)]
struct InputParser {
    pending: Vec<u8>,
}

impl InputParser {
    fn feed(
        &mut self,
        bytes: &[u8],
        layout: &Layout,
        offset: usize,
        download_root: &str,
    ) -> InputResult {
        self.pending.extend_from_slice(bytes);
        let mut result = InputResult::new(layout.clamp_offset(offset));
        let mut consumed = 0;

        while consumed < self.pending.len() {
            let input = &self.pending[consumed..];
            if input[0] == 0x1b {
                match parse_sgr_mouse(input) {
                    MouseParse::Complete(event, length) => {
                        apply_mouse(event, layout, &mut result, download_root);
                        consumed += length;
                        continue;
                    }
                    MouseParse::Incomplete => break,
                    MouseParse::Invalid => {
                        if let Some((delta, length)) = parse_cursor_key(input) {
                            let old_offset = result.offset;
                            result.offset = layout.clamp_offset(apply_delta(old_offset, delta, layout));
                            result.redraw |= old_offset != result.offset;
                            consumed += length;
                            continue;
                        }
                        consumed += 1;
                        continue;
                    }
                }
            }

            match self.pending[consumed] {
                b'q' | b'Q' | 3 => {
                    result.quit = true;
                    result.redraw = true;
                }
                b'j' | b' ' => {
                    let old_offset = result.offset;
                    result.offset = layout.clamp_offset(result.offset.saturating_add(1));
                    result.redraw |= old_offset != result.offset;
                }
                b'k' => {
                    let old_offset = result.offset;
                    result.offset = result.offset.saturating_sub(1);
                    result.redraw |= old_offset != result.offset;
                }
                0x7f => {
                    let old_offset = result.offset;
                    result.offset = result.offset.saturating_sub(1);
                    result.redraw |= old_offset != result.offset;
                }
                _ => {}
            }
            consumed += 1;
        }

        if consumed != 0 {
            self.pending.drain(..consumed);
        }
        result
    }
}

#[cfg(test)]
fn apply_input(input: &[u8], layout: &Layout, offset: usize) -> InputResult {
    InputParser::default().feed(input, layout, offset, DOWNLOAD_ROOT)
}

fn apply_mouse(
    event: MouseEvent,
    layout: &Layout,
    result: &mut InputResult,
    download_root: &str,
) {
    match event.button {
        64 => {
            let old_offset = result.offset;
            result.offset = result.offset.saturating_sub(SCROLL_STEP);
            result.redraw |= old_offset != result.offset;
        }
        65 => {
            let old_offset = result.offset;
            result.offset = layout.clamp_offset(result.offset.saturating_add(SCROLL_STEP));
            result.redraw |= old_offset != result.offset;
        }
        0 if event.pressed => {
            if let Some(action) = layout.action_at(event.x, event.y, result.offset) {
                result.action = Some(action);
                result.output.push_str(&action_output(action, download_root));
                result.redraw = true;
            }
        }
        _ => {}
    }
}

fn apply_delta(offset: usize, delta: isize, layout: &Layout) -> usize {
    if delta.is_negative() {
        offset.saturating_sub(delta.unsigned_abs())
    } else {
        offset.saturating_add(delta as usize).min(layout.max_offset())
    }
}

fn parse_sgr_mouse(input: &[u8]) -> MouseParse {
    if !input.starts_with(b"\x1b[<") {
        return MouseParse::Invalid;
    }

    let mut values = [0usize; 3];
    let mut field = 0;
    let mut value = 0usize;
    let mut digits = 0;
    for (index, &byte) in input[3..].iter().enumerate() {
        match byte {
            b'0'..=b'9' => {
                value = match value
                    .checked_mul(10)
                    .and_then(|value| value.checked_add((byte - b'0') as usize))
                {
                    Some(value) => value,
                    None => return MouseParse::Invalid,
                };
                digits += 1;
            }
            b';' => {
                if digits == 0 || field >= 2 {
                    return MouseParse::Invalid;
                }
                values[field] = value;
                field += 1;
                value = 0;
                digits = 0;
            }
            b'M' | b'm' => {
                if field != 2 || digits == 0 {
                    return MouseParse::Invalid;
                }
                values[field] = value;
                return MouseParse::Complete(
                    MouseEvent {
                        button: values[0],
                        x: values[1].saturating_sub(1),
                        y: values[2].saturating_sub(1),
                        pressed: byte == b'M',
                    },
                    index + 4,
                );
            }
            _ => return MouseParse::Invalid,
        }
    }
    MouseParse::Incomplete
}

fn parse_cursor_key(input: &[u8]) -> Option<(isize, usize)> {
    match input {
        [0x1b, b'[', b'A', ..] => Some((-1, 3)),
        [0x1b, b'[', b'B', ..] => Some((1, 3)),
        [0x1b, b'[', b'5', b'~', ..] => Some((-8, 4)),
        [0x1b, b'[', b'6', b'~', ..] => Some((8, 4)),
        _ => None,
    }
}

fn action_output(action: Action, download_root: &str) -> String {
    match action {
        Action::Download => {
            let mut output = String::new();
            output.push_str("\x1b]8;;z3rm-download:");
            output.push_str(download_root);
            output.push_str("\x1b\\Download server\x1b]8;;\x1b\\");
            output.push_str("\x1b]9;z3rm-download;");
            output.push_str(download_root);
            output.push('\x07');
            output
        }
        Action::Copy => format!(
            "\x1b]9;z3rm-copy;{COPY_BASE64}\x07\x1b]52;c;{COPY_BASE64}\x1b\\",
        ),
    }
}

struct App {
    layout: Layout,
    offset: usize,
    content_root: String,
    image_command: String,
    status: String,
}

impl App {
    fn new(layout: Layout, content_root: String, image_command: String) -> Self {
        Self {
            layout,
            offset: 0,
            content_root,
            image_command,
            status: String::from("Ready — scroll to explore, click a button, or press q to quit."),
        }
    }

    fn handle(&mut self, parser: &mut InputParser, bytes: &[u8]) -> io::Result<bool> {
        let result = parser.feed(bytes, &self.layout, self.offset, &self.content_root);
        self.offset = result.offset;
        if let Some(action) = result.action {
            self.status = match action {
                Action::Download => String::from("Download request sent to the host."),
                Action::Copy => format!("{COPY_TEXT} copied to the host clipboard."),
            };
        }
        if !result.output.is_empty() {
            write_fd(STDOUT_FD, result.output.as_bytes())?;
        }
        if result.redraw {
            draw(self)?;
        }
        Ok(result.quit)
    }
}

struct TerminalGuard {
    original: libc::termios,
    restored: bool,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        let mut original = MaybeUninit::<libc::termios>::uninit();
        let status = unsafe { libc::tcgetattr(STDIN_FD, original.as_mut_ptr()) };
        if status == -1 {
            return Err(io::Error::last_os_error());
        }
        let original = unsafe { original.assume_init() };
        let mut raw = original;
        unsafe { libc::cfmakeraw(&mut raw) };
        if unsafe { libc::tcsetattr(STDIN_FD, libc::TCSAFLUSH, &raw) } == -1 {
            return Err(io::Error::last_os_error());
        }

        let mut guard = Self {
            original,
            restored: false,
        };
        write_fd(
            STDOUT_FD,
            b"\x1b[?1049h\x1b[?25l\x1b[?1000h\x1b[?1006h\x1b[2J\x1b[H",
        )
        .map_err(|error| {
            guard.restore();
            error
        })?;
        Ok(guard)
    }

    fn restore(&mut self) {
        if self.restored {
            return;
        }
        if let Err(error) = write_fd(
            STDOUT_FD,
            b"\x1b[0m\x1b[?1000l\x1b[?1006l\x1b[?25h\x1b[?1049l\x1b[H",
        ) {
            report_io_error("restoring terminal display", &error);
        }
        let status = unsafe { libc::tcsetattr(STDIN_FD, libc::TCSANOW, &self.original) };
        if status == -1 {
            let error = io::Error::last_os_error();
            report_io_error("restoring terminal settings", &error);
        }
        self.restored = true;
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Query the controlling tty. An ioctl failure is a real error; only a
/// successful query with zero dimensions receives the deterministic fallback.
fn terminal_size() -> io::Result<(usize, usize)> {
    let mut window = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let status = unsafe { libc::ioctl(STDIN_FD, libc::TIOCGWINSZ, &mut window) };
    if status == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(terminal_size_from_winsize(window.ws_col, window.ws_row))
}

fn terminal_size_from_winsize(columns: u16, rows: u16) -> (usize, usize) {
    if columns == 0 || rows == 0 {
        (120, 32)
    } else {
        (columns as usize, rows as usize)
    }
}

fn read_fd(fd: libc::c_int, buffer: &mut [u8]) -> io::Result<usize> {
    loop {
        let count = unsafe {
            libc::read(
                fd,
                buffer.as_mut_ptr().cast::<libc::c_void>(),
                buffer.len(),
            )
        };
        if count >= 0 {
            return Ok(count as usize);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return Err(error);
    }
}

fn write_fd(fd: libc::c_int, bytes: &[u8]) -> io::Result<()> {
    let mut written = 0;
    while written < bytes.len() {
        let count = unsafe {
            libc::write(
                fd,
                bytes[written..].as_ptr().cast::<libc::c_void>(),
                bytes.len() - written,
            )
        };
        if count > 0 {
            written += count as usize;
            continue;
        }
        if count == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(error);
        }

        return Err(io::Error::new(io::ErrorKind::WriteZero, "terminal write returned zero"));
    }
    Ok(())
}
fn read_file(path: &str) -> io::Result<Vec<u8>> {
    let c_path = CString::new(path).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "image path contains a NUL byte")
    })?;
    let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd == -1 {
        return Err(io::Error::last_os_error());
    }

    let read_result = read_file_descriptor(fd);
    let close_status = unsafe { libc::close(fd) };
    if close_status == -1 {
        let close_error = io::Error::last_os_error();
        return match read_result {
            Ok(_) => Err(close_error),
            Err(error) => Err(io::Error::new(
                error.kind(),
                format!("{error}; closing {path}: {close_error}"),
            )),
        };
    }
    read_result
}

fn read_file_descriptor(fd: libc::c_int) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(64 * 1024);
    let mut buffer = [0u8; 8192];
    loop {
        let count = read_fd(fd, &mut buffer)?;
        if count == 0 {
            return Ok(bytes);
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
}

fn report_io_error(context: &str, error: &io::Error) {
    let message = format!("z3rm-tui: {context}: {error}\n");
    if let Err(report_error) = write_fd(STDERR_FD, message.as_bytes()) {
        eprintln!(
            "z3rm-tui: unable to report {context}: {report_error}; original error: {error}"
        );
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0] as usize;
        let second = chunk.get(1).copied().unwrap_or(0) as usize;
        let third = chunk.get(2).copied().unwrap_or(0) as usize;
        output.push(TABLE[first >> 2] as char);
        output.push(TABLE[((first & 0x03) << 4) | (second >> 4)] as char);
        if chunk.len() > 1 {
            output.push(TABLE[((second & 0x0f) << 2) | (third >> 6)] as char);
        } else {
            output.push('=');
        }
        if chunk.len() > 2 {
            output.push(TABLE[third & 0x3f] as char);
        } else {
            output.push('=');
        }
    }
    output
}

fn kitty_image_command(png: &[u8]) -> String {
    let encoded = base64_encode(png);
    let mut command = String::with_capacity(encoded.len() + 32);
    command.push_str("\x1b_Ga=T,f=100,i=1,c=56,r=12,q=2;");
    command.push_str(&encoded);
    command.push_str("\x1b\\");
    command
}

fn content_lines() -> [&'static str; PAGE_HEIGHT] {
    let mut lines = [""; PAGE_HEIGHT];
    lines[0] = "  Welcome to the z3rm guest landing page";
    lines[1] = "  A terminal workspace that keeps the interface close to the machine.";
    lines[3] = "  Architecture";
    lines[4] = "  The browser client renders GPUI and owns the visible workspace.";
    lines[5] = "  A real mux_server runs in this i686 Linux guest behind the serial bridge.";
    lines[6] = "  The same framed mux protocol connects panes, layouts, and terminal media.";
    lines[8] = "  The guest is deliberately small: one static server, one PTY, and this TUI.";
    lines[9] = "  No QuickJS, network service, or demo shell is hidden behind this screen.";
    lines[12] = "  Explore the project";
    lines[13] = "  Scroll through this page with the wheel or arrow keys.";
    lines[14] = "  The terminal grid below is sent with Kitty Graphics from the bundled PNG.";
    lines[16] = "  Terminal media";
    lines[18] = "  The image transfer keeps its source bytes intact and is decoded by the client.";
    lines[20] = "  Actions";
    lines[21] = "  Download emits an OSC 9 z3rm action alongside its OSC 8 link.";
    lines[22] = "  Copy emits OSC 52 so the host clipboard receives the install command.";
    lines[25] = "  Install on a host with Cargo, then launch the server from your shell.";
    lines[27] = "  The first pane is this landing program; subsequent panes are regular PTYs.";
    lines[30] = "  Protocol notes";
    lines[31] = "  SGR mouse tracking is enabled only while this program owns the terminal.";
    lines[32] = "  Every exit path restores the saved termios and alternate-screen state.";
    lines[35] = "  Thanks for trying z3rm.";
    lines[37] = "  This page is content, not a shell prompt: press q when you are ready.";
    lines[40] = "  z3rm is built for transparent, inspectable terminal sessions.";
    lines[43] = "  End of landing page.";
    lines
}

fn draw(app: &App) -> io::Result<()> {
    let lines = content_lines();
    write_fd(STDOUT_FD, b"\x1b[2J\x1b[H")?;

    if app.layout.rows >= 1 {
        write_fd(STDOUT_FD, b"\x1b[1;1H\x1b[38;5;45;1m  z3rm\x1b[0m")?;
    }
    if app.layout.rows >= 2 {
        write_fd(
            STDOUT_FD,
            b"\x1b[2;1H\x1b[38;5;250m  Terminal-first collaboration in a real Linux guest.\x1b[0m",
        )?;
    }
    if app.layout.rows >= 3 {
        write_fd(STDOUT_FD, b"\x1b[3;1H\x1b[38;5;238m----------------------------------------------------------------\x1b[0m")?;
    }

    let viewport = app.layout.viewport_height.min(app.layout.rows.saturating_sub(CONTENT_TOP + FOOTER_ROWS));
    for row in 0..viewport {
        let screen_row = CONTENT_TOP + row;
        let page_row = app.offset.saturating_add(row);
        let cursor = format!("\x1b[{};1H", screen_row + 1);
        write_fd(STDOUT_FD, cursor.as_bytes())?;
        if page_row == IMAGE_PAGE_ROW {
            write_fd(STDOUT_FD, app.image_command.as_bytes())?;
        }

        if page_row == app.layout.download.y + 1
            && page_row == app.layout.copy.y + 1
        {
            let download_column = format!("\x1b[{}G", app.layout.download.x + 1);
            write_fd(STDOUT_FD, download_column.as_bytes())?;
            write_fd(STDOUT_FD, b"\x1b[48;5;24;38;5;255m  ")?;
            write_fd(STDOUT_FD, b"\x1b]8;;z3rm-download:")?;
            write_fd(STDOUT_FD, app.content_root.as_bytes())?;
            write_fd(STDOUT_FD, b"\x1b\\Download server\x1b]8;;\x1b\\  \x1b[0m")?;
            let copy_column = format!("\x1b[{}G", app.layout.copy.x + 1);
            write_fd(STDOUT_FD, copy_column.as_bytes())?;
            write_fd(
                STDOUT_FD,
                b"\x1b[48;5;238;38;5;255m  Copy install command  \x1b[0m",
            )?;
        } else if page_row == app.layout.download.y + 1 {
            let download_column = format!("\x1b[{}G", app.layout.download.x + 1);
            write_fd(STDOUT_FD, download_column.as_bytes())?;
            write_fd(STDOUT_FD, b"\x1b[48;5;24;38;5;255m  ")?;
            write_fd(STDOUT_FD, b"\x1b]8;;z3rm-download:")?;
            write_fd(STDOUT_FD, app.content_root.as_bytes())?;
            write_fd(STDOUT_FD, b"\x1b\\Download server\x1b]8;;\x1b\\  \x1b[0m")?;
        } else if page_row == app.layout.copy.y + 1 {
            let copy_column = format!("\x1b[{}G", app.layout.copy.x + 1);
            write_fd(STDOUT_FD, copy_column.as_bytes())?;
            write_fd(
                STDOUT_FD,
                b"\x1b[48;5;238;38;5;255m  Copy install command  \x1b[0m",
            )?;
        } else {
            let line = lines.get(page_row).copied().unwrap_or("");
            let mut visible = line.as_bytes().to_vec();
            visible.truncate(app.layout.columns);
            write_fd(STDOUT_FD, &visible)?;
        }
        write_fd(STDOUT_FD, b"\x1b[K")?;
    }

    if app.layout.rows >= FOOTER_ROWS {
        let status_row = app.layout.rows - 1;
        let indicator_row = app.layout.rows;
        let status = format!("\x1b[{};1H\x1b[38;5;252m  {}\x1b[0m", status_row, app.status);
        write_fd(STDOUT_FD, status.as_bytes())?;
        let indicator = format!(
            "\x1b[{};1H\x1b[38;5;244m  Page {}/{}  •  wheel/↑↓ scroll  •  q quit\x1b[0m",
            indicator_row,
            app.offset + 1,
            app.layout.max_offset() + 1,
        );
        write_fd(STDOUT_FD, indicator.as_bytes())?;
    }
    Ok(())
}

fn run() -> io::Result<()> {
    let content_root = std::env::args().nth(1).unwrap_or_else(|| String::from(DOWNLOAD_ROOT));
    let png = read_file(IMAGE_PATH)?;
    let image_command = kitty_image_command(&png);
    let (columns, rows) = terminal_size()?;
    let terminal = TerminalGuard::enter()?;
    let mut app = App::new(Layout::new(columns, rows), content_root, image_command);
    draw(&app)?;

    let mut parser = InputParser::default();
    let mut input = [0u8; 4096];
    loop {
        let count = read_fd(STDIN_FD, &mut input)?;
        if count == 0 || app.handle(&mut parser, &input[..count])? {
            break;
        }
    }
    drop(terminal);
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        report_io_error("running the landing TUI", &error);
        process::exit(1);
    }
}

const STDERR_FD: libc::c_int = 2;

#[cfg(test)]
mod tests {
    use super::{Action, Layout, Rect, apply_input, terminal_size_from_winsize};

    #[test]
    fn terminal_layout_has_stable_controls() {
        let layout = Layout::new(120, 32);

        assert_eq!(layout.download, Rect::new(4, 11, 26, 3));
        assert_eq!(layout.copy, Rect::new(33, 11, 29, 3));
        assert_eq!(layout.viewport_height, 26);
        assert_eq!(layout.page_height, 46);
    }

    #[test]
    fn wheel_input_clamps_page_offset() {
        let layout = Layout::new(120, 32);
        let mut offset = 0;

        offset = apply_input(b"\x1b[<65;1;1M", &layout, offset).offset;
        assert_eq!(offset, 3);
        offset = apply_input(b"\x1b[<64;1;1M", &layout, offset).offset;
        assert_eq!(offset, 0);

        for _ in 0..100 {
            offset = apply_input(b"\x1b[<65;1;1M", &layout, offset).offset;
        }
        assert_eq!(offset, layout.page_height - layout.viewport_height);
    }

    #[test]
    fn download_click_emits_hyperlink_and_osc_action() {
        let layout = Layout::new(120, 32);
        let result = apply_input(b"\x1b[<0;18;16M", &layout, 0);

        assert_eq!(result.action, Some(Action::Download));
        assert!(result.output.contains("z3rm-download:"));
        assert!(result.output.contains("\x1b]9;z3rm-download;/z3rm-server\x07"));
    }

    #[test]
    fn click_outside_controls_emits_no_action() {
        let layout = Layout::new(120, 32);
        let result = apply_input(b"\x1b[<0;100;30M", &layout, 0);

        assert_eq!(result.action, None);
        assert!(result.output.is_empty());
    }

    #[test]
    fn copy_click_emits_typed_action_and_clipboard_sequence() {
        let layout = Layout::new(120, 32);
        let result = apply_input(b"\x1b[<0;48;16M", &layout, 0);

        assert_eq!(result.action, Some(Action::Copy));
        assert!(result.output.contains("\x1b]9;z3rm-copy;Y2FyZ28gaW5zdGFsbCB6M3Jt\x07"));
        assert!(result.output.contains("\x1b]52;c;Y2FyZ28gaW5zdGFsbCB6M3Jt\x1b\\"));
    }

    #[test]
    fn multiple_clicks_emit_actions_in_input_order() {
        let layout = Layout::new(120, 32);
        let result = apply_input(
            b"\x1b[<0;18;16M\x1b[<0;48;16M",
            &layout,
            0,
        );

        assert_eq!(result.action, Some(Action::Copy));
        let download = result.output.find("\x1b]9;z3rm-download;/z3rm-server\x07");
        let copy_action = result.output.find("\x1b]9;z3rm-copy;");
        let copy_clipboard = result.output.find("\x1b]52;c;");
        assert!(download.is_some());
        assert!(copy_action.is_some());
        assert!(copy_clipboard.is_some());
        assert!(download < copy_action);
        assert!(copy_action < copy_clipboard);
    }

    #[test]
    fn zero_winsize_uses_documented_default() {
        assert_eq!(terminal_size_from_winsize(0, 0), (120, 32));
        assert_eq!(terminal_size_from_winsize(120, 32), (120, 32));
    }
}
