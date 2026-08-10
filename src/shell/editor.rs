//! Small full-screen ASCII editor for the serial shell.

use alloc::vec::Vec;
use core::fmt::{self, Write as _};

use crate::print;
use crate::println;
use crate::uart_rtos::UartRtosExt;

use super::{FsError, PendingRx, ShellState};

const TAB_WIDTH: usize = 4;
const ESCAPE_TIMEOUT_MS: u32 = 25;
const CRLF_TIMEOUT_MS: u32 = super::INPUT_CRLF_TIMEOUT_MS;
const INPUT_BATCH_TIMEOUT_MS: u32 = 10;
const INPUT_YIELD_INTERVAL: usize = 128;
const TERMINAL_PROBE_TIMEOUT_MS: u32 = 200;
const TERMINAL_PROBE_MAX_BYTES: usize = 128;
const CURSOR_REPORT_CAPACITY: usize = 32;
const MIN_COLUMNS: usize = 40;
const MAX_COLUMNS: usize = 240;
const MIN_ROWS: usize = 8;
const MAX_ROWS: usize = 100;
const INPUT_LOST_STATUS: &str = "INPUT LOST - save disabled; exit and reopen";

#[derive(Clone, Copy)]
struct ScreenSize {
    columns: usize,
    rows: usize,
}

#[derive(Clone, Copy)]
struct UnsupportedScreenSize {
    columns: u16,
    rows: u16,
}

impl ScreenSize {
    const fn fallback() -> Self {
        Self {
            columns: crate::config::NANO_COLUMNS,
            rows: crate::config::NANO_ROWS,
        }
    }

    fn from_cursor_report(rows: u16, columns: u16) -> Result<Self, UnsupportedScreenSize> {
        if !(MIN_ROWS..=MAX_ROWS).contains(&(rows as usize))
            || !(MIN_COLUMNS..=MAX_COLUMNS).contains(&(columns as usize))
        {
            return Err(UnsupportedScreenSize { columns, rows });
        }
        Ok(Self {
            columns: columns as usize,
            rows: rows as usize,
        })
    }

    const fn text_rows(self) -> usize {
        self.rows - 3
    }
}

enum CursorReport {
    Incomplete,
    NotReport,
    InvalidReport,
    Complete { rows: u16, columns: u16 },
}

fn parse_cursor_report(bytes: &[u8]) -> CursorReport {
    let mut index = match bytes {
        [0x1b] => return CursorReport::Incomplete,
        [0x1b, b'[', ..] => 2,
        [0x1b, ..] => return CursorReport::NotReport,
        [0x9b, ..] => 1,
        _ => return CursorReport::NotReport,
    };
    if index == bytes.len() {
        return CursorReport::Incomplete;
    }

    let Some(final_index) = bytes[index..]
        .iter()
        .position(|byte| (0x40..=0x7e).contains(byte))
        .map(|offset| index + offset)
    else {
        return CursorReport::Incomplete;
    };
    if final_index + 1 != bytes.len() || bytes[final_index] != b'R' {
        return CursorReport::NotReport;
    }

    if bytes[index] == b'?' {
        index += 1;
    }

    let mut rows = 0u16;
    let mut row_digits = 0;
    while index < final_index && bytes[index].is_ascii_digit() {
        let Some(value) = rows
            .checked_mul(10)
            .and_then(|value| value.checked_add((bytes[index] - b'0') as u16))
        else {
            return CursorReport::InvalidReport;
        };
        rows = value;
        row_digits += 1;
        index += 1;
    }
    if row_digits == 0 || bytes[index] != b';' {
        return CursorReport::InvalidReport;
    }
    index += 1;

    let mut columns = 0u16;
    let mut column_digits = 0;
    while index < final_index && bytes[index].is_ascii_digit() {
        let Some(value) = columns
            .checked_mul(10)
            .and_then(|value| value.checked_add((bytes[index] - b'0') as u16))
        else {
            return CursorReport::InvalidReport;
        };
        columns = value;
        column_digits += 1;
        index += 1;
    }
    if column_digits == 0 || index != final_index {
        return CursorReport::InvalidReport;
    }
    CursorReport::Complete { rows, columns }
}

pub(super) fn run(state: &mut ShellState, name: &str) {
    if state.filesystem.is_none() {
        super::print_mount_error(state, "nano");
        return;
    }

    let (buffer, exists, max_bytes) = {
        let filesystem = state.filesystem.as_mut().unwrap();
        match load_document(filesystem, name) {
            Ok(document) => document,
            Err(error) => {
                report_load_error(error);
                return;
            }
        }
    };

    let mut input = core::mem::replace(&mut state.pending_rx, PendingRx::new());
    let uart = crate::board::BoardResources::get().console();
    let mut input_lost = uart_input_lost(uart);
    let terminal = TerminalGuard::enter();
    let (screen, probe_input_lost) = detect_screen_size(uart, &mut input);
    input_lost |= probe_input_lost || uart_input_lost(uart);
    let screen = match screen {
        Ok(screen) => screen,
        Err(size) => {
            state.pending_rx = input;
            drop(terminal);
            println!(
                "nano: 终端尺寸 {}x{} 超出支持范围 ({}..{} 列, {}..{} 行)",
                size.columns, size.rows, MIN_COLUMNS, MAX_COLUMNS, MIN_ROWS, MAX_ROWS
            );
            return;
        }
    };

    let mut editor = Editor::new(name, buffer, exists, max_bytes, screen);
    editor.input_lost = input_lost;
    let mut keys = KeyReader::new(input);

    let outcome = 'editor: loop {
        editor.ensure_visible();
        print!("{}", Frame { editor: &editor });

        let mut key = keys.read();
        let mut processed = 0usize;
        loop {
            if uart_input_lost(uart) {
                editor.input_lost = true;
            }

            match editor.handle_key(key) {
                Action::None => {}
                Action::Save { exit_after } => {
                    editor.mode = Mode::Editing;
                    if uart_input_lost(uart) {
                        editor.input_lost = true;
                    }
                    if editor.input_lost {
                        editor.status = Some(INPUT_LOST_STATUS);
                    } else if save_document(state, &mut editor) && exit_after {
                        break 'editor ExitOutcome::Clean;
                    }
                    break;
                }
                Action::Exit { discarded } => {
                    break 'editor if discarded {
                        ExitOutcome::Discarded
                    } else {
                        ExitOutcome::Clean
                    };
                }
            }

            if uart_input_lost(uart) {
                editor.input_lost = true;
            }
            processed += 1;
            if crate::config::WDT_ENABLE && processed.is_multiple_of(INPUT_YIELD_INTERVAL) {
                crate::rtos::thread_delay_ms(1);
            }
            let Some(next) = keys.read_timeout(INPUT_BATCH_TIMEOUT_MS) else {
                break;
            };
            key = next;
        }
    };

    state.pending_rx = keys.into_pending();
    drop(terminal);
    if outcome == ExitOutcome::Discarded {
        println!("nano: 未保存的修改已放弃");
    }
}

fn uart_input_lost<const U: u8>(uart: &crate::uart::Uart<U>) -> bool {
    let dropped = uart.rx_dropped_count();
    let (parity, framing, overrun) = uart.rx_error_counts();
    dropped != 0 || parity != 0 || framing != 0 || overrun != 0
}

fn replay_bytes(input: &mut PendingRx, bytes: &[u8]) -> bool {
    let mut lost = false;
    for &byte in bytes {
        if !input.push_back(byte) {
            lost = true;
        }
    }
    lost
}

fn detect_screen_size<const U: u8>(
    uart: &crate::uart::Uart<U>,
    input: &mut PendingRx,
) -> (Result<ScreenSize, UnsupportedScreenSize>, bool) {
    let mut input_lost = false;
    while !input.is_full() {
        let Some(byte) = uart.read_rx() else {
            break;
        };
        let queued = input.push_back(byte);
        debug_assert!(queued);
    }
    if input.remaining_capacity() < CURSOR_REPORT_CAPACITY {
        return (Ok(ScreenSize::fallback()), false);
    }

    print!("\x1b[?25l\x1b[?6l\x1b[r\x1b[999;999H\x1b[6n");
    uart.flush();

    let started = crate::rtos::uptime_ms();
    let mut candidate = [0u8; CURSOR_REPORT_CAPACITY];
    let mut candidate_len = 0;
    let mut detected = None;

    for _ in 0..TERMINAL_PROBE_MAX_BYTES {
        if candidate_len == candidate.len() {
            input_lost = true;
            input_lost |= replay_bytes(input, &candidate[..candidate_len]);
            candidate_len = 0;
            break;
        }
        if candidate_len == 0 && input.remaining_capacity() < CURSOR_REPORT_CAPACITY {
            break;
        }
        let elapsed = crate::rtos::uptime_ms().wrapping_sub(started);
        if elapsed >= TERMINAL_PROBE_TIMEOUT_MS {
            break;
        }
        let Some(byte) = uart.read_rx_timeout_ms(TERMINAL_PROBE_TIMEOUT_MS - elapsed) else {
            break;
        };

        if candidate_len == 0 {
            if matches!(byte, 0x1b | 0x9b) {
                candidate[0] = byte;
                candidate_len = 1;
            } else {
                let queued = input.push_back(byte);
                debug_assert!(queued);
            }
            continue;
        }

        candidate[candidate_len] = byte;
        candidate_len += 1;

        match parse_cursor_report(&candidate[..candidate_len]) {
            CursorReport::Incomplete => {}
            CursorReport::Complete { rows, columns } => {
                detected = Some(ScreenSize::from_cursor_report(rows, columns));
                candidate_len = 0;
                break;
            }
            CursorReport::InvalidReport => {
                detected = Some(Ok(ScreenSize::fallback()));
                candidate_len = 0;
                break;
            }
            CursorReport::NotReport => {
                let restart = matches!(byte, 0x1b | 0x9b);
                let replay_len = candidate_len - usize::from(restart);
                input_lost |= replay_bytes(input, &candidate[..replay_len]);
                if restart && input.remaining_capacity() >= CURSOR_REPORT_CAPACITY {
                    candidate[0] = byte;
                    candidate_len = 1;
                } else {
                    if restart {
                        let queued = input.push_back(byte);
                        debug_assert!(queued);
                    }
                    candidate_len = 0;
                }
            }
        }
    }

    if candidate_len != 0 {
        // Preserve the prefix so KeyReader can consume a late CPR tail, but
        // prohibit saving because an indefinitely delayed tail is ambiguous.
        input_lost = true;
        input_lost |= replay_bytes(input, &candidate[..candidate_len]);
    }
    print!("\x1b[H");
    (
        detected.unwrap_or_else(|| Ok(ScreenSize::fallback())),
        input_lost,
    )
}

enum LoadError {
    Filesystem(FsError),
    TooLarge { size: usize, limit: usize },
    NotAscii { offset: usize },
    OutOfMemory,
    ShortRead,
}

fn report_load_error(error: LoadError) {
    match error {
        LoadError::Filesystem(error) => super::print_fs_error("nano", &error),
        LoadError::TooLarge { size, limit } => {
            println!("nano: 文件大小 {} B，超过编辑上限 {} B", size, limit)
        }
        LoadError::NotAscii { offset } => println!(
            "nano: 仅支持 ASCII 文本，文件在偏移 {} 含不支持的字节",
            offset
        ),
        LoadError::OutOfMemory => println!("nano: 无法分配编辑缓冲区"),
        LoadError::ShortRead => println!("nano: 文件读取提前结束"),
    }
}

fn load_document(
    filesystem: &mut crate::filesystem::FileSystem,
    name: &str,
) -> Result<(Vec<u8>, bool, usize), LoadError> {
    filesystem.verify().map_err(LoadError::Filesystem)?;
    let max_bytes = crate::config::NANO_MAX_BYTES.min(
        filesystem
            .max_write_size(name)
            .map_err(LoadError::Filesystem)? as usize,
    );
    let (size, exists) = match filesystem.stat(name) {
        Ok(info) => (info.size as usize, true),
        Err(littlefs::Error::NotFound) => (0, false),
        Err(error) => return Err(LoadError::Filesystem(error)),
    };
    if size > max_bytes {
        return Err(LoadError::TooLarge {
            size,
            limit: max_bytes,
        });
    }

    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(max_bytes)
        .map_err(|_| LoadError::OutOfMemory)?;
    let mut chunk = [0u8; 128];
    let mut offset = 0;
    while offset < size {
        let requested = (size - offset).min(chunk.len());
        let read = filesystem
            .read(name, offset as u32, &mut chunk[..requested])
            .map_err(LoadError::Filesystem)?;
        if read == 0 {
            return Err(LoadError::ShortRead);
        }
        buffer.extend_from_slice(&chunk[..read]);
        offset += read;
    }

    if let Some(offset) = buffer.iter().position(|byte| !is_editable_byte(*byte)) {
        return Err(LoadError::NotAscii { offset });
    }
    Ok((buffer, exists, max_bytes))
}

fn is_editable_byte(byte: u8) -> bool {
    matches!(byte, b'\n' | b'\t' | 0x20..=0x7e)
}

fn save_document(state: &mut ShellState, editor: &mut Editor<'_>) -> bool {
    if !editor.modified && editor.exists {
        editor.status = Some("No changes to save");
        return true;
    }

    if state.filesystem.is_none() && !state.remount() {
        editor.status = Some("Filesystem unavailable; buffer kept");
        return false;
    }

    let result = state
        .filesystem
        .as_mut()
        .unwrap()
        .write(editor.name, &editor.buffer);
    match result {
        Ok(()) => {
            editor.mark_saved("Saved atomically and verified");
            true
        }
        Err(error) => {
            let recovery_required = state
                .filesystem
                .as_ref()
                .is_some_and(littlefs::FileSystem::recovery_required);
            if !recovery_required {
                editor.status = Some(save_error_message(&error));
                return false;
            }

            if !state.remount() {
                editor.status = Some("Save uncertain; remount failed; buffer kept");
                return false;
            }

            let matches = durable_file_matches(
                state.filesystem.as_mut().unwrap(),
                editor.name,
                &editor.buffer,
            );
            match matches {
                Ok(true) => {
                    editor.mark_saved("Save confirmed after recovery");
                    true
                }
                Ok(false) => {
                    editor.status = Some("Old version recovered; buffer kept; retry ^O");
                    false
                }
                Err(_) => {
                    editor.status = Some("Remounted but could not verify file; buffer kept");
                    false
                }
            }
        }
    }
}

fn save_error_message(error: &FsError) -> &'static str {
    match error {
        littlefs::Error::NoSpace => "No space; buffer kept",
        littlefs::Error::InvalidName => "Invalid filename; buffer kept",
        littlefs::Error::FileTooLarge => "File too large; buffer kept",
        littlefs::Error::NotFormatted => "Filesystem is not formatted; buffer kept",
        littlefs::Error::Corrupt => "Filesystem is corrupt; buffer kept",
        littlefs::Error::DeviceBusy => "Flash is busy; buffer kept",
        littlefs::Error::Device(_) => "Flash device error; buffer kept",
        littlefs::Error::RecoveryRequired => "Remount required; buffer kept",
        littlefs::Error::NotFound | littlefs::Error::AlreadyExists => {
            "Filesystem state changed; buffer kept"
        }
        littlefs::Error::InvalidGeometry => "Invalid filesystem geometry; buffer kept",
    }
}

fn durable_file_matches(
    filesystem: &mut crate::filesystem::FileSystem,
    name: &str,
    expected: &[u8],
) -> Result<bool, FsError> {
    let info = match filesystem.stat(name) {
        Ok(info) => info,
        Err(littlefs::Error::NotFound) => return Ok(false),
        Err(error) => return Err(error),
    };
    if info.size as usize != expected.len() {
        return Ok(false);
    }

    let mut chunk = [0u8; 128];
    let mut offset = 0;
    while offset < expected.len() {
        let requested = (expected.len() - offset).min(chunk.len());
        let read = filesystem.read(name, offset as u32, &mut chunk[..requested])?;
        if read != requested || chunk[..read] != expected[offset..offset + read] {
            return Ok(false);
        }
        offset += read;
    }
    Ok(true)
}

struct TerminalGuard {
    logs_were_enabled: bool,
}

impl TerminalGuard {
    fn enter() -> Self {
        let logs_were_enabled = crate::log::enabled();
        crate::log::set_enabled(false);
        print!("\x1b[?1049h\x1b[2J\x1b[H");
        Self { logs_were_enabled }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        print!("\x1b[0m\x1b[?25h\x1b[?1049l");
        crate::log::set_enabled(self.logs_were_enabled);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExitOutcome {
    Clean,
    Discarded,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Editing,
    ConfirmExit,
}

enum Action {
    None,
    Save { exit_after: bool },
    Exit { discarded: bool },
}

struct Editor<'a> {
    name: &'a str,
    buffer: Vec<u8>,
    cursor: usize,
    top_line: usize,
    left_column: usize,
    goal_column: Option<usize>,
    modified: bool,
    exists: bool,
    input_lost: bool,
    max_bytes: usize,
    screen: ScreenSize,
    status: Option<&'static str>,
    mode: Mode,
}

impl<'a> Editor<'a> {
    fn new(
        name: &'a str,
        buffer: Vec<u8>,
        exists: bool,
        max_bytes: usize,
        screen: ScreenSize,
    ) -> Self {
        Self {
            name,
            buffer,
            cursor: 0,
            top_line: 0,
            left_column: 0,
            goal_column: None,
            modified: false,
            exists,
            input_lost: false,
            max_bytes,
            screen,
            status: if exists {
                Some("ASCII text mode")
            } else {
                Some("New file; ^O writes it to Flash")
            },
            mode: Mode::Editing,
        }
    }

    fn mark_saved(&mut self, status: &'static str) {
        self.modified = false;
        self.exists = true;
        self.status = Some(status);
    }

    fn handle_key(&mut self, key: Key) -> Action {
        if self.mode == Mode::ConfirmExit {
            return match key {
                Key::Char(b'y' | b'Y') => Action::Save { exit_after: true },
                Key::Char(b'n' | b'N') => Action::Exit { discarded: true },
                Key::Ctrl(0x03) | Key::Escape => {
                    self.mode = Mode::Editing;
                    self.status = Some("Exit cancelled");
                    Action::None
                }
                _ => {
                    self.status = Some("Save modified buffer? Y Yes  N No  ^C Cancel");
                    Action::None
                }
            };
        }

        match key {
            Key::Ctrl(0x0f) => Action::Save { exit_after: false },
            Key::Ctrl(0x18) => {
                if self.modified {
                    self.mode = Mode::ConfirmExit;
                    self.status = Some("Save modified buffer? Y Yes  N No  ^C Cancel");
                    Action::None
                } else {
                    Action::Exit { discarded: false }
                }
            }
            Key::Ctrl(0x07) => {
                self.status = Some("Arrows move | ^O save | ^X exit | ^C position");
                Action::None
            }
            Key::Ctrl(0x03) => {
                self.status = None;
                Action::None
            }
            Key::Ctrl(0x01) | Key::Home => {
                self.move_home();
                Action::None
            }
            Key::Ctrl(0x05) | Key::End => {
                self.move_end();
                Action::None
            }
            Key::Ctrl(0x02) | Key::Left => {
                self.move_left();
                Action::None
            }
            Key::Ctrl(0x06) | Key::Right => {
                self.move_right();
                Action::None
            }
            Key::Ctrl(0x10) | Key::Up => {
                self.move_up();
                Action::None
            }
            Key::Ctrl(0x0e) | Key::Down => {
                self.move_down();
                Action::None
            }
            Key::Ctrl(0x19) | Key::PageUp => {
                self.page_up();
                Action::None
            }
            Key::Ctrl(0x16) | Key::PageDown => {
                self.page_down();
                Action::None
            }
            Key::Ctrl(0x04) | Key::Delete => {
                self.delete();
                Action::None
            }
            Key::Backspace => {
                self.backspace();
                Action::None
            }
            Key::Enter => {
                self.insert(b'\n');
                Action::None
            }
            Key::Tab => {
                self.insert(b'\t');
                Action::None
            }
            Key::Char(byte) => {
                self.insert(byte);
                Action::None
            }
            Key::Ctrl(0x0c) => {
                self.status = None;
                Action::None
            }
            Key::CursorReport => Action::None,
            Key::Escape => {
                self.status = Some("Escape");
                Action::None
            }
            Key::Ctrl(_) | Key::Unknown => {
                self.status = Some("Unsupported key; ^G for help");
                Action::None
            }
        }
    }

    fn insert(&mut self, byte: u8) {
        if self.buffer.len() >= self.max_bytes {
            self.status = Some("Buffer limit reached; byte not inserted");
            return;
        }
        self.buffer.insert(self.cursor, byte);
        self.cursor += 1;
        self.changed();
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        self.buffer.remove(self.cursor);
        self.changed();
    }

    fn delete(&mut self) {
        if self.cursor >= self.buffer.len() {
            return;
        }
        self.buffer.remove(self.cursor);
        self.changed();
    }

    fn changed(&mut self) {
        self.modified = true;
        self.goal_column = None;
        self.status = None;
    }

    fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
        self.goal_column = None;
    }

    fn move_right(&mut self) {
        if self.cursor < self.buffer.len() {
            self.cursor += 1;
        }
        self.goal_column = None;
    }

    fn move_home(&mut self) {
        self.cursor = line_start(&self.buffer, self.cursor);
        self.goal_column = None;
    }

    fn move_end(&mut self) {
        self.cursor = line_end(&self.buffer, self.cursor);
        self.goal_column = None;
    }

    fn move_up(&mut self) {
        let start = line_start(&self.buffer, self.cursor);
        if start == 0 {
            return;
        }
        let goal = self
            .goal_column
            .unwrap_or_else(|| cursor_column(&self.buffer, self.cursor));
        let previous_end = start - 1;
        let previous_start = line_start(&self.buffer, previous_end);
        self.cursor = offset_for_column(&self.buffer, previous_start, previous_end, goal);
        self.goal_column = Some(goal);
    }

    fn move_down(&mut self) {
        let end = line_end(&self.buffer, self.cursor);
        if end == self.buffer.len() {
            return;
        }
        let goal = self
            .goal_column
            .unwrap_or_else(|| cursor_column(&self.buffer, self.cursor));
        let next_start = end + 1;
        let next_end = line_end(&self.buffer, next_start);
        self.cursor = offset_for_column(&self.buffer, next_start, next_end, goal);
        self.goal_column = Some(goal);
    }

    fn page_up(&mut self) {
        for _ in 0..self.screen.text_rows() {
            self.move_up();
        }
    }

    fn page_down(&mut self) {
        for _ in 0..self.screen.text_rows() {
            self.move_down();
        }
    }

    fn ensure_visible(&mut self) {
        let (line, column) = cursor_position(&self.buffer, self.cursor);
        let text_rows = self.screen.text_rows();
        if line < self.top_line {
            self.top_line = line;
        } else if line >= self.top_line + text_rows {
            self.top_line = line - text_rows + 1;
        }
        if column < self.left_column {
            self.left_column = column;
        } else if column >= self.left_column + self.screen.columns {
            self.left_column = column - self.screen.columns + 1;
        }
    }
}

fn line_start(buffer: &[u8], cursor: usize) -> usize {
    buffer[..cursor]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |offset| offset + 1)
}

fn line_end(buffer: &[u8], cursor: usize) -> usize {
    buffer[cursor..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(buffer.len(), |offset| cursor + offset)
}

fn cursor_position(buffer: &[u8], cursor: usize) -> (usize, usize) {
    let line = buffer[..cursor]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count();
    (line, cursor_column(buffer, cursor))
}

fn cursor_column(buffer: &[u8], cursor: usize) -> usize {
    let start = line_start(buffer, cursor);
    visual_width(&buffer[start..cursor])
}

fn visual_width(bytes: &[u8]) -> usize {
    let mut column = 0;
    for byte in bytes {
        column = if *byte == b'\t' {
            ((column / TAB_WIDTH) + 1) * TAB_WIDTH
        } else {
            column + 1
        };
    }
    column
}

fn offset_for_column(buffer: &[u8], start: usize, end: usize, target: usize) -> usize {
    let mut offset = start;
    let mut column = 0;
    while offset < end {
        let next = if buffer[offset] == b'\t' {
            ((column / TAB_WIDTH) + 1) * TAB_WIDTH
        } else {
            column + 1
        };
        if next > target {
            break;
        }
        column = next;
        offset += 1;
    }
    offset
}

fn total_lines(buffer: &[u8]) -> usize {
    1 + buffer.iter().filter(|byte| **byte == b'\n').count()
}

fn line_offset(buffer: &[u8], line: usize) -> Option<usize> {
    if line == 0 {
        return Some(0);
    }
    let mut current = 0;
    for (offset, byte) in buffer.iter().enumerate() {
        if *byte == b'\n' {
            current += 1;
            if current == line {
                return Some(offset + 1);
            }
        }
    }
    None
}

struct Frame<'frame, 'name> {
    editor: &'frame Editor<'name>,
}

impl fmt::Display for Frame<'_, '_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let editor = self.editor;
        let (cursor_line, cursor_column) = cursor_position(&editor.buffer, editor.cursor);
        let columns = editor.screen.columns;
        let rows = editor.screen.rows;
        let text_rows = editor.screen.text_rows();

        write!(formatter, "\x1b[?25l\x1b[1;1H\x1b[7m\x1b[2K")?;
        let prefix = " nano-rs  ";
        let suffix = if editor.modified {
            "  [Modified]"
        } else if !editor.exists {
            "  [New File]"
        } else {
            ""
        };
        write!(formatter, "{}", prefix)?;
        let name_width = columns.saturating_sub(prefix.len() + suffix.len());
        write_ascii_clipped(formatter, editor.name, name_width)?;
        write!(formatter, "{}\x1b[0m", suffix)?;

        let mut current_line = line_offset(&editor.buffer, editor.top_line);
        for screen_row in 0..text_rows {
            write!(formatter, "\x1b[{};1H\x1b[2K", screen_row + 2)?;
            let Some(start) = current_line else {
                write!(formatter, "~")?;
                continue;
            };
            let end = editor.buffer[start..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(editor.buffer.len(), |offset| start + offset);
            render_line(
                formatter,
                &editor.buffer[start..end],
                editor.left_column,
                columns,
            )?;
            current_line = if end < editor.buffer.len() {
                Some(end + 1)
            } else {
                None
            };
        }

        write!(formatter, "\x1b[{};1H\x1b[7m\x1b[2K", rows - 1)?;
        if editor.mode == Mode::ConfirmExit {
            write_ascii_clipped(
                formatter,
                "Save modified buffer? Y Yes  N No  ^C Cancel",
                columns,
            )?;
        } else if editor.input_lost {
            write_ascii_clipped(formatter, INPUT_LOST_STATUS, columns)?;
        } else if let Some(status) = editor.status {
            write_ascii_clipped(formatter, status, columns)?;
        } else {
            let mut clipped = ClippedWriter {
                formatter,
                remaining: columns,
            };
            write!(
                clipped,
                "{} / {} B | Line {} / {}, Col {} | ASCII",
                editor.buffer.len(),
                editor.max_bytes,
                cursor_line + 1,
                total_lines(&editor.buffer),
                cursor_column + 1
            )?;
        }
        write!(formatter, "\x1b[0m\x1b[{};1H\x1b[2K", rows)?;
        write_ascii_clipped(
            formatter,
            "^G Help  ^O Write Out  ^X Exit  ^C Position  ^Y Prev  ^V Next",
            columns,
        )?;

        let screen_row = cursor_line - editor.top_line + 2;
        let screen_column = cursor_column - editor.left_column + 1;
        write!(formatter, "\x1b[{};{}H\x1b[?25h", screen_row, screen_column)
    }
}

struct ClippedWriter<'writer, 'formatter> {
    formatter: &'writer mut fmt::Formatter<'formatter>,
    remaining: usize,
}

impl fmt::Write for ClippedWriter<'_, '_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let length = text.len().min(self.remaining);
        self.formatter.write_str(&text[..length])?;
        self.remaining -= length;
        Ok(())
    }
}

fn write_ascii_clipped(
    formatter: &mut fmt::Formatter<'_>,
    text: &str,
    width: usize,
) -> fmt::Result {
    for byte in text.bytes().take(width) {
        write!(formatter, "{}", byte as char)?;
    }
    Ok(())
}

fn render_line(
    formatter: &mut fmt::Formatter<'_>,
    line: &[u8],
    left_column: usize,
    width: usize,
) -> fmt::Result {
    let mut visual_column = 0;
    let mut written = 0;
    for byte in line {
        if *byte == b'\t' {
            let next_tab = ((visual_column / TAB_WIDTH) + 1) * TAB_WIDTH;
            while visual_column < next_tab {
                if visual_column >= left_column && written < width {
                    write!(formatter, " ")?;
                    written += 1;
                }
                visual_column += 1;
            }
        } else {
            if visual_column >= left_column && written < width {
                write!(formatter, "{}", *byte as char)?;
                written += 1;
            }
            visual_column += 1;
        }
        if written == width {
            break;
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Key {
    Char(u8),
    Ctrl(u8),
    Enter,
    Tab,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    CursorReport,
    Escape,
    Unknown,
}

struct KeyReader {
    input: PendingRx,
}

impl KeyReader {
    const fn new(input: PendingRx) -> Self {
        Self { input }
    }

    fn read(&mut self) -> Key {
        let byte = self.read_byte();
        self.decode(byte)
    }

    fn read_timeout(&mut self, timeout_ms: u32) -> Option<Key> {
        let byte = self.read_byte_timeout(timeout_ms)?;
        Some(self.decode(byte))
    }

    fn into_pending(self) -> PendingRx {
        self.input
    }

    fn decode(&mut self, byte: u8) -> Key {
        match byte {
            b'\r' => {
                if let Some(next) = self.read_byte_timeout(CRLF_TIMEOUT_MS)
                    && next != b'\n'
                {
                    let queued = self.input.push_front(next);
                    debug_assert!(queued);
                }
                Key::Enter
            }
            b'\n' => Key::Enter,
            b'\t' => Key::Tab,
            0x08 | 0x7f => Key::Backspace,
            0x1b => self.read_escape(),
            0x9b => self.read_csi(),
            0x20..=0x7e => Key::Char(byte),
            0x00..=0x1f => Key::Ctrl(byte),
            _ => Key::Unknown,
        }
    }

    fn read_escape(&mut self) -> Key {
        let Some(next) = self.read_byte_timeout(ESCAPE_TIMEOUT_MS) else {
            return Key::Escape;
        };
        match next {
            b'[' => self.read_csi(),
            b'O' => match self.read_byte_timeout(ESCAPE_TIMEOUT_MS) {
                Some(b'A') => Key::Up,
                Some(b'B') => Key::Down,
                Some(b'C') => Key::Right,
                Some(b'D') => Key::Left,
                Some(b'H') => Key::Home,
                Some(b'F') => Key::End,
                _ => Key::Unknown,
            },
            byte => {
                let queued = self.input.push_front(byte);
                debug_assert!(queued);
                Key::Escape
            }
        }
    }

    fn read_csi(&mut self) -> Key {
        let mut parameters = [0u16; 3];
        let mut present = [false; 3];
        let mut parameter = 0usize;
        let mut malformed = false;

        for index in 0..16 {
            let Some(byte) = self.read_byte_timeout(ESCAPE_TIMEOUT_MS) else {
                return Key::Unknown;
            };
            if (0x40..=0x7e).contains(&byte) {
                if byte == b'R' {
                    return Key::CursorReport;
                }
                if malformed {
                    return Key::Unknown;
                }
                return match byte {
                    b'A' => Key::Up,
                    b'B' => Key::Down,
                    b'C' => Key::Right,
                    b'D' => Key::Left,
                    b'H' => Key::Home,
                    b'F' => Key::End,
                    b'~' if present[0] => match parameters[0] {
                        1 | 7 => Key::Home,
                        3 => Key::Delete,
                        4 | 8 => Key::End,
                        5 => Key::PageUp,
                        6 => Key::PageDown,
                        _ => Key::Unknown,
                    },
                    _ => Key::Unknown,
                };
            }

            match byte {
                b'0'..=b'9' if !malformed => {
                    let value = parameters[parameter]
                        .checked_mul(10)
                        .and_then(|value| value.checked_add((byte - b'0') as u16));
                    if let Some(value) = value {
                        parameters[parameter] = value;
                        present[parameter] = true;
                    } else {
                        malformed = true;
                    }
                }
                b';' if !malformed => {
                    if parameter + 1 < parameters.len() {
                        parameter += 1;
                    } else {
                        malformed = true;
                    }
                }
                b'?' if index == 0 => {}
                _ => malformed = true,
            }
        }

        match self.discard_csi_tail() {
            Some(b'R') => Key::CursorReport,
            _ => Key::Unknown,
        }
    }

    fn discard_csi_tail(&mut self) -> Option<u8> {
        for _ in 0..32 {
            let byte = self.read_byte_timeout(ESCAPE_TIMEOUT_MS)?;
            if (0x40..=0x7e).contains(&byte) {
                return Some(byte);
            }
        }
        None
    }

    fn read_byte(&mut self) -> u8 {
        self.input.pop_front().unwrap_or_else(|| {
            crate::board::BoardResources::get()
                .console()
                .read_rx_blocking()
        })
    }

    fn read_byte_timeout(&mut self, timeout_ms: u32) -> Option<u8> {
        self.input.pop_front().or_else(|| {
            crate::board::BoardResources::get()
                .console()
                .read_rx_timeout_ms(timeout_ms)
        })
    }
}
