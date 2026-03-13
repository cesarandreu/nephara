use std::collections::{HashMap, HashSet};
use std::io;
use std::time::Duration;

use crossterm::event::{
    self, Event, KeyCode, KeyModifiers,
    EnableMouseCapture, DisableMouseCapture,
    MouseEvent, MouseEventKind, MouseButton,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Terminal;
use tokio::sync::mpsc;

use crate::color as ccolor;
use crate::tui_event::{AgentNeedsSnapshot, LlmCallRecord, MapCell, TickEntrySnapshot, TuiEvent};

// ---------------------------------------------------------------------------
// Log entry types
// ---------------------------------------------------------------------------

pub enum DayBoundaryKind {
    MorningIntention,
    EveningDesire,
    EveningReflection,
}

pub enum LogEntry {
    TickHeader {
        tick:        u32,
        day:         u32,
        time_of_day: &'static str,
    },
    AgentAction(TickEntrySnapshot),
    DayBoundary {
        kind:       DayBoundaryKind,
        agent_id:   usize,
        agent_name: String,
        day:        u32,
        text:       String,
    },
    /// A world event notification (FEAT-19).
    WorldEvent {
        day:  u32,
        text: String,
    },
    SimComplete {
        total_ticks:    u32,
        magic_count:    u32,
        notable:        Vec<String>,
    },
    /// Placeholder shown while the LLM is streaming tokens for this agent.
    Thinking {
        agent_id:   usize,
        agent_name: String,
        tokens:     String,
    },
}

// ---------------------------------------------------------------------------
// TuiApp
// ---------------------------------------------------------------------------

pub struct TuiApp {
    map_cells:            Vec<Vec<MapCell>>,
    tick_maps:            HashMap<u32, Vec<Vec<MapCell>>>,
    displayed_tick:       u32,
    log_entries:          Vec<LogEntry>,
    agent_needs:          Vec<AgentNeedsSnapshot>,
    agent_count:          usize,
    tick:                 u32,
    day:                  u32,
    time_of_day:          &'static str,
    total_ticks:          u32,
    ticks_per_day:        u32,
    night_start_tick:     u32,
    seed:                 u64,
    backend_name:         String,
    model_name:           String,
    god_name:             String,
    scroll_offset:        usize,
    selected:             usize,
    expanded:             HashSet<usize>,
    is_complete:          bool,
    should_quit:          bool,
    roster:               Vec<(String, Color)>,
    show_legend:          bool,
    show_help:            bool,
    inspected_agent:      Option<usize>,
    /// When true, manual scrolling has occurred and auto-scroll is paused.
    scroll_locked:        bool,
    /// Maps agent_id → index of a pending "thinking..." log entry.
    thinking_entry_idx:   HashMap<usize, usize>,
    // Dynamic wrap / hit-testing state (updated each frame by render_log)
    log_wrap_width:       usize,
    log_inner_area:       Rect,
    log_rendered_scroll:  usize,
    // LLM debug overlay
    show_llm_overlay:     bool,
    llm_calls:            HashMap<u32, Vec<LlmCallRecord>>,
    llm_overlay_day:      u32,
    llm_overlay_entry:    usize,
    llm_overlay_scroll:   usize,
    llm_overlay_expanded: HashSet<usize>,
}

impl TuiApp {
    pub fn new(
        agent_count:      usize,
        total_ticks:      u32,
        ticks_per_day:    u32,
        night_start_tick: u32,
        seed:             u64,
        backend_name:     String,
        model_name:       String,
        roster:           Vec<(String, Color)>,
        god_name:         String,
    ) -> Self {
        TuiApp {
            map_cells:            vec![vec![], vec![]],
            tick_maps:            HashMap::new(),
            displayed_tick:       0,
            log_entries:          Vec::new(),
            agent_needs:          Vec::new(),
            agent_count,
            tick:                 0,
            day:                  1,
            time_of_day:          "Dawn",
            total_ticks,
            ticks_per_day,
            night_start_tick,
            seed,
            backend_name,
            model_name,
            god_name,
            scroll_offset:        0,
            selected:             0,
            expanded:             HashSet::new(),
            is_complete:          false,
            should_quit:          false,
            roster,
            show_legend:          false,
            show_help:            false,
            inspected_agent:      None,
            scroll_locked:        false,
            thinking_entry_idx:   HashMap::new(),
            log_wrap_width:       60,
            log_inner_area:       Rect::default(),
            log_rendered_scroll:  0,
            show_llm_overlay:     false,
            llm_calls:            HashMap::new(),
            llm_overlay_day:      1,
            llm_overlay_entry:    0,
            llm_overlay_scroll:   0,
            llm_overlay_expanded: HashSet::new(),
        }
    }

    fn process_event(&mut self, ev: TuiEvent) {
        match ev {
            TuiEvent::TickStart { tick, day, time_of_day } => {
                self.tick         = tick;
                self.day          = day;
                self.time_of_day  = time_of_day;
                self.displayed_tick = tick;
                self.log_entries.push(LogEntry::TickHeader { tick, day, time_of_day });
                if !self.scroll_locked { self.scroll_to_bottom(); }
            }
            TuiEvent::MapUpdate(cells) => {
                self.tick_maps.insert(self.tick, cells.clone());
                self.map_cells = cells;
            }
            TuiEvent::NeedsUpdate(snap) => { self.agent_needs = snap; }
            TuiEvent::AgentAction(snap) => {
                // Remove any pending "thinking" entry for this agent
                if let Some(thinking_idx) = self.thinking_entry_idx.remove(&snap.agent_id) {
                    if thinking_idx < self.log_entries.len() {
                        self.log_entries.remove(thinking_idx);
                        // Adjust selected if it pointed past the removed entry
                        if self.selected > thinking_idx && self.selected > 0 {
                            self.selected -= 1;
                        }
                        // Shift all other thinking indices
                        for v in self.thinking_entry_idx.values_mut() {
                            if *v > thinking_idx { *v -= 1; }
                        }
                    }
                }
                self.log_entries.push(LogEntry::AgentAction(snap));
                if !self.scroll_locked { self.scroll_to_bottom(); }
            }
            TuiEvent::PartialToken { agent_id, token } => {
                if let Some(&idx) = self.thinking_entry_idx.get(&agent_id) {
                    // Update existing thinking entry
                    if let Some(LogEntry::Thinking { ref mut tokens, .. }) = self.log_entries.get_mut(idx) {
                        tokens.push_str(&token);
                    }
                } else {
                    // Create new thinking entry
                    let new_idx = self.log_entries.len();
                    // Find agent name from roster (by id position)
                    let agent_name = self.roster.get(agent_id)
                        .map(|(n, _)| n.clone())
                        .unwrap_or_else(|| format!("Agent {}", agent_id));
                    self.log_entries.push(LogEntry::Thinking { agent_id, agent_name, tokens: token });
                    self.thinking_entry_idx.insert(agent_id, new_idx);
                    if !self.scroll_locked { self.scroll_to_bottom(); }
                }
            }
            TuiEvent::MorningIntention { agent_id, agent_name, day, text } => {
                self.log_entries.push(LogEntry::DayBoundary {
                    kind: DayBoundaryKind::MorningIntention,
                    agent_id, agent_name, day, text,
                });
                if !self.scroll_locked { self.scroll_to_bottom(); }
            }
            TuiEvent::EveningDesire { agent_id, agent_name, day, text } => {
                self.log_entries.push(LogEntry::DayBoundary {
                    kind: DayBoundaryKind::EveningDesire,
                    agent_id, agent_name, day, text,
                });
                if !self.scroll_locked { self.scroll_to_bottom(); }
            }
            TuiEvent::EveningReflection { agent_id, agent_name, day, text } => {
                self.log_entries.push(LogEntry::DayBoundary {
                    kind: DayBoundaryKind::EveningReflection,
                    agent_id, agent_name, day, text,
                });
                if !self.scroll_locked { self.scroll_to_bottom(); }
            }
            TuiEvent::WorldEvent { day, text } => {
                self.log_entries.push(LogEntry::WorldEvent { day, text });
                if !self.scroll_locked { self.scroll_to_bottom(); }
            }
            TuiEvent::SimulationComplete { total_ticks, magic_count, notable_events } => {
                self.log_entries.push(LogEntry::SimComplete {
                    total_ticks, magic_count, notable: notable_events,
                });
                self.is_complete = true;
                if !self.scroll_locked { self.scroll_to_bottom(); }
            }
            TuiEvent::SimulationError(msg) => {
                self.log_entries.push(LogEntry::SimComplete {
                    total_ticks: self.tick,
                    magic_count: 0,
                    notable: vec![format!("ERROR: {}", msg)],
                });
                self.is_complete = true;
            }
            TuiEvent::LlmCall(record) => {
                let day = record.day;
                self.llm_calls.entry(day).or_insert_with(Vec::new).push(record);
            }
        }
    }

    fn scroll_to_bottom(&mut self) {
        self.scroll_offset = usize::MAX;
        self.selected = self.log_entries.len().saturating_sub(1);
    }

    // -----------------------------------------------------------------------
    // Line-count helpers for scroll/selection tracking
    // -----------------------------------------------------------------------

    /// Returns the number of rendered lines for a single log entry.
    fn entry_line_count(&self, idx: usize, entry: &LogEntry) -> usize {
        let is_expanded  = self.expanded.contains(&idx);
        let wrap_width   = self.log_wrap_width;
        match entry {
            LogEntry::TickHeader { .. } => 1,
            LogEntry::AgentAction(snap) => {
                let mut count = 1; // header line
                if snap.prayer_text.is_some() { count += 1; }
                if !snap.outcome_line.is_empty() {
                    let wrapped = wrap_text(&snap.outcome_line, wrap_width);
                    if is_expanded {
                        count += wrapped.len();
                    } else {
                        count += 2.min(wrapped.len());
                        if wrapped.len() > 2 { count += 1; } // "[+N more]" line
                    }
                }
                count
            }
            LogEntry::DayBoundary { text, .. } => {
                let wrapped = wrap_text(text, wrap_width);
                let mut count = 1; // header line
                if is_expanded {
                    count += wrapped.len();
                } else {
                    count += 3.min(wrapped.len());
                    if wrapped.len() > 3 { count += 1; } // "[+N more]" line
                }
                count
            }
            LogEntry::WorldEvent { text, .. } => {
                1 + wrap_text(text, wrap_width.saturating_sub(4)).len().min(3)
            }
            LogEntry::SimComplete { notable, .. } => {
                4 + if notable.is_empty() { 0 } else { 1 + notable.len() }
            }
            LogEntry::Thinking { .. } => 1,
        }
    }

    /// Returns the first rendered line index for the entry at `target_idx`.
    fn entry_first_line(&self, target_idx: usize) -> usize {
        let mut line = 0;
        for (idx, entry) in self.log_entries.iter().enumerate() {
            if idx == target_idx { return line; }
            line += self.entry_line_count(idx, entry);
        }
        line
    }

    /// Returns the entry index that owns the given rendered line.
    fn entry_at_line(&self, line: usize) -> usize {
        let mut current = 0;
        for (idx, entry) in self.log_entries.iter().enumerate() {
            let count = self.entry_line_count(idx, entry);
            if line < current + count {
                return idx;
            }
            current += count;
        }
        self.log_entries.len().saturating_sub(1)
    }

    // -----------------------------------------------------------------------
    // Input
    // -----------------------------------------------------------------------

    /// Toggle expand for expandable entries only; TickHeader and SimComplete are skipped.
    fn toggle_expand(&mut self, idx: usize) {
        if idx >= self.log_entries.len() { return; }
        match &self.log_entries[idx] {
            LogEntry::AgentAction(_) | LogEntry::DayBoundary { .. } => {
                if self.expanded.contains(&idx) {
                    self.expanded.remove(&idx);
                } else {
                    self.expanded.insert(idx);
                }
            }
            _ => {} // TickHeader, SimComplete, Thinking have no expandable body
        }
    }

    fn handle_input(&mut self, key: crossterm::event::KeyEvent) {
        // LLM overlay mode intercepts most keys
        if self.show_llm_overlay {
            match (key.modifiers, key.code) {
                (_, KeyCode::Char('d')) | (_, KeyCode::Esc) => {
                    self.show_llm_overlay = false;
                }
                (_, KeyCode::Char('<')) | (_, KeyCode::Left) => {
                    if self.llm_overlay_day > 1 {
                        self.llm_overlay_day -= 1;
                        self.llm_overlay_entry = 0;
                        self.llm_overlay_scroll = 0;
                    }
                }
                (_, KeyCode::Char('>')) | (_, KeyCode::Right) => {
                    self.llm_overlay_day = self.llm_overlay_day.saturating_add(1);
                    self.llm_overlay_entry = 0;
                    self.llm_overlay_scroll = 0;
                }
                (_, KeyCode::Char('j')) | (_, KeyCode::Down) => {
                    self.llm_overlay_scroll = self.llm_overlay_scroll.saturating_add(1);
                }
                (_, KeyCode::Char('k')) | (_, KeyCode::Up) => {
                    self.llm_overlay_scroll = self.llm_overlay_scroll.saturating_sub(1);
                }
                (_, KeyCode::Tab) | (_, KeyCode::BackTab) => {
                    let max = self.llm_calls.get(&self.llm_overlay_day).map(|v| v.len()).unwrap_or(0);
                    if max > 0 {
                        if matches!(key.code, KeyCode::Tab) {
                            self.llm_overlay_entry = (self.llm_overlay_entry + 1) % max;
                        } else if self.llm_overlay_entry == 0 {
                            self.llm_overlay_entry = max - 1;
                        } else {
                            self.llm_overlay_entry -= 1;
                        }
                        self.llm_overlay_scroll = 0;
                    }
                }
                (_, KeyCode::Char(' ')) | (_, KeyCode::Enter) => {
                    if self.llm_overlay_expanded.contains(&self.llm_overlay_entry) {
                        self.llm_overlay_expanded.remove(&self.llm_overlay_entry);
                    } else {
                        self.llm_overlay_expanded.insert(self.llm_overlay_entry);
                    }
                }
                _ => {}
            }
            return;
        }

        match (key.modifiers, key.code) {
            (_, KeyCode::Char('q')) | (_, KeyCode::Esc) => { self.should_quit = true; }
            (KeyModifiers::NONE, KeyCode::Char('d')) => {
                self.show_llm_overlay = true;
                self.llm_overlay_day = self.day;
                self.llm_overlay_entry = 0;
                self.llm_overlay_scroll = 0;
            }
            (_, KeyCode::Char('j')) | (_, KeyCode::Down) => {
                self.scroll_locked = true;
                self.scroll_offset = self.scroll_offset.saturating_add(1);
                self.selected = self.entry_at_line(self.scroll_offset);
            }
            (_, KeyCode::Char('k')) | (_, KeyCode::Up) => {
                self.scroll_locked = true;
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
                self.selected = self.entry_at_line(self.scroll_offset);
            }
            (KeyModifiers::CONTROL, KeyCode::Char('d')) | (_, KeyCode::PageDown) => {
                self.scroll_locked = true;
                self.scroll_offset = self.scroll_offset.saturating_add(10);
                self.selected = self.entry_at_line(self.scroll_offset);
            }
            (KeyModifiers::CONTROL, KeyCode::Char('u')) | (_, KeyCode::PageUp) => {
                self.scroll_locked = true;
                self.scroll_offset = self.scroll_offset.saturating_sub(10);
                self.selected = self.entry_at_line(self.scroll_offset);
            }
            (_, KeyCode::Char('G')) => {
                self.scroll_locked = false;
                self.scroll_to_bottom();
            }
            (_, KeyCode::Char('[')) => {
                let current = self.selected;
                if let Some(prev) = self.log_entries[..current]
                    .iter()
                    .rposition(|e| matches!(e, LogEntry::TickHeader { .. }))
                {
                    self.selected = prev;
                    self.scroll_offset = self.entry_first_line(prev);
                    if let LogEntry::TickHeader { tick, .. } = &self.log_entries[prev] {
                        self.displayed_tick = *tick;
                    }
                }
            }
            (_, KeyCode::Char(']')) => {
                let current = self.selected;
                if let Some(next) = self.log_entries[current + 1..]
                    .iter()
                    .position(|e| matches!(e, LogEntry::TickHeader { .. }))
                {
                    let next_idx = current + 1 + next;
                    self.selected = next_idx;
                    self.scroll_offset = self.entry_first_line(next_idx);
                    if let LogEntry::TickHeader { tick, .. } = &self.log_entries[next_idx] {
                        self.displayed_tick = *tick;
                    }
                }
            }
            (_, KeyCode::Enter) | (_, KeyCode::Char(' ')) => {
                self.toggle_expand(self.selected);
            }
            (_, KeyCode::Char('l')) => { self.show_legend = !self.show_legend; }
            (_, KeyCode::Char('?')) => { self.show_help = !self.show_help; }
            (_, KeyCode::Char(c @ '1'..='5')) => {
                let idx = (c as usize) - ('1' as usize);
                if idx < self.agent_count {
                    if self.inspected_agent == Some(idx) {
                        self.inspected_agent = None;
                    } else {
                        self.inspected_agent = Some(idx);
                    }
                }
            }
            _ => {}
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollDown => {
                self.scroll_locked = true;
                self.scroll_offset = self.scroll_offset.saturating_add(3);
                self.selected = self.entry_at_line(self.scroll_offset);
            }
            MouseEventKind::ScrollUp => {
                self.scroll_locked = true;
                self.scroll_offset = self.scroll_offset.saturating_sub(3);
                self.selected = self.entry_at_line(self.scroll_offset);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let area = self.log_inner_area;
                if mouse.column >= area.x
                    && mouse.column < area.x + area.width
                    && mouse.row >= area.y
                    && mouse.row < area.y + area.height
                {
                    let rel_row  = (mouse.row - area.y) as usize;
                    let abs_line = self.log_rendered_scroll + rel_row;
                    let entry_idx = self.entry_at_line(abs_line);
                    self.selected     = entry_idx;
                    self.scroll_offset = abs_line;
                    self.toggle_expand(entry_idx);
                }
            }
            _ => {}
        }
    }

    // -----------------------------------------------------------------------
    // Render
    // -----------------------------------------------------------------------

    fn draw(&mut self, f: &mut ratatui::Frame) {
        let area = f.area();

        // Outer layout: title | main | needs
        // borders(2) + header(1) + agents(N)
        let needs_height = 3 + self.agent_count as u16;
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(needs_height),
            ])
            .split(area);

        // Title bar
        let status = if self.is_complete { "DONE" } else { "RUNNING" };
        let lock_indicator = if self.scroll_locked { "  [SCROLL LOCK — G to resume]" } else { "" };
        let tick_in_day = self.tick % self.ticks_per_day;
        let day_icon = if tick_in_day >= self.night_start_tick { "☾" } else { "☀" };
        let day_filled = ((tick_in_day as f32 / self.ticks_per_day as f32) * 8.0).round() as usize;
        let day_bar = format!("{}[{}{}] {}/{}", day_icon,
            "█".repeat(day_filled.min(8)), "░".repeat(8 - day_filled.min(8)),
            tick_in_day, self.ticks_per_day);
        let tick_filled = ((self.tick as f32 / self.total_ticks as f32) * 10.0).round() as usize;
        let tick_bar = format!("[{}{}]", "█".repeat(tick_filled.min(10)), "░".repeat(10 - tick_filled.min(10)));
        let title_text = format!(
            " NEPHARA  model:{}  seed:{}  tick:{}/{} {}  Day {} {}  [{}]  {}{}  ✦ {}",
            self.model_name, self.seed, self.tick, self.total_ticks, tick_bar,
            self.day, day_bar, self.backend_name, status, lock_indicator,
            self.god_name
        );
        let title = Paragraph::new(title_text)
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD));
        f.render_widget(title, outer[0]);

        // Main area: map | log
        let main = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(69),
                Constraint::Min(0),
            ])
            .split(outer[1]);

        self.render_map(f, main[0]);
        if self.show_llm_overlay {
            self.render_llm_overlay(f, main[1]);
        } else {
            self.render_log(f, main[1]);
        }
        self.render_needs(f, outer[2]);

        // Overlays (rendered on top)
        if self.show_help {
            self.render_help_overlay(f, area);
        }
        if let Some(agent_idx) = self.inspected_agent {
            self.render_inspect_panel(f, main[1], agent_idx);
        }
    }

    fn render_map(&self, f: &mut ratatui::Frame, area: ratatui::layout::Rect) {
        let map_title = if self.show_legend { " MAP  [l: hide] " } else { " MAP  [l: legend] " };
        let block = Block::default()
            .title(map_title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));
        let inner = block.inner(area);
        f.render_widget(block, area);

        let mut lines: Vec<Line> = Vec::new();

        let cells = self.tick_maps.get(&self.displayed_tick).unwrap_or(&self.map_cells);

        if !cells.is_empty() && !cells[0].is_empty() {
            for row_cells in cells {
                let mut spans: Vec<Span> = Vec::new();
                for (ci, cell) in row_cells.iter().enumerate() {
                    let style = if cell.bold {
                        Style::default().fg(cell.color).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(cell.color)
                    };
                    spans.push(Span::styled(cell.ch.to_string(), style));
                    if ci + 1 < row_cells.len() {
                        spans.push(Span::raw(" "));
                    }
                }
                lines.push(Line::from(spans));
            }
        }

        lines.push(Line::raw(""));
        for (name, color) in &self.roster {
            let initial = name.chars().next().unwrap_or('?').to_string();
            let mut spans = vec![
                Span::styled(initial, Style::default().fg(*color).add_modifier(Modifier::BOLD)),
                Span::raw(" "),
                Span::styled(name.clone(), Style::default().fg(*color)),
            ];
            if let Some(snap) = self.agent_needs.iter().find(|s| &s.agent_name == name) {
                spans.push(Span::raw(format!("  [{:.0}/{:.0}/{:.0}]",
                    snap.hunger, snap.energy, snap.fun)));
            }
            lines.push(Line::from(spans));
        }

        if self.show_legend {
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "TILES",
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD),
            )));
            let tile_legend: &[(&str, Color, &str)] = &[
                (".", Color::DarkGray,     "Open"),
                ("♣", Color::Green,        "Forest"),
                ("~", Color::Blue,         "River"),
                ("S", Color::Yellow,       "Square"),
                ("⌂", Color::LightYellow,  "Tavern"),
                ("W", Color::Cyan,         "Well"),
                ("M", Color::LightGreen,   "Meadow"),
                ("h", Color::Magenta,      "Home"),
                ("†", Color::LightMagenta, "Temple"),
            ];
            for (ch, color, label) in tile_legend {
                lines.push(Line::from(vec![
                    Span::styled((*ch).to_string(), Style::default().fg(*color)),
                    Span::raw(format!(" {}", label)),
                ]));
            }
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "RESOURCES",
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD),
            )));
            let res_legend: &[(&str, Color, &str)] = &[
                ("✿", Color::LightMagenta, "Berries"),
                ("≋", Color::LightCyan,    "Fish"),
                ("✦", Color::LightRed,     "Campfire"),
                ("✜", Color::LightGreen,   "Herbs"),
                ("·", Color::DarkGray,     "Depleted"),
            ];
            for (ch, color, label) in res_legend {
                lines.push(Line::from(vec![
                    Span::styled((*ch).to_string(), Style::default().fg(*color)),
                    Span::raw(format!(" {}", label)),
                ]));
            }
        }

        let para = Paragraph::new(lines);
        f.render_widget(para, inner);
    }

    fn render_log(&mut self, f: &mut ratatui::Frame, area: ratatui::layout::Rect) {
        let block = Block::default()
            .title(" EVENT LOG  [j/k scroll  [ ] jump  Space expand  q quit] ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));
        let inner = block.inner(area);
        f.render_widget(block, area);

        // Update dynamic wrap width and area for mouse hit-testing
        let wrap_width = (inner.width as usize).saturating_sub(6).max(20);
        self.log_wrap_width = wrap_width;
        self.log_inner_area = inner;

        let log_height  = inner.height as usize;
        let flat        = self.build_log_lines();
        let total_lines = flat.len();

        let max_scroll  = total_lines.saturating_sub(log_height);
        let scroll      = self.scroll_offset.min(max_scroll);
        self.log_rendered_scroll = scroll;
        self.scroll_offset = scroll;   // normalize so j/k work from actual position

        let para = Paragraph::new(flat)
            .scroll((scroll as u16, 0));
        f.render_widget(para, inner);
    }

    fn render_needs(&self, f: &mut ratatui::Frame, area: ratatui::layout::Rect) {
        let block = Block::default()
            .title(" NEEDS ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));
        let inner = block.inner(area);
        f.render_widget(block, area);

        let header = Line::from(vec![
            Span::raw(format!("{:<12}", "Agent")),
            Span::styled(format!("{:^9}", "Satiety"), Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{:^9}", "Energy"),  Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{:^9}", "Fun"),      Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{:^9}", "Social"),   Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{:^9}", "Hygiene"),  Style::default().fg(Color::DarkGray)),
        ]);

        const CRIT:   f32 = 20.0;
        const SEVERE: f32 = 10.0;

        let mut lines = vec![header];
        for snap in &self.agent_needs {
            let name_color = self.roster.iter()
                .find(|(n, _)| n == &snap.agent_name)
                .map(|(_, c)| *c)
                .unwrap_or(Color::White);

            let needs_arr = [snap.hunger, snap.energy, snap.fun, snap.social, snap.hygiene];
            let has_severe = needs_arr.iter().any(|&n| n < SEVERE);
            let has_crit   = needs_arr.iter().any(|&n| n < CRIT);

            let name_style = if has_severe {
                Style::default().fg(name_color).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(name_color)
            };

            let mut spans = vec![
                Span::styled(format!("{:<12}", snap.agent_name), name_style),
                need_span(snap.hunger),
                need_span(snap.energy),
                need_span(snap.fun),
                need_span(snap.social),
                need_span(snap.hygiene),
            ];
            if has_severe {
                spans.push(Span::styled(" ⚠", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)));
            } else if has_crit {
                spans.push(Span::styled(" ⚠", Style::default().fg(Color::Yellow)));
            }
            lines.push(Line::from(spans));
        }

        let para = Paragraph::new(lines);
        f.render_widget(para, inner);
    }

    fn render_help_overlay(&self, f: &mut ratatui::Frame, area: ratatui::layout::Rect) {
        let popup_width  = 50u16;
        let popup_height = 13u16;
        let x = area.x + area.width.saturating_sub(popup_width) / 2;
        let y = area.y + area.height.saturating_sub(popup_height) / 2;
        let popup_area = Rect {
            x, y,
            width:  popup_width.min(area.width),
            height: popup_height.min(area.height),
        };

        let block = Block::default()
            .title(" KEYBINDINGS ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::LightCyan));

        let lines = vec![
            Line::from(vec![
                Span::styled("  q", Style::default().fg(Color::LightYellow).add_modifier(Modifier::BOLD)),
                Span::raw("         Quit"),
            ]),
            Line::from(vec![
                Span::styled("  j / k", Style::default().fg(Color::LightYellow).add_modifier(Modifier::BOLD)),
                Span::raw("     Scroll log down/up"),
            ]),
            Line::from(vec![
                Span::styled("  [ / ]", Style::default().fg(Color::LightYellow).add_modifier(Modifier::BOLD)),
                Span::raw("     Jump to prev/next tick"),
            ]),
            Line::from(vec![
                Span::styled("  Space", Style::default().fg(Color::LightYellow).add_modifier(Modifier::BOLD)),
                Span::raw("       Expand selected entry"),
            ]),
            Line::from(vec![
                Span::styled("  G", Style::default().fg(Color::LightYellow).add_modifier(Modifier::BOLD)),
                Span::raw("           Resume auto-scroll"),
            ]),
            Line::from(vec![
                Span::styled("  l", Style::default().fg(Color::LightYellow).add_modifier(Modifier::BOLD)),
                Span::raw("           Toggle tile legend"),
            ]),
            Line::from(vec![
                Span::styled("  1 – 5", Style::default().fg(Color::LightYellow).add_modifier(Modifier::BOLD)),
                Span::raw("     Inspect agent (toggle)"),
            ]),
            Line::from(vec![
                Span::styled("  ?", Style::default().fg(Color::LightYellow).add_modifier(Modifier::BOLD)),
                Span::raw("           This help overlay"),
            ]),
            Line::from(vec![
                Span::styled("  d", Style::default().fg(Color::LightYellow).add_modifier(Modifier::BOLD)),
                Span::raw("           LLM debug log"),
            ]),
        ];

        let para = Paragraph::new(lines).block(block);
        f.render_widget(ratatui::widgets::Clear, popup_area);
        f.render_widget(para, popup_area);
    }

    fn render_inspect_panel(&self, f: &mut ratatui::Frame, area: ratatui::layout::Rect, agent_idx: usize) {
        let Some(snap) = self.agent_needs.get(agent_idx) else { return; };

        let panel_width  = area.width.min(50);
        let panel_height = area.height.min(20);
        let x = area.x + area.width.saturating_sub(panel_width);
        let y = area.y;
        let panel_area = Rect { x, y, width: panel_width, height: panel_height };

        let agent_color = agent_color_for_id(snap.agent_id);
        let title = format!(" {} ({},{}) ", snap.agent_name, snap.agent_pos.0, snap.agent_pos.1);
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(agent_color));

        let mut lines: Vec<Line> = Vec::new();

        // Needs
        lines.push(Line::from(Span::styled("NEEDS", Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD))));
        let needs_arr = [
            ("Satiety", snap.hunger),
            ("Energy",  snap.energy),
            ("Fun",     snap.fun),
            ("Social",  snap.social),
            ("Hygiene", snap.hygiene),
        ];
        for (label, val) in &needs_arr {
            lines.push(Line::from(vec![
                Span::raw(format!("  {:<9}", label)),
                need_span(*val),
            ]));
        }

        // Devotion
        let dev_filled = ((snap.devotion / 100.0 * 5.0).round() as usize).min(5);
        let dev_bar = format!("{}{}", "█".repeat(dev_filled), "░".repeat(5 - dev_filled));
        lines.push(Line::from(vec![
            Span::raw(format!("  {:<9}", "Devotion")),
            Span::styled(
                format!("{} {:>3.0}", dev_bar, snap.devotion),
                Style::default().fg(Color::Magenta),
            ),
        ]));

        // Memories
        if !snap.memories.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled("MEMORIES", Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD))));
            for mem in snap.memories.iter().take(3) {
                let truncated = if mem.len() > (panel_width as usize).saturating_sub(4) {
                    format!("{}…", &mem[..(panel_width as usize).saturating_sub(5)])
                } else {
                    mem.clone()
                };
                lines.push(Line::from(Span::styled(
                    format!("  {}", truncated),
                    Style::default().fg(Color::Gray),
                )));
            }
        }

        // Beliefs
        if !snap.beliefs.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled("BELIEFS", Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD))));
            for (about, belief) in snap.beliefs.iter().take(3) {
                let text = format!("  About {}: {}", about, belief);
                let truncated = if text.len() > (panel_width as usize).saturating_sub(2) {
                    format!("{}…", &text[..(panel_width as usize).saturating_sub(3)])
                } else {
                    text
                };
                lines.push(Line::from(Span::styled(
                    truncated,
                    Style::default().fg(Color::LightMagenta),
                )));
            }
        }

        let para = Paragraph::new(lines).block(block);
        f.render_widget(ratatui::widgets::Clear, panel_area);
        f.render_widget(para, panel_area);
    }

    fn render_llm_overlay(&self, f: &mut ratatui::Frame, area: ratatui::layout::Rect) {
        let block = Block::default()
            .title(format!(
                " LLM DEBUG — Day {}  [< > day  Tab entry  j/k scroll  Space expand  d/Esc close] ",
                self.llm_overlay_day
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::LightCyan));
        let inner = block.inner(area);
        f.render_widget(block, area);

        let width = inner.width as usize;
        let empty: Vec<LlmCallRecord> = Vec::new();
        let calls = self.llm_calls.get(&self.llm_overlay_day).unwrap_or(&empty);

        let mut lines: Vec<Line<'static>> = Vec::new();

        if calls.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("  No LLM calls recorded for Day {}.", self.llm_overlay_day),
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            for (i, call) in calls.iter().enumerate() {
                let is_selected = i == self.llm_overlay_entry;
                let is_expanded = self.llm_overlay_expanded.contains(&i);
                let prefix = if is_selected { "▶ " } else { "  " };
                let expand_icon = if is_expanded { "▼" } else { "▶" };
                let header_style = if is_selected {
                    Style::default().fg(Color::LightCyan).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Cyan)
                };
                lines.push(Line::from(Span::styled(
                    format!("{}[{}] {}  {}", prefix, call.call_type, call.agent_name, expand_icon),
                    header_style,
                )));
                if is_expanded {
                    lines.push(Line::from(Span::styled(
                        "  PROMPT:".to_string(),
                        Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD),
                    )));
                    for line in wrap_text(&call.prompt, width.saturating_sub(4)) {
                        lines.push(Line::from(Span::styled(
                            format!("    {}", line),
                            Style::default().fg(Color::Gray),
                        )));
                    }
                    lines.push(Line::from(Span::styled(
                        "  RESPONSE:".to_string(),
                        Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD),
                    )));
                    for line in wrap_text(&call.response, width.saturating_sub(4)) {
                        lines.push(Line::from(Span::styled(
                            format!("    {}", line),
                            Style::default().fg(Color::White),
                        )));
                    }
                }
            }
        }

        let total  = lines.len();
        let height = inner.height as usize;
        let scroll = self.llm_overlay_scroll.min(total.saturating_sub(height));
        let para = Paragraph::new(lines).scroll((scroll as u16, 0));
        f.render_widget(para, inner);
    }

    // -----------------------------------------------------------------------
    // Build flat log lines for the event log panel
    // -----------------------------------------------------------------------

    fn build_log_lines(&self) -> Vec<Line<'static>> {
        let mut out: Vec<Line<'static>> = Vec::new();
        let wrap_width = self.log_wrap_width;

        for (idx, entry) in self.log_entries.iter().enumerate() {
            let is_selected = idx == self.selected;
            let is_expanded = self.expanded.contains(&idx);

            match entry {
                LogEntry::TickHeader { tick, day, time_of_day } => {
                    let text = format!(
                        "━━━ TICK {} │ Day {} │ {} ━━━",
                        tick, day, time_of_day
                    );
                    let style = Style::default()
                        .fg(Color::LightBlue)
                        .add_modifier(Modifier::BOLD);
                    let bg = if is_selected {
                        Style::default().fg(Color::LightBlue).add_modifier(Modifier::BOLD).bg(Color::DarkGray)
                    } else {
                        style
                    };
                    out.push(Line::from(Span::styled(text, bg)));
                }

                LogEntry::AgentAction(snap) => {
                    let agent_color = agent_color_for_id(snap.agent_id);
                    let loc_color   = location_rat_color(&snap.location);

                    let header_bg = if is_selected {
                        Style::default().bg(Color::DarkGray)
                    } else {
                        Style::default()
                    };

                    let pos_str = format!("({},{})", snap.agent_pos.0, snap.agent_pos.1);
                    let mut header_spans = vec![
                        Span::styled(
                            format!("[{:<10}]", snap.agent_name),
                            Style::default().fg(agent_color).add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(" @ "),
                        Span::styled(
                            format!("{:<16}", snap.location),
                            Style::default().fg(loc_color),
                        ),
                        Span::raw(format!(" {} │ ", pos_str)),
                        Span::raw(snap.action_line.clone()),
                    ];

                    if let Some(ref tier) = snap.outcome_tier_label {
                        header_spans.push(Span::raw(" │ "));
                        header_spans.push(Span::styled(
                            tier.clone(),
                            Style::default().fg(tier_rat_color(tier)),
                        ));
                    }

                    if let Some(ms) = snap.llm_duration_ms {
                        if ms > 0 {
                            header_spans.push(Span::styled(
                                format!(" ({}ms)", ms),
                                Style::default().fg(Color::DarkGray),
                            ));
                        }
                    }

                    out.push(Line::from(header_spans).patch_style(header_bg));

                    if let Some(ref prayer) = snap.prayer_text {
                        out.push(Line::from(Span::styled(
                            format!("  \"{}\"", prayer),
                            Style::default().fg(Color::LightMagenta).add_modifier(Modifier::ITALIC),
                        )));
                    }

                    if !snap.outcome_line.is_empty() {
                        let wrapped = wrap_text(&snap.outcome_line, wrap_width);
                        let limit = if is_expanded { wrapped.len() } else { 2.min(wrapped.len()) };
                        for line in wrapped.iter().take(limit) {
                            out.push(Line::from(Span::styled(
                                format!("  > {}", line),
                                Style::default().fg(Color::Gray),
                            )));
                        }
                        if !is_expanded && wrapped.len() > 2 {
                            out.push(Line::from(Span::styled(
                                format!("  [+{} more lines — Space to expand]", wrapped.len() - 2),
                                Style::default().fg(Color::DarkGray),
                            )));
                        }
                    }
                }

                LogEntry::DayBoundary { kind, agent_id, agent_name, day, text } => {
                    let agent_color = agent_color_for_id(*agent_id);
                    let (icon, label, color) = match kind {
                        DayBoundaryKind::MorningIntention  => ("☀", "Morning",    Color::LightYellow),
                        DayBoundaryKind::EveningDesire     => ("★", "Desire",     Color::LightMagenta),
                        DayBoundaryKind::EveningReflection => ("✎", "Reflection", Color::LightCyan),
                    };

                    let header_bg = if is_selected {
                        Style::default().bg(Color::DarkGray)
                    } else {
                        Style::default()
                    };

                    out.push(Line::from(vec![
                        Span::styled(icon, Style::default().fg(color)),
                        Span::raw(" "),
                        Span::styled(agent_name.clone(), Style::default().fg(agent_color).add_modifier(Modifier::BOLD)),
                        Span::raw(format!(" — Day {} — ", day)),
                        Span::styled(label, Style::default().fg(color)),
                    ]).patch_style(header_bg));

                    let wrapped = wrap_text(text, wrap_width);
                    let limit = if is_expanded { wrapped.len() } else { 3.min(wrapped.len()) };
                    for line in wrapped.iter().take(limit) {
                        out.push(Line::from(Span::styled(
                            format!("  {}", line),
                            Style::default().fg(Color::Gray),
                        )));
                    }
                    if !is_expanded && wrapped.len() > 3 {
                        out.push(Line::from(Span::styled(
                            format!("  [+{} more — Space to expand]", wrapped.len() - 3),
                            Style::default().fg(Color::DarkGray),
                        )));
                    }
                }

                LogEntry::WorldEvent { day, text } => {
                    let bg = if is_selected { Style::default().bg(Color::DarkGray) } else { Style::default() };
                    out.push(Line::from(vec![
                        Span::styled("⚡ ", Style::default().fg(Color::LightYellow)),
                        Span::styled(format!("Day {} — World Event: ", day), Style::default().fg(Color::LightYellow).add_modifier(Modifier::BOLD)),
                    ]).patch_style(bg));
                    let wrapped = wrap_text(text, wrap_width.saturating_sub(4));
                    for line in wrapped.iter().take(3) {
                        out.push(Line::from(Span::styled(
                            format!("  {}", line),
                            Style::default().fg(Color::Yellow),
                        )));
                    }
                }

                LogEntry::Thinking { agent_id, agent_name, tokens } => {
                    let agent_color = agent_color_for_id(*agent_id);
                    out.push(Line::from(vec![
                        Span::styled(
                            format!("[{:<10}]", agent_name),
                            Style::default().fg(agent_color).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            " thinking…".to_string(),
                            Style::default().fg(Color::DarkGray),
                        ),
                        Span::styled(
                            {
                                let start = tokens.len().saturating_sub(40);
                                let start = (start..=tokens.len()).find(|&i| tokens.is_char_boundary(i)).unwrap_or(tokens.len());
                                format!(" {}", &tokens[start..])
                            },
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]));
                }

                LogEntry::SimComplete { total_ticks, magic_count, notable } => {
                    let days = total_ticks / 48;
                    out.push(Line::from(Span::styled(
                        "═".repeat(50),
                        Style::default().fg(Color::LightGreen),
                    )));
                    out.push(Line::from(Span::styled(
                        format!("  SIMULATION COMPLETE — {} days ({} ticks)  Magic: {}",
                            days, total_ticks, magic_count),
                        Style::default().fg(Color::LightGreen).add_modifier(Modifier::BOLD),
                    )));
                    if !notable.is_empty() {
                        out.push(Line::from(Span::styled(
                            "  Notable events:",
                            Style::default().fg(Color::LightGreen),
                        )));
                        for ev in notable {
                            out.push(Line::from(Span::styled(
                                format!("    * {}", ev),
                                Style::default().fg(Color::Green),
                            )));
                        }
                    }
                    out.push(Line::from(Span::styled(
                        "  Press q to exit.",
                        Style::default().fg(Color::DarkGray),
                    )));
                    out.push(Line::from(Span::styled(
                        "═".repeat(50),
                        Style::default().fg(Color::LightGreen),
                    )));
                }
            }
        }

        out
    }

    // -----------------------------------------------------------------------
    // Main run loop
    // -----------------------------------------------------------------------

    pub fn run(&mut self, mut rx: mpsc::Receiver<TuiEvent>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        enable_raw_mode().map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        let backend  = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend).map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        let result = self.event_loop(&mut terminal, &mut rx);

        // Always cleanup even on error
        let _ = disable_raw_mode();
        let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture);
        let _ = terminal.show_cursor();

        result
    }

    fn event_loop(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
        rx:       &mut mpsc::Receiver<TuiEvent>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        loop {
            // Drain pending events
            loop {
                match rx.try_recv() {
                    Ok(ev)  => self.process_event(ev),
                    Err(_)  => break,
                }
            }

            terminal.draw(|f| self.draw(f))
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

            if self.should_quit {
                break;
            }

            if event::poll(Duration::from_millis(16))
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?
            {
                match event::read()
                    .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?
                {
                    Event::Key(k)   => self.handle_input(k),
                    Event::Mouse(m) => self.handle_mouse(m),
                    _ => {}
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for segment in text.split('\n') {
        let segment = segment.trim();
        if segment.is_empty() { continue; }
        let mut current = String::new();
        for word in segment.split_whitespace() {
            if current.is_empty() {
                current.push_str(word);
            } else if current.len() + 1 + word.len() <= width {
                current.push(' ');
                current.push_str(word);
            } else {
                out.push(current.clone());
                current = word.to_string();
            }
        }
        if !current.is_empty() {
            out.push(current);
        }
    }
    if out.is_empty() {
        out.push(text.to_string());
    }
    out
}

fn need_span(v: f32) -> Span<'static> {
    let color = ccolor::to_ratatui_color(ccolor::needs_color(v));
    let filled = ((v / 100.0 * 5.0).round() as usize).min(5);
    let bar = format!("{}{}", "█".repeat(filled), "░".repeat(5 - filled));
    Span::styled(
        format!("{} {:>3.0}", bar, v),
        Style::default().fg(color),
    )
}

fn agent_color_for_id(id: usize) -> Color {
    ccolor::to_ratatui_color(ccolor::agent_color(id))
}

fn location_rat_color(loc: &str) -> Color {
    ccolor::to_ratatui_color(ccolor::location_color(loc))
}

fn tier_rat_color(tier: &str) -> Color {
    ccolor::to_ratatui_color(ccolor::tier_color(tier))
}
