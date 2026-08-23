//! In-browser shell and Linux userland environment for the WebAssembly client.
//!
//! Provides an interactive command-line environment backed by an in-memory
//! virtual filesystem (VFS) with real z3rm project assets and z3rm multiplexer
//! CLI commands. All output is encoded with standard ANSI escape sequences
//! and fed directly into the `WebTerminal` (Alacritty core).

use std::collections::{BTreeMap, HashMap};

/// A node in the virtual filesystem.
#[derive(Clone, Debug)]
pub enum VfsNode {
    File { content: String, executable: bool },
    Directory { children: BTreeMap<String, VfsNode> },
}

/// In-memory virtual filesystem for the wasm demo.
#[derive(Clone, Debug)]
pub struct VirtualFs {
    root: VfsNode,
    cwd: String,
}

impl VirtualFs {
    pub fn new() -> Self {
        let mut fs = Self {
            root: VfsNode::Directory {
                children: BTreeMap::new(),
            },
            cwd: "/home/user/z3rm".to_string(),
        };
        fs.seed_default_files();
        fs
    }

    fn seed_default_files(&mut self) {
        self.mkdir_p("/home/user/z3rm/crates/z3rm/src");
        self.mkdir_p("/home/user/z3rm/crates/mux_server/src");
        self.mkdir_p("/home/user/z3rm/crates/workspace/src");
        self.mkdir_p("/home/user/z3rm/crates/terminal_view/src");
        self.mkdir_p("/home/user/z3rm/docs/specs");
        self.mkdir_p("/home/user/.config/z3rm");

        self.write_file(
            "/home/user/z3rm/README.md",
            "# z3rm — Persistent GPU-Accelerated Terminal Workspace\n\n\
             Your shells outlive the window.\n\
             mux_server owns PTYs, alacritty grid, layout, and scrollback.\n\
             The GUI client renders state over structured binary protocol.\n",
        );

        self.write_file(
            "/home/user/z3rm/Cargo.toml",
            "[workspace]\n\
             members = [\"crates/z3rm\", \"crates/mux_server\", \"crates/workspace\", \"crates/terminal_view\"]\n\
             resolver = \"2\"\n\n\
             [workspace.package]\n\
             version = \"1.12.0\"\n\
             edition = \"2024\"\n",
        );

        self.write_file(
            "/home/user/z3rm/NOTES.md",
            "# Session Architecture Notes\n\n\
             - §3.1 Server-canonical terminal state: mux_server owns authority\n\
             - §3.3 Row-level grid diffs driven by PaneDirty notifications\n\
             - §3.4 Authoritative reconnect reconciles from full SessionSnapshot\n\
             - §4.1 Crash-safe shadow snapshots with monotonic sequence numbers\n\
             - §5.2 QuickJS extension host runs on dedicated background thread\n",
        );

        self.write_file(
            "/home/user/z3rm/crates/z3rm/src/main.rs",
            "//! z3rm entry point\n\
             fn main() {\n\
                 println!(\"Starting z3rm persistent workspace...\");\n\
             }\n",
        );

        self.write_file(
            "/home/user/z3rm/crates/mux_server/src/pane.rs",
            "//! mux_server pane management\n\
             pub struct Pane {\n\
                 pub id: String,\n\
                 pub generation: u64,\n\
             }\n",
        );

        self.write_file(
            "/home/user/.config/z3rm/config.toml",
            "[theme]\n\
             name = \"One Dark\"\n\n\
             [terminal]\n\
             font_family = \"IBM Plex Mono\"\n\
             font_size = 14.0\n\
             cursor_shape = \"block\"\n",
        );
    }

    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    pub fn set_cwd(&mut self, path: &str) -> Result<(), String> {
        let abs = self.resolve_path(path);
        if self.get_node(&abs).map_or(false, |n| matches!(n, VfsNode::Directory { .. })) {
            self.cwd = abs;
            Ok(())
        } else {
            Err(format!("cd: no such file or directory: {path}"))
        }
    }

    pub fn resolve_path(&self, path: &str) -> String {
        if path.is_empty() {
            return self.cwd.clone();
        }
        let clean_path = if path.starts_with('~') {
            format!("/home/user{}", &path[1..])
        } else if !path.starts_with('/') {
            format!("{}/{}", self.cwd, path)
        } else {
            path.to_string()
        };

        let mut segments = Vec::new();
        for seg in clean_path.split('/') {
            match seg {
                "" | "." => {}
                ".." => {
                    segments.pop();
                }
                other => segments.push(other),
            }
        }
        format!("/{}", segments.join("/"))
    }

    pub fn mkdir_p(&mut self, path: &str) {
        let abs = self.resolve_path(path);
        let segments: Vec<&str> = abs.split('/').filter(|s| !s.is_empty()).collect();
        let mut current = &mut self.root;
        for seg in segments {
            if let VfsNode::Directory { children } = current {
                current = children
                    .entry(seg.to_string())
                    .or_insert_with(|| VfsNode::Directory {
                        children: BTreeMap::new(),
                    });
            }
        }
    }

    pub fn write_file(&mut self, path: &str, content: &str) {
        let abs = self.resolve_path(path);
        if let Some((parent_path, file_name)) = abs.rsplit_once('/') {
            self.mkdir_p(parent_path);
            if let Some(VfsNode::Directory { children }) = self.get_node_mut(parent_path) {
                children.insert(
                    file_name.to_string(),
                    VfsNode::File {
                        content: content.to_string(),
                        executable: false,
                    },
                );
            }
        }
    }

    pub fn read_file(&self, path: &str) -> Result<String, String> {
        let abs = self.resolve_path(path);
        match self.get_node(&abs) {
            Some(VfsNode::File { content, .. }) => Ok(content.clone()),
            Some(VfsNode::Directory { .. }) => Err(format!("cat: {path}: Is a directory")),
            None => Err(format!("cat: {path}: No such file or directory")),
        }
    }

    pub fn list_dir(&self, path: &str) -> Result<Vec<(String, bool, bool)>, String> {
        let abs = self.resolve_path(path);
        match self.get_node(&abs) {
            Some(VfsNode::Directory { children }) => {
                let mut list = Vec::new();
                for (name, node) in children {
                    let is_dir = matches!(node, VfsNode::Directory { .. });
                    let is_exec = matches!(node, VfsNode::File { executable: true, .. });
                    list.push((name.clone(), is_dir, is_exec));
                }
                Ok(list)
            }
            Some(VfsNode::File { .. }) => Err(format!("ls: {path}: Not a directory")),
            None => Err(format!("ls: {path}: No such file or directory")),
        }
    }

    pub fn remove(&mut self, path: &str) -> Result<(), String> {
        let abs = self.resolve_path(path);
        if let Some((parent_path, file_name)) = abs.rsplit_once('/') {
            if let Some(VfsNode::Directory { children }) = self.get_node_mut(parent_path) {
                if children.remove(file_name).is_some() {
                    return Ok(());
                }
            }
        }
        Err(format!("rm: cannot remove '{path}': No such file or directory"))
    }

    fn get_node(&self, abs_path: &str) -> Option<&VfsNode> {
        if abs_path == "/" || abs_path.is_empty() {
            return Some(&self.root);
        }
        let segments: Vec<&str> = abs_path.split('/').filter(|s| !s.is_empty()).collect();
        let mut current = &self.root;
        for seg in segments {
            match current {
                VfsNode::Directory { children } => {
                    current = children.get(seg)?;
                }
                _ => return None,
            }
        }
        Some(current)
    }

    fn get_node_mut(&mut self, abs_path: &str) -> Option<&mut VfsNode> {
        if abs_path == "/" || abs_path.is_empty() {
            return Some(&mut self.root);
        }
        let segments: Vec<&str> = abs_path.split('/').filter(|s| !s.is_empty()).collect();
        let mut current = &mut self.root;
        for seg in segments {
            match current {
                VfsNode::Directory { children } => {
                    current = children.get_mut(seg)?;
                }
                _ => return None,
            }
        }
        Some(current)
    }
}

/// Interactive Linux-like shell running inside the WASM demo.
pub struct WasmShell {
    vfs: VirtualFs,
    line_buffer: Vec<char>,
    cursor_pos: usize,
    history: Vec<String>,
    history_idx: Option<usize>,
    user: String,
    hostname: String,
}

impl WasmShell {
    pub fn new() -> Self {
        Self {
            vfs: VirtualFs::new(),
            line_buffer: Vec::new(),
            cursor_pos: 0,
            history: Vec::new(),
            history_idx: None,
            user: "user".to_string(),
            hostname: "z3rm".to_string(),
        }
    }

    /// Format the current prompt string with ANSI colors.
    pub fn format_prompt(&self) -> String {
        let display_cwd = if self.vfs.cwd().starts_with("/home/user") {
            format!("~{}", &self.vfs.cwd()["/home/user".len()..])
        } else {
            self.vfs.cwd().to_string()
        };
        format!(
            "\x1b[1;32m{}@{}\x1b[0m:\x1b[1;34m{}\x1b[0m$ ",
            self.user, self.hostname, display_cwd
        )
    }

    /// Initial banner printed when a new shell pane opens.
    pub fn banner() -> String {
        "\x1b[1;36mz3rm terminal workspace\x1b[0m — \x1b[90m(WebAssembly client with real Alacritty grid)\x1b[0m\r\n\
         Type \x1b[1;33mhelp\x1b[0m for commands, \x1b[1;33mls\x1b[0m to explore, or \x1b[1;33mz3rm status\x1b[0m for mux details.\r\n\r\n"
            .to_string()
    }

    /// Handle raw input string or escape sequence. Returns ANSI byte stream to feed to terminal.
    pub fn handle_input(&mut self, input: &str) -> String {
        let mut output = String::new();

        // Handle special multi-byte ANSI sequences directly
        match input {
            // Left arrow: move cursor left
            "\x1b[D" => {
                if self.cursor_pos > 0 {
                    self.cursor_pos -= 1;
                    output.push_str("\x1b[D");
                }
                return output;
            }
            // Right arrow: move cursor right
            "\x1b[C" => {
                if self.cursor_pos < self.line_buffer.len() {
                    self.cursor_pos += 1;
                    output.push_str("\x1b[C");
                }
                return output;
            }
            // Home: move cursor to start of line
            "\x1b[H" => {
                if self.cursor_pos > 0 {
                    output.push_str(&format!("\x1b[{}D", self.cursor_pos));
                    self.cursor_pos = 0;
                }
                return output;
            }
            // End: move cursor to end of line
            "\x1b[F" => {
                let remaining = self.line_buffer.len() - self.cursor_pos;
                if remaining > 0 {
                    output.push_str(&format!("\x1b[{}C", remaining));
                    self.cursor_pos = self.line_buffer.len();
                }
                return output;
            }
            // Up arrow: history backward
            "\x1b[A" => {
                if !self.history.is_empty() {
                    let next_idx = match self.history_idx {
                        None => self.history.len().saturating_sub(1),
                        Some(idx) => idx.saturating_sub(1),
                    };
                    self.history_idx = Some(next_idx);
                    let cmd = &self.history[next_idx];
                    // Clear current line on screen
                    if self.cursor_pos > 0 {
                        output.push_str(&format!("\x1b[{}D", self.cursor_pos));
                    }
                    output.push_str("\x1b[K");
                    output.push_str(cmd);
                    self.line_buffer = cmd.chars().collect();
                    self.cursor_pos = self.line_buffer.len();
                }
                return output;
            }
            // Down arrow: history forward
            "\x1b[B" => {
                if let Some(idx) = self.history_idx {
                    if idx + 1 < self.history.len() {
                        let next_idx = idx + 1;
                        self.history_idx = Some(next_idx);
                        let cmd = &self.history[next_idx];
                        if self.cursor_pos > 0 {
                            output.push_str(&format!("\x1b[{}D", self.cursor_pos));
                        }
                        output.push_str("\x1b[K");
                        output.push_str(cmd);
                        self.line_buffer = cmd.chars().collect();
                        self.cursor_pos = self.line_buffer.len();
                    } else {
                        self.history_idx = None;
                        if self.cursor_pos > 0 {
                            output.push_str(&format!("\x1b[{}D", self.cursor_pos));
                        }
                        output.push_str("\x1b[K");
                        self.line_buffer.clear();
                        self.cursor_pos = 0;
                    }
                }
                return output;
            }
            _ => {}
        }

        for c in input.chars() {
            match c {
                // Enter / Return: execute command
                '\r' | '\n' => {
                    output.push_str("\r\n");
                    let command: String = self.line_buffer.iter().collect();
                    let command = command.trim().to_string();
                    if !command.is_empty() {
                        self.history.push(command.clone());
                    }
                    self.line_buffer.clear();
                    self.cursor_pos = 0;
                    self.history_idx = None;

                    if !command.is_empty() {
                        let exec_out = self.execute_command(&command);
                        if !exec_out.is_empty() {
                            output.push_str(&exec_out);
                            if !exec_out.ends_with("\r\n") && !exec_out.ends_with('\n') {
                                output.push_str("\r\n");
                            }
                        }
                    }
                    output.push_str(&self.format_prompt());
                }
                // Backspace (ASCII 8 or 127)
                '\x08' | '\x7f' => {
                    if self.cursor_pos > 0 {
                        self.cursor_pos -= 1;
                        self.line_buffer.remove(self.cursor_pos);
                        output.push_str("\x08\x1b[K");
                        let remaining: String = self.line_buffer[self.cursor_pos..].iter().collect();
                        if !remaining.is_empty() {
                            output.push_str(&remaining);
                            output.push_str(&format!("\x1b[{}D", self.line_buffer.len() - self.cursor_pos));
                        }
                    }
                }
                // Ctrl+C (ETX = 3): cancel line
                '\x03' => {
                    output.push_str("^C\r\n");
                    self.line_buffer.clear();
                    self.cursor_pos = 0;
                    self.history_idx = None;
                    output.push_str(&self.format_prompt());
                }
                // Ctrl+L (FF = 12): clear screen
                '\x0c' => {
                    output.push_str("\x1b[2J\x1b[H");
                    output.push_str(&self.format_prompt());
                    let line_str: String = self.line_buffer.iter().collect();
                    output.push_str(&line_str);
                    if self.cursor_pos < self.line_buffer.len() {
                        output.push_str(&format!(
                            "\x1b[{}D",
                            self.line_buffer.len() - self.cursor_pos
                        ));
                    }
                }
                // Tab (HT = 9): autocomplete
                '\t' => {
                    let current_str: String = self.line_buffer.iter().collect();
                    let word = current_str
                        .split_whitespace()
                        .last()
                        .unwrap_or("")
                        .to_string();
                    if !word.is_empty() {
                        if let Ok(entries) = self.vfs.list_dir(self.vfs.cwd()) {
                            let matches: Vec<&str> = entries
                                .iter()
                                .map(|(name, _, _)| name.as_str())
                                .filter(|name| name.starts_with(&word))
                                .collect();
                            if matches.len() == 1 {
                                let completion = &matches[0][word.len()..];
                                for ch in completion.chars() {
                                    self.line_buffer.insert(self.cursor_pos, ch);
                                    self.cursor_pos += 1;
                                }
                                output.push_str(completion);
                            } else if matches.len() > 1 {
                                output.push_str("\r\n");
                                for m in matches {
                                    output.push_str(&format!("{m}  "));
                                }
                                output.push_str("\r\n");
                                output.push_str(&self.format_prompt());
                                let line_str: String = self.line_buffer.iter().collect();
                                output.push_str(&line_str);
                            }
                        }
                    }
                }
                // Standard printable character
                c if !c.is_control() => {
                    self.line_buffer.insert(self.cursor_pos, c);
                    self.cursor_pos += 1;
                    let remaining: String = self.line_buffer[self.cursor_pos - 1..].iter().collect();
                    output.push_str(&remaining);
                    let trailing_chars = self.line_buffer.len() - self.cursor_pos;
                    if trailing_chars > 0 {
                        output.push_str(&format!("\x1b[{}D", trailing_chars));
                    }
                }
                _ => {}
            }
        }
        output
    }
    /// Execute a shell command string and return the formatted ANSI output.
    pub fn execute_command(&mut self, cmd_line: &str) -> String {
        let parts: Vec<&str> = cmd_line.split_whitespace().collect();
        if parts.is_empty() {
            return String::new();
        }

        let cmd = parts[0];
        let args = &parts[1..];

        match cmd {
            "help" => self.cmd_help(),
            "ls" => self.cmd_ls(args),
            "cd" => self.cmd_cd(args),
            "pwd" => format!("{}\r\n", self.vfs.cwd()),
            "cat" => self.cmd_cat(args),
            "echo" => self.cmd_echo(args),
            "clear" => "\x1b[2J\x1b[H".to_string(),
            "uname" => self.cmd_uname(args),
            "whoami" => format!("{}\r\n", self.user),
            "id" => "uid=1000(user) gid=1000(user) groups=1000(user),4(adm),27(sudo)\r\n"
                .to_string(),
            "date" => "Sun Aug 23 08:30:00 UTC 2026\r\n".to_string(),
            "mkdir" => self.cmd_mkdir(args),
            "touch" => self.cmd_touch(args),
            "rm" => self.cmd_rm(args),
            "z3rm" => self.cmd_z3rm(args),
            "cargo" => self.cmd_cargo(args),
            "git" => self.cmd_git(args),
            _ => format!(
                "\x1b[31mzsh: command not found: {}\x1b[0m\r\nType \x1b[33mhelp\x1b[0m for available commands.\r\n",
                cmd
            ),
        }
    }

    fn cmd_help(&self) -> String {
        "\x1b[1;36mAvailable Shell Commands:\x1b[0m\r\n\
         \x1b[33m  ls [-la] [path]\x1b[0m    List files and directories with color\r\n\
         \x1b[33m  cd [path]\x1b[0m          Change current working directory\r\n\
         \x1b[33m  pwd\x1b[0m                Print current working directory\r\n\
         \x1b[33m  cat [file]\x1b[0m         Display file contents\r\n\
         \x1b[33m  echo [text]\x1b[0m        Print text to terminal\r\n\
         \x1b[33m  mkdir [path]\x1b[0m       Create virtual directory\r\n\
         \x1b[33m  touch [file]\x1b[0m       Create empty virtual file\r\n\
         \x1b[33m  rm [file]\x1b[0m          Remove virtual file\r\n\
         \x1b[33m  clear\x1b[0m              Clear terminal screen\r\n\
         \x1b[33m  uname [-a]\x1b[0m         System identification\r\n\
         \x1b[33m  whoami / id / date\x1b[0m User and time info\r\n\
         \r\n\
         \x1b[1;36mz3rm Multiplexer Commands:\x1b[0m\r\n\
         \x1b[32m  z3rm status\x1b[0m        Show mux server status and active connections\r\n\
         \x1b[32m  z3rm list-panes\x1b[0m    List all panes in the attached session\r\n\
         \x1b[32m  z3rm list-windows\x1b[0m  List all windows in the attached session\r\n\
         \x1b[32m  z3rm list-sessions\x1b[0m List active persistent sessions\r\n\
         \x1b[32m  z3rm attach -t <s>\x1b[0m Reconcile view from authoritative snapshot\r\n\
         \x1b[32m  cargo test\x1b[0m         Run the z3rm test suite simulation\r\n\
         \x1b[32m  git status\x1b[0m         Show workspace repository status\r\n"
            .to_string()
    }

    fn cmd_ls(&self, args: &[&str]) -> String {
        let long = args.iter().any(|a| a.contains('l'));
        let target_path = args
            .iter()
            .find(|a| !a.starts_with('-'))
            .copied()
            .unwrap_or("");

        match self.vfs.list_dir(target_path) {
            Ok(entries) => {
                if entries.is_empty() {
                    return String::new();
                }
                let mut out = String::new();
                for (name, is_dir, is_exec) in entries {
                    if long {
                        let mode_str = if is_dir { "drwxr-xr-x" } else { "-rw-r--r--" };
                        let name_fmt = if is_dir {
                            format!("\x1b[1;34m{name}/\x1b[0m")
                        } else if is_exec {
                            format!("\x1b[1;32m{name}*\x1b[0m")
                        } else {
                            name.clone()
                        };
                        out.push_str(&format!("{mode_str}  1 user user  4096 Aug 23 08:30 {name_fmt}\r\n"));
                    } else {
                        let name_fmt = if is_dir {
                            format!("\x1b[1;34m{name}/\x1b[0m  ")
                        } else if is_exec {
                            format!("\x1b[1;32m{name}*\x1b[0m  ")
                        } else {
                            format!("{name}  ")
                        };
                        out.push_str(&name_fmt);
                    }
                }
                if !long {
                    out.push_str("\r\n");
                }
                out
            }
            Err(e) => format!("\x1b[31m{e}\x1b[0m\r\n"),
        }
    }

    fn cmd_cd(&mut self, args: &[&str]) -> String {
        let target = args.first().copied().unwrap_or("~");
        if let Err(e) = self.vfs.set_cwd(target) {
            format!("\x1b[31m{e}\x1b[0m\r\n")
        } else {
            String::new()
        }
    }

    fn cmd_cat(&self, args: &[&str]) -> String {
        if args.is_empty() {
            return "cat: missing file argument\r\n".to_string();
        }
        let mut out = String::new();
        for file in args {
            match self.vfs.read_file(file) {
                Ok(content) => {
                    for line in content.lines() {
                        out.push_str(line);
                        out.push_str("\r\n");
                    }
                }
                Err(e) => {
                    out.push_str(&format!("\x1b[31m{e}\x1b[0m\r\n"));
                }
            }
        }
        out
    }

    fn cmd_echo(&self, args: &[&str]) -> String {
        let joined = args.join(" ");
        format!("{joined}\r\n")
    }

    fn cmd_uname(&self, args: &[&str]) -> String {
        if args.iter().any(|a| *a == "-a") {
            "Linux z3rm-wasm 6.12.0-z3rm #1 SMP PREEMPT WebAssembly x86_64 GNU/Linux\r\n"
                .to_string()
        } else {
            "Linux\r\n".to_string()
        }
    }

    fn cmd_mkdir(&mut self, args: &[&str]) -> String {
        if args.is_empty() {
            return "mkdir: missing operand\r\n".to_string();
        }
        for dir in args {
            self.vfs.mkdir_p(dir);
        }
        String::new()
    }

    fn cmd_touch(&mut self, args: &[&str]) -> String {
        if args.is_empty() {
            return "touch: missing file operand\r\n".to_string();
        }
        for file in args {
            self.vfs.write_file(file, "");
        }
        String::new()
    }

    fn cmd_rm(&mut self, args: &[&str]) -> String {
        if args.is_empty() {
            return "rm: missing operand\r\n".to_string();
        }
        let mut out = String::new();
        for file in args {
            if let Err(e) = self.vfs.remove(file) {
                out.push_str(&format!("\x1b[31m{e}\x1b[0m\r\n"));
            }
        }
        out
    }

    fn cmd_z3rm(&self, args: &[&str]) -> String {
        let subcmd = args.first().copied().unwrap_or("status");
        match subcmd {
            "status" => {
                "\x1b[1;36m=== z3rm Mux Server Status ===\x1b[0m\r\n\
                 \x1b[1mServer:\x1b[0m       z3rm_server v1.12.0 (authoritative daemon)\r\n\
                 \x1b[1mUptime:\x1b[0m       19h 42m (keep_alive = true)\r\n\
                 \x1b[1mSessions:\x1b[0m     3 active (work, observe, shell)\r\n\
                 \x1b[1mWindows:\x1b[0m      8 total\r\n\
                 \x1b[1mPanes:\x1b[0m        17 total (all backed by real Alacritty emulators)\r\n\
                 \x1b[1mClients:\x1b[0m      2 connected (1 GPUI GUI client, 1 CLI observer)\r\n\
                 \x1b[1mProtocols:\x1b[0m    mux_protocol v1.2, structured grid diffs\r\n\
                 \x1b[1mExtension:\x1b[0m    QuickJS host (dedicated OS thread, status-bar online)\r\n"
                    .to_string()
            }
            "list-panes" => {
                "\x1b[1;33mPANE_ID         GEN    SIZE     TITLE     CWD\x1b[0m\r\n\
                 pane-editor     1842   120x32   editor    ~/z3rm/work\r\n\
                 pane-tests      128    120x32   tests     ~/z3rm/work\r\n\
                 pane-logs       907    120x32   logs      ~/z3rm/observe\r\n\
                 pane-metrics    332    120x32   metrics   ~/z3rm/observe\r\n\
                 pane-shell      45     120x32   shell     ~/z3rm/shell\r\n"
                    .to_string()
            }
            "list-windows" => {
                "\x1b[1;33mWIN_ID      PANES  LAYOUT           TITLE\x1b[0m\r\n\
                 window-0    2      left_right[62%]  work\r\n\
                 window-1    2      top_bottom[68%]  observe\r\n\
                 window-2    1      single           shell\r\n"
                    .to_string()
            }
            "list-sessions" => {
                "\x1b[1;33mSESSION_ID  WINDOWS  PANES  ATTACHED\x1b[0m\r\n\
                 work        3        5      true (current)\r\n\
                 monitoring  2        4      false\r\n\
                 scratch     1        1      false\r\n"
                    .to_string()
            }
            "attach" => {
                let target = args.get(1).copied().unwrap_or("work");
                format!(
                    "\x1b[1;32mAttached to session '{}'\x1b[0m\r\n\
                     Reconciled from authoritative SessionSnapshot (generation 1842).\r\n",
                    target
                )
            }
            "split-window" => {
                let dir = if args.iter().any(|a| *a == "-h") {
                    "horizontal"
                } else {
                    "vertical"
                };
                format!("\x1b[1;32mCreated new pane with {dir} split.\x1b[0m\r\n")
            }
            _ => format!("z3rm: unknown subcommand: '{subcmd}'. Try 'z3rm status'.\r\n"),
        }
    }

    fn cmd_cargo(&self, args: &[&str]) -> String {
        let sub = args.first().copied().unwrap_or("test");
        match sub {
            "test" => {
                "\x1b[1;36m   Compiling\x1b[0m z3rm v1.12.0 (/home/user/z3rm)\r\n\
                 \x1b[1;36m    Finished\x1b[0m `test` profile [unoptimized + debuginfo] in 1.42s\r\n\
                 \x1b[1;32m     Running\x1b[0m unittests src/main.rs\r\n\
                 running 128 tests\r\n\
                 test mux::layout::split_depth_bounded ... \x1b[32mok\x1b[0m\r\n\
                 test mux::reconnect::atomic_snapshot ... \x1b[32mok\x1b[0m\r\n\
                 test mux::scrollback::authoritative_grid ... \x1b[32mok\x1b[0m\r\n\
                 test a11y::semantic_actions ... \x1b[32mok\x1b[0m\r\n\
                 test result: \x1b[1;32mok\x1b[0m. 128 passed; 0 failed; 0 ignored\r\n"
                    .to_string()
            }
            "build" => {
                "\x1b[1;36m   Compiling\x1b[0m z3rm v1.12.0 (/home/user/z3rm)\r\n\
                 \x1b[1;32m    Finished\x1b[0m `dev` profile [unoptimized + debuginfo] in 2.15s\r\n"
                    .to_string()
            }
            _ => format!("cargo: unsupported argument: '{sub}'\r\n"),
        }
    }

    fn cmd_git(&self, args: &[&str]) -> String {
        let sub = args.first().copied().unwrap_or("status");
        match sub {
            "status" => {
                "On branch \x1b[1;32mmain\x1b[0m\r\n\
                 Your branch is up to date with 'origin/main'.\r\n\
                 nothing to commit, working tree clean\r\n"
                    .to_string()
            }
            "log" => {
                "\x1b[33mcommit b2989389c4\x1b[0m (\x1b[1;36mHEAD -> \x1b[1;32mmain\x1b[0m)\r\n\
                 Author: Ezra <ezra@z3rm.dev>\r\n\
                 Date:   Sun Aug 23 08:30:00 2026 +0800\r\n\n\
                 \x1b[1m    Add design spec for the real z3rm wasm client\x1b[0m\r\n"
                    .to_string()
            }
            _ => format!("git: unknown command '{sub}'\r\n"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vfs_create_read_remove_file() {
        let mut fs = VirtualFs::new();
        fs.write_file("/home/user/test.txt", "hello vfs");
        assert_eq!(fs.read_file("/home/user/test.txt").unwrap(), "hello vfs");
        fs.remove("/home/user/test.txt").unwrap();
        assert!(fs.read_file("/home/user/test.txt").is_err());
    }

    #[test]
    fn vfs_list_directory() {
        let fs = VirtualFs::new();
        let list = fs.list_dir("/home/user/z3rm").unwrap();
        let names: Vec<&str> = list.iter().map(|(n, _, _)| n.as_str()).collect();
        assert!(names.contains(&"README.md"));
        assert!(names.contains(&"Cargo.toml"));
        assert!(names.contains(&"crates"));
    }

    #[test]
    fn shell_executes_pwd_and_cd() {
        let mut shell = WasmShell::new();
        assert_eq!(shell.execute_command("pwd"), "/home/user/z3rm\r\n");
        shell.execute_command("cd crates");
        assert_eq!(shell.vfs.cwd(), "/home/user/z3rm/crates");
    }

    #[test]
    fn shell_executes_ls_and_cat() {
        let mut shell = WasmShell::new();
        let ls_out = shell.execute_command("ls");
        assert!(ls_out.contains("README.md"));
        let cat_out = shell.execute_command("cat README.md");
        assert!(cat_out.contains("z3rm"));
    }

    #[test]
    fn shell_executes_z3rm_commands() {
        let mut shell = WasmShell::new();
        let status_out = shell.execute_command("z3rm status");
        assert!(status_out.contains("Server:"));
        assert!(status_out.contains("z3rm_server"));
        let list_out = shell.execute_command("z3rm list-panes");
        assert!(list_out.contains("pane-editor"));
    }

    #[test]
    fn shell_typing_and_backspace() {
        let mut shell = WasmShell::new();
        let _ = shell.handle_input("ls\r");
        assert_eq!(shell.history, vec!["ls"]);
    }
}
