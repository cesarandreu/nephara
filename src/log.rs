use std::fs::{self, OpenOptions};
use std::io::Write as IoWrite;
use std::collections::HashMap;

use chrono::Local;
use colored::Colorize;
use tracing::warn;

use crate::agent::{Agent, AgentBeliefs, Inventory, ItemKind};
use crate::color;

// ---------------------------------------------------------------------------
// ANSI stripping — used to write plain text to log file
// ---------------------------------------------------------------------------

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c2 in chars.by_ref() { if c2 == 'm' { break; } }
        } else {
            out.push(c);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Run directory setup
// ---------------------------------------------------------------------------

pub struct RunLog {
    pub run_id:   String,
    pub log_path: String,
    /// When true, suppress stdout output (TUI mode — file log still written).
    pub tui_mode: bool,
}

impl RunLog {
    pub fn new(seed: u64) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let ts     = Local::now().format("%Y%m%d_%H%M%S");
        let run_id = format!("{}_{}", ts, seed);
        let dir    = format!("runs/{}", run_id);
        fs::create_dir_all(&dir)?;
        let log_path = format!("{}/tick_log.txt", dir);
        Ok(RunLog { run_id, log_path, tui_mode: false })
    }

    /// Test-only constructor: no directory creation, all writes are no-ops.
    #[cfg(test)]
    pub fn new_test() -> Self {
        RunLog {
            run_id:   "test".to_string(),
            log_path: "/dev/null".to_string(),
            tui_mode: true,
        }
    }

    /// Append a prompt+response pair to runs/{id}/llm_debug.md (always written).
    pub fn write_llm_debug(&self, call_type: &str, agent: &str, prompt: &str, response: &str) {
        let path = format!("runs/{}/llm_debug.md", self.run_id);
        let entry = format!(
            "## {} — {}\n### PROMPT\n```\n{}\n```\n### RESPONSE\n```\n{}\n```\n---\n\n",
            call_type, agent, prompt, response
        );
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
            let _ = IoWrite::write_all(&mut f, entry.as_bytes());
        }
    }

    /// Print the colored string to stdout (unless tui_mode); write plain text to file.
    pub fn write_line(&self, line: &str) {
        if !self.tui_mode {
            println!("{}", line);
        }

        let plain = strip_ansi(line);
        if let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
        {
            let _ = writeln!(f, "{}", plain);
        } else {
            warn!("Could not write to tick log file");
        }
    }
}

// ---------------------------------------------------------------------------
// Tick header
// ---------------------------------------------------------------------------

// FEAT-7: blank separator line after the header for readability.
pub fn tick_header(tick: u32, day: u32, time_of_day: &str) -> String {
    let s = format!("\n=== TICK {} | Day {} | {} ===", tick, day, time_of_day);
    format!("{}\n", s.color(colored::Color::BrightBlue).bold())
}

pub fn time_of_day(tick_in_day: u32, night_start: u32) -> &'static str {
    if tick_in_day < 8             { "Dawn" }
    else if tick_in_day < 16      { "Morning" }
    else if tick_in_day < night_start     { "Afternoon" }
    else if tick_in_day < night_start + 8 { "Evening" }
    else                           { "Night" }
}

// ---------------------------------------------------------------------------
// Needs footer
// ---------------------------------------------------------------------------

pub fn needs_footer(agents: &[Agent]) -> String {
    let header = format!("  {:<12}{:>8}{:>8}{:>8}{:>8}{:>8}",
        "Needs:", "Satiety", "Energy", "Fun", "Social", "Hygiene");
    let mut rows = vec![header];
    for (i, a) in agents.iter().enumerate() {
        let name_padded = format!("{:<12}", a.name());
        let colored_name = format!("{}", name_padded.color(color::agent_color(i)));
        let row = format!("  {}{}{}{}{}{}",
            colored_name,
            fmt_need_val(a.needs.hunger,  8),
            fmt_need_val(a.needs.energy,  8),
            fmt_need_val(a.needs.fun,     8),
            fmt_need_val(a.needs.social,  8),
            fmt_need_val(a.needs.hygiene, 8),
        );
        rows.push(row);
    }
    rows.join("\n")
}

fn fmt_need_val(v: f32, width: usize) -> String {
    format!("{}", format!("{:>width$.0}", v, width = width).color(color::needs_color(v)))
}

// ---------------------------------------------------------------------------
// Agent tick entry lines
// ---------------------------------------------------------------------------

pub struct TickEntry {
    pub agent_id:           usize,
    pub agent_pos:          (u8, u8),
    pub agent_name:         String,
    pub location:           String,
    pub action_line:        String,
    pub outcome_line:       String,
    /// The outcome tier label (e.g. "Success"), present only when a d20 check was made.
    pub outcome_tier_label: Option<String>,
    /// Total LLM time for this agent's turn in milliseconds.
    pub llm_duration_ms:    Option<u64>,
    /// True when the agent is currently busy (multi-tick action in progress).
    pub is_busy:            bool,
}

impl TickEntry {
    pub fn format(&self) -> Vec<String> {
        let agent_c = color::agent_color(self.agent_id);
        let loc_c   = color::location_color(&self.location);

        // Pad BEFORE colorizing so visual alignment is preserved in the terminal.
        let name_padded = format!("{:<10}", self.agent_name);
        let loc_padded  = format!("{:<16}", self.location);

        let colored_name = format!("{}", name_padded.color(agent_c).bold());
        let colored_loc  = format!("{}", loc_padded.color(loc_c));
        let pos_str      = format!("({},{})", self.agent_pos.0, self.agent_pos.1);

        // Color the tier label at the end of action_line when present.
        let colored_action_line = if let Some(ref tier) = self.outcome_tier_label {
            let suffix = format!(" | {}", tier);
            if self.action_line.ends_with(&suffix) {
                let prefix = &self.action_line[..self.action_line.len() - suffix.len()];
                format!("{} | {}", prefix, format!("{}", tier.color(color::tier_color(tier))))
            } else {
                self.action_line.clone()
            }
        } else {
            self.action_line.clone()
        };

        let timing_suffix = match self.llm_duration_ms {
            Some(ms) if ms > 0 => format!(" ({}ms)", ms),
            _ => String::new(),
        };

        let header = format!("  [{}] @ {} {} | {}{}",
            colored_name, colored_loc, pos_str, colored_action_line, timing_suffix);
        let mut lines = vec![header];

        if self.outcome_line.is_empty() {
            return lines;
        }

        const PREFIX_FIRST: &str = "             > ";
        const PREFIX_CONT:  &str = "               ";
        const WRAP_AT: usize = 65;

        let mut first_line = true;
        for segment in self.outcome_line.split('\n') {
            let segment = segment.trim();
            if segment.is_empty() { continue; }

            let mut current = String::new();
            for word in segment.split_whitespace() {
                if current.is_empty() {
                    current.push_str(word);
                } else if current.len() + 1 + word.len() <= WRAP_AT {
                    current.push(' ');
                    current.push_str(word);
                } else {
                    let prefix = if first_line { PREFIX_FIRST } else { PREFIX_CONT };
                    lines.push(format!("{}{}", prefix, current));
                    first_line = false;
                    current = word.to_string();
                }
            }
            if !current.is_empty() {
                let prefix = if first_line { PREFIX_FIRST } else { PREFIX_CONT };
                lines.push(format!("{}{}", prefix, current));
                first_line = false;
            }
        }

        lines
    }
}

// ---------------------------------------------------------------------------
// State dump (JSON snapshot) — single file, overwritten each dump interval
// ---------------------------------------------------------------------------

pub fn write_state_dump(
    run_id: &str,
    agents: &[Agent],
    seed:   u64,
) {
    let dir  = format!("runs/{}", run_id);
    let path = format!("{}/state_dump.json", dir);

    let state = serde_json::json!({
        "seed":  seed,
        "tick":  agents.first().map(|_| 0u32).unwrap_or(0),
        "agents": agents.iter().map(|a| serde_json::json!({
            "name":     a.name(),
            "pos":      [a.pos.0, a.pos.1],
            "busy_ticks": a.busy_ticks,
            "needs": {
                "hunger":  a.needs.hunger,
                "energy":  a.needs.energy,
                "fun":     a.needs.fun,
                "social":  a.needs.social,
                "hygiene": a.needs.hygiene,
            },
            "memory": a.memory.iter().take(5).collect::<Vec<_>>(),
            "inventory": a.inventory.iter()
                .map(|(k, v)| (format!("{:?}", k), v))
                .collect::<std::collections::HashMap<_, _>>(),
        })).collect::<Vec<_>>(),
    });

    match fs::write(&path, serde_json::to_string_pretty(&state).unwrap_or_default()) {
        Ok(_)  => {},
        Err(e) => warn!("Could not write state dump to {}: {}", path, e),
    }
}

// ---------------------------------------------------------------------------
// Introspection log
// ---------------------------------------------------------------------------

pub fn log_introspection(run_id: &str, agent_name: &str, day: u32, call_type: &str, content: &str) {
    let path  = format!("runs/{}/introspection.md", run_id);
    let entry = format!("\n### {} — Day {} {}\n{}\n", agent_name, day, call_type, content);
    let file  = OpenOptions::new().create(true).append(true).open(&path);
    match file {
        Ok(mut f) => { let _ = f.write_all(entry.as_bytes()); }
        Err(e)    => warn!("Could not write introspection for {}: {}", agent_name, e),
    }
}

// ---------------------------------------------------------------------------
// Chronicle — unified append-only log (replaces prayers, praises, etc.)
// ---------------------------------------------------------------------------

/// Append a timestamped entry to `souls/{name}.chronicle.md`.
/// Header format: `## Run {run_id} | Day {day} | Tick {tick} | {tod} | {date} | {entry_type}`
pub fn append_chronicle(
    souls_dir:  &str,
    name:       &str,
    run_id:     &str,
    day:        u32,
    tick:       u32,
    tod:        &str,
    entry_type: &str,
    content:    &str,
) {
    let path  = format!("{}/{}.chronicle.md", souls_dir, name.to_lowercase());
    let date  = Local::now().format("%Y-%m-%d");
    let entry = format!(
        "\n## Run {} | Day {} | Tick {} | {} | {} | {}\n{}\n",
        run_id, day, tick, tod, date, entry_type, content
    );
    let file  = OpenOptions::new().create(true).append(true).open(&path);
    match file {
        Ok(mut f) => { let _ = f.write_all(entry.as_bytes()); }
        Err(e)    => warn!("Could not append chronicle ({}) for {}: {}", entry_type, name, e),
    }
}

// ---------------------------------------------------------------------------
// Oracle persistence
// ---------------------------------------------------------------------------

pub fn load_oracle_response(souls_dir: &str, agent_name: &str) -> String {
    let path = format!("{}/{}.oracle_responses.md", souls_dir, agent_name.to_lowercase());
    fs::read_to_string(&path).unwrap_or_default()
}

/// Archive oracle response to chronicle and clear the oracle_responses.md file.
pub fn archive_oracle_response(
    souls_dir:  &str,
    agent_name: &str,
    run_id:     &str,
    day:        u32,
    tick:       u32,
    tod:        &str,
    content:    &str,
) {
    append_chronicle(souls_dir, agent_name, run_id, day, tick, tod, "oracle", content);
    // Clear the response file so the oracle is not re-read
    let response_path = format!("{}/{}.oracle_responses.md", souls_dir, agent_name.to_lowercase());
    if let Err(e) = fs::write(&response_path, "") {
        warn!("Could not clear oracle responses for {}: {}", agent_name, e);
    }
}

// ---------------------------------------------------------------------------
// Journal excerpt loader — reads chronicle.md, filters journal entries
// ---------------------------------------------------------------------------

/// Read the last `n_days` journal-type sections from an agent's chronicle file.
/// Returns an empty string when no sections are found or the file is missing.
pub fn load_journal_excerpt(souls_dir: &str, name: &str, n_days: usize) -> String {
    let path = format!("{}/{}.chronicle.md", souls_dir, name.to_lowercase());
    let content = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return String::new(),
    };

    let mut sections: Vec<String> = Vec::new();
    let mut current: Option<String> = None;

    for line in content.lines() {
        if line.starts_with("## ") {
            // Header format: "## Run {run_id} | Day {day} | Tick {tick} | {tod} | {date} | {entry_type}"
            // Field index 5 (0-based) is the entry_type when splitting on " | "
            let parts: Vec<&str> = line.splitn(7, " | ").collect();
            let is_journal = parts.get(5).map(|s| *s == "journal").unwrap_or(false);
            if let Some(s) = current.take() { sections.push(s); }
            if is_journal {
                current = Some(format!("{}\n", line));
            }
        } else if let Some(ref mut s) = current {
            s.push_str(line);
            s.push('\n');
        }
    }
    if let Some(s) = current { sections.push(s); }

    if sections.is_empty() { return String::new(); }

    let start = sections.len().saturating_sub(n_days);
    sections[start..].join("\n")
}

// ---------------------------------------------------------------------------
// Agent state persistence — replaces story, growth, relationships, beliefs
// ---------------------------------------------------------------------------

/// Consolidated agent state loaded at startup.
pub struct AgentState {
    pub story:         String,
    pub scores:        HashMap<String, u32>,
    pub xp:            HashMap<String, u32>,
    pub relationships: HashMap<String, f32>,
    pub beliefs:       HashMap<String, AgentBeliefs>,
    pub inventory:     Inventory,
}

impl Default for AgentState {
    fn default() -> Self {
        AgentState {
            story:         String::new(),
            scores:        HashMap::new(),
            xp:            HashMap::new(),
            relationships: HashMap::new(),
            beliefs:       HashMap::new(),
            inventory:     HashMap::new(),
        }
    }
}

/// Load consolidated agent state from `souls/{name}.state.md`.
/// Returns defaults if the file is missing or unparseable.
pub fn load_state(souls_dir: &str, name: &str) -> AgentState {
    let path    = format!("{}/{}.state.md", souls_dir, name.to_lowercase());
    let content = match fs::read_to_string(&path) {
        Ok(s)  => s,
        Err(_) => return AgentState::default(),
    };

    let mut state   = AgentState::default();
    let mut section = "";
    let mut story_lines: Vec<String> = Vec::new();

    for line in content.lines() {
        if line.starts_with("## ") {
            // Flush story if we just finished that section
            if section == "story" {
                state.story = story_lines.join("\n").trim().to_string();
            }
            section = match line.trim() {
                "## Story"         => "story",
                "## Attributes"    => "attributes",
                "## Relationships" => "relationships",
                "## Beliefs"       => "beliefs",
                "## Inventory"     => "inventory",
                _                  => "",
            };
            story_lines.clear();
            continue;
        }

        match section {
            "story" => {
                story_lines.push(line.to_string());
            }
            "attributes" => {
                // Format: "vigor: 3 xp: 0"
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 4 && parts[2] == "xp:" {
                    if let (Some(attr), Ok(score), Ok(x)) = (
                        parts[0].strip_suffix(':'),
                        parts[1].parse::<u32>(),
                        parts[3].parse::<u32>(),
                    ) {
                        state.scores.insert(attr.to_string(), score);
                        state.xp.insert(attr.to_string(), x);
                    }
                }
            }
            "relationships" => {
                // Format: "Rowan: 25.0"
                let line = line.trim();
                if let Some((k, v)) = line.split_once(':') {
                    if let Ok(val) = v.trim().parse::<f32>() {
                        state.relationships.insert(k.trim().to_string(), val);
                    }
                }
            }
            "beliefs" => {
                // Format: `Rowan: "some rumor text"`
                let line = line.trim();
                if let Some(colon_pos) = line.find(':') {
                    let about = line[..colon_pos].trim();
                    let rest  = line[colon_pos + 1..].trim();
                    let rumor = rest.trim_matches('"');
                    if !about.is_empty() && !rumor.is_empty() {
                        state.beliefs
                            .entry(about.to_string())
                            .or_insert_with(AgentBeliefs::default)
                            .rumors
                            .push(rumor.to_string());
                    }
                }
            }
            "inventory" => {
                // Format: `Berry: 2`
                let line = line.trim();
                if let Some((k, v)) = line.split_once(':') {
                    if let Ok(count) = v.trim().parse::<u8>() {
                        let kind = match k.trim() {
                            "Berry"       => Some(ItemKind::Berry),
                            "Fish"        => Some(ItemKind::Fish),
                            "Herb"        => Some(ItemKind::Herb),
                            "CookedMeal"  => Some(ItemKind::CookedMeal),
                            _             => None,
                        };
                        if let Some(kind) = kind {
                            state.inventory.insert(kind, count);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    // Flush story if it was the last section
    if section == "story" {
        state.story = story_lines.join("\n").trim().to_string();
    }

    state
}

/// Overwrite `souls/{name}.state.md` with the agent's current state.
pub fn save_state(
    souls_dir:     &str,
    name:          &str,
    run_id:        &str,
    story:         &str,
    attrs:         &crate::agent::Attributes,
    xp:            &HashMap<String, u32>,
    relationships: &HashMap<String, f32>,
    beliefs:       &HashMap<String, AgentBeliefs>,
    inventory:     &Inventory,
) {
    let path = format!("{}/{}.state.md", souls_dir, name.to_lowercase());
    let date = Local::now().format("%Y-%m-%d");

    let mut body = format!(
        "# State — {}\nLast run: {} | {}\n\n## Story\n{}\n\n## Attributes\nvigor: {} xp: {}\nwit: {} xp: {}\ngrace: {} xp: {}\nheart: {} xp: {}\nnumen: {} xp: {}\n",
        name, run_id, date,
        story.trim(),
        attrs.vigor, xp.get("vigor").copied().unwrap_or(0),
        attrs.wit,   xp.get("wit")  .copied().unwrap_or(0),
        attrs.grace, xp.get("grace").copied().unwrap_or(0),
        attrs.heart, xp.get("heart").copied().unwrap_or(0),
        attrs.numen, xp.get("numen").copied().unwrap_or(0),
    );

    if !relationships.is_empty() {
        body.push_str("\n## Relationships\n");
        let mut pairs: Vec<_> = relationships.iter().collect();
        pairs.sort_by_key(|(k, _)| k.as_str());
        for (other, v) in pairs {
            body.push_str(&format!("{}: {:.1}\n", other, v));
        }
    }

    if !beliefs.is_empty() {
        body.push_str("\n## Beliefs\n");
        let mut pairs: Vec<_> = beliefs.iter().collect();
        pairs.sort_by_key(|(k, _)| k.as_str());
        for (about, ab) in pairs {
            if let Some(rumor) = ab.rumors.last() {
                body.push_str(&format!("{}: \"{}\"\n", about, rumor));
            }
        }
    }

    if !inventory.is_empty() {
        body.push_str("\n## Inventory\n");
        let mut items: Vec<_> = inventory.iter().collect();
        items.sort_by_key(|(k, _)| format!("{:?}", k));
        for (kind, count) in items {
            body.push_str(&format!("{}: {}\n", format!("{:?}", kind), count));
        }
    }

    if let Err(e) = fs::write(&path, body) {
        warn!("Could not save state for {}: {}", name, e);
    }
}

// ---------------------------------------------------------------------------
// Run summary (terminal output)
// ---------------------------------------------------------------------------

pub fn print_run_summary(
    log:            &RunLog,
    ticks:          u32,
    agents:         &[Agent],
    magic_count:    u32,
    notable_events: &[String],
    seed:           u64,
) {
    let days = ticks / 48;
    log.write_line(&format!("\n{}", "=".repeat(60)));
    log.write_line(&format!("  RUN COMPLETE — seed:{} | {} days ({} ticks)", seed, days, ticks));
    log.write_line("=".repeat(60).as_str());
    log.write_line(&format!("  Magic casts: {}", magic_count));
    log.write_line("  Final needs:");
    for a in agents {
        log.write_line(&format!("    {} [{}]", a.name(), a.needs.compact()));
    }
    if !notable_events.is_empty() {
        log.write_line("  Notable events:");
        for ev in notable_events.iter() {
            log.write_line(&format!("    * {}", ev));
        }
    }
    log.write_line("=".repeat(60).as_str());
}

// ---------------------------------------------------------------------------
// Post-run summary file (FEAT-11)
// ---------------------------------------------------------------------------

pub fn write_run_summary(
    run_id:          &str,
    seed:            u64,
    total_ticks:     u32,
    agents:          &[Agent],
    initial_needs:   &[(String, crate::agent::Needs)],
    magic_count:     u32,
    notable_events:  &[String],
    run_duration_ms: u64,
    is_test_run:     bool,
    backend:         &str,
    model:           &str,
    smart_model:     Option<&str>,
    llm_url:         &str,
) {
    if is_test_run { return; }

    let days    = total_ticks / 48;
    let date    = Local::now().format("%Y-%m-%d %H:%M:%S");
    let dir     = format!("runs/{}", run_id);
    let path    = format!("{}/summary.md", dir);

    let smart_str = smart_model.unwrap_or("—");
    let mut body = format!(
        "# Run {} Summary\n\nGenerated: {}\n\n## Config\n- Backend: {}\n- Model: {}\n- Smart model: {}\n- LLM URL: {}\n- Seed: {}\n- Ticks: {}\n- Agents: {}\n\n## Overview\n- Seed: {}\n- Duration: {} ticks / {} day{}\n- Agents: {}\n- Magic spells cast: {}\n- Wall time: {:.1}s\n\n",
        run_id, date,
        backend, model, smart_str, llm_url,
        seed, total_ticks, agents.len(),
        seed,
        total_ticks, days, if days == 1 { "" } else { "s" },
        agents.len(),
        magic_count,
        run_duration_ms as f64 / 1000.0,
    );

    body.push_str("## Agent Summaries\n");
    for agent in agents {
        let name = agent.name();
        body.push_str(&format!("\n### {}\n", name));
        body.push_str(&format!("- Final needs: Satiety:{:.0} Energy:{:.0} Fun:{:.0} Social:{:.0} Hygiene:{:.0}\n",
            agent.needs.hunger, agent.needs.energy, agent.needs.fun, agent.needs.social, agent.needs.hygiene));
        if let Some((_, init)) = initial_needs.iter().find(|(n, _)| n == name) {
            body.push_str(&format!("- Net changes: Satiety:{:+.0} Energy:{:+.0} Fun:{:+.0} Social:{:+.0} Hygiene:{:+.0}\n",
                agent.needs.hunger - init.hunger,
                agent.needs.energy - init.energy,
                agent.needs.fun    - init.fun,
                agent.needs.social - init.social,
                agent.needs.hygiene - init.hygiene,
            ));
        }
    }

    if !notable_events.is_empty() {
        body.push_str("\n## Notable Events\n");
        for ev in notable_events {
            body.push_str(&format!("- {}\n", ev));
        }
    } else {
        body.push_str("\n## Notable Events\nA quiet run.\n");
    }

    if let Err(e) = fs::write(&path, body) {
        warn!("Could not write run summary to {}: {}", path, e);
    }
}
