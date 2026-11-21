use std::collections::VecDeque;

use crate::color::Color32;
use unicode_width::UnicodeWidthChar;
use vte::{Params, Perform};

pub const FG: Color32 = Color32::from_rgb(219, 227, 238);
pub const BG: Color32 = Color32::from_gray(25);
const PALETTE: [u32; 16] = [
    0x222a37, 0xf47983, 0x8dce9d, 0xedc584, 0x87adf8, 0xc9a3ee, 0x78d0cd, 0xc7d2e2, 0x78869a,
    0xffa4ae, 0xaeedba, 0xffdb9f, 0xadcbff, 0xe2c2ff, 0xa7efea, 0xf0f5fc,
];

fn indexed(index: u16) -> Color32 {
    match index {
        0..=15 => {
            let rgb = PALETTE[index as usize];
            Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
        }
        16..=231 => {
            let i = index - 16;
            let level = |n| if n == 0 { 0 } else { 55 + 40 * n as u8 };
            Color32::from_rgb(level(i / 36), level(i / 6 % 6), level(i % 6))
        }
        232..=255 => Color32::from_gray(8 + ((index - 232) * 10) as u8),
        _ => FG,
    }
}

#[derive(Clone, Copy)]
struct Style {
    fg: Color32,
    bg: Color32,
    reverse: bool,
    underline: bool,
    hidden: bool,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            fg: FG,
            bg: BG,
            reverse: false,
            underline: false,
            hidden: false,
        }
    }
}

#[derive(Clone)]
pub struct Cell {
    pub glyph: char,
    pub extra: String,
    pub width: u8,
    pub fg: Color32,
    pub bg: Color32,
    pub underline: bool,
}

impl Cell {
    fn blank(style: Style) -> Self {
        let (fg, bg) = if style.reverse {
            (style.bg, style.fg)
        } else {
            (style.fg, style.bg)
        };
        Self {
            glyph: ' ',
            extra: String::new(),
            width: 1,
            fg,
            bg,
            underline: style.underline,
        }
    }
}

type Row = Vec<Cell>;
pub type Point = (usize, usize);

struct Screen {
    lines: Vec<Row>,
    row: usize,
    col: usize,
    saved: (usize, usize, Style),
    top: usize,
    bottom: usize,
    wrapped: bool,
}

impl Screen {
    fn new(cols: usize, rows: usize) -> Self {
        Self {
            lines: vec![vec![Cell::blank(Style::default()); cols]; rows],
            row: 0,
            col: 0,
            saved: (0, 0, Style::default()),
            top: 0,
            bottom: rows - 1,
            wrapped: false,
        }
    }
}

pub struct Command {
    pub head: usize,
    pub start: usize,
    pub end: Option<Point>,
    pub status: u16,
    pub label: String,
}

pub struct FolderPrompt {
    pub directory: String,
    pub spans: Vec<(Point, Point)>,
}

pub struct Terminal {
    screen: Screen,
    primary: Option<Screen>,
    history: VecDeque<Row>,
    first: usize,
    pub cols: usize,
    pub rows: usize,
    pub cursor_visible: bool,
    pub application_cursor: bool,
    pub bracketed_paste: bool,
    pub at_prompt: bool,
    pub ghost: String,
    pub suggestions: Vec<String>,
    pub directory: Option<String>,
    pub reported_directory: Option<String>,
    pub input: String,
    pub input_start: Option<Point>,
    prompt_head: Option<usize>,
    pub commands: VecDeque<Command>,
    pub archive: Vec<String>,
    pub history_version: u64,
    pub prompt_icons: VecDeque<Point>,
    pub folder_spans: Vec<(Point, Point)>,
    pub folder_prompts: VecDeque<FolderPrompt>,
    folder_start: Option<Point>,
    pub navigation_ready: Option<bool>,
    pub elevation_prepared: Option<bool>,
    pub prompt_generation: u64,
    pending: Option<usize>,
    command_head: Option<usize>,
    style: Style,
    autowrap: bool,
    origin: bool,
    insert: bool,
    pub replies: Vec<u8>,
}

impl Terminal {
    pub fn new(cols: usize, rows: usize) -> Self {
        Self {
            screen: Screen::new(cols, rows),
            primary: None,
            history: VecDeque::new(),
            first: 0,
            cols,
            rows,
            cursor_visible: true,
            application_cursor: false,
            bracketed_paste: false,
            at_prompt: false,
            ghost: String::new(),
            suggestions: Vec::new(),
            directory: None,
            reported_directory: None,
            input: String::new(),
            input_start: None,
            prompt_head: None,
            commands: VecDeque::new(),
            archive: Vec::new(),
            history_version: 0,
            prompt_icons: VecDeque::new(),
            folder_spans: Vec::new(),
            folder_prompts: VecDeque::new(),
            folder_start: None,
            navigation_ready: None,
            elevation_prepared: None,
            prompt_generation: 0,
            pending: None,
            command_head: None,
            style: Style::default(),
            autowrap: true,
            origin: false,
            insert: false,
            replies: Vec::new(),
        }
    }

    pub fn alternate(&self) -> bool {
        self.primary.is_some()
    }
    pub fn live_top(&self) -> usize {
        if self.alternate() {
            0
        } else {
            self.first + self.history.len()
        }
    }
    pub fn oldest(&self) -> usize {
        if self.alternate() { 0 } else { self.first }
    }
    pub fn has_running_command(&self) -> bool {
        self.pending.is_some()
    }

    pub fn live_prompt_start(&self) -> Option<usize> {
        (self.at_prompt && !self.alternate())
            .then_some(self.prompt_head)
            .flatten()
    }

    pub fn display_bottom(&self) -> usize {
        if self.alternate() {
            return self.rows - 1;
        }
        let last = self
            .screen
            .lines
            .iter()
            .rposition(|line| {
                line.iter().any(|cell| {
                    cell.glyph != ' '
                        || !cell.extra.is_empty()
                        || cell.underline
                        || (!self.at_prompt && cell.bg != BG)
                })
            })
            .unwrap_or(0);
        self.live_top() + last.max(self.screen.row)
    }

    pub fn cursor(&self) -> Point {
        (self.live_top() + self.screen.row, self.screen.col)
    }

    pub fn is_output_row(&self, row: usize) -> bool {
        self.pending.is_some_and(|start| row >= start)
            || self.commands.iter().rev().any(|command| {
                command.start <= row
                    && command.end.is_some_and(|end| row <= end.0)
                    && self.input_start.is_none_or(|start| start.0 != row)
            })
    }

    pub fn line(&self, index: usize) -> Option<&[Cell]> {
        let top = self.live_top();
        if index < top {
            return self
                .history
                .get(index.checked_sub(self.first)?)
                .map(Vec::as_slice);
        }
        self.screen.lines.get(index - top).map(Vec::as_slice)
    }

    pub fn content_columns(line: &[Cell]) -> usize {
        line.iter()
            .enumerate()
            .rfind(|(_, cell)| {
                cell.glyph != ' ' || !cell.extra.is_empty() || cell.bg != BG || cell.underline
            })
            .map_or(0, |(col, cell)| {
                (col + usize::from(cell.width.max(1))).min(line.len())
            })
    }

    pub fn trim_selection(&self, a: Point, b: Point) -> Option<(Point, Point)> {
        let (start, end) = (a.min(b), a.max(b));
        let mut trimmed_start = None;
        let mut trimmed_end = None;
        for row in start.0.max(self.oldest())..=end.0.min(self.display_bottom()) {
            let Some(line) = self.line(row) else {
                continue;
            };
            let columns = Self::content_columns(line);
            let left = if row == start.0 { start.1 } else { 0 };
            if left >= columns {
                continue;
            }
            let right = if row == end.0 {
                end.1.min(columns - 1)
            } else {
                columns - 1
            };
            let left = if line[left].width == 0 {
                left.saturating_sub(1)
            } else {
                left
            };
            trimmed_start.get_or_insert((row, left));
            trimmed_end = Some((row, right));
        }
        Some((trimmed_start?, trimmed_end?))
    }

    pub fn text(&self, start: Point, end: Point) -> String {
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        let mut result = String::new();
        for index in start.0.max(self.oldest())..=end.0.min(self.live_top() + self.rows - 1) {
            let Some(line) = self.line(index) else {
                continue;
            };
            let from = if index == start.0 { start.1 } else { 0 };
            let to = if index == end.0 {
                end.1 + 1
            } else {
                line.len()
            };
            let mut text = String::new();
            for cell in line.iter().take(to).skip(from) {
                if cell.width != 0 {
                    text.push(cell.glyph);
                    text.push_str(&cell.extra);
                }
            }
            if index != start.0.max(self.oldest()) {
                result.push('\n');
            }
            result.push_str(if start.0 == end.0 {
                &text
            } else {
                text.trim_end()
            });
        }
        result
    }

    fn blank(&self) -> Row {
        vec![Cell::blank(self.style); self.cols]
    }

    fn push_history(&mut self, row: Row) {
        self.history.push_back(row);
        if self.history.len() > 5000 {
            self.history.pop_front();
            self.first += 1;
        }
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        if (self.cols, self.rows) == (cols, rows) {
            return;
        }
        let mut removed = Vec::new();
        let alternate = self.alternate();
        for (index, screen) in std::iter::once(&mut self.screen)
            .chain(self.primary.iter_mut())
            .enumerate()
        {
            if screen.row >= rows {
                let count = screen.row + 1 - rows;
                let dropped = screen.lines.drain(..count);
                if !alternate || index == 1 {
                    removed.extend(dropped);
                } else {
                    drop(dropped);
                }
                screen.row -= count;
            }
            for line in &mut screen.lines {
                line.resize(cols, Cell::blank(Style::default()));
                if line[cols - 1].width == 2 {
                    line[cols - 1] = Cell::blank(Style::default());
                }
            }
            screen
                .lines
                .resize(rows, vec![Cell::blank(Style::default()); cols]);
            screen.col = screen.col.min(cols - 1);
            screen.top = 0;
            screen.bottom = rows - 1;
            screen.wrapped = false;
        }
        for row in removed {
            self.push_history(row);
        }
        self.cols = cols;
        self.rows = rows;
    }

    fn scroll_up(&mut self, top: usize, bottom: usize) {
        let row = self.screen.lines.remove(top);
        self.screen.lines.insert(bottom, self.blank());
        if top == 0 && bottom == self.rows - 1 && !self.alternate() {
            self.push_history(row);
        }
    }

    fn scroll_down(&mut self, top: usize, bottom: usize) {
        self.screen.lines.remove(bottom);
        self.screen.lines.insert(top, self.blank());
    }

    fn lf(&mut self) {
        self.screen.wrapped = false;
        if self.screen.row == self.screen.bottom {
            self.scroll_up(self.screen.top, self.screen.bottom);
        } else {
            self.screen.row = (self.screen.row + 1).min(self.rows - 1);
        }
    }

    fn move_to(&mut self, row: usize, col: usize) {
        self.screen.row = row.min(self.rows - 1);
        self.screen.col = col.min(self.cols - 1);
        self.screen.wrapped = false;
    }

    fn erase(&mut self, from: usize, to: usize) {
        self.screen.lines[self.screen.row][from..to].fill(Cell::blank(self.style));
    }

    fn sgr(&mut self, params: &Params) {
        let mut args = params.iter();
        while let Some(arg) = args.next() {
            match arg[0] {
                0 => self.style = Style::default(),
                4 | 21 => self.style.underline = true,
                7 => self.style.reverse = true,
                8 => self.style.hidden = true,
                24 => self.style.underline = false,
                27 => self.style.reverse = false,
                28 => self.style.hidden = false,
                30..=37 => self.style.fg = indexed(arg[0] - 30),
                40..=47 => self.style.bg = indexed(arg[0] - 40),
                90..=97 => self.style.fg = indexed(arg[0] - 82),
                100..=107 => self.style.bg = indexed(arg[0] - 92),
                39 => self.style.fg = FG,
                49 => self.style.bg = BG,
                38 | 48 => {
                    let color = if arg.len() > 1 {
                        match arg[1] {
                            5 => arg.get(2).map(|v| indexed(*v)),
                            2 if arg.len() >= 5 => {
                                let rgb = &arg[arg.len() - 3..];
                                Some(Color32::from_rgb(rgb[0] as u8, rgb[1] as u8, rgb[2] as u8))
                            }
                            _ => None,
                        }
                    } else {
                        match args.next().map(|v| v[0]) {
                            Some(5) => args.next().map(|v| indexed(v[0])),
                            Some(2) => {
                                let rgb: Vec<_> =
                                    args.by_ref().take(3).map(|v| v[0] as u8).collect();
                                if let [r, g, b] = rgb[..] {
                                    Some(Color32::from_rgb(r, g, b))
                                } else {
                                    None
                                }
                            }
                            _ => None,
                        }
                    };
                    if let Some(color) = color {
                        if arg[0] == 38 {
                            self.style.fg = color;
                        } else {
                            self.style.bg = color;
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn mode(&mut self, mode: u16, private: bool, enabled: bool) {
        match (private, mode) {
            (true, 1) => self.application_cursor = enabled,
            (true, 6) => {
                self.origin = enabled;
                self.move_to(if enabled { self.screen.top } else { 0 }, 0);
            }
            (true, 7) => self.autowrap = enabled,
            (true, 25) => self.cursor_visible = enabled,
            (true, 47 | 1047 | 1049) => {
                if enabled && !self.alternate() {
                    self.primary = Some(std::mem::replace(
                        &mut self.screen,
                        Screen::new(self.cols, self.rows),
                    ));
                } else if !enabled && let Some(primary) = self.primary.take() {
                    self.screen = primary;
                }
            }
            (true, 2004) => self.bracketed_paste = enabled,
            (false, 4) => self.insert = enabled,
            _ => {}
        }
    }

    fn save(&mut self) {
        self.screen.saved = (self.screen.row, self.screen.col, self.style);
    }

    fn restore(&mut self) {
        let (row, col, style) = self.screen.saved;
        self.move_to(row, col);
        self.style = style;
    }
}

impl Perform for Terminal {
    fn print(&mut self, c: char) {
        let width = c.width().unwrap_or(0);
        if width == 0 {
            let mut col = if self.screen.wrapped {
                self.screen.col
            } else {
                self.screen.col.saturating_sub(1)
            };
            let line = &mut self.screen.lines[self.screen.row];
            if line[col].width == 0 {
                col = col.saturating_sub(1);
            }
            line[col].extra.push(c);
            return;
        }
        if self.autowrap && (self.screen.wrapped || self.screen.col + width > self.cols) {
            self.lf();
            self.screen.col = 0;
        }
        let col = self.screen.col;
        if col + width > self.cols {
            return;
        }
        let line = &mut self.screen.lines[self.screen.row];
        if self.insert {
            line[col..].rotate_right(width);
        }
        if line[col].width == 0 && col > 0 {
            line[col - 1] = Cell::blank(self.style);
        }
        if col + width < self.cols && line[col + width].width == 0 {
            line[col + width] = Cell::blank(self.style);
        }
        let mut cell = Cell::blank(self.style);
        cell.glyph = if self.style.hidden { ' ' } else { c };
        cell.width = width as u8;
        line[col] = cell;
        if width == 2 {
            line[col + 1] = Cell {
                width: 0,
                ..Cell::blank(self.style)
            };
        }
        self.screen.wrapped = col + width == self.cols;
        self.screen.col = (col + width).min(self.cols - 1);
    }

    fn execute(&mut self, byte: u8) {
        let (row, col) = (self.screen.row, self.screen.col);
        match byte {
            b'\r' => self.move_to(row, 0),
            b'\n' | 0x0b | 0x0c => self.lf(),
            0x08 => self.move_to(row, col.saturating_sub(1)),
            b'\t' => self.move_to(row, (col / 8 + 1) * 8),
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        if ignore || !intermediates.is_empty() {
            return;
        }
        match byte {
            b'7' => self.save(),
            b'8' => self.restore(),
            b'D' => self.lf(),
            b'E' => {
                self.lf();
                self.screen.col = 0;
            }
            b'M' => {
                if self.screen.row == self.screen.top {
                    self.scroll_down(self.screen.top, self.screen.bottom);
                } else {
                    self.screen.row = self.screen.row.saturating_sub(1);
                }
                self.screen.wrapped = false;
            }
            b'c' => *self = Self::new(self.cols, self.rows),
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediate: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }
        let mut args = params.iter().map(|p| usize::from(p[0]));
        let value = args.next().unwrap_or(0);
        let n = value.max(1);
        if matches!(action, 'h' | 'l') {
            if intermediate.is_empty() || intermediate == b"?" {
                for p in params {
                    self.mode(p[0], intermediate == b"?", action == 'h');
                }
            }
            return;
        }
        if !intermediate.is_empty() {
            return;
        }
        let (row, col) = (self.screen.row, self.screen.col);
        let (top, bottom) = (self.screen.top, self.screen.bottom);
        let origin = if self.origin { top } else { 0 };
        let limit = if self.origin { bottom } else { self.rows - 1 };
        match action {
            'A' => self.move_to(
                row.saturating_sub(n).max(if row >= top { top } else { 0 }),
                col,
            ),
            'B' | 'e' => self.move_to(
                (row + n).min(if row <= bottom { bottom } else { limit }),
                col,
            ),
            'C' | 'a' => self.move_to(row, col + n),
            'D' => self.move_to(row, col.saturating_sub(n)),
            'E' => self.move_to(row + n, 0),
            'F' => self.move_to(row.saturating_sub(n), 0),
            'G' | '`' => self.move_to(row, n - 1),
            'd' => self.move_to((origin + n - 1).min(limit), col),
            'H' | 'f' => self.move_to(
                (origin + n - 1).min(limit),
                args.next().unwrap_or(1).max(1) - 1,
            ),
            'm' => self.sgr(params),
            'K' => match value {
                0 => self.erase(col, self.cols),
                1 => self.erase(0, col + 1),
                2 => self.erase(0, self.cols),
                _ => {}
            },
            'J' => {
                let blank = self.blank();
                match value {
                    0 => {
                        self.erase(col, self.cols);
                        self.screen.lines[row + 1..].fill(blank);
                    }
                    1 => {
                        self.erase(0, col + 1);
                        self.screen.lines[..row].fill(blank);
                    }
                    2 => self.screen.lines.fill(blank),
                    3 if !self.alternate() => {
                        self.first += self.history.len();
                        self.history.clear();
                    }
                    _ => {}
                }
            }
            'X' => self.erase(col, (col + n).min(self.cols)),
            '@' | 'P' => {
                let count = n.min(self.cols - col);
                let line = &mut self.screen.lines[row];
                if action == '@' {
                    line[col..].rotate_right(count);
                    line[col..col + count].fill(Cell::blank(self.style));
                } else {
                    line[col..].rotate_left(count);
                    line[self.cols - count..].fill(Cell::blank(self.style));
                }
            }
            'L' | 'M' if (top..=bottom).contains(&row) => {
                for _ in 0..n.min(bottom - row + 1) {
                    if action == 'L' {
                        self.scroll_down(row, bottom);
                    } else {
                        self.scroll_up(row, bottom);
                    }
                }
            }
            'S' | 'T' => {
                for _ in 0..n.min(bottom - top + 1) {
                    if action == 'S' {
                        self.scroll_up(top, bottom);
                    } else {
                        self.scroll_down(top, bottom);
                    }
                }
            }
            'r' => {
                let end = args
                    .next()
                    .filter(|v| *v != 0)
                    .unwrap_or(self.rows)
                    .min(self.rows);
                if n < end {
                    self.screen.top = n - 1;
                    self.screen.bottom = end - 1;
                    self.move_to(if self.origin { n - 1 } else { 0 }, 0);
                }
            }
            's' => self.save(),
            'u' => self.restore(),
            'n' if value == 5 => self.replies.extend_from_slice(b"\x1b[0n"),
            'n' if value == 6 => self
                .replies
                .extend(format!("\x1b[{};{}R", row + 1, col + 1).bytes()),
            'c' => self.replies.extend_from_slice(b"\x1b[?1;2c"),
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _: bool) {
        if self.alternate() {
            return;
        }
        if params.first().copied() == Some(b"7") {
            self.reported_directory = params
                .get(1)
                .filter(|value| params.len() == 2 && value.len() <= 4096)
                .and_then(|value| std::str::from_utf8(value).ok())
                .map(str::to_owned);
            return;
        }
        if params.first().copied() != Some(b"133") {
            return;
        }
        match params.get(1).copied() {
            Some(b"H") if self.archive.len() < 300 => {
                let text: String = params
                    .iter()
                    .skip(2)
                    .map(|part| String::from_utf8_lossy(part))
                    .collect::<Vec<_>>()
                    .join(";")
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(720)
                    .collect();
                if !text.trim().is_empty() {
                    self.archive.push(text);
                    self.history_version = self.history_version.wrapping_add(1);
                }
            }
            Some(b"W") => {
                self.prompt_generation = self.prompt_generation.wrapping_add(1);
                let directory = params
                    .iter()
                    .skip(2)
                    .map(|part| String::from_utf8_lossy(part))
                    .collect::<Vec<_>>()
                    .join(";");
                self.directory = Some(
                    directory
                        .chars()
                        .filter(|c| !c.is_control())
                        .take(1024)
                        .collect(),
                );
            }
            Some(b"A") => {
                self.at_prompt = true;
                self.folder_spans.clear();
                self.folder_start = None;
                self.prompt_head = Some(self.cursor().0);
            }
            Some(b"G") if self.at_prompt => {
                let position = self.cursor();
                if self.prompt_icons.back() != Some(&position) {
                    self.prompt_icons.push_back(position);
                    while self.prompt_icons.len() > 512 {
                        self.prompt_icons.pop_front();
                    }
                }
            }
            Some(b"X") if self.at_prompt => self.folder_start = Some(self.cursor()),
            Some(b"Y") if self.at_prompt => {
                if let Some(start) = self.folder_start.take()
                    && self.folder_spans.len() < 128
                {
                    self.folder_spans.push((start, self.cursor()));
                }
            }
            Some(b"U") if self.at_prompt => {
                self.elevation_prepared = Some(params.get(2).copied() == Some(b"1"));
            }
            Some(b"N") if self.at_prompt => {
                self.navigation_ready = Some(params.get(2).copied() == Some(b"1"));
            }
            Some(b"B") if self.at_prompt => {
                self.input_start = Some(self.cursor());
                if let Some(directory) = self.directory.as_ref()
                    && let Some(first) = self.folder_spans.first()
                {
                    let prompt = FolderPrompt {
                        directory: directory.clone(),
                        spans: self.folder_spans.clone(),
                    };
                    if let Some(last) = self.folder_prompts.back_mut()
                        && last.spans.first() == Some(first)
                    {
                        *last = prompt;
                    } else {
                        self.folder_prompts.push_back(prompt);
                        if self.folder_prompts.len() > 512 {
                            self.folder_prompts.pop_front();
                        }
                    }
                }
            }
            Some(b"I" | b"P" | b"S") if self.at_prompt => {
                let value: String = params
                    .iter()
                    .skip(2)
                    .map(|part| String::from_utf8_lossy(part))
                    .collect::<Vec<_>>()
                    .join(";")
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(768)
                    .collect();
                match params[1] {
                    b"I" => {
                        self.input = value;
                        self.suggestions.clear();
                    }
                    b"P" => self.ghost = value,
                    _ if !value.is_empty() && self.suggestions.len() < 8 => {
                        self.suggestions.push(value)
                    }
                    _ => {}
                }
            }
            Some(b"E" | b"C") => {
                self.at_prompt = false;
                self.folder_spans.clear();
                self.folder_start = None;
                self.ghost.clear();
                self.suggestions.clear();
                self.input.clear();
                if params[1] == b"E" {
                    return;
                }
                let row = self.cursor().0;
                self.command_head = self
                    .prompt_head
                    .take()
                    .or_else(|| Some(row.saturating_sub(1)));
                self.input_start = None;
                self.pending = Some(row);
            }
            Some(b"D") => {
                let Some(start) = self.pending.take() else {
                    return;
                };
                let status = params
                    .get(2)
                    .and_then(|p| std::str::from_utf8(p).ok())
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(0);
                let (end, col) = self.cursor();
                let end = if col == 0 {
                    end.checked_sub(1).filter(|end| *end >= start).map(|end| {
                        (
                            end,
                            self.line(end)
                                .map_or(0, |line| line.len().saturating_sub(1)),
                        )
                    })
                } else {
                    Some((end, col - 1))
                };
                let label = params
                    .get(3)
                    .map(|p| String::from_utf8_lossy(p).into_owned())
                    .filter(|label| !label.is_empty())
                    .unwrap_or_else(|| "Command".into());
                self.commands.push_back(Command {
                    head: self
                        .command_head
                        .take()
                        .unwrap_or_else(|| start.saturating_sub(1)),
                    start,
                    end,
                    status,
                    label,
                });
                self.history_version = self.history_version.wrapping_add(1);
                if self.commands.len() > 200 {
                    self.commands.pop_front();
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_primary_screen_when_full_screen_app_exits() {
        let mut terminal = Terminal::new(16, 3);
        let mut parser = vte::Parser::new();
        parser.advance(&mut terminal, b"shell output\x1b[?1049h\x1b[2;2Hvim");
        assert!(terminal.alternate());
        assert_eq!(terminal.text((1, 1), (1, 3)), "vim");
        parser.advance(&mut terminal, b"\x1b[?1049l");
        assert_eq!(terminal.text((0, 0), (0, 11)), "shell output");
        assert_eq!(terminal.cursor(), (0, 12));
    }

    #[test]
    fn retains_command_output_after_scrolling_and_records_exit_status() {
        let mut terminal = Terminal::new(16, 2);
        let mut parser = vte::Parser::new();
        parser.advance(
            &mut terminal,
            b"> false\r\n\x1b]133;C\x07one\r\ntwo\r\n\x1b]133;D;1;false\x07\x1b]133;A\x07> ",
        );
        let command = terminal.commands.back().unwrap();
        assert_eq!(command.label, "false");
        assert_eq!(command.status, 1);
        assert_eq!(
            terminal.text((command.start, 0), command.end.unwrap()),
            "one\ntwo"
        );
        assert!(terminal.at_prompt);
    }

    #[test]
    fn tracks_full_width_characters_and_terminal_colors() {
        let mut terminal = Terminal::new(8, 2);
        let mut parser = vte::Parser::new();
        parser.advance(&mut terminal, "\x1b[38;2;10;20;30m界a\x1b[0m".as_bytes());
        let row = terminal.line(0).unwrap();
        assert_eq!(row[0].width, 2);
        assert_eq!(row[1].width, 0);
        assert_eq!(row[2].fg, Color32::from_rgb(10, 20, 30));
        assert_eq!(terminal.text((0, 0), (0, 2)), "界a");
    }
}
