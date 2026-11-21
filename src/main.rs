mod color;
mod engine;
mod folders;
mod highlight;
mod pty;
mod theme;

use std::{cell::RefCell, io::Read, os::fd::AsRawFd, rc::Rc, sync::mpsc::TryRecvError};

use std::{
    ops::Range,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use adw::prelude::*;
use gtk::{gdk, gio, glib, pango};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use color::{Color32, PROMPT_BADGE, PROMPT_DIRECTORY, PROMPT_DIRECTORY_ALT, PROMPT_JOIN};
use engine::{BG, FG, Point, Terminal};
use folders::FolderIcon;
use pty::{LaunchOptions, Session};
use theme::Theme;

const PREVIEW: &[u8] = b"\x1b[27;5;32~";
const ACCEPT: &[u8] = b"\x1b[27;5;33~";
const PREPARE_ELEVATED: &[u8] = b"\x1b[27;6;13~";
const NAVIGATION_CHECK: &[u8] = b"\x1b[27;5;34~";
const INPUT_BEGIN: &[u8] = b"\x1b[27;5;40~";
const INPUT_FORWARD: &[u8] = b"\x1b[27;5;41~";
const INPUT_DELETE: &[u8] = b"\x1b[27;5;42~";
const INPUT_CLEAR: &[u8] = b"\x1b[27;5;40~\x1b[27;5;43~";

enum InputAction {
    SelectAll,
    Delete(Point, Point),
    MoveCaret { point: Point, fraction: f64 },
}

enum Navigation {
    Idle,
    Checking(PathBuf),
    Canceled,
}

#[derive(Debug, Eq, PartialEq)]
struct Viewport {
    first: usize,
    end: usize,
    blank_above: usize,
    pinned: Range<usize>,
    pinned_slot: usize,
}

impl Viewport {
    fn for_terminal(terminal: &Terminal, scroll: Option<usize>) -> Self {
        let rows = terminal.rows;
        let bottom = terminal.display_bottom();
        let oldest = terminal.oldest();
        let live_first = bottom.saturating_sub(rows - 1).max(oldest);
        let first = scroll.unwrap_or(live_first).clamp(oldest, live_first);
        if first < live_first
            && let Some(head) = terminal.live_prompt_start()
        {
            let head = head.clamp(oldest, bottom);
            let start = head
                .max((bottom + 1).saturating_sub(rows - 1))
                .min(terminal.cursor().0);
            let pinned = start..(bottom + 1).min(start + rows - 1);
            let pinned_slot = rows - pinned.len();
            return Self {
                first,
                end: (first + pinned_slot).min(head),
                blank_above: 0,
                pinned,
                pinned_slot,
            };
        }
        let end = (first + rows).min(bottom + 1);
        Self {
            first,
            end,
            blank_above: rows - (end - first),
            pinned: 0..0,
            pinned_slot: rows,
        }
    }

    fn rows(&self) -> impl Iterator<Item = usize> + '_ {
        (self.first..self.end).chain(self.pinned.clone())
    }

    fn contains(&self, row: usize) -> bool {
        (self.first..self.end).contains(&row) || self.pinned.contains(&row)
    }

    fn display_row(&self, row: usize) -> usize {
        if self.pinned.contains(&row) {
            self.pinned_slot + row - self.pinned.start
        } else {
            self.blank_above + row.saturating_sub(self.first)
        }
    }

    fn terminal_row(&self, display: usize) -> usize {
        if !self.pinned.is_empty() && display >= self.pinned_slot {
            return (self.pinned.start + display - self.pinned_slot).min(self.pinned.end - 1);
        }
        (self.first + display.saturating_sub(self.blank_above)).min(self.end.saturating_sub(1))
    }

    fn selection_edge(&self, anchor: usize, target: usize) -> Option<usize> {
        match (self.pinned.contains(&anchor), self.pinned.contains(&target)) {
            (false, true) => self.end.checked_sub(1),
            (true, false) => Some(self.pinned.start),
            _ => None,
        }
    }
}

struct PreviewFrame {
    bytes: Vec<u8>,
    started: Instant,
    last_input: Instant,
    revision: u64,
}

impl PreviewFrame {
    fn input_offset(&self) -> Option<usize> {
        let reply = b"\x1b]133;I;";
        self.bytes
            .windows(reply.len())
            .position(|chunk| chunk == reply)
    }

    fn complete(&self, now: Instant) -> bool {
        const PROMPT_END: &[u8] = b"\x1b]133;B\x07";
        self.bytes.len() >= 256 * 1024
            || now.duration_since(self.started) >= Duration::from_millis(340)
            || (now.duration_since(self.last_input) >= Duration::from_millis(12)
                && self.input_offset().is_some_and(|offset| {
                    self.bytes[offset..]
                        .windows(PROMPT_END.len())
                        .any(|chunk| chunk == PROMPT_END)
                }))
    }
}

#[derive(Clone, Copy, Default)]
struct PasswordMask {
    anchor: Option<Point>,
    count: usize,
}

impl PasswordMask {
    fn track(&mut self, prompt: Option<Point>) {
        if self.anchor != prompt {
            self.anchor = prompt;
            self.count = 0;
        }
    }
}

struct State {
    navigation: Navigation,
    navigation_blocked: bool,
    terminal: Terminal,
    parser: vte::Parser,
    session: Option<Session>,
    selection: Option<(Point, Point)>,
    selection_drag: Option<(Point, (f64, f64))>,
    top: Option<usize>,
    font_size: f64,
    cell: (f64, f64),
    grid_top: f64,
    suggestion_index: usize,
    suggestion_navigated: bool,
    preview_due: Option<Instant>,
    preview_frame: Option<PreviewFrame>,
    input_revision: u64,
    input_action: Option<InputAction>,
    password_mask: PasswordMask,
}

impl State {
    fn new(launch: &LaunchOptions) -> (Self, Option<String>) {
        let (session, error) = match Session::spawn(100, 30, launch) {
            Ok(session) => (Some(session), None),
            Err(error) => (
                None,
                Some(format!("Could not start the terminal session: {error}")),
            ),
        };
        (
            Self {
                navigation: Navigation::Idle,
                navigation_blocked: false,
                terminal: Terminal::new(100, 30),
                parser: vte::Parser::new(),
                session,
                selection: None,
                selection_drag: None,
                top: None,
                font_size: 14.0,
                cell: (9.0, 20.0),
                grid_top: 10.0,
                suggestion_index: 0,
                suggestion_navigated: false,
                preview_due: None,
                preview_frame: None,
                input_revision: 0,
                input_action: None,
                password_mask: PasswordMask::default(),
            },
            error,
        )
    }

    fn preview_pending(&self) -> bool {
        self.preview_due.is_some() || self.preview_frame.is_some()
    }

    fn read_output(&mut self) -> bool {
        let mut closed = false;
        let was_alternate = self.terminal.alternate();
        let generation = self.terminal.prompt_generation;
        if let Some(session) = &self.session {
            for _ in 0..64 {
                match session.output.try_recv() {
                    Ok(bytes) => {
                        if let Some(frame) = &mut self.preview_frame {
                            frame.bytes.extend(bytes);
                            frame.last_input = Instant::now();
                        } else {
                            self.parser.advance(&mut self.terminal, &bytes);
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        closed = true;
                        break;
                    }
                }
            }
        }
        let now = Instant::now();
        if self
            .preview_frame
            .as_ref()
            .is_some_and(|frame| closed || frame.complete(now))
            && let Some(frame) = self.preview_frame.take()
        {
            let received_input = frame.input_offset().is_some();
            self.parser.advance(&mut self.terminal, &frame.bytes);
            if frame.revision != self.input_revision {
                self.clear_suggestions();
            } else if let Some(action) = self.input_action.take()
                && received_input
                && self.prompt()
            {
                self.apply_input_action(action);
            }
        }
        if let Some(session) = &self.session
            && !self.terminal.replies.is_empty()
        {
            session.send(std::mem::take(&mut self.terminal.replies));
        }
        if was_alternate != self.terminal.alternate() {
            self.top = None;
            self.selection = None;
        }
        if let Some(ready) = self.terminal.navigation_ready.take()
            && let Navigation::Checking(path) =
                std::mem::replace(&mut self.navigation, Navigation::Idle)
        {
            if ready && self.prompt() {
                let quoted = path.to_string_lossy().replace('\'', "'\\''");
                self.edit(format!("builtin cd -- '{quoted}'\r").into_bytes(), false);
            } else if !ready {
                self.navigation_blocked = true;
            }
        }
        if !self.terminal.at_prompt || generation != self.terminal.prompt_generation {
            self.navigation = Navigation::Idle;
        }
        if self.preview_frame.is_none() && self.preview_due.is_some_and(|deadline| now >= deadline)
        {
            self.preview_due = None;
            if !closed && self.prompt() && self.send(PREVIEW) {
                self.preview_frame = Some(PreviewFrame {
                    bytes: Vec::new(),
                    started: now,
                    last_input: now,
                    revision: self.input_revision,
                });
            }
        }
        closed
    }

    fn ssh_foreground(&self) -> bool {
        self.session
            .as_ref()
            .and_then(Session::foreground_program)
            .as_deref()
            == Some("ssh")
    }

    fn prompt(&self) -> bool {
        self.terminal.at_prompt
            && !self.terminal.alternate()
            && self
                .session
                .as_ref()
                .is_some_and(|s| s.integrated && s.shell_foreground())
    }

    fn send(&mut self, bytes: impl Into<Vec<u8>>) -> bool {
        self.session
            .as_ref()
            .is_some_and(|session| session.send(bytes.into()))
    }

    fn edit(&mut self, bytes: impl Into<Vec<u8>>, preview: bool) {
        if matches!(self.navigation, Navigation::Checking(_)) {
            self.navigation = Navigation::Canceled;
        }
        self.selection = None;
        self.clear_suggestions();
        self.input_revision = self.input_revision.wrapping_add(1);
        self.input_action = None;
        let bytes = bytes.into();
        let at_prompt = self.prompt();
        let show_live = !at_prompt || bytes.last() == Some(&b'\r');
        self.preview_due = None;
        if self.send(bytes) {
            if show_live {
                self.top = None;
                self.terminal.input.clear();
            } else if preview {
                self.preview_due = Some(Instant::now() + Duration::from_millis(85));
            }
        }
    }

    fn request_input_action(&mut self, action: InputAction) {
        if !self.prompt() {
            return;
        }
        self.clear_suggestions();
        self.input_action = Some(action);
        if !self
            .preview_frame
            .as_ref()
            .is_some_and(|frame| frame.revision == self.input_revision)
        {
            self.preview_due = Some(Instant::now());
        }
    }

    fn click_input(&mut self, x: f64, y: f64) {
        let Some(start) = self.terminal.input_start else {
            return;
        };
        let point = self.point(x, y);
        let display = self.viewport().display_row(point.0);
        let y0 = self.grid_top + display as f64 * self.cell.1;
        if point >= start
            && y >= y0
            && y < y0 + self.cell.1
            && x >= 12.0
            && x < 12.0 + self.terminal.cols as f64 * self.cell.0
        {
            self.request_input_action(InputAction::MoveCaret {
                point,
                fraction: ((x - 12.0) / self.cell.0).fract(),
            });
        }
    }

    fn apply_input_action(&mut self, action: InputAction) {
        let Some(start) = self.terminal.input_start else {
            return;
        };
        let text = &self.terminal.input;
        let spans = input_spans(text, start, self.terminal.cols);
        let Some(last) = spans.last().map(|(_, end)| *end) else {
            return;
        };
        match action {
            InputAction::SelectAll => {
                self.clear_suggestions();
                self.selection = Some((start, last));
            }
            InputAction::Delete(a, b) => {
                let (left, right) = (a.min(b), a.max(b));
                if left.0 < start.0 || right.0 > last.0 || right < start || left > last {
                    self.selection = None;
                    return;
                }
                let (left, right) = (left.max(start), right.min(last));
                let begin = spans.partition_point(|&(_, end)| end < left);
                let end = spans.partition_point(|&(start, _)| start <= right);
                if begin < end {
                    self.send_input_edit(begin, end - begin);
                }
            }
            InputAction::MoveCaret { point, fraction } => {
                if point < start || point.0 > last.0 {
                    return;
                }
                let index = spans.partition_point(|&(first, end)| {
                    end < point
                        || (first <= point
                            && point <= end
                            && (point.1.saturating_sub(first.1) as f64 + fraction)
                                >= (end.1 - first.1 + 1) as f64 * 0.5)
                });
                self.send_input_edit(index, 0);
            }
        }
    }

    fn send_input_edit(&mut self, position: usize, delete: usize) {
        let mut bytes = INPUT_BEGIN.to_vec();
        for _ in 0..position {
            bytes.extend_from_slice(INPUT_FORWARD);
        }
        for _ in 0..delete {
            bytes.extend_from_slice(INPUT_DELETE);
        }
        self.edit(bytes, true);
    }

    fn take_copy_selection(&mut self) -> Option<String> {
        let (start, end) = self.selection.take()?;
        let (start, end) = self.terminal.trim_selection(start, end)?;
        let text = self.terminal.text(start, end);
        (!text.trim().is_empty()).then_some(text)
    }

    fn clear_suggestions(&mut self) {
        self.terminal.ghost.clear();
        self.terminal.suggestions.clear();
        self.suggestion_index = 0;
        self.suggestion_navigated = false;
    }

    fn selected_suggestion(&self) -> &str {
        self.terminal
            .suggestions
            .get(self.suggestion_index)
            .map_or(&self.terminal.ghost, String::as_str)
    }

    fn move_suggestion(&mut self, direction: isize) {
        let count = self.terminal.suggestions.len();
        if count > 1 {
            self.suggestion_navigated = true;
            self.suggestion_index =
                (self.suggestion_index as isize + direction).rem_euclid(count as isize) as usize;
        }
    }

    fn accept_navigated_suggestion(&mut self) -> bool {
        if self.suggestion_navigated && self.prompt() && self.terminal.suggestions.len() > 1 {
            self.accept_suggestion();
            true
        } else {
            false
        }
    }

    fn accept_suggestion(&mut self) {
        if let Some(suffix) = self
            .terminal
            .suggestions
            .get(self.suggestion_index)
            .cloned()
        {
            self.paste(&suffix);
        } else {
            self.edit(ACCEPT, false);
        }
    }

    fn sudo_password_prompt(&self) -> bool {
        if self.terminal.at_prompt || self.terminal.alternate() {
            return false;
        }
        let (row, _) = self.terminal.cursor();
        let text = self.terminal.text((row, 0), (row, self.terminal.cols - 1));
        let text = text.trim();
        text.starts_with("[sudo]")
            && text.ends_with(':')
            && self
                .session
                .as_ref()
                .is_some_and(Session::sudo_has_hidden_input)
    }

    fn track_password_prompt(&mut self) -> bool {
        let prompt = self.sudo_password_prompt().then(|| self.terminal.cursor());
        self.password_mask.track(prompt);
        prompt.is_some()
    }

    fn send_password_text(&mut self, text: &str) {
        let typed: String = text
            .chars()
            .filter(|character| !character.is_control())
            .collect();
        if !typed.is_empty() && self.send(typed.as_bytes()) {
            self.password_mask.count = self
                .password_mask
                .count
                .saturating_add(typed.chars().count());
        }
    }

    fn send_password_key(&mut self, key: gdk::Key, modifiers: gdk::ModifierType) {
        use gdk::{Key as K, ModifierType as M};
        if modifiers.contains(M::CONTROL_MASK) {
            let bytes: &[u8] = match key.to_lower() {
                K::c => &[3],
                K::d => &[4],
                K::u => &[21],
                _ => return,
            };
            if self.send(bytes) {
                self.password_mask.count = 0;
            }
        } else {
            match key {
                K::Escape => {
                    if self.send([3]) {
                        self.password_mask.count = 0;
                    }
                }
                K::Return | K::KP_Enter => {
                    if self.send([13]) {
                        self.password_mask.count = 0;
                    }
                }
                K::BackSpace | K::Delete | K::KP_Delete => {
                    if self.send([127]) {
                        self.password_mask.count = self.password_mask.count.saturating_sub(1);
                    }
                }
                _ => {
                    if let Some(character) = key.to_unicode() {
                        self.send_password_text(character.encode_utf8(&mut [0; 4]));
                    }
                }
            }
        }
    }

    fn folder_target(&self, index: usize) -> Option<&Path> {
        breadcrumb_target(
            self.terminal.directory.as_deref()?,
            self.terminal.folder_spans.len(),
            index,
        )
    }

    fn folder_at(&self, x: f64, y: f64) -> Option<PathBuf> {
        if !self.prompt()
            || x < 12.0
            || y < self.grid_top + self.viewport().blank_above as f64 * self.cell.1
        {
            return None;
        }
        let (row, col) = self.point(x, y);
        let viewport = self.viewport();
        if !viewport.pinned.is_empty()
            && y < self.grid_top + viewport.pinned_slot as f64 * self.cell.1
        {
            return None;
        }
        if y >= self.grid_top + self.terminal.rows as f64 * self.cell.1
            || x >= 12.0 + self.terminal.cols as f64 * self.cell.0
        {
            return None;
        }
        let spans = &self.terminal.folder_spans;
        let point = (row, col);
        let index = spans
            .iter()
            .position(|&(start, end)| start <= point && point < end)
            .or_else(|| {
                if self.terminal.line(row)?.get(col)?.bg != PROMPT_JOIN {
                    return None;
                }
                let x_in_cell = (x - 12.0).rem_euclid(self.cell.0);
                let row_y = self.grid_top + viewport.display_row(row) as f64 * self.cell.1;
                let half = (self.cell.1 * 0.5 - 2.0).max(1.0);
                let distance = ((y - row_y - self.cell.1 * 0.5).abs() / half).min(1.0);
                if x_in_cell <= breadcrumb_join_edge(self.cell.0, distance) {
                    spans.iter().position(|&(_, end)| end == point)
                } else {
                    spans.iter().position(|&(start, _)| start == (row, col + 1))
                }
            })?;
        self.folder_target(index).map(Path::to_path_buf)
    }

    fn request_navigation(&mut self, path: PathBuf) {
        if self.prompt()
            && matches!(self.navigation, Navigation::Idle)
            && self
                .session
                .as_ref()
                .is_some_and(|session| session.send(NAVIGATION_CHECK.to_vec()))
        {
            self.navigation = Navigation::Checking(path);
        }
    }

    fn paste(&mut self, text: &str) {
        if self.track_password_prompt() {
            self.send_password_text(text);
            return;
        }
        let clean: String = text
            .replace("\r\n", "\n")
            .chars()
            .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
            .collect();
        if clean.is_empty() {
            return;
        }
        let clean = clean.replace('\n', "\r");
        let input = if self.terminal.bracketed_paste {
            format!("\x1b[200~{clean}\x1b[201~")
        } else {
            clean
        };
        self.edit(input.into_bytes(), true);
    }

    fn live_view_top(&self) -> usize {
        Viewport::for_terminal(&self.terminal, None).first
    }

    fn viewport(&self) -> Viewport {
        Viewport::for_terminal(&self.terminal, self.top)
    }

    fn jump(&mut self, row: usize) {
        let oldest = self.terminal.oldest();
        let latest = self.live_view_top();
        let row = row.clamp(oldest, latest);
        self.top = (row < latest).then_some(row);
    }

    fn scroll(&mut self, delta: isize) {
        self.jump(self.viewport().first.saturating_add_signed(delta));
        if let Some((_, (x, y))) = self.selection_drag {
            self.extend_selection_drag(x, y);
        }
    }

    fn extend_selection_drag(&mut self, x: f64, y: f64) {
        let Some((anchor, pointer)) = &mut self.selection_drag else {
            return;
        };
        *pointer = (x, y);
        let anchor = *anchor;
        let mut target = self.point(x, y);
        if let Some(edge) = self.viewport().selection_edge(anchor.0, target.0) {
            target = (
                edge,
                if edge < target.0 {
                    self.terminal.cols - 1
                } else {
                    0
                },
            );
        }
        self.selection = self.terminal.trim_selection(anchor, target);
    }

    fn point(&self, x: f64, y: f64) -> Point {
        (
            self.viewport()
                .terminal_row(((y - self.grid_top) / self.cell.1).floor().max(0.0) as usize),
            (((x - 12.0) / self.cell.0).floor().max(0.0) as usize).min(self.terminal.cols - 1),
        )
    }

    fn find(&mut self, query: &str) -> bool {
        if query.is_empty() {
            return false;
        }
        let terminal = &self.terminal;
        let end = terminal.live_top() + terminal.rows;
        let split = self
            .selection
            .map_or(end, |(point, _)| point.0)
            .clamp(terminal.oldest(), end);
        for row in (terminal.oldest()..split).rev().chain((split..end).rev()) {
            let Some(line) = terminal.line(row) else {
                continue;
            };
            let text: String = line.iter().map(|cell| cell.glyph).collect();
            let lower = text.to_lowercase();
            if let Some(start) = lower.find(&query.to_lowercase()) {
                let col = lower[..start].chars().count();
                self.selection = Some(((row, col), (row, col + query.chars().count() - 1)));
                self.jump(row.saturating_sub(2));
                return true;
            }
        }
        false
    }

    fn last_failure(&mut self) -> bool {
        if let Some(command) = self
            .terminal
            .commands
            .iter()
            .rev()
            .find(|c| c.status != 0 && c.head >= self.terminal.oldest())
        {
            self.jump(command.head.saturating_sub(1));
            true
        } else {
            false
        }
    }
}

fn input_spans(text: &str, start: Point, cols: usize) -> Vec<(Point, Point)> {
    let (mut row, mut col) = start;
    let mut spans = Vec::new();
    for ch in text.chars() {
        let width = ch.width().unwrap_or(0);
        if width == 0 {
            continue;
        }
        if col + width > cols {
            col = 0;
            row += 1;
        }
        spans.push(((row, col), (row, col + width - 1)));
        col += width;
    }
    spans
}

fn breadcrumb_target(directory: &str, count: usize, index: usize) -> Option<&Path> {
    let path = Path::new(directory);
    if !path.is_absolute() {
        return None;
    }
    let home = std::env::home_dir();
    let expected = home
        .as_deref()
        .and_then(|home| path.strip_prefix(home).ok())
        .map_or_else(
            || path.ancestors().count(),
            |relative| 1 + relative.components().count(),
        );
    if count == 3 && expected > count {
        return match index {
            0 => path.ancestors().nth(expected - 1),
            // The ellipsis opens the same parent menu as the current folder.
            1 | 2 => Some(path),
            _ => None,
        };
    }
    if count != expected {
        return None;
    }
    path.ancestors().nth(count.checked_sub(index + 1)?)
}

fn rounded_fill(cr: &gtk::cairo::Context, x: f64, y: f64, width: f64, height: f64) {
    let r = 4.0_f64.min(width * 0.5).min(height * 0.5);
    let (right, bottom) = (x + width, y + height);
    use std::f64::consts::{FRAC_PI_2, PI};
    cr.new_sub_path();
    cr.arc(right - r, y + r, r, -FRAC_PI_2, 0.0);
    cr.arc(right - r, bottom - r, r, 0.0, FRAC_PI_2);
    cr.arc(x + r, bottom - r, r, FRAC_PI_2, PI);
    cr.arc(x + r, y + r, r, PI, 3.0 * FRAC_PI_2);
    cr.close_path();
    let _ = cr.fill();
}

fn breadcrumb_color(fill: Color32, theme: Theme) -> Option<Color32> {
    let surface = theme.surface.mix(theme.fg, 0.07);
    match fill {
        PROMPT_BADGE => Some(theme.accent),
        PROMPT_DIRECTORY => Some(surface),
        PROMPT_DIRECTORY_ALT => Some(surface.mix(theme.accent, 0.20)),
        _ => None,
    }
}

fn breadcrumb_join_edge(width: f64, distance_from_center: f64) -> f64 {
    width * (0.88 - 0.62 * distance_from_center)
}

struct SuggestionMenu {
    x: f64,
    y: f64,
    width: f64,
    row_height: f64,
    first: usize,
    slots: usize,
}

impl SuggestionMenu {
    fn for_prompt(state: &State, area_width: f64) -> Option<Self> {
        let terminal = &state.terminal;
        let choices = &terminal.suggestions;
        if choices.len() < 2 || state.selection.is_some() || !state.prompt() {
            return None;
        }
        let viewport = state.viewport();
        let cursor = terminal.cursor();
        if !viewport.contains(cursor.0) {
            return None;
        }
        let (row, col) = terminal
            .input_start
            .filter(|(line, _)| viewport.contains(*line))
            .unwrap_or(cursor);
        let anchor_x = 12.0 + col as f64 * state.cell.0;
        let anchor_y = state.grid_top + viewport.display_row(row) as f64 * state.cell.1;
        let row_height = state.cell.1 + 2.0;
        let slots = (((anchor_y - 13.0) / row_height).floor().max(0.0) as usize)
            .min(5)
            .min(choices.len());
        if slots < 2 {
            return None;
        }
        let first = state
            .suggestion_index
            .saturating_sub(slots - 1)
            .min(choices.len() - slots);
        let longest = choices
            .iter()
            .map(|choice| choice.width())
            .max()
            .unwrap_or(0)
            + terminal.input.width();
        let width = (longest as f64 * state.cell.0 + 26.0)
            .clamp(160.0, 488.0)
            .min((area_width - 24.0).max(1.0));
        Some(Self {
            x: anchor_x.clamp(10.0, (area_width - width - 11.0).max(10.0)),
            y: anchor_y - slots as f64 * row_height - 12.0,
            width,
            row_height,
            first,
            slots,
        })
    }

    fn hit(&self, x: f64, y: f64) -> Option<usize> {
        let offset = y - self.y - 4.0;
        (x >= self.x
            && x < self.x + self.width
            && offset >= 0.0
            && offset < self.row_height * self.slots as f64)
            .then(|| self.first + (offset / self.row_height) as usize)
    }
}

fn render_suggestions(
    cr: &gtk::cairo::Context,
    layout: &pango::Layout,
    theme: Theme,
    state: &State,
    menu: SuggestionMenu,
) {
    let SuggestionMenu {
        x,
        y,
        width,
        row_height: height,
        first,
        slots,
    } = menu;
    let choices = &state.terminal.suggestions;
    let selected = state.suggestion_index.min(choices.len() - 1);
    let box_height = slots as f64 * height + 8.0;
    theme.muted.mix(theme.surface, 0.53).cairo(cr);
    rounded_fill(cr, x, y, width, box_height);
    theme.surface.cairo(cr);
    rounded_fill(cr, x + 1.0, y + 1.0, width - 2.0, box_height - 2.0);
    layout.set_ellipsize(pango::EllipsizeMode::End);
    layout.set_width(((width - 20.0).max(1.0) * f64::from(pango::SCALE)) as i32);
    for index in first..first + slots {
        let row_y = y + 4.0 + (index - first) as f64 * height;
        if index == selected {
            theme.selection.cairo(cr);
            rounded_fill(cr, x + 4.0, row_y, width - 8.0, height);
        }
        theme.fg.cairo(cr);
        layout.set_text(&format!("{}{}", state.terminal.input, choices[index]));
        let (_, text_height) = layout.pixel_size();
        cr.move_to(x + 10.0, row_y + (height - f64::from(text_height)) * 0.5);
        pangocairo::functions::show_layout(cr, layout);
    }
    layout.set_width(-1);
    layout.set_ellipsize(pango::EllipsizeMode::None);
}

fn draw(area: &gtk::DrawingArea, cr: &gtk::cairo::Context, w: i32, h: i32, state: &mut State) {
    let theme = Theme::from_widget(area.upcast_ref());
    let layout = pangocairo::functions::create_layout(cr);
    let mut font = pango::FontDescription::from_string("Monospace");
    font.set_absolute_size(state.font_size * f64::from(pango::SCALE));
    layout.set_font_description(Some(&font));
    layout.set_text("M");
    let (width, base_height) = layout.pixel_size();
    let (width, height) = (f64::from(width.max(1)), f64::from(base_height + 3));
    state.cell = (width, height);
    let cols = (((f64::from(w) - 24.0) / width) as usize).clamp(2, 500);
    let rows = (((f64::from(h) - 20.0) / height) as usize).clamp(2, 250);
    state.grid_top = if state.terminal.alternate() {
        10.0
    } else {
        (f64::from(h) - 10.0 - rows as f64 * height).max(10.0)
    };
    if (cols, rows) != (state.terminal.cols, state.terminal.rows) {
        state.terminal.resize(cols, rows);
        if state.top.is_some_and(|top| top >= state.live_view_top()) {
            state.top = None;
        }
        state.selection = None;
        if let Some(session) = &state.session {
            session.resize(cols, rows);
        }
    }
    theme.bg.cairo(cr);
    let _ = cr.paint();

    let terminal = &state.terminal;
    let viewport = state.viewport();
    let position = |row: usize, col: usize| {
        (
            12.0 + col as f64 * width,
            state.grid_top + viewport.display_row(row) as f64 * height,
        )
    };
    let selected = state.selection.map(|(a, b)| a.min(b)..=a.max(b));
    let fg = |color| {
        if color == FG { theme.fg } else { color }
    };
    let highlight = |color: Color32| {
        if theme.dark {
            color
        } else {
            color.mix(Color32(0, 0, 0), 0.49)
        }
    };
    let text = |s: &str, x: f64, y: f64, color: Color32| {
        color.cairo(cr);
        layout.set_text(s);
        cr.move_to(x, y + 1.5);
        pangocairo::functions::show_layout(cr, &layout);
    };
    let mut input = std::collections::HashMap::new();
    if state.prompt()
        && let Some((mut row, mut col)) = terminal.input_start
    {
        let chars: Vec<_> = terminal.input.chars().collect();
        for (&ch, tint) in chars.iter().zip(highlight::shell(&chars)) {
            let n = ch.width().unwrap_or(0);
            if n == 0 {
                continue;
            }
            if col + n > cols {
                row += 1;
                col = 0;
            }
            if let Some(tint) = tint {
                input.insert((row, col), (ch, tint));
            }
            col += n;
        }
    }

    if !viewport.pinned.is_empty() {
        let y = state.grid_top + viewport.pinned_slot as f64 * height;
        theme.surface.cairo(cr);
        cr.rectangle(0.0, y - 1.0, f64::from(w), f64::from(h) - y + 1.0);
        let _ = cr.fill();
        theme.muted.mix(theme.surface, 0.65).cairo(cr);
        cr.rectangle(0.0, y - 1.0, f64::from(w), 1.0);
        let _ = cr.fill();
    }

    for row in viewport.rows() {
        let Some(line) = terminal.line(row) else {
            continue;
        };
        let output = if !terminal.alternate() && terminal.is_output_row(row) {
            highlight::output(&line.iter().map(|cell| cell.glyph).collect::<Vec<_>>())
        } else {
            Vec::new()
        };
        let limit = cols.min(line.len());
        let mut col = 0;
        while col < limit {
            let (x, y) = position(row, col);
            let fill = line[col].bg;
            let prompt_color = if terminal.alternate() {
                None
            } else {
                breadcrumb_color(fill, theme)
            };
            if let Some(color) = prompt_color {
                let end = col
                    + line[col..limit]
                        .iter()
                        .take_while(|cell| cell.bg == fill)
                        .count();
                let span = width * (end - col) as f64;
                let joins_left = col > 0 && line[col - 1].bg == PROMPT_JOIN;
                let joins_right = end < limit && line[end].bg == PROMPT_JOIN;
                color.cairo(cr);
                rounded_fill(cr, x, y + 2.0, span, height - 4.0);
                if joins_left || joins_right {
                    if joins_left {
                        cr.rectangle(x, y + 2.0, 4.0, height - 4.0);
                    }
                    if joins_right {
                        cr.rectangle(x + span - 4.0, y + 2.0, 4.0, height - 4.0);
                    }
                    let _ = cr.fill();
                }
                col = end;
            } else if !terminal.alternate() && fill == PROMPT_JOIN {
                let left = col
                    .checked_sub(1)
                    .and_then(|i| breadcrumb_color(line[i].bg, theme));
                let right = line
                    .get(col + 1)
                    .and_then(|cell| breadcrumb_color(cell.bg, theme));
                right.unwrap_or(theme.bg).cairo(cr);
                cr.rectangle(x, y + 2.0, width, height - 4.0);
                let _ = cr.fill();
                if let Some(left) = left {
                    left.cairo(cr);
                    let shoulder = x + breadcrumb_join_edge(width, 1.0);
                    cr.move_to(x, y + 2.0);
                    cr.line_to(shoulder, y + 2.0);
                    cr.line_to(x + breadcrumb_join_edge(width, 0.0), y + height * 0.5);
                    cr.line_to(shoulder, y + height - 2.0);
                    cr.line_to(x, y + height - 2.0);
                    cr.close_path();
                    let _ = cr.fill();
                }
                col += 1;
            } else {
                if fill != BG {
                    fill.cairo(cr);
                    cr.rectangle(x, y, width, height);
                    let _ = cr.fill();
                }
                col += 1;
            }
        }
        if let Some(range) = &selected {
            theme.selection.cairo(cr);
            for col in 0..Terminal::content_columns(line).min(limit) {
                if range.contains(&(row, col)) {
                    let (x, y) = position(row, col);
                    cr.rectangle(x, y, width, height);
                }
            }
            let _ = cr.fill();
        }
        let tint_at = |col: usize| {
            let cell = &line[col];
            input
                .get(&(row, col))
                .filter(|(ch, _)| *ch == cell.glyph)
                .map(|(_, c)| *c)
                .or_else(|| {
                    if cell.fg == FG {
                        output.get(col).copied().flatten()
                    } else {
                        None
                    }
                })
                .map(highlight)
                .unwrap_or_else(|| fg(cell.fg))
        };
        let mut col = 0;
        let mut run = String::new();
        while col < limit {
            let cell = &line[col];
            if cell.width == 0 || (cell.glyph == ' ' && !cell.underline && cell.extra.is_empty()) {
                col += 1;
                continue;
            }
            let (x, y) = position(row, col);
            let tint = tint_at(col);
            if cell.width == 1 && cell.glyph.is_ascii() && !cell.underline && cell.extra.is_empty()
            {
                run.clear();
                let mut end = col;
                while end < limit {
                    let next = &line[end];
                    if next.width != 1
                        || !next.glyph.is_ascii()
                        || next.underline
                        || !next.extra.is_empty()
                        || tint_at(end) != tint
                    {
                        break;
                    }
                    run.push(next.glyph);
                    end += 1;
                }
                text(run.trim_end(), x, y, tint);
                col = end;
                continue;
            }
            if cell.extra.is_empty() {
                text(cell.glyph.encode_utf8(&mut [0; 4]), x, y, tint);
            } else {
                text(&format!("{}{}", cell.glyph, cell.extra), x, y, tint);
            }
            if cell.underline {
                tint.cairo(cr);
                cr.rectangle(x, y + height - 2.0, width * f64::from(cell.width), 1.0);
                let _ = cr.fill();
            }
            col += 1;
        }
    }

    if !terminal.alternate() {
        for &(row, col) in &terminal.prompt_icons {
            if !viewport.contains(row) || col + 4 > cols {
                continue;
            }
            if terminal
                .line(row)
                .and_then(|line| line.get(col..col + 4))
                .is_some_and(|cells| {
                    cells
                        .iter()
                        .all(|cell| cell.glyph == ' ' && cell.bg == PROMPT_BADGE)
                })
            {
                let (x, y) = position(row, col + 2);
                let y = y + height / 2.0;
                theme.accent_fg.cairo(cr);
                cr.set_line_width(1.65);
                cr.set_line_cap(gtk::cairo::LineCap::Round);
                cr.move_to(x - 5.0, y - 4.0);
                cr.line_to(x - 1.0, y);
                cr.line_to(x - 5.0, y + 4.0);
                cr.move_to(x + 1.5, y + 4.0);
                cr.line_to(x + 6.0, y + 4.0);
                let _ = cr.stroke();
            }
        }
    }

    if !terminal.alternate() {
        for prompt in &terminal.folder_prompts {
            for (index, &((row, col), _)) in prompt.spans.iter().enumerate() {
                if !viewport.contains(row) || col + 2 >= cols {
                    continue;
                }
                let reserved = terminal.line(row).and_then(|line| line.get(col..col + 3));
                if !reserved.is_some_and(|cells| {
                    cells.iter().all(|cell| {
                        cell.glyph == ' '
                            && matches!(cell.bg, PROMPT_DIRECTORY | PROMPT_DIRECTORY_ALT)
                    })
                }) {
                    continue;
                }
                let Some(target) = breadcrumb_target(&prompt.directory, prompt.spans.len(), index)
                else {
                    continue;
                };
                let (x, y) = position(row, col);
                theme.fg.cairo(cr);
                FolderIcon::for_path(target).draw(cr, x + width * 1.7, y + height * 0.5);
            }
        }
    }

    let (row, col) = terminal.cursor();
    if state.selection.is_none()
        && viewport.contains(row)
        && !terminal.alternate()
        && state.ssh_foreground()
    {
        let line = terminal.text((row, 0), (row, cols - 1));
        if let Some((identity, path, prompt_columns)) = ssh_prompt_location(&line)
            && prompt_columns <= col
        {
            let (x, y) = position(row, 0);
            let reported = terminal
                .reported_directory
                .as_deref()
                .and_then(ssh_reported_path);
            let label = if path == "~" {
                identity.to_owned()
            } else {
                format_ssh_breadcrumb(identity, reported.as_deref().unwrap_or(path))
            };
            let available = prompt_columns as f64 * width;
            let block_height = height - 4.0;
            let badge_width = 2.5 * width;
            let join_width = width * 0.7;
            let label_x = x + badge_width + join_width;
            let label_width = available - badge_width - join_width - 8.0;
            if label_width > width * 2.0 {
                theme.bg.cairo(cr);
                cr.rectangle(x, y, available, height);
                let _ = cr.fill();
                theme.surface.mix(theme.fg, 0.07).cairo(cr);
                rounded_fill(cr, x, y + 2.0, available - 3.0, block_height);
                theme.accent.cairo(cr);
                rounded_fill(cr, x, y + 2.0, badge_width, block_height);
                cr.rectangle(x + badge_width - 4.0, y + 2.0, 4.0, block_height);
                cr.move_to(x + badge_width, y + 2.0);
                cr.line_to(label_x, y + height * 0.5);
                cr.line_to(x + badge_width, y + height - 2.0);
                cr.close_path();
                let _ = cr.fill();

                layout.set_text(">_");
                let (icon_width, icon_height) = layout.pixel_size();
                theme.accent_fg.cairo(cr);
                cr.move_to(
                    x + (badge_width - f64::from(icon_width)) * 0.5,
                    y + (height - f64::from(icon_height)) * 0.5,
                );
                pangocairo::functions::show_layout(cr, &layout);
                let icon = if path == "~" {
                    FolderIcon::Home
                } else {
                    FolderIcon::Folder
                };
                theme.fg.cairo(cr);
                icon.draw(cr, label_x + 7.0, y + height * 0.5);
                let mut compact_font = font.clone();
                compact_font.set_absolute_size(state.font_size * 0.84 * f64::from(pango::SCALE));
                layout.set_font_description(Some(&compact_font));
                let text_width = (label_width - 13.0).max(1.0);
                layout.set_text(identity);
                let identity_width = f64::from(layout.pixel_size().0);
                if identity_width > text_width {
                    let scaled =
                        (state.font_size * 0.84 * text_width / (identity_width + 2.0)).max(8.0);
                    compact_font.set_absolute_size(scaled * f64::from(pango::SCALE));
                    layout.set_font_description(Some(&compact_font));
                }
                layout.set_width((text_width * f64::from(pango::SCALE)) as i32);
                layout.set_ellipsize(pango::EllipsizeMode::End);
                layout.set_text(&label);
                let (_, label_height) = layout.pixel_size();
                cr.move_to(label_x + 16.0, y + (height - f64::from(label_height)) * 0.5);
                pangocairo::functions::show_layout(cr, &layout);
                layout.set_font_description(Some(&font));
                layout.set_width(-1);
                layout.set_ellipsize(pango::EllipsizeMode::None);
            }
        }
    }
    if terminal.cursor_visible
        && viewport.contains(row)
        && (state.top.is_none() || viewport.pinned.contains(&row))
    {
        if state.selection.is_none() && state.prompt() {
            let mut at = col;
            for ch in state.selected_suggestion().chars() {
                let n = ch.width().unwrap_or(0);
                if at + n > cols {
                    break;
                }
                if ch != ' ' && n > 0 {
                    let (x, y) = position(row, at);
                    text(ch.encode_utf8(&mut [0; 4]), x, y, theme.muted);
                }
                at += n;
            }
        }
        let mut caret = col;
        if state.password_mask.anchor == Some((row, col)) && state.password_mask.count > 0 {
            let visible = state.password_mask.count.min(cols.saturating_sub(col + 1));
            let (x, y) = position(row, col);
            text(&"•".repeat(visible), x, y, theme.fg);
            caret += visible;
        }
        let (x, y) = position(row, caret);
        (if area.has_focus() {
            theme.accent
        } else {
            theme.muted
        })
        .cairo(cr);
        cr.rectangle(x, y + 1.0, 2.0, height - 2.0);
        let _ = cr.fill();
    }

    if let Some(menu) = SuggestionMenu::for_prompt(state, f64::from(w)) {
        render_suggestions(cr, &layout, theme, state, menu);
    }

    let history = state.live_view_top() - terminal.oldest();
    if !terminal.alternate() && history > 0 {
        let span = (f64::from(h) - 16.0).max(1.0);
        let thumb = (span * rows as f64 / (history + rows) as f64)
            .clamp(1.0, span)
            .max(21.0)
            .min(span);
        let offset = (span - thumb) * (viewport.first - terminal.oldest()) as f64 / history as f64;
        theme.muted.cairo(cr);
        cr.rectangle(f64::from(w - 6), 8.0 + offset, 2.0, thumb);
        let _ = cr.fill();
    }
}

fn key_sequence(
    key: gdk::Key,
    mods: gdk::ModifierType,
    application_cursor: bool,
) -> Option<Vec<u8>> {
    use gdk::{Key as K, ModifierType as M};
    let ctrl = mods.contains(M::CONTROL_MASK);
    let shift = mods.contains(M::SHIFT_MASK);
    let alt = mods.contains(M::ALT_MASK);
    let mut bytes = Vec::new();
    if alt {
        bytes.push(27);
    }
    if ctrl && let Some(ch) = key.to_unicode() {
        let ch = ch.to_ascii_uppercase();
        if matches!(ch, '@'..='_' | ' ') {
            bytes.push(if ch == ' ' { 0 } else { ch as u8 & 31 });
            return Some(bytes);
        }
    }
    let suffix = match key {
        K::Up | K::KP_Up => 'A',
        K::Down | K::KP_Down => 'B',
        K::Right | K::KP_Right => 'C',
        K::Left | K::KP_Left => 'D',
        K::Home | K::KP_Home => 'H',
        K::End | K::KP_End => 'F',
        _ => '\0',
    };
    if suffix != '\0' {
        let modifier = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
        return Some(
            if modifier != 1 {
                format!("\x1b[1;{modifier}{suffix}")
            } else if application_cursor {
                format!("\x1bO{suffix}")
            } else {
                format!("\x1b[{suffix}")
            }
            .into_bytes(),
        );
    }
    let mut utf8 = [0; 4];
    bytes.extend_from_slice(match key {
        K::Return | K::KP_Enter => b"\r",
        K::ISO_Left_Tab => b"\x1b[Z",
        K::Tab => b"\t",
        K::Escape => b"\x1b",
        K::BackSpace if ctrl => b"\x08",
        K::BackSpace => b"\x7f",
        K::Insert => b"\x1b[2~",
        K::Delete => b"\x1b[3~",
        K::Page_Up => b"\x1b[5~",
        K::Page_Down => b"\x1b[6~",
        K::F1 => b"\x1bOP",
        K::F2 => b"\x1bOQ",
        K::F3 => b"\x1bOR",
        K::F4 => b"\x1bOS",
        K::F5 => b"\x1b[15~",
        K::F6 => b"\x1b[17~",
        K::F7 => b"\x1b[18~",
        K::F8 => b"\x1b[19~",
        K::F9 => b"\x1b[20~",
        K::F10 => b"\x1b[21~",
        K::F11 => b"\x1b[23~",
        K::F12 => b"\x1b[24~",
        _ if !ctrl => key
            .to_unicode()
            .filter(|c| !c.is_control())?
            .encode_utf8(&mut utf8)
            .as_bytes(),
        _ => return None,
    });
    Some(bytes)
}

fn paste_clipboard(
    area: &gtk::DrawingArea,
    state: &Rc<RefCell<State>>,
    password_prompt: Option<Point>,
    require_ssh: bool,
) {
    area.clipboard().read_text_async(
        None::<&gio::Cancellable>,
        glib::clone!(
            #[weak]
            state,
            #[weak]
            area,
            move |result| {
                if let Ok(Some(text)) = result {
                    let mut state = state.borrow_mut();
                    if (require_ssh && !state.ssh_foreground())
                        || (password_prompt.is_some()
                            && (!state.track_password_prompt()
                                || state.password_mask.anchor != password_prompt))
                    {
                        return;
                    }
                    state.paste(&text);
                    area.queue_draw();
                }
            }
        ),
    );
}

fn menu_action(
    group: &gio::SimpleActionGroup,
    name: &str,
    activate: impl Fn() + 'static,
) -> gio::SimpleAction {
    let action = gio::SimpleAction::new(name, None);
    action.connect_activate(move |_, _| activate());
    group.add_action(&action);
    action
}

fn install_terminal_menu(
    area: &gtk::DrawingArea,
    state: &Rc<RefCell<State>>,
    search: &gtk::SearchBar,
    history: &adw::OverlaySplitView,
    pane: &gtk::Box,
    titles: &Rc<SessionTitles>,
) {
    let actions = gio::SimpleActionGroup::new();
    let copy = menu_action(
        &actions,
        "copy",
        glib::clone!(
            #[weak]
            state,
            #[weak]
            area,
            move || {
                let mut state = state.borrow_mut();
                if let Some(text) = state.take_copy_selection() {
                    area.clipboard().set_text(&text);
                }
                area.queue_draw();
            }
        ),
    );
    menu_action(
        &actions,
        "paste",
        glib::clone!(
            #[weak]
            area,
            #[weak]
            state,
            move || {
                paste_clipboard(&area, &state, None, false);
            }
        ),
    );
    menu_action(
        &actions,
        "select-all",
        glib::clone!(
            #[weak]
            area,
            #[weak]
            state,
            move || {
                let mut state = state.borrow_mut();
                let terminal = &state.terminal;
                state.selection = terminal.trim_selection(
                    (terminal.oldest(), 0),
                    (terminal.display_bottom(), terminal.cols - 1),
                );
                area.queue_draw();
            }
        ),
    );
    menu_action(
        &actions,
        "find",
        glib::clone!(
            #[weak]
            search,
            move || {
                search.set_search_mode(true);
            }
        ),
    );
    menu_action(
        &actions,
        "history",
        glib::clone!(
            #[weak]
            history,
            move || {
                history.set_show_sidebar(!history.shows_sidebar());
            }
        ),
    );
    let close = menu_action(
        &actions,
        "close-pane",
        glib::clone!(
            #[weak]
            pane,
            #[strong]
            titles,
            move || {
                glib::idle_add_local_once(glib::clone!(
                    #[weak]
                    pane,
                    #[strong]
                    titles,
                    move || {
                        titles.request_close_pane(pane.upcast_ref());
                    }
                ));
            }
        ),
    );
    for (name, direction) in [
        ("split-vertical", gtk::Orientation::Horizontal),
        ("split-horizontal", gtk::Orientation::Vertical),
    ] {
        menu_action(
            &actions,
            name,
            glib::clone!(
                #[weak]
                pane,
                #[strong]
                titles,
                move || {
                    glib::idle_add_local_once(glib::clone!(
                        #[weak]
                        pane,
                        #[strong]
                        titles,
                        move || {
                            if let Some(view) = titles.view.upgrade() {
                                split_pane(&view, pane.upcast_ref(), direction, &titles);
                            }
                        }
                    ));
                }
            ),
        );
    }
    area.insert_action_group("terminal", Some(&actions));

    let show = Rc::new(glib::clone!(
        #[weak]
        state,
        #[weak]
        area,
        #[weak]
        search,
        #[weak]
        pane,
        #[strong]
        actions,
        move |x: f64, y: f64| {
            area.grab_focus();
            copy.set_enabled(state.borrow().selection.is_some());
            close.set_enabled(
                pane.parent()
                    .is_some_and(|parent| parent.is::<gtk::Paned>()),
            );
            let popover = gtk::Popover::new();
            popover.set_has_arrow(false);
            let entries = gtk::Box::new(gtk::Orientation::Vertical, 0);
            for (caption, name) in [
                ("Copy", "copy"),
                ("Paste", "paste"),
                ("Select All", "select-all"),
                ("Find in Scrollback", "find"),
                ("Command History", "history"),
                ("Close Pane", "close-pane"),
            ] {
                let Some(action) = actions
                    .lookup_action(name)
                    .and_downcast::<gio::SimpleAction>()
                else {
                    continue;
                };
                let label = gtk::Label::new(Some(caption));
                label.set_xalign(0.0);
                let entry = gtk::Button::new();
                entry.set_widget_name(&format!("stride-menu-{name}"));
                entry.set_child(Some(&label));
                entry.add_css_class("flat");
                action
                    .bind_property("enabled", &entry, "sensitive")
                    .sync_create()
                    .build();
                entry.connect_clicked(glib::clone!(
                    #[weak]
                    popover,
                    #[strong]
                    action,
                    move |_| {
                        popover.popdown();
                        action.activate(None);
                    }
                ));
                entries.append(&entry);
            }
            let menu_stack = gtk::Stack::new();
            menu_stack.set_transition_type(gtk::StackTransitionType::SlideLeftRight);
            let split_menu = gtk::Box::new(gtk::Orientation::Vertical, 0);
            let split = gtk::Button::new();
            split.set_widget_name("stride-menu-split");
            split.add_css_class("flat");
            let split_label = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            let text = gtk::Label::new(Some("Split"));
            text.set_hexpand(true);
            text.set_xalign(0.0);
            split_label.append(&text);
            split_label.append(&gtk::Image::from_icon_name("go-next-symbolic"));
            split.set_child(Some(&split_label));

            let back = gtk::Button::with_label("‹  Split");
            back.add_css_class("flat");
            back.connect_clicked(glib::clone!(
                #[weak]
                menu_stack,
                move |_| menu_stack.set_visible_child_name("main")
            ));
            split_menu.append(&back);
            for (label, name) in [
                ("Vertical", "split-vertical"),
                ("Horizontal", "split-horizontal"),
            ] {
                let Some(action) = actions.lookup_action(name) else {
                    continue;
                };
                let item = gtk::Button::with_label(label);
                item.set_widget_name(&format!("stride-menu-{name}"));
                item.add_css_class("flat");
                item.connect_clicked(glib::clone!(
                    #[weak]
                    popover,
                    #[strong]
                    action,
                    move |_| {
                        popover.popdown();
                        action.activate(None);
                    }
                ));
                split_menu.append(&item);
            }
            split.connect_clicked(glib::clone!(
                #[weak]
                menu_stack,
                #[weak]
                back,
                move |_| {
                    menu_stack.set_visible_child_name("split");
                    back.grab_focus();
                }
            ));
            entries.append(&split);
            menu_stack.add_named(&entries, Some("main"));
            menu_stack.add_named(&split_menu, Some("split"));
            menu_stack.set_visible_child_name("main");
            popover.set_child(Some(&menu_stack));
            popover.set_parent(&area);
            popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
            popover.connect_closed(glib::clone!(
                #[weak]
                area,
                #[weak]
                search,
                move |popover| {
                    let popover = popover.clone();
                    glib::idle_add_local_once(glib::clone!(
                        #[strong]
                        popover,
                        #[weak]
                        area,
                        #[weak]
                        search,
                        move || {
                            if popover.parent().is_some() {
                                popover.unparent();
                            }
                            if !search.is_search_mode() && area.root().is_some() {
                                area.grab_focus();
                            }
                        }
                    ));
                }
            ));
            popover.popup();
        }
    ));
    pane.connect_parent_notify(glib::clone!(
        #[weak]
        actions,
        move |pane| {
            if let Some(close) = actions
                .lookup_action("close-pane")
                .and_downcast::<gio::SimpleAction>()
            {
                close.set_enabled(
                    pane.parent()
                        .is_some_and(|parent| parent.is::<gtk::Paned>()),
                );
            }
        }
    ));

    let click = gtk::GestureClick::new();
    click.set_button(gdk::BUTTON_SECONDARY);
    click.set_propagation_phase(gtk::PropagationPhase::Capture);
    click.connect_released(glib::clone!(
        #[strong]
        show,
        move |gesture, _, x, y| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            show(x, y);
        }
    ));
    area.add_controller(click);

    let keyboard = gtk::EventControllerKey::new();
    keyboard.set_propagation_phase(gtk::PropagationPhase::Capture);
    keyboard.connect_key_pressed(glib::clone!(
        #[weak]
        area,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_, key, _, mods| {
            if key == gdk::Key::Menu
                || (key == gdk::Key::F10 && mods.contains(gdk::ModifierType::SHIFT_MASK))
            {
                show(16.0, f64::from((area.height() - 28).max(0)));
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        }
    ));
    area.add_controller(keyboard);
}

fn install_input(
    area: &gtk::DrawingArea,
    state: &Rc<RefCell<State>>,
    search: &gtk::SearchBar,
    overlay: &adw::ToastOverlay,
) {
    let selection_keys = gtk::EventControllerKey::new();
    selection_keys.set_name(Some("stride-input-selection"));
    selection_keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    selection_keys.connect_key_pressed(glib::clone!(
        #[weak]
        state,
        #[weak]
        area,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_, key, _, mods| {
            use gdk::{Key as K, ModifierType as M};
            if mods.intersects(M::ALT_MASK | M::SUPER_MASK) {
                return glib::Propagation::Proceed;
            }
            let ctrl = mods.contains(M::CONTROL_MASK);
            let mut state = state.borrow_mut();
            if !state.prompt() {
                return glib::Propagation::Proceed;
            }
            if ctrl && !mods.contains(M::SHIFT_MASK) && key.to_lower() == K::a {
                state.request_input_action(InputAction::SelectAll);
            } else if !ctrl && matches!(key, K::Delete | K::KP_Delete | K::BackSpace) {
                if matches!(state.input_action, Some(InputAction::SelectAll)) {
                    state.edit(INPUT_CLEAR, true);
                } else if let Some((start, end)) = state.selection {
                    state.request_input_action(InputAction::Delete(start, end));
                } else {
                    return glib::Propagation::Proceed;
                }
            } else {
                return glib::Propagation::Proceed;
            }
            area.queue_draw();
            glib::Propagation::Stop
        }
    ));
    area.add_controller(selection_keys);

    let keyboard = gtk::EventControllerKey::new();
    let im = gtk::IMMulticontext::new();
    im.set_client_widget(Some(area));
    keyboard.set_im_context(Some(&im));
    im.connect_commit(glib::clone!(
        #[weak]
        state,
        #[weak]
        area,
        move |_, text| {
            let mut state = state.borrow_mut();
            if state.track_password_prompt() {
                state.send_password_text(text);
            } else {
                state.edit(text.as_bytes(), true);
            }
            area.queue_draw();
        }
    ));
    keyboard.connect_key_pressed(glib::clone!(
        #[weak]
        area,
        #[weak]
        state,
        #[weak]
        search,
        #[weak]
        overlay,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_, key, _, mods| {
            use gdk::{Key as K, ModifierType as M};
            let ctrl = mods.contains(M::CONTROL_MASK);
            let shift = mods.contains(M::SHIFT_MASK);
            let alt = mods.contains(M::ALT_MASK);
            let lower = key.to_lower();
            let mut state_ref = state.borrow_mut();
            if state_ref.track_password_prompt() {
                if ctrl && lower == K::v && shift {
                    paste_clipboard(&area, &state, state_ref.password_mask.anchor, false);
                } else {
                    state_ref.send_password_key(key, mods);
                }
                area.queue_draw();
                return glib::Propagation::Stop;
            }
            if ctrl && lower == K::c {
                if let Some(text) = state_ref.take_copy_selection() {
                    area.clipboard().set_text(&text);
                    overlay.add_toast(
                        adw::Toast::builder()
                            .title("Copied selection")
                            .timeout(2)
                            .build(),
                    );
                } else if !shift {
                    state_ref.edit([3], false);
                }
            } else if ctrl
                && lower == K::v
                && (shift
                    || state_ref.prompt()
                    || (!state_ref.terminal.alternate() && state_ref.ssh_foreground()))
            {
                paste_clipboard(&area, &state, None, !shift && !state_ref.prompt());
            } else if ctrl && shift && matches!(key, K::Return | K::KP_Enter) && state_ref.prompt()
            {
                state_ref.edit(PREPARE_ELEVATED, false);
            } else if ctrl && shift && lower == K::f {
                search.set_search_mode(true);
            } else if ctrl && shift && lower == K::l {
                if !state_ref.last_failure() {
                    overlay.add_toast(
                        adw::Toast::builder()
                            .title("No failed command in scrollback")
                            .timeout(2)
                            .build(),
                    );
                }
            } else if ctrl && matches!(key, K::plus | K::equal | K::minus) {
                let delta = if key == K::minus { -1.0 } else { 1.0 };
                state_ref.font_size = (state_ref.font_size + delta).clamp(9.0, 28.0);
            } else if shift
                && matches!(key, K::Page_Up | K::Page_Down)
                && !state_ref.terminal.alternate()
            {
                let rows = state_ref.terminal.rows as isize;
                state_ref.scroll(if key == K::Page_Up { -rows } else { rows });
            } else if !ctrl
                && !shift
                && !alt
                && state_ref.prompt()
                && state_ref.terminal.suggestions.len() > 1
                && matches!(key, K::Up | K::Down)
            {
                state_ref.move_suggestion(if key == K::Down { 1 } else { -1 });
            } else if !ctrl
                && !shift
                && !alt
                && matches!(key, K::Return | K::KP_Enter)
                && state_ref.accept_navigated_suggestion()
            {
                // The next Enter submits the completed line.
            } else if key == K::Escape && !state_ref.terminal.suggestions.is_empty() {
                state_ref.clear_suggestions();
            } else if matches!(key, K::Right | K::Tab)
                && !ctrl
                && !shift
                && !alt
                && state_ref.prompt()
                && !state_ref.selected_suggestion().is_empty()
            {
                state_ref.accept_suggestion();
            } else if let Some(bytes) =
                key_sequence(key, mods, state_ref.terminal.application_cursor)
            {
                let preview = !matches!(
                    key,
                    K::Return
                        | K::KP_Enter
                        | K::Escape
                        | K::Tab
                        | K::ISO_Left_Tab
                        | K::F1
                        | K::F2
                        | K::F3
                        | K::F4
                ) && !(ctrl && matches!(lower, K::d | K::r | K::s | K::z | K::v));
                state_ref.edit(bytes, preview);
            } else {
                return glib::Propagation::Proceed;
            }
            area.queue_draw();
            glib::Propagation::Stop
        }
    ));
    area.add_controller(keyboard);

    let drag = gtk::GestureDrag::new();
    drag.set_button(gdk::BUTTON_PRIMARY);
    drag.connect_drag_begin(glib::clone!(
        #[weak]
        state,
        #[weak]
        area,
        move |_, x, y| {
            area.grab_focus();
            let mut state = state.borrow_mut();
            let on_suggestion = SuggestionMenu::for_prompt(&state, f64::from(area.width()))
                .and_then(|menu| menu.hit(x, y))
                .is_some();
            state.selection = None;
            state.selection_drag = (!on_suggestion && x < f64::from(area.width()) - 11.0)
                .then(|| (state.point(x, y), (x, y)));
            area.queue_draw();
        }
    ));
    drag.connect_drag_update(glib::clone!(
        #[weak]
        state,
        #[weak]
        area,
        move |gesture, dx, dy| {
            let Some((x, y)) = gesture.start_point() else {
                return;
            };
            let mut state = state.borrow_mut();
            if x >= f64::from(area.width()) - 11.0 {
                let oldest = state.terminal.oldest();
                let latest = state.live_view_top();
                let ratio =
                    ((y + dy - 8.0) / f64::from((area.height() - 16).max(1))).clamp(0.0, 1.0);
                let row = oldest + (ratio * (latest - oldest) as f64).round() as usize;
                state.top = (row < latest).then_some(row);
            } else {
                state.extend_selection_drag(x + dx, y + dy);
            }
            area.queue_draw();
        }
    ));
    drag.connect_drag_end(glib::clone!(
        #[weak]
        state,
        move |_, _, _| {
            state.borrow_mut().selection_drag = None;
        }
    ));
    drag.connect_cancel(glib::clone!(
        #[weak]
        state,
        move |_, _| {
            state.borrow_mut().selection_drag = None;
        }
    ));
    area.add_controller(drag);

    let click = gtk::GestureClick::new();
    click.set_button(gdk::BUTTON_PRIMARY);
    let press = Rc::new(std::cell::Cell::new((0.0_f64, 0.0_f64)));
    click.connect_pressed(glib::clone!(
        #[strong]
        press,
        #[weak]
        area,
        #[weak]
        state,
        move |_, _, x, y| {
            press.set((x, y));
            area.grab_focus();
            state.borrow_mut().selection = None;
            area.queue_draw();
        }
    ));
    click.connect_released(glib::clone!(
        #[weak]
        state,
        #[weak]
        area,
        move |_, count, x, y| {
            let (start_x, start_y) = press.get();
            if count != 1 || (x - start_x).abs() + (y - start_y).abs() > 5.0 {
                return;
            }
            let mut view = state.borrow_mut();
            if view.selection.is_some() {
                return;
            }

            if let Some(menu) = SuggestionMenu::for_prompt(&view, f64::from(area.width()))
                && let Some(index) = menu.hit(start_x, start_y)
            {
                if menu.hit(x, y) == Some(index) {
                    view.suggestion_index = index;
                    view.accept_suggestion();
                    area.queue_draw();
                }
                return;
            }

            let Some(target) = view.folder_at(x, y) else {
                view.click_input(x, y);
                return;
            };
            if view.terminal.directory.as_deref().map(Path::new) == Some(&target) {
                drop(view);
                show_parents(&area, &state, &target, x, y);
            } else {
                view.request_navigation(target);
            }
        }
    ));
    area.add_controller(click);

    let motion = gtk::EventControllerMotion::new();
    motion.connect_motion(glib::clone!(
        #[weak]
        state,
        #[weak]
        area,
        move |_, x, y| {
            let state = state.borrow();
            let suggestion = SuggestionMenu::for_prompt(&state, f64::from(area.width()))
                .and_then(|menu| menu.hit(x, y))
                .is_some();
            let target = if suggestion {
                None
            } else {
                state.folder_at(x, y)
            };
            area.set_cursor_from_name(Some(if suggestion || target.is_some() {
                "pointer"
            } else {
                "text"
            }));
            let path = target.as_deref().and_then(Path::to_str);
            if area.tooltip_text().as_deref() != path {
                area.set_tooltip_text(path);
            }
        }
    ));
    motion.connect_leave(glib::clone!(
        #[weak]
        area,
        move |_| area.set_tooltip_text(None)
    ));
    area.add_controller(motion);

    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    scroll.connect_scroll(glib::clone!(
        #[weak]
        state,
        #[weak]
        area,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_, _, dy| {
            let mut state = state.borrow_mut();
            if state.terminal.alternate() {
                return glib::Propagation::Proceed;
            }
            let rows = (dy.abs() * 3.0).round().max(1.0) as isize;
            state.scroll(if dy < 0.0 { -rows } else { rows });
            area.queue_draw();
            glib::Propagation::Stop
        }
    ));
    area.add_controller(scroll);
}

fn show_parents(
    area: &gtk::DrawingArea,
    state: &Rc<RefCell<State>>,
    current: &Path,
    x: f64,
    y: f64,
) {
    if current.parent().is_none() {
        return;
    }
    let popover = gtk::Popover::new();
    let list = gtk::Box::new(gtk::Orientation::Vertical, 2);
    for path in current.ancestors().skip(1) {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.append(&gtk::Image::from_icon_name(
            FolderIcon::for_path(path).name(),
        ));
        let name = path
            .file_name()
            .map_or_else(|| "/".into(), std::ffi::OsStr::to_string_lossy);
        let label = gtk::Label::new(Some(&name));
        label.set_xalign(0.0);
        label.set_hexpand(true);
        label.set_ellipsize(pango::EllipsizeMode::End);
        label.set_max_width_chars(40);
        row.append(&label);
        let action = gtk::Button::new();
        action.set_child(Some(&row));
        action.add_css_class("flat");
        action.set_tooltip_text(path.to_str());
        let path = path.to_path_buf();
        action.connect_clicked(glib::clone!(
            #[weak]
            popover,
            #[weak]
            state,
            #[weak]
            area,
            move |_| {
                state.borrow_mut().request_navigation(path.clone());
                popover.popdown();
                area.grab_focus();
            }
        ));
        list.append(&action);
    }
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .max_content_height(250)
        .propagate_natural_width(true)
        .propagate_natural_height(true)
        .child(&list)
        .build();
    popover.set_child(Some(&scroll));
    popover.set_parent(area);
    popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
    popover.connect_closed(|popover| popover.unparent());
    popover.popup();
}

fn button(icon: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::from_icon_name(icon);
    button.set_tooltip_text(Some(tooltip));
    button.set_focus_on_click(false);
    button
}

const DEFAULT_TITLE: &str = "Stride";

struct PaneTitle {
    pane: glib::WeakRef<gtk::Widget>,
    state: std::rc::Weak<RefCell<State>>,
    text: String,
    last_focused: u64,
}

struct SessionTitles {
    window: glib::WeakRef<adw::ApplicationWindow>,
    view: glib::WeakRef<adw::TabView>,
    panes: RefCell<Vec<PaneTitle>>,
    clock: std::cell::Cell<u64>,
}

impl SessionTitles {
    fn new(window: &adw::ApplicationWindow, view: &adw::TabView) -> Rc<Self> {
        Rc::new(Self {
            window: window.downgrade(),
            view: view.downgrade(),
            panes: RefCell::new(Vec::new()),
            clock: std::cell::Cell::new(0),
        })
    }

    fn register(&self, pane: &gtk::Box, state: &Rc<RefCell<State>>) {
        self.panes.borrow_mut().push(PaneTitle {
            pane: pane.upcast_ref::<gtk::Widget>().downgrade(),
            state: Rc::downgrade(state),
            text: String::from("Home"),
            last_focused: 0,
        });
    }

    fn running_programs(&self, root: &gtk::Widget) -> Vec<String> {
        let mut programs = Vec::new();
        for entry in self.panes.borrow().iter() {
            let Some(pane) = entry.pane.upgrade() else {
                continue;
            };
            if &pane != root && !pane.is_ancestor(root) {
                continue;
            }
            let Some(state) = entry.state.upgrade() else {
                continue;
            };
            let state = state.borrow();
            let Some(session) = &state.session else {
                continue;
            };
            let mut active = session.running_programs();
            if active.is_empty() && state.terminal.has_running_command() {
                active.push("Shell command".into());
            }
            for program in active {
                if !programs.contains(&program) {
                    programs.push(program);
                }
            }
        }
        programs
    }

    fn close_exited_shell(&self, pane: &gtk::Widget) {
        if close_split_pane(pane) {
            return;
        }
        let (Some(view), Some(parent)) = (self.view.upgrade(), pane.parent()) else {
            return;
        };
        if let Some(page) = (0..view.n_pages())
            .map(|index| view.nth_page(index))
            .find(|page| page.child() == parent)
        {
            view.close_page(&page);
        }
    }

    fn request_close_pane(&self, pane: &gtk::Widget) -> bool {
        if !pane
            .parent()
            .is_some_and(|parent| parent.is::<gtk::Paned>())
        {
            return false;
        }
        let programs = self.running_programs(pane);
        if programs.is_empty() {
            return close_split_pane(pane);
        }
        if let Some(window) = self.window.upgrade() {
            confirm_running_close(
                &window,
                "Close pane?",
                &programs,
                glib::clone!(
                    #[weak]
                    pane,
                    move |accepted| {
                        if accepted {
                            close_split_pane(&pane);
                        }
                    }
                ),
            );
        }
        true
    }

    fn update(&self, pane: &gtk::Box, text: String) {
        let mut entries = self.panes.borrow_mut();
        let Some(entry) = entries
            .iter_mut()
            .find(|entry| entry.pane.upgrade().as_ref() == Some(pane.upcast_ref()))
        else {
            return;
        };
        if entry.text == text {
            return;
        }
        entry.text = text;
        drop(entries);
        self.refresh();
    }

    fn focus_selected(&self) {
        let Some(root) = self
            .view
            .upgrade()
            .and_then(|view| view.selected_page())
            .map(|page| page.child())
        else {
            return;
        };
        let preferred = self
            .panes
            .borrow()
            .iter()
            .filter_map(|entry| {
                let pane = entry.pane.upgrade()?;
                pane.is_ancestor(&root)
                    .then_some((entry.last_focused, pane))
            })
            .max_by_key(|(rank, _)| *rank)
            .map(|(_, pane)| pane);
        if let Some(grid) = preferred
            .as_ref()
            .and_then(first_terminal)
            .or_else(|| first_terminal(&root))
        {
            grid.grab_focus();
        }
    }

    fn refresh(&self) {
        let (Some(window), Some(view)) = (self.window.upgrade(), self.view.upgrade()) else {
            return;
        };
        let focused = gtk::prelude::GtkWindowExt::focus(&window);
        let pages = view.pages();
        let selected = view.selected_page();
        let mut entries = self.panes.borrow_mut();
        entries.retain(|entry| entry.pane.upgrade().is_some());
        if let Some(focused) = focused
            && let Some(entry) = entries.iter_mut().find(|entry| {
                entry
                    .pane
                    .upgrade()
                    .is_some_and(|pane| focused.is_ancestor(&pane))
            })
        {
            let index = self.clock.get().wrapping_add(1);
            self.clock.set(index);
            entry.last_focused = index;
        }
        let titles: Vec<_> = (0..pages.n_items())
            .filter_map(|index| {
                let page = pages.item(index).and_downcast::<adw::TabPage>()?;
                let root = page.child();
                let title = entries
                    .iter()
                    .filter(|entry| {
                        entry
                            .pane
                            .upgrade()
                            .is_some_and(|pane| pane.is_ancestor(&root))
                    })
                    .max_by_key(|entry| entry.last_focused)
                    .map(|entry| entry.text.clone())?;
                Some((page, title))
            })
            .collect();
        drop(entries);

        for (page, title) in titles {
            if page.title() != title {
                page.set_title(&title);
            }
            if selected.as_ref() == Some(&page) {
                let window_title = if title == "Home" {
                    DEFAULT_TITLE.to_owned()
                } else {
                    format!("{title} — {DEFAULT_TITLE}")
                };
                if window.title().as_deref() != Some(window_title.as_str()) {
                    window.set_title(Some(&window_title));
                }
            }
        }
    }
}

fn pane_title(state: &State) -> String {
    let path = state.terminal.directory.as_deref().map(Path::new);
    let folder = match path {
        Some(path) if std::env::home_dir().as_deref() == Some(path) => "Home".into(),
        Some(path) => path
            .file_name()
            .map_or_else(|| "/".into(), std::ffi::OsStr::to_string_lossy),
        None if !state
            .session
            .as_ref()
            .is_some_and(|session| session.integrated) =>
        {
            "Terminal".into()
        }
        None => "Home".into(),
    };
    if let Some(program) = state.session.as_ref().and_then(Session::foreground_program) {
        format!("{program} · {folder}")
    } else {
        folder.into_owned()
    }
}

fn populate_history(
    items: &gtk::Box,
    filter: &str,
    area: &gtk::DrawingArea,
    state: &Rc<RefCell<State>>,
) {
    while let Some(child) = items.first_child() {
        items.remove(&child);
    }
    let filter = filter.trim().to_lowercase();
    let snapshot = state.borrow();
    let heading = |text| {
        let label = gtk::Label::new(Some(text));
        label.add_css_class("dim-label");
        label.set_xalign(0.0);
        items.append(&label);
    };
    let command_label = |text| {
        let label = gtk::Label::new(Some(text));
        label.set_xalign(0.0);
        label.set_hexpand(true);
        label.set_ellipsize(pango::EllipsizeMode::End);
        label.set_tooltip_text(Some(text));
        label
    };
    let mut any = false;
    let mut live_heading = false;
    for command in snapshot
        .terminal
        .commands
        .iter()
        .rev()
        .filter(|entry| entry.label.to_lowercase().contains(&filter))
    {
        if !live_heading {
            heading("This session");
            live_heading = true;
        }
        any = true;
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        let status = gtk::Image::from_icon_name(if command.status == 0 {
            "object-select-symbolic"
        } else {
            "dialog-warning-symbolic"
        });
        status.set_tooltip_text(Some(&format!("Exit {}", command.status)));
        row.append(&status);
        let jump = gtk::Button::new();
        jump.set_child(Some(&command_label(&command.label)));
        jump.set_hexpand(true);
        jump.add_css_class("flat");
        jump.set_sensitive(command.head >= snapshot.terminal.oldest());
        let (head, start, end) = (command.head, command.start, command.end);
        jump.connect_clicked(glib::clone!(
            #[weak]
            area,
            #[weak]
            state,
            move |_| {
                state.borrow_mut().jump(head.saturating_sub(2));
                area.grab_focus();
                area.queue_draw();
            }
        ));
        row.append(&jump);
        let copy = button("edit-copy-symbolic", "Copy command output");
        copy.add_css_class("flat");
        copy.set_sensitive(end.is_some() && start >= snapshot.terminal.oldest());
        copy.connect_clicked(glib::clone!(
            #[weak]
            area,
            #[weak]
            state,
            move |_| {
                if let Some(end) = end {
                    let text = state.borrow().terminal.text((start, 0), end);
                    area.clipboard()
                        .set_text(text.trim_end_matches([' ', '\n']));
                }
            }
        ));
        row.append(&copy);
        items.append(&row);
    }
    let mut archive_heading = false;
    for text in snapshot
        .terminal
        .archive
        .iter()
        .rev()
        .filter(|entry| entry.to_lowercase().contains(&filter))
    {
        if !archive_heading {
            heading("Earlier sessions");
            archive_heading = true;
        }
        any = true;
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        row.append(&command_label(text));
        let copy = button("edit-copy-symbolic", "Copy command");
        copy.add_css_class("flat");
        let text = text.clone();
        copy.connect_clicked(glib::clone!(
            #[weak]
            area,
            move |_| area.clipboard().set_text(&text)
        ));
        row.append(&copy);
        items.append(&row);
    }
    if !any {
        heading(if filter.is_empty() {
            "No Bash history yet"
        } else {
            "No matching commands"
        });
    }
}

fn ssh_reported_path(uri: &str) -> Option<glib::GString> {
    let uri = glib::Uri::parse(uri, glib::UriFlags::NONE).ok()?;
    if uri.scheme() != "file"
        || uri.userinfo().is_some()
        || uri.query().is_some()
        || uri.fragment().is_some()
    {
        return None;
    }
    let host = uri
        .host()
        .filter(|host| !host.is_empty())
        .unwrap_or_else(|| "SSH".into());
    let path = uri.path();
    if !path.starts_with('/') || host.chars().chain(path.chars()).any(char::is_control) {
        return None;
    }
    Some(path)
}

fn ssh_prompt_location(line: &str) -> Option<(&str, &str, usize)> {
    let prompt = ["$ ", "# "]
        .into_iter()
        .filter_map(|marker| line.find(marker))
        .min()?;
    let (identity, path) = line[..prompt].split_once(':')?;
    let (user, host) = identity.split_once('@')?;
    let valid = |value: &str| {
        !value.is_empty()
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    };
    if !valid(user)
        || !valid(host)
        || !(path == "~" || path.starts_with("~/") || path.starts_with('/'))
        || path.chars().any(char::is_control)
    {
        return None;
    }
    Some((identity, path, line[..prompt + 2].width()))
}

fn format_ssh_breadcrumb(identity: &str, path: &str) -> String {
    let mut components = path.split('/').filter(|part| !part.is_empty());
    let crumb = match (components.next(), components.next(), components.next()) {
        (None, _, _) => "/".into(),
        (Some(first), None, _) => first.to_owned(),
        (Some(first), Some(second), None) => format!("{first}  ›  {second}"),
        (_, _, Some(third)) => format!("…  ›  {}", components.last().unwrap_or(third)),
    };
    format!("{identity}  ›  {crumb}")
}

fn build_terminal_pane(titles: &Rc<SessionTitles>, launch: &LaunchOptions) -> gtk::Box {
    let (state, error) = State::new(launch);
    let state = Rc::new(RefCell::new(state));
    let pane = gtk::Box::new(gtk::Orientation::Vertical, 0);
    pane.add_css_class("stride-terminal-pane");
    titles.register(&pane, &state);
    pane.set_hexpand(true);
    pane.set_vexpand(true);

    let layout = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let search = gtk::SearchBar::new();
    let entry = gtk::SearchEntry::new();
    entry.set_placeholder_text(Some("Find in scrollback"));
    search.set_child(Some(&entry));
    search.connect_entry(&entry);
    search.set_show_close_button(true);
    layout.append(&search);

    let area = gtk::DrawingArea::builder()
        .focusable(true)
        .hexpand(true)
        .vexpand(true)
        .build();
    area.connect_has_focus_notify(|area| area.queue_draw());
    area.add_css_class("view");
    area.add_css_class("stride-terminal-grid");
    layout.append(&area);
    let overlay = adw::ToastOverlay::new();
    overlay.set_child(Some(&layout));
    overlay.set_vexpand(true);

    search.connect_search_mode_enabled_notify(glib::clone!(
        #[weak]
        area,
        #[weak]
        entry,
        move |search| {
            if search.is_search_mode() {
                entry.grab_focus();
            } else {
                area.grab_focus();
            }
        }
    ));
    entry.connect_activate(glib::clone!(
        #[weak]
        state,
        #[weak]
        area,
        #[weak]
        overlay,
        move |entry| {
            if !state.borrow_mut().find(&entry.text()) {
                overlay.add_toast(
                    adw::Toast::builder()
                        .title("No matching output")
                        .timeout(2)
                        .build(),
                );
            }
            area.queue_draw();
        }
    ));

    let history = adw::OverlaySplitView::new();
    history.set_sidebar_position(gtk::PackType::End);
    history.set_min_sidebar_width(215.0);
    history.set_max_sidebar_width(355.0);
    history.set_vexpand(true);
    history.set_content(Some(&overlay));
    let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 6);
    sidebar.add_css_class("stride-history");
    let history_header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let history_title = gtk::Label::new(Some("Command History"));
    history_title.add_css_class("heading");
    history_title.set_xalign(0.0);
    history_title.set_hexpand(true);
    history_header.append(&history_title);
    let close_history = button("window-close-symbolic", "Close history");
    close_history.add_css_class("flat");
    close_history.connect_clicked(glib::clone!(
        #[weak]
        history,
        #[weak]
        area,
        move |_| {
            history.set_show_sidebar(false);
            area.grab_focus();
        }
    ));
    history_header.append(&close_history);
    sidebar.append(&history_header);
    let history_search = gtk::SearchEntry::new();
    history_search.set_placeholder_text(Some("Search commands"));
    sidebar.append(&history_search);
    let history_items = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let history_scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&history_items)
        .build();
    sidebar.append(&history_scroll);
    history.set_sidebar(Some(&sidebar));
    history.set_show_sidebar(false);
    pane.append(&history);
    history.connect_show_sidebar_notify(glib::clone!(
        #[weak]
        history_items,
        #[weak]
        history_search,
        #[weak]
        area,
        #[weak]
        state,
        move |history| {
            if history.shows_sidebar() {
                populate_history(&history_items, &history_search.text(), &area, &state);
            }
        }
    ));
    history_search.connect_search_changed(glib::clone!(
        #[weak]
        history_items,
        #[weak]
        area,
        #[weak]
        state,
        move |entry| {
            populate_history(&history_items, &entry.text(), &area, &state);
        }
    ));

    if let Some(error) = error {
        overlay.add_toast(adw::Toast::new(&error));
    }
    area.set_draw_func(glib::clone!(
        #[weak]
        state,
        move |area, cr, w, h| {
            draw(area, cr, w, h, &mut state.borrow_mut());
        }
    ));
    install_input(&area, &state, &search, &overlay);
    install_terminal_menu(&area, &state, &search, &history, &pane, titles);
    let preview_state = Rc::downgrade(&state);
    let preview_area = area.downgrade();
    glib::timeout_add_local(Duration::from_millis(17), move || {
        let (Some(state), Some(area)) = (preview_state.upgrade(), preview_area.upgrade()) else {
            return glib::ControlFlow::Break;
        };
        let mut state = state.borrow_mut();
        if state.preview_pending() {
            let hidden = state.preview_frame.is_some();
            state.read_output();
            if !hidden || state.preview_frame.is_none() {
                area.queue_draw();
            }
        }
        glib::ControlFlow::Continue
    });
    let weak_state = Rc::downgrade(&state);
    let pane_for_title = pane.downgrade();
    let titles_for_timer = titles.clone();
    let weak_area = area.downgrade();
    glib::timeout_add_local(std::time::Duration::from_millis(125), move || {
        let (Some(state), Some(area)) = (weak_state.upgrade(), weak_area.upgrade()) else {
            return glib::ControlFlow::Break;
        };
        let (title, changed) = {
            let mut state = state.borrow_mut();
            let before = state.password_mask.anchor;
            state.track_password_prompt();
            if !state.ssh_foreground() {
                state.terminal.reported_directory = None;
            }
            (pane_title(&state), before != state.password_mask.anchor)
        };
        if let Some(pane) = pane_for_title.upgrade() {
            titles_for_timer.update(&pane, title);
        }
        if changed {
            area.queue_draw();
        }
        glib::ControlFlow::Continue
    });

    let wake = state
        .borrow()
        .session
        .as_ref()
        .and_then(|session| session.wake.try_clone().ok());
    if let Some(mut wake) = wake {
        let weak_state = Rc::downgrade(&state);
        let weak_area = area.downgrade();
        let weak_overlay = overlay.downgrade();
        let weak_history = history.downgrade();
        let weak_history_items = history_items.downgrade();
        let weak_history_search = history_search.downgrade();
        let weak_pane = pane.downgrade();
        let weak_titles = Rc::downgrade(titles);
        let close_on_exit = launch.command.is_empty();

        glib::source::unix_fd_add_local_full(
            wake.as_raw_fd(),
            glib::Priority::DEFAULT_IDLE,
            glib::IOCondition::IN | glib::IOCondition::HUP | glib::IOCondition::ERR,
            move |_, _| {
                let (Some(state), Some(area)) = (weak_state.upgrade(), weak_area.upgrade()) else {
                    return glib::ControlFlow::Break;
                };
                let mut buffer = [0; 256];
                while matches!(wake.read(&mut buffer), Ok(n) if n > 0) {}
                let mut session_state = state.borrow_mut();
                let version = session_state.terminal.history_version;
                let closed = session_state.read_output();
                if version != session_state.terminal.history_version
                    && weak_history
                        .upgrade()
                        .is_some_and(|history| history.shows_sidebar())
                    && let (Some(items), Some(search)) =
                        (weak_history_items.upgrade(), weak_history_search.upgrade())
                {
                    drop(session_state);
                    populate_history(&items, &search.text(), &area, &state);
                    session_state = state.borrow_mut();
                }
                if let Some(prepared) = session_state.terminal.elevation_prepared.take()
                    && let Some(overlay) = weak_overlay.upgrade()
                {
                    let message = if prepared {
                        "Elevated command prepared. Press Enter to run it"
                    } else {
                        "Elevated shortcut requires a simple external command"
                    };
                    overlay.add_toast(adw::Toast::builder().title(message).timeout(3).build());
                }
                if std::mem::take(&mut session_state.navigation_blocked)
                    && let Some(overlay) = weak_overlay.upgrade()
                {
                    overlay.add_toast(
                        adw::Toast::builder()
                            .title("Finish or clear the current command first")
                            .timeout(2)
                            .build(),
                    );
                }
                area.queue_draw();
                if closed {
                    session_state.session = None;
                    if close_on_exit {
                        let pane = weak_pane.clone();
                        let titles = weak_titles.clone();
                        glib::idle_add_local_once(move || {
                            if let (Some(pane), Some(titles)) = (pane.upgrade(), titles.upgrade()) {
                                titles.close_exited_shell(pane.upcast_ref());
                            }
                        });
                    } else if let Some(overlay) = weak_overlay.upgrade() {
                        overlay.add_toast(adw::Toast::new("Program exited"));
                    }
                    glib::ControlFlow::Break
                } else {
                    glib::ControlFlow::Continue
                }
            },
        );
    }
    let style = adw::StyleManager::default();
    let weak = area.downgrade();
    let handler = style.connect_notify_local(None, move |_, _| {
        if let Some(area) = weak.upgrade() {
            area.queue_draw();
        }
    });
    let handler = RefCell::new(Some(handler));
    pane.connect_destroy(move |_| {
        if let Some(handler) = handler.take() {
            style.disconnect(handler);
        }
        // Closing the pane releases its own child and wake stream.
        state.borrow_mut().session = None;
    });
    pane
}

fn first_terminal(widget: &gtk::Widget) -> Option<gtk::Widget> {
    if widget.has_css_class("stride-terminal-grid") {
        return Some(widget.clone());
    }
    let mut child = widget.first_child();
    while let Some(candidate) = child {
        if let Some(grid) = first_terminal(&candidate) {
            return Some(grid);
        }
        child = candidate.next_sibling();
    }
    None
}

fn append_tab(view: &adw::TabView, titles: &Rc<SessionTitles>) {
    append_tab_with_launch(view, titles, &LaunchOptions::default());
}

fn append_tab_with_launch(view: &adw::TabView, titles: &Rc<SessionTitles>, launch: &LaunchOptions) {
    let page_body = gtk::Box::new(gtk::Orientation::Vertical, 0);
    page_body.set_focusable(true);
    page_body.add_css_class("stride-tab-page");
    page_body.append(&build_terminal_pane(titles, launch));
    let page = view.append(&page_body);
    page.set_title("Home");
    page.set_icon(Some(&gio::ThemedIcon::new("utilities-terminal-symbolic")));
    view.set_selected_page(&page);
    titles.focus_selected();
    titles.refresh();
}

fn active_pane(window: &adw::ApplicationWindow, view: &adw::TabView) -> Option<gtk::Widget> {
    let page = view.selected_page()?.child();
    let mut widget = gtk::prelude::GtkWindowExt::focus(window)
        .filter(|focused| focused.is_ancestor(&page))
        .or_else(|| first_terminal(&page));
    while let Some(current) = widget {
        if current.has_css_class("stride-terminal-pane") {
            return Some(current);
        }
        widget = current.parent();
    }
    None
}

fn replace_pane(parent: &gtk::Widget, old: &gtk::Widget, new: &gtk::Widget) {
    if let Some(container) = parent.downcast_ref::<gtk::Box>() {
        container.remove(old);
        container.append(new);
    } else if let Some(split) = parent.downcast_ref::<gtk::Paned>() {
        if split.start_child().as_ref() == Some(old) {
            split.set_start_child(gtk::Widget::NONE);
            split.set_start_child(Some(new));
        } else if split.end_child().as_ref() == Some(old) {
            split.set_end_child(gtk::Widget::NONE);
            split.set_end_child(Some(new));
        }
    }
}

fn split_active(
    window: &adw::ApplicationWindow,
    view: &adw::TabView,
    direction: gtk::Orientation,
    titles: &Rc<SessionTitles>,
) {
    let Some(pane) = active_pane(window, view) else {
        return;
    };
    split_pane(view, &pane, direction, titles);
}

fn split_pane(
    view: &adw::TabView,
    pane: &gtk::Widget,
    direction: gtk::Orientation,
    titles: &Rc<SessionTitles>,
) {
    let Some(parent) = pane.parent() else {
        return;
    };
    let size = if direction == gtk::Orientation::Horizontal {
        pane.width()
    } else {
        pane.height()
    };
    let split = gtk::Paned::new(direction);
    split.set_wide_handle(true);
    split.set_resize_start_child(true);
    split.set_resize_end_child(true);
    split.set_shrink_start_child(true);
    split.set_shrink_end_child(true);
    split.set_hexpand(true);
    split.set_vexpand(true);
    if let Some(page) = view.selected_page() {
        page.child().grab_focus();
    }
    replace_pane(&parent, pane, split.upcast_ref());
    let second = build_terminal_pane(titles, &LaunchOptions::default());
    split.set_start_child(Some(pane));
    split.set_end_child(Some(&second));
    split.set_position((size / 2).max(1));
    if let Some(terminal) = first_terminal(second.upcast_ref()) {
        terminal.grab_focus();
    }
}

fn close_split_pane(pane: &gtk::Widget) -> bool {
    let Some(split) = pane.parent().and_downcast::<gtk::Paned>() else {
        return false;
    };
    let Some(parent) = split.parent() else {
        return false;
    };
    let sibling = if split.start_child().as_ref() == Some(pane) {
        split.end_child()
    } else {
        split.start_child()
    };
    let Some(sibling) = sibling else {
        return false;
    };
    let mut ancestor = Some(parent.clone());
    while let Some(widget) = ancestor {
        if widget.has_css_class("stride-tab-page") {
            widget.grab_focus();
            break;
        }
        ancestor = widget.parent();
    }
    split.set_start_child(gtk::Widget::NONE);
    split.set_end_child(gtk::Widget::NONE);
    replace_pane(&parent, split.upcast_ref(), &sibling);
    if let Some(terminal) = first_terminal(&sibling) {
        terminal.grab_focus();
    }
    true
}

fn close_focused(window: &adw::ApplicationWindow, view: &adw::TabView, titles: &SessionTitles) {
    if active_pane(window, view).is_some_and(|pane| titles.request_close_pane(&pane)) {
        return;
    }
    if let Some(page) = view.selected_page() {
        view.close_page(&page);
    }
}

fn focus_adjacent_pane(window: &adw::ApplicationWindow, view: &adw::TabView, direction: isize) {
    fn collect(node: &gtk::Widget, grids: &mut Vec<gtk::Widget>) {
        if let Some(split) = node.downcast_ref::<gtk::Paned>() {
            for pane in [split.start_child(), split.end_child()]
                .into_iter()
                .flatten()
            {
                collect(&pane, grids);
            }
        } else if let Some(grid) = first_terminal(node) {
            grids.push(grid);
        }
    }
    let Some(tree) = view
        .selected_page()
        .and_then(|page| page.child().first_child())
    else {
        return;
    };
    let mut grids = Vec::new();
    collect(&tree, &mut grids);
    if grids.is_empty() {
        return;
    }
    let current = active_pane(window, view).and_then(|pane| first_terminal(&pane));
    let index = grids
        .iter()
        .position(|grid| Some(grid) == current.as_ref())
        .unwrap_or(0);
    grids[(index as isize + direction).rem_euclid(grids.len() as isize) as usize].grab_focus();
}

fn install_tab_menu(
    window: &adw::ApplicationWindow,
    view: &adw::TabView,
    titles: &Rc<SessionTitles>,
) {
    let target = Rc::new(glib::WeakRef::<adw::TabPage>::new());
    view.connect_setup_menu(glib::clone!(
        #[strong]
        target,
        move |_, page| {
            if let Some(page) = page {
                target.set(Some(page));
            }
        }
    ));
    let actions = gio::SimpleActionGroup::new();
    menu_action(
        &actions,
        "new",
        glib::clone!(
            #[weak]
            view,
            #[strong]
            titles,
            move || append_tab(&view, &titles)
        ),
    );
    menu_action(
        &actions,
        "close",
        glib::clone!(
            #[weak]
            view,
            #[strong]
            target,
            move || {
                if let Some(page) = target.upgrade() {
                    view.close_page(&page);
                }
            }
        ),
    );
    for (name, orientation) in [
        ("split-horizontal", gtk::Orientation::Horizontal),
        ("split-vertical", gtk::Orientation::Vertical),
    ] {
        menu_action(
            &actions,
            name,
            glib::clone!(
                #[weak]
                window,
                #[weak]
                view,
                #[strong]
                target,
                #[strong]
                titles,
                move || {
                    if let Some(page) = target.upgrade() {
                        view.set_selected_page(&page);
                        split_active(&window, &view, orientation, &titles);
                    }
                }
            ),
        );
    }
    window.insert_action_group("tab", Some(&actions));
    let menu = gio::Menu::new();
    menu.append(Some("New Tab"), Some("tab.new"));
    menu.append(Some("Split Side by Side"), Some("tab.split-horizontal"));
    menu.append(Some("Split Top and Bottom"), Some("tab.split-vertical"));
    menu.append(Some("Close Tab"), Some("tab.close"));
    view.set_menu_model(Some(&menu));
}

fn confirm_running_close(
    window: &adw::ApplicationWindow,
    heading: &str,
    programs: &[String],
    resolve: impl FnOnce(bool) + 'static,
) {
    if window.visible_dialog().is_some() {
        resolve(false);
        return;
    }
    let names = programs
        .iter()
        .take(5)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    let text = format!("Still running: {names}\nClosing can stop these processes.");
    let dialog = adw::AlertDialog::new(Some(heading), Some(&text));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("close", "Close Anyway");
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.set_response_appearance("close", adw::ResponseAppearance::Destructive);
    let resolve = RefCell::new(Some(resolve));
    dialog.connect_response(None, move |_, response| {
        if let Some(resolve) = resolve.take() {
            let accepted = response == "close";
            glib::idle_add_local_once(move || resolve(accepted));
        }
    });
    dialog.present(Some(window));
}

fn window_edge(w: i32, h: i32, x: f64, y: f64) -> Option<gdk::SurfaceEdge> {
    use gdk::SurfaceEdge::*;
    let horizontal = if x < 5.0 {
        -1
    } else if x >= f64::from(w) - 5.0 {
        1
    } else {
        0
    };
    let vertical = if y < 5.0 {
        -1
    } else if y >= f64::from(h) - 5.0 {
        1
    } else {
        0
    };
    match (horizontal, vertical) {
        (-1, -1) => Some(NorthWest),
        (1, -1) => Some(NorthEast),
        (-1, 1) => Some(SouthWest),
        (1, 1) => Some(SouthEast),
        (0, -1) => Some(North),
        (0, 1) => Some(South),
        (-1, 0) => Some(West),
        (1, 0) => Some(East),
        _ => None,
    }
}

fn install_window_resize(window: &adw::ApplicationWindow) {
    let edges = gtk::GestureClick::new();
    edges.set_button(gdk::BUTTON_PRIMARY);
    edges.set_propagation_phase(gtk::PropagationPhase::Capture);
    edges.connect_pressed(glib::clone!(
        #[weak]
        window,
        move |gesture, _, x, y| {
            if window.is_maximized() || window.is_fullscreen() {
                return;
            }
            if let Some(edge) = window_edge(window.width(), window.height(), x, y)
                && let Ok(surface) = window.surface().and_dynamic_cast::<gdk::Toplevel>()
                && let Some(device) = gesture.current_event_device()
                && let Some((x, y)) = gesture.current_event().and_then(|event| event.position())
            {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                surface.begin_resize(edge, Some(&device), 1, x, y, gesture.current_event_time());
            }
        }
    ));
    window.add_controller(edges);
}

fn build_window(app: &adw::Application, launch: &LaunchOptions) {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title(DEFAULT_TITLE)
        .default_width(900)
        .default_height(585)
        .build();
    window.set_icon_name(Some("io.stride.Terminal"));
    let toolbar = adw::ToolbarView::new();
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    header.add_css_class("stride-header");
    let window_handle = gtk::WindowHandle::new();
    let header_overlay = gtk::Overlay::new();
    header_overlay.set_child(Some(&header));
    window_handle.set_child(Some(&header_overlay));
    toolbar.add_top_bar(&window_handle);
    header.append(&gtk::WindowControls::new(gtk::PackType::Start));

    let view = adw::TabView::new();
    let titles = SessionTitles::new(&window, &view);
    let tab_bar = adw::TabBar::new();
    tab_bar.set_view(Some(&view));
    tab_bar.set_autohide(false);
    tab_bar.set_expand_tabs(false);
    tab_bar.set_hexpand(true);
    install_tab_menu(&window, &view, &titles);
    toolbar.set_content(Some(&view));
    window.set_content(Some(&toolbar));

    let heading = gtk::Stack::new();
    heading.set_hexpand(true);
    let single_title = adw::WindowTitle::new(DEFAULT_TITLE, "");
    window
        .bind_property("title", &single_title, "title")
        .sync_create()
        .build();
    single_title.set_halign(gtk::Align::Center);
    single_title.set_valign(gtk::Align::Center);
    single_title.set_can_target(false);
    header_overlay.add_overlay(&single_title);
    let drag_space = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let move_window = gtk::GestureDrag::new();
    move_window.set_button(gdk::BUTTON_PRIMARY);
    move_window.connect_drag_begin(glib::clone!(
        #[weak]
        window,
        move |gesture, _, _| {
            if let Ok(surface) = window.surface().and_dynamic_cast::<gdk::Toplevel>()
                && let Some(device) = gesture.current_event_device()
                && let Some((x, y)) = gesture.current_event().and_then(|event| event.position())
            {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                surface.begin_move(&device, 1, x, y, gesture.current_event_time());
            }
        }
    ));
    drag_space.add_controller(move_window);
    heading.add_named(&drag_space, Some("window-title"));
    heading.add_named(&tab_bar, Some("tabs"));
    heading.set_visible_child_name("window-title");
    view.connect_n_pages_notify(glib::clone!(
        #[weak]
        heading,
        #[weak]
        single_title,
        move |view| {
            single_title.set_visible(view.n_pages() <= 1);
            heading.set_visible_child_name(if view.n_pages() > 1 {
                "tabs"
            } else {
                "window-title"
            });
        }
    ));
    header.append(&heading);
    for (icon, tooltip, name) in [
        (
            "system-search-symbolic",
            "Find in scrollback · Ctrl+Shift+F",
            "terminal.find",
        ),
        (
            "document-open-recent-symbolic",
            "Command history",
            "terminal.history",
        ),
    ] {
        let action = button(icon, tooltip);
        action.add_css_class("flat");
        header.append(&action);
        action.connect_clicked(glib::clone!(
            #[weak]
            window,
            #[weak]
            view,
            move |_| {
                if let Some(grid) =
                    active_pane(&window, &view).and_then(|pane| first_terminal(&pane))
                {
                    let _ = grid.activate_action(name, None);
                }
            }
        ));
    }
    header.append(&gtk::WindowControls::new(gtk::PackType::End));

    view.connect_selected_page_notify(glib::clone!(
        #[strong]
        titles,
        move |_| {
            titles.focus_selected();
            titles.refresh();
        }
    ));
    window.connect_focus_widget_notify(glib::clone!(
        #[strong]
        titles,
        move |_| titles.refresh()
    ));
    view.connect_close_page(glib::clone!(
        #[weak]
        window,
        #[strong]
        titles,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |view, page| {
            let programs = titles.running_programs(&page.child());
            if programs.is_empty() {
                return glib::Propagation::Proceed;
            }
            confirm_running_close(
                &window,
                "Close tab?",
                &programs,
                glib::clone!(
                    #[weak]
                    view,
                    #[weak]
                    page,
                    move |accepted| view.close_page_finish(&page, accepted)
                ),
            );
            glib::Propagation::Stop
        }
    ));
    let confirmed = Rc::new(std::cell::Cell::new(false));
    window.connect_close_request(glib::clone!(
        #[strong]
        titles,
        move |window| {
            if confirmed.replace(false) {
                return glib::Propagation::Proceed;
            }
            let programs = titles.running_programs(window.upcast_ref());
            if programs.is_empty() {
                return glib::Propagation::Proceed;
            }
            confirm_running_close(
                window,
                "Close window?",
                &programs,
                glib::clone!(
                    #[weak]
                    window,
                    #[strong]
                    confirmed,
                    move |accepted| {
                        if accepted {
                            confirmed.set(true);
                            window.close();
                        }
                    }
                ),
            );
            glib::Propagation::Stop
        }
    ));
    view.connect_page_detached(glib::clone!(
        #[weak]
        window,
        move |view, _, _| {
            if view.n_pages() == 0 {
                window.close();
            }
        }
    ));

    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys.connect_key_pressed(glib::clone!(
        #[weak]
        window,
        #[weak]
        view,
        #[strong]
        titles,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_, key, _, mods| {
            use gdk::{Key as K, ModifierType as M};
            if window.visible_dialog().is_some() {
                return glib::Propagation::Proceed;
            }
            if !mods.contains(M::CONTROL_MASK) || mods.contains(M::ALT_MASK) {
                return glib::Propagation::Proceed;
            }
            if mods.contains(M::SHIFT_MASK) {
                match key.to_lower() {
                    K::t => append_tab(&view, &titles),
                    K::e => split_active(&window, &view, gtk::Orientation::Horizontal, &titles),
                    K::o => split_active(&window, &view, gtk::Orientation::Vertical, &titles),
                    K::w => close_focused(&window, &view, &titles),
                    K::braceleft | K::bracketleft => focus_adjacent_pane(&window, &view, -1),
                    K::braceright | K::bracketright => focus_adjacent_pane(&window, &view, 1),
                    _ => return glib::Propagation::Proceed,
                }
            } else {
                match key.to_lower() {
                    K::t => append_tab(&view, &titles),
                    K::Page_Up => {
                        view.select_previous_page();
                    }
                    K::Page_Down => {
                        view.select_next_page();
                    }
                    _ => return glib::Propagation::Proceed,
                }
            }
            glib::Propagation::Stop
        }
    ));
    window.add_controller(keys);
    install_window_resize(&window);
    append_tab_with_launch(&view, &titles, launch);
    window.present();
    titles.focus_selected();
}

fn main() -> glib::ExitCode {
    let mut launch = LaunchOptions::default();
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("-h" | "--help") => {
                println!("Usage: stride [--working-directory DIRECTORY] [-e COMMAND [ARG ...]]");
                return glib::ExitCode::SUCCESS;
            }
            Some("--version") => {
                println!("Stride {}", env!("CARGO_PKG_VERSION"));
                return glib::ExitCode::SUCCESS;
            }
            Some("--working-directory") => {
                let Some(directory) = args.next().filter(|value| !value.is_empty()) else {
                    eprintln!("stride: --working-directory requires a directory");
                    return glib::ExitCode::FAILURE;
                };
                launch.directory = Some(directory.into());
            }
            Some("-e" | "--") => {
                launch.command = args.collect();
                if launch.command.first().is_none_or(|value| value.is_empty()) {
                    eprintln!("stride: {arg:?} requires a command");
                    return glib::ExitCode::FAILURE;
                }
                break;
            }
            _ => {
                eprintln!("stride: unknown argument {arg:?}. See stride --help.");
                return glib::ExitCode::FAILURE;
            }
        }
    }
    let app = adw::Application::builder()
        .application_id("io.stride.Terminal")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_startup(|_| {
        if let Some(display) = gdk::Display::default() {
            let css = gtk::CssProvider::new();
            css.load_from_data(
                ".stride-header { min-height: 42px; padding: 0 7px; background-color: @headerbar_bg_color; color: @headerbar_fg_color; }
                 .stride-history { padding: 8px; background-color: @sidebar_bg_color; color: @sidebar_fg_color; }",
            );
            gtk::style_context_add_provider_for_display(&display, &css, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        }
    });
    app.connect_activate(move |app| build_window(app, &launch));
    app.run_with_args(&["stride"])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[track_caller]
    fn wait(state: &mut State, mut ready: impl FnMut(&State) -> bool) {
        let until = Instant::now() + Duration::from_secs(4);
        loop {
            state.read_output();
            if ready(state) {
                break;
            }
            assert!(
                Instant::now() < until,
                "shell did not reach the expected state"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn anchors_prompt_and_reveals_history_on_vertical_resize() {
        let mut terminal = Terminal::new(40, 4);
        let mut parser = vte::Parser::new();
        parser.advance(&mut terminal, b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\n> ");
        let prompt = terminal.cursor().0;
        let compact = Viewport::for_terminal(&terminal, None);
        assert_eq!(compact.display_row(prompt), 3);
        assert_eq!(compact.first, 2);

        terminal.resize(40, 8);
        let expanded = Viewport::for_terminal(&terminal, None);
        assert_eq!(terminal.cursor().0, prompt);
        assert_eq!(expanded.first, 0);
        assert_eq!(expanded.display_row(prompt), 7);
        assert_eq!(expanded.terminal_row(7), prompt);
        assert_eq!(
            terminal.text((expanded.first, 0), (expanded.first, 2)),
            "one"
        );

        parser.advance(&mut terminal, b"\x1b[?1049h\x1b[3;1Hvim");
        let fullscreen = Viewport::for_terminal(&terminal, None);
        assert_eq!(fullscreen.first, 0);
        assert_eq!(fullscreen.blank_above, 0);
        assert_eq!(fullscreen.display_row(terminal.cursor().0), 2);
    }

    #[test]
    fn keeps_the_live_prompt_separate_from_scrolled_output() {
        let mut terminal = Terminal::new(40, 4);
        let mut parser = vte::Parser::new();
        parser.advance(
            &mut terminal,
            b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\n\x1b]133;A\x07> draft",
        );
        let prompt = terminal.cursor().0;
        let view = Viewport::for_terminal(&terminal, Some(0));
        assert_eq!(view.rows().collect::<Vec<_>>(), vec![0, 1, 2, prompt]);
        assert_eq!(view.display_row(prompt), 3);
        assert_eq!(view.terminal_row(3), prompt);
        assert_eq!(view.selection_edge(1, prompt), Some(2));
        assert_eq!(view.selection_edge(prompt, 1), Some(prompt));
        let live = Viewport::for_terminal(&terminal, None);
        assert!(live.pinned.is_empty());
        assert_eq!(live.display_row(prompt), 3);
    }

    #[test]
    #[ignore = "requires a GTK display and a temporary HOME"]
    fn opens_tabs_splits_nested_panes_and_closes_only_the_focused_pane() {
        fn find_widget<T: IsA<gtk::Widget>>(widget: &gtk::Widget) -> Option<T> {
            if let Some(found) = widget.downcast_ref::<T>() {
                return Some(found.clone());
            }
            let mut child = widget.first_child();
            while let Some(widget) = child {
                if let Some(found) = find_widget::<T>(&widget) {
                    return Some(found);
                }
                child = widget.next_sibling();
            }
            None
        }
        adw::init().unwrap();
        let app = adw::Application::builder()
            .application_id("io.stride.Terminal.Test")
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gio::Cancellable>).unwrap();
        build_window(&app, &LaunchOptions::default());
        let window = app
            .active_window()
            .and_downcast::<adw::ApplicationWindow>()
            .unwrap();
        let context = glib::MainContext::default();
        for _ in 0..30 {
            while context.pending() {
                context.iteration(false);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let view = find_widget::<adw::TabView>(window.upcast_ref()).unwrap();
        let bar = find_widget::<adw::TabBar>(window.upcast_ref()).unwrap();
        let heading = bar.parent().and_downcast::<gtk::Stack>().unwrap();
        assert_eq!(
            heading.visible_child_name().as_deref(),
            Some("window-title")
        );
        assert_eq!(window.title().as_deref(), Some("Stride"));
        assert_eq!(view.n_pages(), 1);
        let initial_page = view.selected_page().unwrap();
        let original = active_pane(&window, &view).unwrap();
        assert_eq!(view.menu_model().unwrap().n_items(), 4);
        let grid = first_terminal(&original).unwrap();
        assert!(find_widget::<gtk::PasswordEntry>(&original).is_none());
        let controllers = grid.observe_controllers();
        let menu_click = (0..controllers.n_items())
            .find_map(|index| {
                controllers
                    .item(index)
                    .and_downcast::<gtk::GestureClick>()
                    .filter(|controller| controller.button() == gdk::BUTTON_SECONDARY)
            })
            .unwrap();
        menu_click.emit_by_name::<()>("released", &[&1i32, &30.0f64, &40.0f64]);
        let menu = grid.first_child().and_downcast::<gtk::Popover>().unwrap();
        assert!(menu.is_visible());
        menu.popdown();
        while context.pending() {
            context.iteration(false);
        }

        let input = (0..controllers.n_items())
            .find_map(|index| {
                controllers
                    .item(index)
                    .and_downcast::<gtk::EventControllerKey>()
                    .filter(|keys| keys.propagation_phase() != gtk::PropagationPhase::Capture)
            })
            .unwrap();
        let selection_keys = (0..controllers.n_items())
            .find_map(|index| {
                controllers
                    .item(index)
                    .and_downcast::<gtk::EventControllerKey>()
                    .filter(|keys| keys.name().as_deref() == Some("stride-input-selection"))
            })
            .unwrap();
        for key in [
            gdk::Key::w,
            gdk::Key::r,
            gdk::Key::o,
            gdk::Key::n,
            gdk::Key::g,
        ] {
            assert!(
                input.emit_by_name::<bool>(
                    "key-pressed",
                    &[&key, &0u32, &gdk::ModifierType::empty()]
                )
            );
        }
        assert!(selection_keys.emit_by_name::<bool>(
            "key-pressed",
            &[&gdk::Key::a, &0u32, &gdk::ModifierType::CONTROL_MASK,]
        ));
        assert!(selection_keys.emit_by_name::<bool>(
            "key-pressed",
            &[&gdk::Key::BackSpace, &0u32, &gdk::ModifierType::empty(),]
        ));
        let deletion_deadline = Instant::now() + Duration::from_millis(320);
        while Instant::now() < deletion_deadline {
            while context.pending() {
                context.iteration(false);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        for key in [
            gdk::Key::c,
            gdk::Key::d,
            gdk::Key::space,
            gdk::Key::slash,
            gdk::Key::u,
            gdk::Key::s,
            gdk::Key::r,
            gdk::Key::Return,
        ] {
            assert!(
                input.emit_by_name::<bool>(
                    "key-pressed",
                    &[&key, &0u32, &gdk::ModifierType::empty()]
                )
            );
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while window.title().as_deref() != Some("usr — Stride") {
            while context.pending() {
                context.iteration(false);
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(initial_page.title(), "usr");
        let single = find_widget::<adw::WindowTitle>(window.upcast_ref()).unwrap();
        assert!(single.is_visible());
        assert_eq!(single.title(), "usr — Stride");
        let handle = find_widget::<gtk::WindowHandle>(window.upcast_ref()).unwrap();
        let bounds = single.compute_bounds(&handle).unwrap();
        assert!((bounds.x() + bounds.width() * 0.5 - handle.width() as f32 * 0.5).abs() <= 1.0);

        let controllers = window.observe_controllers();
        let shortcuts = (0..controllers.n_items())
            .find_map(|index| {
                controllers
                    .item(index)
                    .and_downcast::<gtk::EventControllerKey>()
                    .filter(|keys| keys.propagation_phase() == gtk::PropagationPhase::Capture)
            })
            .unwrap();
        assert!(shortcuts.emit_by_name::<bool>(
            "key-pressed",
            &[&gdk::Key::t, &0u32, &gdk::ModifierType::CONTROL_MASK,]
        ));
        assert_eq!(view.n_pages(), 2);
        assert_eq!(heading.visible_child_name().as_deref(), Some("tabs"));
        let second_page = view.selected_page().unwrap();
        assert_ne!(second_page, initial_page);
        assert_ne!(active_pane(&window, &view).unwrap(), original);
        view.set_selected_page(&initial_page);
        assert_eq!(active_pane(&window, &view), Some(original.clone()));

        let split_keys = gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK;
        menu_click.emit_by_name::<()>("released", &[&1i32, &30.0f64, &40.0f64]);
        let menu = grid.first_child().and_downcast::<gtk::Popover>().unwrap();
        let context_stack = menu.child().and_downcast::<gtk::Stack>().unwrap();
        let context_item = |section, name| {
            let mut child = context_stack.child_by_name(section).unwrap().first_child();
            loop {
                let entry = child.expect("Context menu item missing");
                if entry.widget_name() == name {
                    break entry.downcast::<gtk::Button>().unwrap();
                }
                child = entry.next_sibling();
            }
        };
        context_item("main", "stride-menu-split").emit_clicked();
        assert_eq!(context_stack.visible_child_name().as_deref(), Some("split"));
        context_item("split", "stride-menu-split-vertical").emit_clicked();
        while context.pending() {
            context.iteration(false);
        }
        let outer = initial_page
            .child()
            .first_child()
            .and_downcast::<gtk::Paned>()
            .unwrap();
        assert_eq!(outer.orientation(), gtk::Orientation::Horizontal);
        assert_eq!(outer.start_child(), Some(original.clone()));
        assert_eq!(outer.end_child(), active_pane(&window, &view));

        assert!(shortcuts.emit_by_name::<bool>("key-pressed", &[&gdk::Key::o, &0u32, &split_keys]));
        let inner = outer.end_child().and_downcast::<gtk::Paned>().unwrap();
        assert_eq!(inner.orientation(), gtk::Orientation::Vertical);
        let surviving = inner.start_child().unwrap();
        let closing = first_terminal(&inner.end_child().unwrap()).unwrap();
        let menu_controllers = closing.observe_controllers();
        let context_click = (0..menu_controllers.n_items())
            .find_map(|index| {
                menu_controllers
                    .item(index)
                    .and_downcast::<gtk::GestureClick>()
                    .filter(|gesture| gesture.button() == gdk::BUTTON_SECONDARY)
            })
            .unwrap();
        context_click.emit_by_name::<()>("released", &[&1i32, &30.0f64, &40.0f64]);
        let menu = closing
            .first_child()
            .and_downcast::<gtk::Popover>()
            .unwrap();
        let mut child = menu
            .child()
            .and_downcast::<gtk::Stack>()
            .unwrap()
            .child_by_name("main")
            .unwrap()
            .first_child();
        let close_entry = loop {
            let entry = child.expect("Close Pane menu item missing");
            if entry.widget_name() == "stride-menu-close-pane" {
                break entry.downcast::<gtk::Button>().unwrap();
            }
            child = entry.next_sibling();
        };
        assert!(close_entry.is_sensitive());
        close_entry.emit_clicked();
        while context.pending() {
            context.iteration(false);
        }
        assert_eq!(outer.end_child(), Some(surviving.clone()));
        assert_eq!(outer.start_child(), Some(original.clone()));
        assert_eq!(view.n_pages(), 2);

        let history = find_widget::<adw::OverlaySplitView>(&original).unwrap();
        assert!(!history.shows_sidebar());
        grid.activate_action("terminal.history", None).unwrap();
        assert!(history.shows_sidebar());
        grid.activate_action("terminal.history", None).unwrap();
        assert!(!history.shows_sidebar());

        if Path::new("/usr/bin/top").is_file() {
            let live_grid = first_terminal(&surviving).unwrap();
            let events = live_grid.observe_controllers();
            let keyboard = (0..events.n_items())
                .find_map(|index| {
                    events
                        .item(index)
                        .and_downcast::<gtk::EventControllerKey>()
                        .filter(|keys| keys.propagation_phase() != gtk::PropagationPhase::Capture)
                })
                .unwrap();
            let press = (0..events.n_items())
                .find_map(|index| {
                    events
                        .item(index)
                        .and_downcast::<gtk::GestureClick>()
                        .filter(|click| click.button() == gdk::BUTTON_PRIMARY)
                })
                .unwrap();
            for key in [gdk::Key::t, gdk::Key::o, gdk::Key::p, gdk::Key::Return] {
                assert!(keyboard.emit_by_name::<bool>(
                    "key-pressed",
                    &[&key, &0u32, &gdk::ModifierType::empty()],
                ));
            }
            let wait_for_window = |condition: &dyn Fn() -> bool| {
                let deadline = Instant::now() + Duration::from_secs(4);
                while !condition() {
                    while context.pending() {
                        context.iteration(false);
                    }
                    assert!(
                        Instant::now() < deadline,
                        "GTK did not reach the expected state"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
            };
            // The live app owns only its pane while the Gtk window can resize independently.
            wait_for_window(&|| initial_page.title().starts_with("top ·"));
            for scope in ["Close pane?", "Close tab?", "Close window?"] {
                match scope {
                    "Close pane?" => {
                        live_grid
                            .activate_action("terminal.close-pane", None)
                            .unwrap();
                    }
                    "Close tab?" => view.close_page(&initial_page),
                    _ => window.close(),
                }
                wait_for_window(&|| window.visible_dialog().is_some());
                let dialog = window
                    .visible_dialog()
                    .and_downcast::<adw::AlertDialog>()
                    .unwrap();
                assert_eq!(dialog.heading().as_deref(), Some(scope));
                assert!(dialog.body().contains("top"));
                assert!(dialog.close());
                wait_for_window(&|| window.visible_dialog().is_none());
                while context.pending() {
                    context.iteration(false);
                }
                assert_eq!(view.n_pages(), 2);
                assert_eq!(outer.end_child(), Some(surviving.clone()));
            }
            let width = window.width();
            window.set_default_size(width + 90, window.height() + 60);
            wait_for_window(&|| window.width() > width);

            grid.grab_focus();
            assert_eq!(active_pane(&window, &view), Some(original.clone()));
            press.emit_by_name::<()>("pressed", &[&1i32, &30.0f64, &40.0f64]);
            assert_eq!(active_pane(&window, &view), Some(surviving));
            assert!(keyboard.emit_by_name::<bool>(
                "key-pressed",
                &[&gdk::Key::c, &0u32, &gdk::ModifierType::CONTROL_MASK],
            ));
            wait_for_window(&|| !initial_page.title().starts_with("top ·"));

            for key in [gdk::Key::t, gdk::Key::o, gdk::Key::p, gdk::Key::Return] {
                assert!(keyboard.emit_by_name::<bool>(
                    "key-pressed",
                    &[&key, &0u32, &gdk::ModifierType::empty()],
                ));
            }
            wait_for_window(&|| initial_page.title().starts_with("top ·"));
            live_grid
                .activate_action("terminal.close-pane", None)
                .unwrap();
            wait_for_window(&|| window.visible_dialog().is_some());
            let dialog = window.visible_dialog().unwrap();
            let mut descendants = vec![dialog.upcast::<gtk::Widget>()];
            let confirm = loop {
                let widget = descendants.pop().expect("Close Anyway button missing");
                if let Some(button) = widget.downcast_ref::<gtk::Button>()
                    && button.label().as_deref() == Some("Close Anyway")
                {
                    break button.clone();
                }
                let mut child = widget.first_child();
                while let Some(widget) = child {
                    child = widget.next_sibling();
                    descendants.push(widget);
                }
            };
            confirm.emit_clicked();
            wait_for_window(&|| initial_page.child().first_child() == Some(original.clone()));
        }
        assert_eq!(view.selected_page(), Some(initial_page.clone()));
        view.emit_by_name::<()>("setup-menu", &[&Some(second_page.clone())]);
        bar.activate_action("tab.split-horizontal", None).unwrap();
        let tab_split = second_page
            .child()
            .first_child()
            .and_downcast::<gtk::Paned>()
            .unwrap();
        assert_eq!(tab_split.orientation(), gtk::Orientation::Horizontal);
        assert_eq!(view.selected_page(), Some(second_page.clone()));

        let exit_shell = |pane: &gtk::Widget| {
            let controllers = first_terminal(pane).unwrap().observe_controllers();
            let input = (0..controllers.n_items())
                .find_map(|index| {
                    controllers
                        .item(index)
                        .and_downcast::<gtk::EventControllerKey>()
                        .filter(|keys| keys.propagation_phase() != gtk::PropagationPhase::Capture)
                })
                .unwrap();
            for key in [
                gdk::Key::e,
                gdk::Key::x,
                gdk::Key::i,
                gdk::Key::t,
                gdk::Key::Return,
            ] {
                assert!(input.emit_by_name::<bool>(
                    "key-pressed",
                    &[&key, &0u32, &gdk::ModifierType::empty()]
                ));
            }
        };
        let wait_for_exit = |condition: &dyn Fn() -> bool| {
            let deadline = Instant::now() + Duration::from_secs(4);
            while !condition() {
                while context.pending() {
                    context.iteration(false);
                }
                assert!(
                    Instant::now() < deadline,
                    "Exited shell did not close its view"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        let remaining = tab_split.start_child().unwrap();
        exit_shell(&tab_split.end_child().unwrap());
        wait_for_exit(&|| second_page.child().first_child().as_ref() == Some(&remaining));
        assert_eq!(view.n_pages(), 2);

        view.emit_by_name::<()>("setup-menu", &[&Some(second_page.clone())]);
        bar.activate_action("tab.new", None).unwrap();
        let extra_page = view.selected_page().unwrap();
        assert_eq!(view.n_pages(), 3);
        view.set_selected_page(&initial_page);
        view.emit_by_name::<()>("setup-menu", &[&Some(extra_page)]);
        bar.activate_action("tab.close", None).unwrap();
        assert_eq!(view.n_pages(), 2);

        exit_shell(&remaining);
        wait_for_exit(&|| view.n_pages() == 1);
        assert_eq!(view.selected_page().as_ref(), Some(&initial_page));
        assert_eq!(
            heading.visible_child_name().as_deref(),
            Some("window-title")
        );

        exit_shell(&original);
        wait_for_exit(&|| !window.is_visible());
    }

    #[test]
    fn ssh_prompt_breadcrumb_uses_home_and_absolute_paths() {
        assert_eq!(
            ssh_prompt_location("wuild@voracious:~$ "),
            Some(("wuild@voracious", "~", 19))
        );
        assert_eq!(
            ssh_prompt_location("root@host.example:/var/Project Folder# ls"),
            Some(("root@host.example", "/var/Project Folder", 39))
        );
        assert_eq!(
            ssh_prompt_location("Last login: 14:06:14 from 192.0.2.3"),
            None
        );
        assert_eq!(ssh_prompt_location("host:~/missing-user$ "), None);
    }

    #[test]
    fn ssh_breadcrumb_decodes_paths_without_changing_local_directory() {
        let mut terminal = Terminal::new(40, 4);
        vte::Parser::new().advance(
            &mut terminal,
            b"\x1b]133;W;/local\x07\x1b]7;file://remote/home/user/a%20folder\x07",
        );
        assert_eq!(terminal.directory.as_deref(), Some("/local"));
        let path = ssh_reported_path(terminal.reported_directory.as_deref().unwrap()).unwrap();
        assert_eq!(path.as_str(), "/home/user/a folder");
        assert_eq!(
            format_ssh_breadcrumb("user@remote", &path),
            "user@remote  ›  …  ›  a folder"
        );
        assert!(ssh_reported_path("file://remote/home/%0Abad").is_none());
        assert!(ssh_reported_path("https://remote/home").is_none());
    }

    #[test]
    fn suggestion_click_targets_the_visible_choice_after_scrolling() {
        let menu = SuggestionMenu {
            x: 30.0,
            y: 20.0,
            width: 160.0,
            row_height: 24.0,
            first: 3,
            slots: 2,
        };
        assert_eq!(menu.hit(40.0, 25.0), Some(3));
        assert_eq!(menu.hit(40.0, 48.0), Some(4));
        assert_eq!(menu.hit(40.0, 23.0), None);
        assert_eq!(menu.hit(40.0, 72.0), None);
        assert_eq!(menu.hit(190.0, 48.0), None);
    }

    #[test]
    fn inline_password_mask_resets_when_the_prompt_changes() {
        let mut mask = PasswordMask::default();
        mask.track(Some((2, 20)));
        mask.count = 5;
        mask.track(Some((2, 20)));
        assert_eq!(mask.count, 5);
        mask.track(Some((4, 20)));
        assert_eq!(mask.count, 0);
        mask.count = 3;
        mask.track(None);
        assert_eq!(mask.count, 0);
    }

    #[test]
    fn completion_waits_for_readline_to_redraw_the_prompt() {
        let now = Instant::now();
        let mut frame = PreviewFrame {
            bytes: b"\r\x1b[K".to_vec(),
            started: now,
            last_input: now,
            revision: 0,
        };
        assert!(!frame.complete(now + Duration::from_millis(18)));
        frame.bytes.extend_from_slice(
            b"\x1b]133;I;sudo dnf install \"name\"\x07\x1b]133;B\x07sudo dnf install \"name\"",
        );
        assert!(!frame.complete(now + Duration::from_millis(8)));
        assert!(frame.complete(now + Duration::from_millis(18)));
    }

    #[test]
    fn resize_handles_are_limited_to_the_outer_edges() {
        assert_eq!(
            window_edge(900, 585, 2.0, 2.0),
            Some(gdk::SurfaceEdge::NorthWest)
        );
        assert_eq!(
            window_edge(900, 585, 500.0, 583.0),
            Some(gdk::SurfaceEdge::South)
        );
        assert_eq!(window_edge(900, 585, 888.0, 10.0), None);
    }

    #[test]
    fn drag_selection_keeps_its_anchor_while_scrolling() {
        let (mut state, _) = State::new(&LaunchOptions {
            command: vec![std::ffi::OsString::new()],
            ..LaunchOptions::default()
        });
        state.terminal = Terminal::new(40, 4);
        state.parser.advance(
            &mut state.terminal,
            b"zero\r\none\r\ntwo\r\nthree\r\nfour\r\nfive",
        );
        let anchor = state.point(13.0, 71.0);
        state.selection_drag = Some((anchor, (13.0, 11.0)));
        state.extend_selection_drag(13.0, 11.0);
        let original_start = state.selection.unwrap().0;
        state.scroll(-2);
        let (start, end) = state.selection.unwrap();
        assert!(start < original_start);
        assert_eq!(end, anchor);
        assert_eq!(
            state.terminal.text(start, end),
            "zero\none\ntwo\nthree\nfour\nf"
        );

        state.selection_drag = None;
        state.scroll(2);
        assert_eq!(state.selection, Some((start, end)));
    }

    #[test]
    fn selection_ignores_empty_input_cells_but_keeps_text_indentation() {
        let mut terminal = Terminal::new(40, 5);
        let mut parser = vte::Parser::new();
        parser.advance(
            &mut terminal,
            b"  code\r\n\x1b[48;2;3;7;11m    \x1b[0m  \x1b]133;B\x07",
        );
        assert!(terminal.trim_selection((1, 6), (1, 39)).is_none());
        assert!(terminal.trim_selection((2, 0), (4, 39)).is_none());
        let (start, end) = terminal.trim_selection((0, 39), (0, 0)).unwrap();
        assert_eq!(terminal.text(start, end), "  code");
        let (start, end) = terminal.trim_selection((1, 0), (1, 39)).unwrap();
        assert!(terminal.text(start, end).trim().is_empty());
    }

    #[test]
    fn sends_control_keys_to_the_terminal() {
        use gdk::{Key as K, ModifierType as M};
        assert_eq!(key_sequence(K::v, M::CONTROL_MASK, false), Some(vec![22]));
        assert_eq!(key_sequence(K::Return, M::empty(), false), Some(vec![13]));
        assert_eq!(
            key_sequence(K::Right, M::empty(), true),
            Some(b"\x1bOC".to_vec())
        );
    }

    #[test]
    #[ignore = "run with SHELL=/bin/bash and a temporary HOME"]
    fn live_bash_prediction_paste_interrupt_and_hidden_password() {
        let (mut state, error) = State::new(&LaunchOptions::default());
        assert!(error.is_none(), "{error:?}");
        wait(&mut state, State::prompt);
        assert!(!state.terminal.prompt_icons.is_empty());
        for part in [
            b"sudo dnf install ".as_slice(),
            b"\"",
            b"n",
            b"a",
            b"m",
            b"e",
            b"\"",
        ] {
            state.edit(part, true);
        }
        wait(&mut state, |s| {
            s.terminal.input == "sudo dnf install \"name\""
        });
        assert!(state.terminal.commands.is_empty());
        let generation = state.terminal.prompt_generation;
        state.edit([3], false);
        wait(&mut state, |s| {
            s.terminal.prompt_generation != generation && s.prompt()
        });

        state.edit(b"echo alpha beta".as_slice(), true);
        wait(&mut state, |s| s.terminal.input == "echo alpha beta");
        state.request_input_action(InputAction::SelectAll);
        wait(&mut state, |s| s.selection.is_some());
        assert_eq!(
            state.take_copy_selection().as_deref(),
            Some("echo alpha beta")
        );

        let (row, col) = state.terminal.input_start.unwrap();
        let x = 12.0 + (col as f64 + 11.1) * state.cell.0;
        let y = state.grid_top + (state.viewport().display_row(row) as f64 + 0.5) * state.cell.1;
        state.click_input(x, y);
        wait(&mut state, |s| {
            s.terminal.cursor() == (row, col + 11) && !s.preview_pending()
        });
        state.edit(b"Z".as_slice(), true);
        wait(&mut state, |s| s.terminal.input == "echo alpha Zbeta");

        let (row, col) = state.terminal.input_start.unwrap();
        state.selection = Some(((row, col + 5), (row, col + 10)));
        state.request_input_action(InputAction::Delete((row, col + 5), (row, col + 10)));
        wait(&mut state, |s| s.terminal.input == "echo Zbeta");
        state.request_input_action(InputAction::SelectAll);
        wait(&mut state, |s| s.selection.is_some());
        let (start, end) = state.selection.unwrap();
        state.request_input_action(InputAction::Delete(start, end));
        wait(&mut state, |s| {
            s.terminal.input.is_empty() && !s.preview_pending()
        });
        assert!(state.terminal.commands.is_empty());

        state.edit(b"print".as_slice(), true);
        wait(&mut state, |s| !s.terminal.ghost.is_empty());
        assert!(state.terminal.commands.is_empty());
        let generation = state.terminal.prompt_generation;
        state.edit([3], false);
        wait(&mut state, |s| {
            s.terminal.prompt_generation != generation && s.prompt()
        });

        state.edit(b"cd /usr/sh".as_slice(), true);
        wait(&mut state, |s| s.terminal.ghost == "are/");
        state.edit(ACCEPT, false);
        wait(&mut state, |s| s.terminal.input == "cd /usr/share/");
        assert!(state.terminal.commands.is_empty());
        let generation = state.terminal.prompt_generation;
        state.edit([3], false);
        wait(&mut state, |s| {
            s.terminal.prompt_generation != generation && s.prompt()
        });

        state.paste("printf 'stride-native\\n'");
        state.edit([13], false);
        wait(&mut state, |s| s.terminal.commands.len() == 1 && s.prompt());
        let entry = state.terminal.commands.back().unwrap();
        assert_eq!(
            state
                .terminal
                .text((entry.start, 0), entry.end.unwrap())
                .trim_end(),
            "stride-native"
        );

        state.edit(b"printf 'stride".as_slice(), true);
        wait(&mut state, |s| s.terminal.ghost == "-native\\n'");
        state.edit(ACCEPT, false);
        wait(&mut state, |s| {
            s.terminal.input == "printf 'stride-native\\n'"
        });
        assert_eq!(state.terminal.commands.len(), 1);
        state.edit([13], false);
        wait(&mut state, |s| s.terminal.commands.len() == 2 && s.prompt());

        state.edit(b"sleep 20\r".as_slice(), false);
        wait(&mut state, |s| {
            !s.terminal.at_prompt && !s.session.as_ref().unwrap().shell_foreground()
        });
        wait(&mut state, |s| pane_title(s) == "sleep · Home");
        state.edit([3], false);
        wait(&mut state, |s| s.terminal.commands.len() == 3 && s.prompt());
        assert_eq!(state.terminal.commands.back().unwrap().status, 130);

        state.edit(
            b"read -rs -p 'Password: ' value; test -n \"$value\"\r".as_slice(),
            false,
        );
        wait(&mut state, |s| {
            !s.terminal.at_prompt
                && s.terminal
                    .text((s.terminal.cursor().0, 0), (s.terminal.cursor().0, 12))
                    .starts_with("Password: ")
        });
        state.edit(b"stride-dummy-secret\r".as_slice(), true);
        wait(&mut state, |s| s.terminal.commands.len() == 4 && s.prompt());
        assert_eq!(state.terminal.commands.back().unwrap().status, 0);
        let text = state.terminal.text(
            (0, 0),
            (
                state.terminal.live_top() + state.terminal.rows - 1,
                state.terminal.cols - 1,
            ),
        );
        assert!(!text.contains("stride-dummy-secret"));

        state.edit(b"test \"$PWD\" = \"$HOME\"\r".as_slice(), false);
        wait(&mut state, |s| s.terminal.commands.len() == 5 && s.prompt());
        assert_eq!(state.terminal.commands.back().unwrap().status, 0);
        let home = std::env::home_dir().unwrap();
        let name = home.file_name().unwrap().to_string_lossy();
        let (row, _) = state.terminal.cursor();
        let prompt = state
            .terminal
            .text((row, 0), (row, state.terminal.cols - 1));
        let user = std::process::Command::new("id")
            .arg("-un")
            .output()
            .unwrap();
        let user = String::from_utf8(user.stdout).unwrap();
        let user = user.trim();
        assert_eq!(prompt.trim(), user);
        assert!(!prompt.contains(name.as_ref()));
        assert_eq!(state.folder_target(0), Some(home.as_path()));
        assert!(!prompt.contains(home.to_string_lossy().as_ref()));

        state.edit(
            b"mkdir -p Git/fishmaster; cd Git/fishmaster\r".as_slice(),
            false,
        );
        wait(&mut state, |s| s.terminal.commands.len() == 6 && s.prompt());
        let (row, _) = state.terminal.cursor();
        let prompt = state
            .terminal
            .text((row, 0), (row, state.terminal.cols - 1));
        assert_eq!(
            prompt.split_whitespace().collect::<Vec<_>>(),
            [user, "Git", "fishmaster"]
        );
        assert!(!prompt.contains(name.as_ref()));
        assert_eq!(state.folder_target(0), Some(home.as_path()));
        assert_eq!(state.folder_target(1), Some(home.join("Git").as_path()));
        assert_eq!(
            state.folder_target(2),
            Some(home.join("Git/fishmaster").as_path())
        );

        state.edit(b"cd /usr/share\r".as_slice(), false);
        wait(&mut state, |s| s.terminal.commands.len() == 7 && s.prompt());
        let (row, _) = state.terminal.cursor();
        let path = state
            .terminal
            .text((row, 0), (row, state.terminal.cols - 1));
        assert!(path.contains(" /     usr     share "));
        let cells = state.terminal.line(row).unwrap();
        assert!(
            cells
                .iter()
                .any(|cell| cell.bg == PROMPT_DIRECTORY_ALT && cell.glyph == 'u')
        );
        assert!(
            cells
                .iter()
                .any(|cell| cell.bg == PROMPT_DIRECTORY && cell.glyph == 's')
        );
        assert!(
            state
                .terminal
                .folder_prompts
                .iter()
                .any(|prompt| prompt.directory == home.to_string_lossy())
        );
        assert_eq!(state.terminal.folder_spans.len(), 3);
        assert_eq!(state.folder_target(0), Some(Path::new("/")));
        let (start, _) = state.terminal.folder_spans[1];
        let x = 12.0 + (start.1 as f64 + 0.5) * state.cell.0;
        let y =
            state.grid_top + (state.viewport().display_row(start.0) as f64 + 0.5) * state.cell.1;
        let target = state.folder_at(x, y).unwrap();
        assert_eq!(target, Path::new("/usr"));
        state.edit(b"echo keep-this-draft".as_slice(), true);
        wait(&mut state, |s| s.terminal.input == "echo keep-this-draft");
        state.request_navigation(target.clone());
        wait(&mut state, |s| s.navigation_blocked);
        assert_eq!(state.terminal.directory.as_deref(), Some("/usr/share"));
        assert_eq!(state.terminal.input, "echo keep-this-draft");
        let generation = state.terminal.prompt_generation;
        state.edit([3], false);
        wait(&mut state, |s| {
            s.terminal.prompt_generation != generation && s.prompt()
        });
        state.navigation_blocked = false;
        state.request_navigation(target);
        wait(&mut state, |s| {
            s.terminal.directory.as_deref() == Some("/usr") && s.prompt()
        });

        state.edit(b"printf '%s\\n' {1..60}\r".as_slice(), false);
        wait(&mut state, |s| s.terminal.commands.len() == 9 && s.prompt());
        state.scroll(-5);
        let view = state.top.unwrap();
        state.edit(b"cd /usr/sh".as_slice(), true);
        wait(&mut state, |s| s.terminal.ghost == "are/");
        assert_eq!(state.top, Some(view));
        assert!(state.viewport().pinned.contains(&state.terminal.cursor().0));
        state.edit(ACCEPT, false);
        wait(&mut state, |s| s.terminal.input == "cd /usr/share/");
        assert_eq!(state.top, Some(view));
        state.edit([13], false);
        wait(&mut state, |s| {
            s.terminal.directory.as_deref() == Some("/usr/share") && s.prompt()
        });
        assert!(state.top.is_none());

        for (index, command) in [
            "echo stride-repeated",
            "true",
            "echo stride-repeated",
            "echo stride-recent",
        ]
        .iter()
        .enumerate()
        {
            state.edit(format!("{command}\r").into_bytes(), false);
            wait(&mut state, |s| {
                s.terminal.commands.len() == 11 + index && s.prompt()
            });
        }
        let (mut reopened, error) = State::new(&LaunchOptions::default());
        assert!(error.is_none(), "{error:?}");
        wait(&mut reopened, State::prompt);
        assert!(
            reopened
                .terminal
                .archive
                .iter()
                .any(|entry| entry == "echo stride-repeated")
        );

        reopened.edit(b"sudo echo stride-re".as_slice(), true);
        wait(&mut reopened, |s| s.terminal.ghost == "peated");
        wait(&mut reopened, |s| {
            s.terminal.suggestions.iter().any(|value| value == "cent")
        });
        let other = reopened
            .terminal
            .suggestions
            .iter()
            .position(|value| value == "cent")
            .unwrap();
        for _ in 0..other {
            reopened.move_suggestion(1);
        }
        assert_eq!(reopened.selected_suggestion(), "cent");
        assert!(reopened.accept_navigated_suggestion());
        wait(&mut reopened, |s| {
            s.terminal.input == "sudo echo stride-recent"
        });
        assert!(reopened.terminal.commands.is_empty());

        let generation = reopened.terminal.prompt_generation;
        reopened.edit([3], false);
        wait(&mut reopened, |s| {
            s.terminal.prompt_generation != generation && s.prompt()
        });
        reopened.edit(b"systemctl daemon-reload".as_slice(), true);
        wait(&mut reopened, |s| {
            s.terminal.input == "systemctl daemon-reload"
        });
        if Path::new("/usr/bin/systemctl").is_file() {
            reopened.edit(PREPARE_ELEVATED, false);
            wait(&mut reopened, |s| {
                s.terminal.elevation_prepared == Some(true)
                    && s.terminal.input == "sudo -- systemctl daemon-reload"
            });
            assert!(reopened.terminal.commands.is_empty());
        }

        state.edit(b"sleep 20 &\r".as_slice(), false);
        wait(&mut state, |s| {
            s.prompt()
                && s.session
                    .as_ref()
                    .unwrap()
                    .running_programs()
                    .iter()
                    .any(|program| program == "sleep")
        });
        state.edit(b"kill %1\r".as_slice(), false);
        wait(&mut state, |s| {
            s.prompt() && s.session.as_ref().unwrap().running_programs().is_empty()
        });

        let deep = "Git/fishmaster/hidden-a/hidden-b/a-very-long-folder-name-for-stride";
        let path = home.join(deep);
        state.edit(format!("cd; mkdir -p {deep}; cd {deep}\r"), false);
        wait(&mut state, |s| {
            s.prompt() && s.terminal.directory.as_deref() == path.to_str()
        });
        let (row, _) = state.terminal.cursor();
        let prompt = state
            .terminal
            .text((row, 0), (row, state.terminal.cols - 1));
        assert_eq!(
            prompt.split_whitespace().collect::<Vec<_>>(),
            [user, "…", "a-very-long-folder-na…"]
        );
        assert_eq!(state.terminal.folder_spans.len(), 3);
        assert_eq!(state.folder_target(0), Some(home.as_path()));
        assert_eq!(state.folder_target(1), Some(path.as_path()));
        assert_eq!(state.folder_target(2), Some(path.as_path()));
    }
}
