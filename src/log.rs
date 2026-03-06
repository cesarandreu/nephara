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
}

impl RunLog {
    pub fn new(seed: u64) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let ts     = Local::now().format("%Y%m%d_%H%M%S");
        let run_id = format!("{}_{}", ts, seed);
        let dir    = format!("runs/{}", run_id);
        fs::create_dir_all(&dir)?;
        let log_path = format!("{}/tick_log.txt", dir);
        Ok(RunLog { run_id, log_path })
    }

    /// Print the colored string to stdout; write plain text to file.
    pub fn write_line(&self, line: &str) {
        println!("{}", line);

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

pub fn tick_header(tick: u32, day: u32, time_of_day: &str) -> String {
    let s = format!("\n=== TICK {} | Day {} | {} ===", tick, day, time_of_day);
    format!("{}", s.color(colored::Color::BrightBlue).bold())
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

        let header = format!("  [{}] @ {} {} | {}",
            colored_name, colored_loc, pos_str, colored_action_line);
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
// Run summary
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
