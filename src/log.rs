use std::fs::{self, OpenOptions};
use std::io::Write as IoWrite;

use chrono::Local;
use colored::Colorize;
use tracing::warn;

use crate::agent::Agent;
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
    /// When true, write every LLM prompt+response to runs/{id}/llm_debug.md.
    pub debug_llm: bool,
}

impl RunLog {
    pub fn new(seed: u64) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let ts     = Local::now().format("%Y%m%d_%H%M%S");
        let run_id = format!("{}_{}", ts, seed);
        let dir    = format!("runs/{}", run_id);
        fs::create_dir_all(&dir)?;
        let log_path = format!("{}/tick_log.txt", dir);
        Ok(RunLog { run_id, log_path, tui_mode: false, debug_llm: false })
    }

    /// Append a prompt+response pair to runs/{id}/llm_debug.md (no-op if debug_llm is false).
    pub fn write_llm_debug(&self, call_type: &str, agent: &str, prompt: &str, response: &str) {
        if !self.debug_llm { return; }
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
// State dump (JSON snapshot)
// ---------------------------------------------------------------------------

pub fn write_state_dump(
    run_id:  &str,
    tick:    u32,
    agents:  &[Agent],
    seed:    u64,
) {
    let dir  = format!("runs/{}", run_id);
    let path = format!("{}/state_dump_tick_{:04}.json", dir, tick);

    let state = serde_json::json!({
        "seed":  seed,
        "tick":  tick,
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
// Wishes file
// ---------------------------------------------------------------------------

pub fn append_wishes(souls_dir: &str, agent_name: &str, header: &str, content: &str) {
    let path  = format!("{}/{}.wishes.md", souls_dir, agent_name.to_lowercase());
    let entry = format!("\n{}\n{}\n", header, content);
    let file  = OpenOptions::new().create(true).append(true).open(&path);
    match file {
        Ok(mut f) => { let _ = f.write_all(entry.as_bytes()); }
        Err(e)    => warn!("Could not append wishes for {}: {}", agent_name, e),
    }
}

// ---------------------------------------------------------------------------
// Prayer persistence
// ---------------------------------------------------------------------------

// FEAT-8: Richer headers with run_id, day, tick, tod.
pub fn append_prayer(
    souls_dir:  &str,
    agent_name: &str,
    run_id:     &str,
    day:        u32,
    tick:       u32,
    tod:        &str,
    content:    &str,
) {
    let path  = format!("{}/{}.prayers.md", souls_dir, agent_name.to_lowercase());
    let date  = Local::now().format("%Y-%m-%d");
    let header = format!("## Run {} | Day {} | Tick {} | {} | {}", run_id, day, tick, tod, date);
    let entry = format!("\n{}\n{}\n", header, content);
    let file  = OpenOptions::new().create(true).append(true).open(&path);
    match file {
        Ok(mut f) => { let _ = f.write_all(entry.as_bytes()); }
        Err(e)    => warn!("Could not append prayer for {}: {}", agent_name, e),
    }
}

// ---------------------------------------------------------------------------
// Praise persistence (FEAT-15)
// ---------------------------------------------------------------------------

pub fn append_praise(
    souls_dir:  &str,
    agent_name: &str,
    run_id:     &str,
    day:        u32,
    tick:       u32,
    tod:        &str,
    content:    &str,
) {
    let path  = format!("{}/{}.praises.md", souls_dir, agent_name.to_lowercase());
    let date  = Local::now().format("%Y-%m-%d");
    let header = format!("## Run {} | Day {} | Tick {} | {} | {}", run_id, day, tick, tod, date);
    let entry = format!("\n{}\n{}\n", header, content);
    let file  = OpenOptions::new().create(true).append(true).open(&path);
    match file {
        Ok(mut f) => { let _ = f.write_all(entry.as_bytes()); }
        Err(e)    => warn!("Could not append praise for {}: {}", agent_name, e),
    }
}

// ---------------------------------------------------------------------------
// Haiku persistence (FEAT-16)
// ---------------------------------------------------------------------------

pub fn append_haiku(
    souls_dir:  &str,
    agent_name: &str,
    run_id:     &str,
    day:        u32,
    tick:       u32,
    tod:        &str,
    haiku:      &str,
    score:      u32,
    verdict:    &str,
) {
    let path  = format!("{}/{}.haikus.md", souls_dir, agent_name.to_lowercase());
    let date  = Local::now().format("%Y-%m-%d");
    let header = format!("## Run {} | Day {} | Tick {} | {} | {} | Score: {}", run_id, day, tick, tod, date, score);
    let entry = format!("\n{}\n{}\n\n*{}*\n", header, haiku, verdict);
    let file  = OpenOptions::new().create(true).append(true).open(&path);
    match file {
        Ok(mut f) => { let _ = f.write_all(entry.as_bytes()); }
        Err(e)    => warn!("Could not append haiku for {}: {}", agent_name, e),
    }
}

// ---------------------------------------------------------------------------
// Oracle persistence
// ---------------------------------------------------------------------------

pub fn load_oracle_response(souls_dir: &str, agent_name: &str) -> String {
    let path = format!("{}/{}.oracle_responses.md", souls_dir, agent_name.to_lowercase());
    fs::read_to_string(&path).unwrap_or_default()
}

// FEAT-8: Richer header with run_id and day.
pub fn archive_oracle_response(souls_dir: &str, agent_name: &str, run_id: &str, day: u32, content: &str) {
    // Append to history
    let history_path = format!("{}/{}.oracle_history.md", souls_dir, agent_name.to_lowercase());
    let date  = Local::now().format("%Y-%m-%d");
    let entry = format!("\n## {} | Run {} | Day {} — {}\n{}\n", agent_name, run_id, day, date, content);
    let file  = OpenOptions::new().create(true).append(true).open(&history_path);
    match file {
        Ok(mut f) => { let _ = f.write_all(entry.as_bytes()); }
        Err(e)    => warn!("Could not append oracle history for {}: {}", agent_name, e),
    }
    // Clear the response file
    let response_path = format!("{}/{}.oracle_responses.md", souls_dir, agent_name.to_lowercase());
    if let Err(e) = fs::write(&response_path, "") {
        warn!("Could not clear oracle responses for {}: {}", agent_name, e);
    }
}

// ---------------------------------------------------------------------------
// Attribute growth persistence (FEAT-21)
// ---------------------------------------------------------------------------

/// Saved attribute scores + XP. Applies on top of soul seed values.
pub struct GrowthData {
    pub scores: std::collections::HashMap<String, u32>,
    pub xp:     std::collections::HashMap<String, u32>,
}

/// Load grown attribute scores and XP from `souls/{name}.growth.md`.
/// Returns empty maps if the file is missing or unparseable.
pub fn load_growth(souls_dir: &str, name: &str) -> GrowthData {
    let path    = format!("{}/{}.growth.md", souls_dir, name.to_lowercase());
    let content = match fs::read_to_string(&path) { Ok(s) => s, Err(_) => return GrowthData { scores: Default::default(), xp: Default::default() } };
    let mut scores = std::collections::HashMap::new();
    let mut xp     = std::collections::HashMap::new();
    for line in content.lines() {
        // Format: "- vigor: 7 xp: 2"
        let line = line.trim().trim_start_matches('-').trim();
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 4 && parts[2] == "xp:" {
            // "vigor:" "7" "xp:" "2"  → strip trailing ':'
            if let (Some(attr), Ok(score), Ok(x)) = (
                parts[0].strip_suffix(':'),
                parts[1].parse::<u32>(),
                parts[3].parse::<u32>(),
            ) {
                scores.insert(attr.to_string(), score);
                xp.insert(attr.to_string(), x);
            }
        }
    }
    GrowthData { scores, xp }
}

/// Overwrite `souls/{name}.growth.md` with the agent's current attribute scores and XP.
pub fn save_growth(
    souls_dir: &str,
    name:      &str,
    run_id:    &str,
    attrs:     &crate::agent::Attributes,
    xp:        &std::collections::HashMap<String, u32>,
) {
    let path = format!("{}/{}.growth.md", souls_dir, name.to_lowercase());
    let date = Local::now().format("%Y-%m-%d");
    let body = format!(
        "# Attribute Growth — {}\nLast updated: Run {} — {}\n\n- vigor: {} xp: {}\n- wit: {} xp: {}\n- grace: {} xp: {}\n- heart: {} xp: {}\n- numen: {} xp: {}\n",
        name, run_id, date,
        attrs.vigor, xp.get("vigor").copied().unwrap_or(0),
        attrs.wit,   xp.get("wit")  .copied().unwrap_or(0),
        attrs.grace, xp.get("grace").copied().unwrap_or(0),
        attrs.heart, xp.get("heart").copied().unwrap_or(0),
        attrs.numen, xp.get("numen").copied().unwrap_or(0),
    );
    if let Err(e) = fs::write(&path, body) {
        warn!("Could not save growth for {}: {}", name, e);
    }
}

// ---------------------------------------------------------------------------
// Relationship persistence (FEAT-18)
// ---------------------------------------------------------------------------

/// Append current affinity values to `souls/{name}.relationships.md`.
pub fn save_relationships(
    souls_dir: &str,
    name:      &str,
    run_id:    &str,
    affinity:  &std::collections::HashMap<String, f32>,
) {
    if affinity.is_empty() { return; }
    let path  = format!("{}/{}.relationships.md", souls_dir, name.to_lowercase());
    let date  = Local::now().format("%Y-%m-%d");
    let mut entry = format!("\n## Run {} — {}\n", run_id, date);
    let mut pairs: Vec<_> = affinity.iter().collect();
    pairs.sort_by_key(|(k, _)| k.as_str());
    for (other, v) in pairs {
        entry.push_str(&format!("- {}: {:.1}\n", other, v));
    }
    let file = OpenOptions::new().create(true).append(true).open(&path);
    match file {
        Ok(mut f) => { let _ = f.write_all(entry.as_bytes()); }
        Err(e)    => warn!("Could not save relationships for {}: {}", name, e),
    }
}

/// Load the most recent affinity values for each named agent from
/// `souls/{name}.relationships.md`.
pub fn load_relationships(souls_dir: &str, name: &str) -> std::collections::HashMap<String, f32> {
    let path    = format!("{}/{}.relationships.md", souls_dir, name.to_lowercase());
    let content = match fs::read_to_string(&path) { Ok(s) => s, Err(_) => return Default::default() };
    let mut map: std::collections::HashMap<String, f32> = Default::default();
    for line in content.lines() {
        // Format: "- Rowan: 15.0"
        let line = line.trim().trim_start_matches('-').trim();
        if let Some((k, v)) = line.split_once(':') {
            if let Ok(val) = v.trim().parse::<f32>() {
                map.insert(k.trim().to_string(), val);
            }
        }
    }
    map
}

// ---------------------------------------------------------------------------
// Journal excerpt loader (FEAT-20)
// ---------------------------------------------------------------------------

/// Read the last `n_days` narrative day-sections from an agent's journal file.
/// Day sections are those whose header matches "## Run ... Day ... —".
/// Returns an empty string when no sections are found or the file is missing.
pub fn load_journal_excerpt(souls_dir: &str, name: &str, n_days: usize) -> String {
    let path = format!("{}/{}.journal.md", souls_dir, name.to_lowercase());
    let content = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return String::new(),
    };

    let mut sections: Vec<String> = Vec::new();
    let mut current: Option<String> = None;

    for line in content.lines() {
        // Detect day-narrative headers: "## Run ... Day ... — date"
        if line.starts_with("## Run ") && line.contains(" Day ") && line.contains(" — ") {
            if let Some(s) = current.take() { sections.push(s); }
            current = Some(format!("{}\n", line));
        } else if line.starts_with("## ") {
            // Some other header (e.g. bullet-list run summary) — flush and skip
            if let Some(s) = current.take() { sections.push(s); }
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
// Story persistence
// ---------------------------------------------------------------------------

pub fn load_story(souls_dir: &str, name: &str) -> String {
    let path = format!("{}/{}.story.md", souls_dir, name.to_lowercase());
    fs::read_to_string(&path).unwrap_or_default()
}

pub fn save_story(souls_dir: &str, name: &str, story: &str) {
    let path = format!("{}/{}.story.md", souls_dir, name.to_lowercase());
    if let Err(e) = fs::write(&path, story) {
        warn!("Could not save story for {}: {}", name, e);
    }
}

pub fn append_day_journal(souls_dir: &str, agent_name: &str, run_id: &str, day: u32, story: &str) {
    let path  = format!("{}/{}.journal.md", souls_dir, agent_name.to_lowercase());
    let date  = Local::now().format("%Y-%m-%d");
    let entry = format!("\n## Run {} Day {} — {}\n{}\n", run_id, day, date, story);
    let file  = OpenOptions::new().create(true).append(true).open(&path);
    match file {
        Ok(mut f) => { let _ = f.write_all(entry.as_bytes()); }
        Err(e)    => warn!("Could not append day journal for {}: {}", agent_name, e),
    }
}

// ---------------------------------------------------------------------------
// Journal append
// ---------------------------------------------------------------------------

pub fn append_journal(
    souls_dir:      &str,
    agent_name:     &str,
    run_id:         &str,
    ticks:          u32,
    notable_events: &[String],
) {
    let days    = ticks / 48;
    let path    = format!("{}/{}.journal.md", souls_dir, agent_name.to_lowercase());
    let date    = Local::now().format("%Y-%m-%d");

    let mut entry = format!(
        "\n## Run {} — {} — {} day{} ({} ticks)\n",
        run_id,
        date,
        days,
        if days == 1 { "" } else { "s" },
        ticks,
    );

    if notable_events.is_empty() {
        entry.push_str("- A quiet run. Nothing of great note occurred.\n");
    } else {
        for event in notable_events {
            entry.push_str(&format!("- {}\n", event));
        }
    }

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path);

    match file {
        Ok(mut f) => { let _ = f.write_all(entry.as_bytes()); }
        Err(e)    => warn!("Could not append journal for {}: {}", agent_name, e),
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
