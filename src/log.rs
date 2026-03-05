use std::fs::{self, OpenOptions};
use std::io::Write as IoWrite;
use std::path::Path;

use chrono::Local;
use tracing::warn;

use crate::agent::Agent;

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

    pub fn write_line(&self, line: &str) {
        // Print to stdout
        println!("{}", line);

        // Append to file
        if let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
        {
            let _ = writeln!(f, "{}", line);
        } else {
            warn!("Could not write to tick log file");
        }
    }

    pub fn write_block(&self, lines: &[String]) {
        for line in lines {
            self.write_line(line);
        }
    }
}

// ---------------------------------------------------------------------------
// Tick header
// ---------------------------------------------------------------------------

pub fn tick_header(tick: u32, day: u32, time_of_day: &str) -> String {
    format!("\n=== TICK {} | Day {} | {} ===", tick, day, time_of_day)
}

pub fn time_of_day(tick_in_day: u32, night_start: u32) -> &'static str {
    if tick_in_day < 8             { "Dawn" }
    else if tick_in_day < 16       { "Morning" }
    else if tick_in_day < night_start      { "Afternoon" }
    else if tick_in_day < night_start + 8  { "Evening" }
    else                           { "Night" }
}

// ---------------------------------------------------------------------------
// Needs footer
// ---------------------------------------------------------------------------

pub fn needs_footer(agents: &[Agent]) -> String {
    let parts: Vec<String> = agents
        .iter()
        .map(|a| format!("{} [{}]", a.name(), a.needs.compact()))
        .collect();
    format!("  Needs: {}", parts.join(" | "))
}

// ---------------------------------------------------------------------------
// Agent tick entry lines
// ---------------------------------------------------------------------------

pub struct TickEntry {
    pub agent_name:   String,
    pub location:     String,
    pub action_line:  String,   // "Chat with Elara | Heart 15 vs DC 8 | Success"
    pub outcome_line: String,   // "> Warm conversation... [Social +20, Fun +8]"
}

impl TickEntry {
    pub fn format(&self) -> Vec<String> {
        let header = format!("  [{:<10}] @ {:<16} | {}", self.agent_name, self.location, self.action_line);
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
