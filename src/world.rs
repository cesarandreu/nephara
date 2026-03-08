use chrono::Local as ChronoLocal;
use colored::Colorize;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, warn};

use crate::action::{self, Action, OutcomeTier, Resolution};
use crate::agent::{Agent, NeedChanges};
use crate::color;
use crate::config::Config;
use crate::llm::LlmBackend;
use crate::log::{self as runlog, RunLog, TickEntry};
use crate::magic;
use crate::soul::SoulSeed;
use crate::tui_event::{AgentNeedsSnapshot, DayEvent, DayEventKind, MapCell, TuiEvent};

// ---------------------------------------------------------------------------
// Grid constants
// ---------------------------------------------------------------------------

pub const GRID_W: usize = 32;
pub const GRID_H: usize = 32;

/// Home positions per agent index (x=col, y=row).
pub const HOME_POSITIONS: &[(u8, u8)] = &[
    ( 5, 17),  // 0
    ( 8, 22),  // 1
    (23, 22),  // 2
    ( 5, 24),  // 3 — south of home 0
    (11, 24),  // 4 — south of square, west of river
    ( 5, 27),  // 5
    (11, 27),  // 6
    (23, 26),  // 7 — deeper meadow
];

pub const MAX_AGENTS: usize = HOME_POSITIONS.len();

// ---------------------------------------------------------------------------
// TileType
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum TileType {
    Open,
    Forest,
    River,
    Square,
    Tavern,
    Well,
    Meadow,
    Home(usize),
    Temple,
}

fn tile_char(tile: TileType) -> char {
    match tile {
        TileType::Open    => '.',
        TileType::Forest  => 'F',
        TileType::River   => '~',
        TileType::Square  => 'S',
        TileType::Tavern  => 'V',
        TileType::Well    => 'W',
        TileType::Meadow  => 'M',
        TileType::Home(_) => 'h',
        TileType::Temple  => 'P',
    }
}

// ---------------------------------------------------------------------------
// Resource nodes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum ResourceKind {
    BerryBush,
    FishSchool,
    Campfire,
    HerbPatch,
}

pub struct ResourceNode {
    pub kind:         ResourceKind,
    pub pos:          (u8, u8),
    pub charges:      u32,
    pub max_charges:  u32,
    pub respawn_ticks: u32,
}

impl ResourceNode {
    pub fn is_available(&self) -> bool {
        self.charges > 0
    }

    pub fn deplete(&mut self, respawn_ticks: u32) {
        if self.charges > 0 {
            self.charges -= 1;
            if self.charges == 0 {
                self.respawn_ticks = respawn_ticks;
            }
        }
    }

    pub fn tick_respawn(&mut self) {
        if self.charges < self.max_charges && self.respawn_ticks > 0 {
            self.respawn_ticks -= 1;
            if self.respawn_ticks == 0 {
                self.charges = self.max_charges;
            }
        }
    }

    pub fn map_char(&self) -> char {
        if self.is_available() {
            match self.kind {
                ResourceKind::BerryBush  => '✿',
                ResourceKind::FishSchool => '≋',
                ResourceKind::Campfire   => '✦',
                ResourceKind::HerbPatch  => '✜',
            }
        } else {
            '·'
        }
    }

    pub fn node_color(&self) -> colored::Color {
        if self.is_available() {
            match self.kind {
                ResourceKind::BerryBush  => colored::Color::BrightMagenta,
                ResourceKind::FishSchool => colored::Color::BrightCyan,
                ResourceKind::Campfire   => colored::Color::BrightRed,
                ResourceKind::HerbPatch  => colored::Color::BrightGreen,
            }
        } else {
            colored::Color::BrightBlack
        }
    }
}

fn build_resource_nodes(n_agents: usize) -> Vec<ResourceNode> {
    let mut nodes = Vec::new();

    for &pos in &[(3u8, 3u8), (8, 5), (12, 7)] {
        nodes.push(ResourceNode {
            kind: ResourceKind::BerryBush,
            pos,
            charges: 3,
            max_charges: 3,
            respawn_ticks: 0,
        });
    }

    for &pos in &[(16u8, 6u8), (16, 14)] {
        nodes.push(ResourceNode {
            kind: ResourceKind::FishSchool,
            pos,
            charges: 4,
            max_charges: 4,
            respawn_ticks: 0,
        });
    }

    for &pos in &HOME_POSITIONS[..n_agents] {
        nodes.push(ResourceNode {
            kind: ResourceKind::Campfire,
            pos,
            charges: 5,
            max_charges: 5,
            respawn_ticks: 0,
        });
    }

    for &pos in &[(5u8, 7u8), (10, 4)] {
        nodes.push(ResourceNode {
            kind: ResourceKind::HerbPatch,
            pos,
            charges: 2,
            max_charges: 2,
            respawn_ticks: 0,
        });
    }

    nodes
}

// ---------------------------------------------------------------------------
// Helper: visible state label for nearby agents
// ---------------------------------------------------------------------------

fn agent_visible_state(a: &Agent, config: &Config) -> Option<&'static str> {
    let t = &config.needs.thresholds;
    if a.is_busy()                       { return Some("busy"); }
    if a.needs.energy < t.penalty_severe { return Some("exhausted"); }
    if a.needs.hunger < t.penalty_severe { return Some("hungry"); }
    if a.needs.fun    < t.penalty_severe { return Some("withdrawn"); }
    if a.needs.social < t.penalty_severe { return Some("lonely"); }
    None
}

// ---------------------------------------------------------------------------
// Tick result (returned from World::tick)
// ---------------------------------------------------------------------------

pub struct TickResult {
    pub tick:        u32,
    pub day:         u32,
    pub time_of_day: &'static str,
    pub entries:     Vec<TickEntry>,
    pub map:         String,
    /// Day-boundary events (morning intentions, evening reflections/desires)
    /// generated during this tick.
    pub day_events:  Vec<DayEvent>,
}

// ---------------------------------------------------------------------------
// World
// ---------------------------------------------------------------------------

pub struct World {
    pub tick_num:            u32,
    pub agents:              Vec<Agent>,
    pub seed:                u64,
    pub config:              Config,
    pub run_log:             RunLog,
    pub souls_dir:           String,
    pub notable_events:      Vec<(usize, String)>,
    pub magic_count:         u32,
    pub magic_cast_this_day: Vec<bool>,
    pub resource_nodes:      Vec<ResourceNode>,
    pub is_test_run:         bool,
    pub pending_day_events:  Vec<DayEvent>,
    /// When true (--no-tui mode), stream LLM action tokens to stdout.
    pub token_echo:          bool,
    /// When Some (TUI mode), send PartialToken events to the TUI.
    pub tui_tx:              Option<tokio::sync::mpsc::Sender<TuiEvent>>,
    grid:                    [[TileType; GRID_W]; GRID_H],
    rng:                     StdRng,
    llm:                     Arc<dyn LlmBackend>,
    llm_smart:               Arc<dyn LlmBackend>,
    llm_call_counter:        u64,
}

impl World {
    // -----------------------------------------------------------------------
    // Construction
    // -----------------------------------------------------------------------

    pub fn new(
        seeds:       Vec<SoulSeed>,
        config:      Config,
        seed:        u64,
        rng:         StdRng,
        llm:         Arc<dyn LlmBackend>,
        llm_smart:   Arc<dyn LlmBackend>,
        run_log:     RunLog,
        souls_dir:   String,
        is_test_run: bool,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        if seeds.is_empty() {
            return Err("at least one soul seed is required (souls/ directory is empty)".into());
        }
        if seeds.len() > MAX_AGENTS {
            return Err(format!(
                "{} soul seeds found but maximum supported is {} — remove some from souls/",
                seeds.len(), MAX_AGENTS
            ).into());
        }
        let n_agents = seeds.len();
        let agents = seeds.iter().enumerate()
            .map(|(i, s)| Agent::from_soul(i, s, &config, HOME_POSITIONS[i]))
            .collect();
        let grid           = build_grid(n_agents);
        let resource_nodes = build_resource_nodes(n_agents);
        Ok(World {
            tick_num: 0,
            agents,
            seed,
            config,
            run_log,
            souls_dir,
            notable_events: Vec::new(),
            magic_count: 0,
            magic_cast_this_day: vec![false; n_agents],
            resource_nodes,
            is_test_run,
            pending_day_events: Vec::new(),
            token_echo: false,
            tui_tx: None,
            grid,
            rng,
            llm,
            llm_smart,
            llm_call_counter: 0,
        })
    }

    /// Load life stories and oracle responses for each agent (called after construction).
    pub async fn load_stories(&mut self) {
        for agent in &mut self.agents {
            agent.life_story = runlog::load_story(&self.souls_dir, &agent.identity.name);
            let oracle = runlog::load_oracle_response(&self.souls_dir, &agent.identity.name);
            if !oracle.trim().is_empty() {
                agent.oracle_pending = true;
                tracing::info!(agent = %agent.identity.name, "Oracle response pending");
            }
            // FEAT-20: load raw journal excerpt; will be summarized by summarize_journal_memories()
            agent.journal_summary = runlog::load_journal_excerpt(
                &self.souls_dir, &agent.identity.name, self.config.memory.journal_n_runs,
            );
        }
    }

    /// FEAT-20: LLM-summarize each agent's raw journal excerpt into 1–2 sentences.
    /// Skipped in mock/test mode; keeps raw excerpt on LLM failure.
    pub async fn summarize_journal_memories(&mut self) {
        if self.is_test_run { return; }

        let n_agents = self.agents.len();
        let mut loaded = 0usize;
        for i in 0..n_agents {
            let excerpt = self.agents[i].journal_summary.clone();
            if excerpt.is_empty() { continue; }
            let name = self.agents[i].identity.name.clone();
            let prompt = format!(
                "You are {}. Here are your recent journal entries:\n{}\n\nSummarize in 1-2 sentences what {} remembers from past days.",
                name, excerpt, name
            );
            let max_tokens = self.config.llm.journal_summary_max_tokens;
            match self.llm_smart.generate(&prompt, max_tokens, Some(self.seed), None, None).await {
                Ok(summary) => {
                    let summary = summary.trim().to_string();
                    if !summary.is_empty() {
                        self.agents[i].journal_summary = summary;
                        loaded += 1;
                    }
                }
                Err(e) => warn!("Journal memory summarization failed for {}: {}", name, e),
            }
        }
        if loaded > 0 {
            tracing::info!("Loaded journal memories for {} agents", loaded);
        }
    }

    /// Build a token streaming sender for the current agent's main action LLM call.
    /// Returns None if streaming is disabled. When Some, the spawned task forwards
    /// tokens to stdout (no-tui mode) or to the TUI as PartialToken events (TUI mode).
    fn make_token_tx(&self, idx: usize) -> Option<UnboundedSender<String>> {
        if self.token_echo {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
            tokio::spawn(async move {
                use std::io::Write;
                while let Some(token) = rx.recv().await {
                    print!("{}", token);
                    let _ = std::io::stdout().flush();
                }
                println!();
            });
            Some(tx)
        } else if let Some(ref tui_tx) = self.tui_tx {
            let tui_tx = tui_tx.clone();
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
            tokio::spawn(async move {
                while let Some(token) = rx.recv().await {
                    let _ = tui_tx.send(TuiEvent::PartialToken { agent_id: idx, token }).await;
                }
            });
            Some(tx)
        } else {
            None
        }
    }

    // -----------------------------------------------------------------------
    // Tick
    // -----------------------------------------------------------------------

    pub async fn tick(&mut self) -> Result<TickResult, Box<dyn std::error::Error + Send + Sync>> {
        let tick        = self.tick_num;
        let tpd         = self.config.time.ticks_per_day;
        let day         = tick / tpd + 1;
        let tick_in_day = tick % tpd;
        let is_night    = tick_in_day >= self.config.time.night_start_tick;
        let tod         = runlog::time_of_day(tick_in_day, self.config.time.night_start_tick);

        if tick_in_day == 0 {
            if tick > 0 {
                let prev_day = day - 1;
                for idx in 0..self.agents.len() {
                    self.end_of_day_reflection(idx, prev_day).await?;
                }
                for idx in 0..self.agents.len() {
                    self.end_of_day_desires(idx, prev_day).await?;
                }
            }
            for idx in 0..self.agents.len() {
                self.morning_planning(idx, day).await?;
            }
            for flag in &mut self.magic_cast_this_day { *flag = false; }
        }

        let mut order: Vec<usize> = (0..self.agents.len()).collect();
        order.shuffle(&mut self.rng);

        let mut entries = Vec::new();

        for &idx in &order {
            let entry = self.process_agent(idx, tick, day, is_night, tod).await?;
            entries.push(entry);
        }

        // Passive need decay
        for agent in &mut self.agents {
            agent.needs.apply_decay(&self.config.needs.decay_per_tick);
        }

        // Resource node respawn countdown
        for node in &mut self.resource_nodes {
            node.tick_respawn();
        }

        self.tick_num += 1;

        let map        = self.render_map();
        let day_events = std::mem::take(&mut self.pending_day_events);
        Ok(TickResult { tick, day, time_of_day: tod, entries, map, day_events })
    }

    // -----------------------------------------------------------------------
    // Process one agent for the current tick
    // -----------------------------------------------------------------------

    async fn process_agent(
        &mut self,
        idx:      usize,
        tick:     u32,
        day:      u32,
        is_night: bool,
        tod:      &str,
    ) -> Result<TickEntry, Box<dyn std::error::Error + Send + Sync>> {
        // --- Busy tick ---
        if self.agents[idx].is_busy() {
            if let Some(energy) = self.agents[idx].sleep_energy_tick {
                self.agents[idx].needs.energy += energy;
                self.agents[idx].needs.clamp();
            }
            self.agents[idx].busy_ticks -= 1;
            let ticks_left = self.agents[idx].busy_ticks;
            let tile       = self.tile_at(self.agents[idx].pos);
            let loc_name   = self.tile_name(tile);
            return Ok(TickEntry {
                agent_id:           idx,
                agent_pos:          self.agents[idx].pos,
                agent_name:         self.agents[idx].name().to_string(),
                location:           loc_name,
                action_line:        format!("(busy — {} tick{} remaining)", ticks_left, if ticks_left == 1 { "" } else { "s" }),
                outcome_line:       String::new(),
                outcome_tier_label: None,
                llm_duration_ms:    None,
            });
        }

        let t0 = Instant::now();

        // --- Forced sleep if energy < forced_action threshold ---
        let (action, reason, description) = if self.agents[idx].needs.energy < self.config.needs.thresholds.forced_action
            && self.is_at_own_home(idx)
        {
            (Action::Sleep, None, None)
        } else if self.agents[idx].needs.energy < self.config.needs.thresholds.forced_action {
            (Action::Move { destination: "home".to_string() }, None, None)
        } else {
            let canonical = self.available_canonical_names(idx);
            let canonical_strs: Vec<&str> = canonical.iter().copied().collect();
            let schema = action::build_action_schema(&canonical_strs);

            let prompt    = self.build_prompt(idx, tick, day, is_night, tod, self.magic_cast_this_day[idx]);
            let token_tx  = self.make_token_tx(idx);
            let call_seed = Some(self.seed.wrapping_add(self.llm_call_counter));
            self.llm_call_counter += 1;
            let llm = Arc::clone(&self.llm);
            let raw = llm
                .generate(&prompt, self.config.llm.max_tokens, call_seed, Some(&schema), token_tx)
                .await
                .unwrap_or_else(|e| {
                    warn!("LLM error for {}: {}", self.agents[idx].name(), e);
                    String::new()
                });
            self.run_log.write_llm_debug("action", self.agents[idx].name(), &prompt, &raw);
            debug!(target: "action", agent = %self.agents[idx].name(), raw = %raw, "Agent action response");
            action::parse_response(&raw)
        };

        let action   = self.validate(idx, action);
        let tile     = self.tile_at(self.agents[idx].pos);
        let loc_name = self.tile_name(tile);
        let mut entry = self.resolve_and_apply(idx, action, &loc_name, tick, day, tod, is_night, description).await?;

        if let Some(r) = reason.filter(|r| !r.is_empty()) {
            entry.outcome_line = format!("{}\n({})", entry.outcome_line, r);
        }

        entry.llm_duration_ms = Some(t0.elapsed().as_millis() as u64);
        Ok(entry)
    }

    // -----------------------------------------------------------------------
    // Validate action — returns the action unchanged or wander
    // -----------------------------------------------------------------------

    fn validate(&self, idx: usize, action: Action) -> Action {
        let pos  = self.agents[idx].pos;
        let tile = self.tile_at(pos);

        match action {
            Action::ReadOracle if !(
                self.tile_at(self.agents[idx].pos) == TileType::Temple
                    && self.agents[idx].oracle_pending
            ) => self.wander_action(idx),
            Action::Eat     if !self.tile_allows(tile, "eat")     => self.wander_action(idx),
            Action::Cook    if !self.tile_allows(tile, "cook")    => self.wander_action(idx),
            Action::Sleep   if !self.is_at_own_home(idx)          => self.wander_action(idx),
            Action::Forage  if !self.tile_allows(tile, "forage")  => self.wander_action(idx),
            Action::Fish    if !self.tile_allows(tile, "fish")    => self.wander_action(idx),
            Action::Exercise if !self.tile_allows(tile, "exercise") => self.wander_action(idx),
            Action::Bathe   if !self.tile_allows(tile, "bathe")   => self.wander_action(idx),
            Action::Explore if !self.tile_allows(tile, "explore") => self.wander_action(idx),
            Action::Play    if !self.tile_allows(tile, "play")    => self.wander_action(idx),
            Action::Wander  => self.wander_action(idx),

            Action::Chat { target_name } => {
                let target_ok = self.agents.iter().enumerate().any(|(i, a)| {
                    i != idx
                        && a.name().eq_ignore_ascii_case(&target_name)
                        && Self::chebyshev_dist(a.pos, pos) <= 1
                        && !a.is_busy()
                });
                if target_ok {
                    return Action::Chat { target_name };
                }
                let partner_name = self.agents.iter()
                    .find(|a| a.id != idx && Self::chebyshev_dist(a.pos, pos) <= 1 && !a.is_busy())
                    .map(|a| a.name().to_string());
                match partner_name {
                    Some(name) => Action::Chat { target_name: name },
                    None       => self.wander_action(idx),
                }
            }

            Action::Move { destination } => {
                if self.parse_tile_type(&destination, idx).is_some() {
                    Action::Move { destination }
                } else {
                    self.wander_action(idx)
                }
            }

            other => other,
        }
    }

    fn wander_action(&self, idx: usize) -> Action {
        let pos          = self.agents[idx].pos;
        let current_tile = self.tile_at(pos);
        let options = [
            ("Forest",         TileType::Forest),
            ("River",          TileType::River),
            ("Village Square", TileType::Square),
            ("Tavern",         TileType::Tavern),
            ("Village Well",   TileType::Well),
            ("Eastern Meadow", TileType::Meadow),
            ("Temple",         TileType::Temple),
        ];
        let valid: Vec<_> = options.iter()
            .filter(|(_, t)| *t != current_tile)
            .collect();
        if valid.is_empty() {
            return Action::Rest;
        }
        let pick = (self.tick_num as usize + idx) % valid.len();
        Action::Move { destination: valid[pick].0.to_string() }
    }

    // -----------------------------------------------------------------------
    // Resolve and apply
    // -----------------------------------------------------------------------

    async fn resolve_and_apply(
        &mut self,
        idx:         usize,
        action:      Action,
        loc_name:    &str,
        tick:        u32,
        day:         u32,
        tod:         &str,
        is_night:    bool,
        description: Option<String>,
    ) -> Result<TickEntry, Box<dyn std::error::Error + Send + Sync>> {
        match action {
            // ---- Move ----
            Action::Move { destination } => {
                let target_type = self.parse_tile_type(&destination, idx)
                    .unwrap_or(TileType::Square);
                let pos          = self.agents[idx].pos;
                let current_tile = self.tile_at(pos);

                if current_tile == target_type {
                    let arrived = self.tile_name(current_tile);
                    let mem = format!("Tick {tick} | Day {day} | {tod} | Arrived at {arrived}");
                    let buf = self.config.memory.buffer_size;
                    self.agents[idx].push_memory(mem, buf);
                    return Ok(TickEntry {
                        agent_id:           idx,
                        agent_pos:          self.agents[idx].pos,
                        agent_name:         self.agents[idx].name().to_string(),
                        location:           loc_name.to_string(),
                        action_line:        format!("Move > {} (arrived)", arrived),
                        outcome_line:       format!("{} is already at {}.", self.agents[idx].name(), arrived),
                        outcome_tier_label: None,
                        llm_duration_ms:    None,
                    });
                }

                if let Some(nearest) = self.nearest_tile_of_type(pos, target_type) {
                    let next_pos = Self::step_toward(pos, nearest);
                    self.agents[idx].pos = next_pos;
                    let mem = format!("Tick {tick} | Day {day} | {tod} | Moving toward {destination}");
                    let buf = self.config.memory.buffer_size;
                    self.agents[idx].push_memory(mem, buf);
                    Ok(TickEntry {
                        agent_id:           idx,
                        agent_pos:          self.agents[idx].pos,
                        agent_name:         self.agents[idx].name().to_string(),
                        location:           loc_name.to_string(),
                        action_line:        format!("Move → {}", destination),
                        outcome_line:       format!("{} moves toward {}.", self.agents[idx].name(), destination),
                        outcome_tier_label: None,
                        llm_duration_ms:    None,
                    })
                } else {
                    Ok(TickEntry {
                        agent_id:           idx,
                        agent_pos:          self.agents[idx].pos,
                        agent_name:         self.agents[idx].name().to_string(),
                        location:           loc_name.to_string(),
                        action_line:        format!("Move → {} (unreachable)", destination),
                        outcome_line:       format!("{} wanders, unable to find {}.", self.agents[idx].name(), destination),
                        outcome_tier_label: None,
                        llm_duration_ms:    None,
                    })
                }
            }

            // ---- Chat ----
            Action::Chat { target_name } => {
                self.resolve_chat(idx, &target_name, loc_name, tick, day, tod, is_night).await
            }

            // ---- Cast Intent ----
            Action::CastIntent { intent } => {
                self.resolve_cast_intent(idx, &intent, loc_name, tick, day, tod).await
            }

            // ---- Pray ----
            Action::Pray { prayer } => {
                self.resolve_pray(idx, &prayer, loc_name, tick, day, tod).await
            }

            // ---- Praise ----
            Action::Praise { praise_text } => {
                self.resolve_praise(idx, &praise_text, loc_name, tick, day, tod).await
            }

            // ---- Compose ----
            Action::Compose { haiku } => {
                self.resolve_compose(idx, &haiku, loc_name, tick, day, tod).await
            }

            // ---- Read Oracle ----
            Action::ReadOracle => {
                self.resolve_read_oracle(idx, loc_name, tick, day, tod).await
            }

            // ---- Sleep ----
            Action::Sleep => {
                let duration     = self.config.actions.sleep.duration_ticks.unwrap_or(16);
                let energy_ptick = self.config.actions.sleep.energy_restore_per_tick.unwrap_or(6.25);
                self.agents[idx].busy_ticks        = duration - 1;
                self.agents[idx].sleep_energy_tick = Some(energy_ptick);
                self.agents[idx].needs.energy     += energy_ptick;
                self.agents[idx].needs.clamp();
                let mem = format!("Tick {tick} | Day {day} | {tod} | Fell asleep");
                let buf = self.config.memory.buffer_size;
                self.agents[idx].push_memory(mem, buf);
                Ok(TickEntry {
                    agent_id:           idx,
                    agent_pos:          self.agents[idx].pos,
                    agent_name:         self.agents[idx].name().to_string(),
                    location:           loc_name.to_string(),
                    action_line:        "Sleep".to_string(),
                    outcome_line:       format!("{} falls into a deep sleep.", self.agents[idx].name()),
                    outcome_tier_label: None,
                    llm_duration_ms:    None,
                })
            }

            // ---- Standard d20 resolution ----
            action => {
                let res = {
                    let attrs  = &self.agents[idx].attributes;
                    let needs  = &self.agents[idx].needs;
                    let config = &self.config;
                    action::resolve(&action, attrs, needs, config, is_night, &mut self.rng)
                };

                if res.duration > 1 {
                    self.agents[idx].busy_ticks        = res.duration - 1;
                    self.agents[idx].sleep_energy_tick = None;
                }

                let pos = self.agents[idx].pos;
                let mut need_changes = res.need_changes.clone();

                // Resource node bonus (Success/CriticalSuccess only)
                if matches!(res.tier, OutcomeTier::Success | OutcomeTier::CriticalSuccess) {
                    let node_idx = self.find_resource_node(pos, &action);
                    if let Some(ni) = node_idx {
                        let respawn = self.config.world.resource_respawn_ticks;
                        self.apply_resource_at(ni, &mut need_changes, respawn);
                    }
                }

                // Well + Bathe override: more effective hygiene restoration
                if matches!(&action, Action::Bathe) && self.tile_at(pos) == TileType::Well {
                    need_changes.hygiene = Some(80.0 * res.tier.multiplier());
                }

                self.agents[idx].needs.apply(&need_changes);

                let nearby: Vec<String> = self.agents.iter()
                    .filter(|a| a.id != idx && Self::chebyshev_dist(a.pos, self.agents[idx].pos) <= 1)
                    .map(|a| a.name().to_string())
                    .collect();
                let agent_name_str = self.agents[idx].name().to_string();
                let dm_prompt = Self::build_dm_prompt(
                    &agent_name_str, &res.action.display(), &res.tier, loc_name, &nearby,
                    description.as_deref(),
                );
                let call_seed = Some(self.seed.wrapping_add(self.llm_call_counter));
                self.llm_call_counter += 1;
                let llm = Arc::clone(&self.llm);
                let narrator_max = self.config.llm.narrator_max_tokens;
                debug!(target: "narrate", agent = %agent_name_str, action = %res.action.display(),
                       tier = %res.tier.label(), "DM Narrator prompt sent");
                let narrator_result = llm.generate(&dm_prompt, narrator_max, call_seed, None, None).await;
                self.run_log.write_llm_debug("narrator", &agent_name_str, &dm_prompt,
                    narrator_result.as_ref().map(|s| s.as_str()).unwrap_or(""));
                let narrative = match narrator_result {
                    Ok(n) if !n.trim().is_empty() => {
                        let n = n.trim().to_string();
                        debug!(target: "narrate", narrative = %n, "GM Narrator response");
                        n
                    },
                    _ => self.narrative_for(&res, idx),
                };

                let check_line  = res.check_line();
                let action_line = if check_line.is_empty() {
                    res.action.display()
                } else {
                    format!("{} | {}", res.action.display(), check_line)
                };

                if res.tier == OutcomeTier::CriticalSuccess {
                    let ev = format!("Day {day}: {} got a critical success on {}",
                        self.agents[idx].name(), res.action.name());
                    self.notable_events.push((idx, ev));
                }
                if res.tier == OutcomeTier::CriticalFail {
                    let ev = format!("Day {day}: {} critically failed at {}",
                        self.agents[idx].name(), res.action.name());
                    self.notable_events.push((idx, ev));
                }

                let needs_note = need_changes.describe();
                let mem = format!("Tick {tick} | Day {day} | {tod} | {} — {} [{}]",
                    res.action.name(), res.tier.label(), needs_note);
                let buf = self.config.memory.buffer_size;
                self.agents[idx].push_memory(mem, buf);

                let tier_label = res.tier.label().to_string();
                Ok(TickEntry {
                    agent_id:           idx,
                    agent_pos:          self.agents[idx].pos,
                    agent_name:         self.agents[idx].name().to_string(),
                    location:           loc_name.to_string(),
                    action_line,
                    outcome_line:       narrative,
                    outcome_tier_label: if res.dc > 0 { Some(tier_label) } else { None },
                    llm_duration_ms:    None,
                })
            }
        }
    }

    // -----------------------------------------------------------------------
    // Resource node helpers
    // -----------------------------------------------------------------------

    /// Find a charged resource node at `pos` compatible with `action`. Returns its index.
    fn find_resource_node(&self, pos: (u8, u8), action: &Action) -> Option<usize> {
        let compatible: &[ResourceKind] = match action {
            Action::Forage => &[ResourceKind::BerryBush, ResourceKind::HerbPatch],
            Action::Fish   => &[ResourceKind::FishSchool],
            Action::Cook   => &[ResourceKind::Campfire],
            _              => return None,
        };
        self.resource_nodes.iter().position(|n| {
            n.pos == pos && compatible.contains(&n.kind) && n.is_available()
        })
    }

    /// Apply the resource bonus to `need_changes` and deplete the node.
    fn apply_resource_at(&mut self, node_idx: usize, need_changes: &mut NeedChanges, respawn_ticks: u32) {
        match self.resource_nodes[node_idx].kind {
            ResourceKind::BerryBush => {
                need_changes.hunger = Some(need_changes.hunger.unwrap_or(0.0) + 15.0);
            }
            ResourceKind::FishSchool => {
                need_changes.hunger = Some(need_changes.hunger.unwrap_or(0.0) + 20.0);
            }
            ResourceKind::Campfire => {
                need_changes.fun = Some(need_changes.fun.unwrap_or(0.0) + 10.0);
            }
            ResourceKind::HerbPatch => {
                need_changes.fun     = Some(need_changes.fun    .unwrap_or(0.0) + 10.0);
                need_changes.hygiene = Some(need_changes.hygiene.unwrap_or(0.0) + 5.0);
            }
        }
        self.resource_nodes[node_idx].deplete(respawn_ticks);
    }

    // -----------------------------------------------------------------------
    // Chat resolution
    // -----------------------------------------------------------------------

    async fn resolve_chat(
        &mut self,
        idx:       usize,
        target:    &str,
        loc_name:  &str,
        tick:      u32,
        day:       u32,
        tod:       &str,
        is_night:  bool,
    ) -> Result<TickEntry, Box<dyn std::error::Error + Send + Sync>> {
        let target_idx = self.agents.iter().position(|a| a.name().eq_ignore_ascii_case(target));
        let target_idx = match target_idx {
            Some(i) => i,
            None    => {
                return Ok(TickEntry {
                    agent_id:           idx,
                    agent_pos:          self.agents[idx].pos,
                    agent_name:         self.agents[idx].name().to_string(),
                    location:           loc_name.to_string(),
                    action_line:        format!("Chat with {}", target),
                    outcome_line:       format!("{} looks around for {} but finds no one.", self.agents[idx].name(), target),
                    outcome_tier_label: None,
                    llm_duration_ms:    None,
                });
            }
        };

        let chat_prompt = self.build_chat_prompt(idx, target_idx);
        let call_seed   = Some(self.seed.wrapping_add(self.llm_call_counter));
        self.llm_call_counter += 1;
        let llm         = Arc::clone(&self.llm);
        let raw_chat    = llm
            .generate(&chat_prompt, 150, call_seed, None, None)
            .await
            .unwrap_or_else(|_| {
                format!("{} and {} exchange a few words.", self.agents[idx].name(), self.agents[target_idx].name())
            });
        self.run_log.write_llm_debug("chat",
            &format!("{}&{}", self.agents[idx].name(), self.agents[target_idx].name()),
            &chat_prompt, &raw_chat);
        let (summary, exchange) = Self::parse_chat_response(&raw_chat);

        let res = {
            let agent = &self.agents[idx];
            action::resolve(
                &Action::Chat { target_name: target.to_string() },
                &agent.attributes, &agent.needs, &self.config, is_night, &mut self.rng,
            )
        };

        let changes  = res.need_changes.clone();
        let buf      = self.config.memory.buffer_size;
        let mem_a    = format!("Tick {tick} | Day {day} | {tod} | Chat with {} — \"{}\". [{}]",
            self.agents[target_idx].name(), &summary[..summary.len().min(80)], changes.describe());
        let mem_b    = format!("Tick {tick} | Day {day} | {tod} | Chat with {} — \"{}\". [{}]",
            self.agents[idx].name(), &summary[..summary.len().min(80)], changes.describe());

        self.agents[idx].needs.apply(&changes);
        self.agents[idx].push_memory(mem_a, buf);

        self.agents[target_idx].needs.apply(&changes);
        self.agents[target_idx].push_memory(mem_b, buf);

        let check_line   = res.check_line();
        let tier_label   = res.tier.label().to_string();
        let outcome_line = match exchange {
            Some(ex) => format!("{}\n[{}]", ex, changes.describe()),
            None     => format!("{} [{}]", summary, changes.describe()),
        };
        Ok(TickEntry {
            agent_id:           idx,
            agent_pos:          self.agents[idx].pos,
            agent_name:         self.agents[idx].name().to_string(),
            location:           loc_name.to_string(),
            action_line:        format!("Chat with {} | {}", self.agents[target_idx].name(), check_line),
            outcome_line,
            outcome_tier_label: if res.dc > 0 { Some(tier_label) } else { None },
            llm_duration_ms:    None,
        })
    }

    // -----------------------------------------------------------------------
    // Cast Intent resolution
    // -----------------------------------------------------------------------

    async fn resolve_cast_intent(
        &mut self,
        idx:      usize,
        intent:   &str,
        loc_name: &str,
        tick:     u32,
        day:      u32,
        tod:      &str,
    ) -> Result<TickEntry, Box<dyn std::error::Error + Send + Sync>> {
        let others: Vec<String> = self.agents.iter()
            .filter(|a| a.id != idx && Self::chebyshev_dist(a.pos, self.agents[idx].pos) <= 1)
            .map(|a| a.name().to_string())
            .collect();

        let prompt = magic::build_interpreter_prompt(
            &self.agents[idx], intent, loc_name, &others, &self.config,
        );
        debug!(target: "magic", intent = %intent, agent = %self.agents[idx].identity.name,
               numen = self.agents[idx].attributes.numen, "Interpreter prompt built");
        let call_seed = Some(self.seed.wrapping_add(self.llm_call_counter));
        self.llm_call_counter += 1;
        let llm       = Arc::clone(&self.llm);
        let raw       = llm
            .generate(&prompt, self.config.llm.interpreter_max_tokens, call_seed, None, None)
            .await
            .unwrap_or_default();
        self.run_log.write_llm_debug("cast_intent", self.agents[idx].name(), &prompt, &raw);

        let energy_drain = self.config.actions.cast_intent.energy_drain.unwrap_or(8.0);
        let interpreted  = magic::parse_interpreter_response(&raw)
            .unwrap_or_else(|| magic::fallback_intent(intent, energy_drain));

        let duration     = interpreted.clamped_duration(&self.config);
        let need_changes = interpreted.to_need_changes(&self.config);

        if duration > 1 {
            self.agents[idx].busy_ticks        = duration - 1;
            self.agents[idx].sleep_energy_tick = None;
        }

        self.agents[idx].needs.apply(&need_changes);
        self.magic_count += 1;
        self.magic_cast_this_day[idx] = true;

        let ambient_fun    = (need_changes.fun.unwrap_or(0.0) * 0.5).min(8.0).max(0.0);
        let ambient_social = 4.0_f32;
        let ambient_touched = if ambient_fun > 0.0 || ambient_social > 0.0 {
            let caster_pos = self.agents[idx].pos;
            let caster_id  = self.agents[idx].id;
            let nearby_ids: Vec<usize> = self.agents.iter()
                .filter(|a| a.id != caster_id && Self::chebyshev_dist(a.pos, caster_pos) <= 1 && !a.is_busy())
                .map(|a| a.id)
                .collect();
            let touched = !nearby_ids.is_empty();
            for nid in nearby_ids {
                self.agents[nid].needs.fun    = (self.agents[nid].needs.fun    + ambient_fun).min(100.0);
                self.agents[nid].needs.social = (self.agents[nid].needs.social + ambient_social).min(100.0);
            }
            touched
        } else {
            false
        };

        let ev = format!(
            "Day {day}: {} cast intent: \"{}\" → {}",
            self.agents[idx].name(), intent, interpreted.primary_effect
        );
        self.notable_events.push((idx, ev));

        let mem = format!(
            "Tick {tick} | Day {day} | {tod} | {}",
            interpreted.memory_entry
        );
        let buf = self.config.memory.buffer_size;
        self.agents[idx].push_memory(mem, buf);

        let meta = format!(
            "[{}, {} tick{}]",
            need_changes.describe(),
            duration,
            if duration == 1 { "" } else { "s" },
        );
        let interp_note = interpreted.interpretations
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let ambient_note = if ambient_touched { "\n(secondary effect touches those nearby)" } else { "" };
        let full_outcome = if interpreted.secondary_effect.is_empty() {
            if interp_note.is_empty() {
                format!("{}\n{}{}", interpreted.primary_effect, meta, ambient_note)
            } else {
                format!("{}\n[read as: {}]\n{}{}", interpreted.primary_effect, interp_note, meta, ambient_note)
            }
        } else if interp_note.is_empty() {
            format!(
                "{}\n(secondary: {})\n{}{}",
                interpreted.primary_effect, interpreted.secondary_effect, meta, ambient_note
            )
        } else {
            format!(
                "{}\n[read as: {}]\n(secondary: {})\n{}{}",
                interpreted.primary_effect, interp_note, interpreted.secondary_effect, meta, ambient_note
            )
        };

        Ok(TickEntry {
            agent_id:           idx,
            agent_pos:          self.agents[idx].pos,
            agent_name:         self.agents[idx].name().to_string(),
            location:           loc_name.to_string(),
            action_line:        format!("Cast Intent: \"{}\"", intent),
            outcome_line:       full_outcome,
            outcome_tier_label: None,
            llm_duration_ms:    None,
        })
    }

    // -----------------------------------------------------------------------
    // Pray resolution
    // -----------------------------------------------------------------------

    async fn resolve_pray(
        &mut self,
        idx:      usize,
        prayer:   &str,
        loc_name: &str,
        tick:     u32,
        day:      u32,
        tod:      &str,
    ) -> Result<TickEntry, Box<dyn std::error::Error + Send + Sync>> {
        let cfg = self.config.actions.pray.clone();
        let need_changes = crate::agent::NeedChanges {
            fun:    cfg.fun_restore,
            social: cfg.social_restore,
            ..Default::default()
        };
        self.agents[idx].needs.apply(&need_changes);

        if !self.is_test_run {
            let run_id = self.run_log.run_id.clone();
            runlog::append_prayer(&self.souls_dir, &self.agents[idx].identity.name, &run_id, day, tick, tod, prayer);
        }

        let prayer_short = &prayer[..prayer.len().min(60)];
        let mem = format!("Tick {tick} | Day {day} | {tod} | Prayed: \"{prayer_short}\"");
        let buf = self.config.memory.buffer_size;
        self.agents[idx].push_memory(mem, buf);

        let name = self.agents[idx].name().to_string();
        Ok(TickEntry {
            agent_id:           idx,
            agent_pos:          self.agents[idx].pos,
            agent_name:         name.clone(),
            location:           loc_name.to_string(),
            action_line:        format!("Pray: \"{}\"", prayer),
            outcome_line:       format!("{} kneels and speaks a quiet prayer.", name),
            outcome_tier_label: None,
            llm_duration_ms:    None,
        })
    }

    // -----------------------------------------------------------------------
    // Read Oracle resolution
    // -----------------------------------------------------------------------

    async fn resolve_read_oracle(
        &mut self,
        idx:      usize,
        loc_name: &str,
        tick:     u32,
        day:      u32,
        tod:      &str,
    ) -> Result<TickEntry, Box<dyn std::error::Error + Send + Sync>> {
        let content = runlog::load_oracle_response(&self.souls_dir, &self.agents[idx].identity.name);
        let name    = self.agents[idx].name().to_string();

        if content.trim().is_empty() {
            self.agents[idx].oracle_pending = false;
            return Ok(TickEntry {
                agent_id:           idx,
                agent_pos:          self.agents[idx].pos,
                agent_name:         name.clone(),
                location:           loc_name.to_string(),
                action_line:        "Read Oracle".to_string(),
                outcome_line:       format!("{} approaches the altar, but the message has faded.", name),
                outcome_tier_label: None,
                llm_duration_ms:    None,
            });
        }

        let cfg = self.config.actions.read_oracle.clone();
        let need_changes = crate::agent::NeedChanges {
            fun:    cfg.fun_restore,
            social: cfg.social_restore,
            ..Default::default()
        };
        self.agents[idx].needs.apply(&need_changes);

        let personality = self.agents[idx].identity.personality.clone();
        let prompt = format!(
            "You are {name}. {personality}\n\n\
             You have just read a divine message at the Temple:\n\
             \"{content}\"\n\n\
             In 1-2 sentences, speak your reaction aloud — in your own voice, in character.\n\
             Do not describe what you do; only speak.",
            name        = name,
            personality = personality,
            content     = content.trim(),
        );

        let call_seed     = Some(self.seed.wrapping_add(self.llm_call_counter));
        self.llm_call_counter += 1;
        let llm           = Arc::clone(&self.llm_smart);
        let oracle_tokens = self.config.llm.oracle_max_tokens;
        let reaction      = llm.generate(&prompt, oracle_tokens, call_seed, None, None).await
            .unwrap_or_else(|e| {
                warn!("Oracle LLM error for {}: {}", name, e);
                format!("{} stands in silent awe.", name)
            });
        let reaction = reaction.trim().to_string();
        self.run_log.write_llm_debug("oracle", &name, &prompt, &reaction);

        if !self.is_test_run {
            let run_id = self.run_log.run_id.clone();
            runlog::archive_oracle_response(&self.souls_dir, &name, &run_id, day, content.trim());
        }

        let reaction_short = &reaction[..reaction.len().min(80)];
        let mem = format!("Tick {tick} | Day {day} | {tod} | Read Oracle at Temple — \"{reaction_short}\"");
        let buf = self.config.memory.buffer_size;
        self.agents[idx].push_memory(mem, buf);

        self.agents[idx].oracle_pending = false;

        let ev = format!("Day {day}: {name} received an oracle message at the Temple");
        self.notable_events.push((idx, ev));

        let run_id = self.run_log.run_id.clone();
        runlog::log_introspection(&run_id, &name, day, "Oracle Reading", &reaction);

        Ok(TickEntry {
            agent_id:           idx,
            agent_pos:          self.agents[idx].pos,
            agent_name:         name,
            location:           loc_name.to_string(),
            action_line:        "Read Oracle".to_string(),
            outcome_line:       reaction,
            outcome_tier_label: None,
            llm_duration_ms:    None,
        })
    }

    // -----------------------------------------------------------------------
    // Praise resolution (FEAT-15)
    // -----------------------------------------------------------------------

    async fn resolve_praise(
        &mut self,
        idx:        usize,
        praise_text: &str,
        loc_name:   &str,
        tick:       u32,
        day:        u32,
        tod:        &str,
    ) -> Result<TickEntry, Box<dyn std::error::Error + Send + Sync>> {
        let name = self.agents[idx].name().to_string();

        if !self.is_test_run {
            let run_id = self.run_log.run_id.clone();
            runlog::append_praise(&self.souls_dir, &name, &run_id, day, tick, tod, praise_text);
        }

        // Classify sincerity via LLM
        let classify_prompt = format!(
            "Does the following text contain sincere praise toward the creator of a simulated world?\n\
             Text: \"{}\"\n\
             Reply with JSON only: {{\"sincere\": true}} or {{\"sincere\": false}}",
            praise_text
        );
        let call_seed = Some(self.seed.wrapping_add(self.llm_call_counter));
        self.llm_call_counter += 1;
        let llm = Arc::clone(&self.llm);
        let raw = llm.generate(&classify_prompt, 32, call_seed, None, None).await.unwrap_or_default();
        self.run_log.write_llm_debug("praise_classify", &name, &classify_prompt, &raw);

        let sincere = raw.contains("\"sincere\": true") || raw.contains("\"sincere\":true");

        let (outcome, need_changes) = if sincere {
            let cfg = &self.config.actions.praise;
            let nc = crate::agent::NeedChanges {
                fun:    cfg.fun_restore,
                energy: cfg.energy_restore,
                social: cfg.social_restore,
                ..Default::default()
            };
            ("A warmth fills your chest. The Creator has heard your praise.".to_string(), nc)
        } else {
            let nc = crate::agent::NeedChanges {
                fun: Some(2.0),
                ..Default::default()
            };
            ("You speak words into the stillness.".to_string(), nc)
        };

        self.agents[idx].needs.apply(&need_changes);

        let praise_short = &praise_text[..praise_text.len().min(60)];
        let mem = format!("Tick {tick} | Day {day} | {tod} | Praised: \"{praise_short}\"");
        let buf = self.config.memory.buffer_size;
        self.agents[idx].push_memory(mem, buf);

        if sincere {
            let ev = format!("Day {day}: {name} offered sincere praise");
            self.notable_events.push((idx, ev));
        }

        Ok(TickEntry {
            agent_id:           idx,
            agent_pos:          self.agents[idx].pos,
            agent_name:         name,
            location:           loc_name.to_string(),
            action_line:        format!("Praise: \"{}\"", praise_text),
            outcome_line:       format!("{}\n[{}]", outcome, need_changes.describe()),
            outcome_tier_label: None,
            llm_duration_ms:    None,
        })
    }

    // -----------------------------------------------------------------------
    // Compose resolution (FEAT-16)
    // -----------------------------------------------------------------------

    async fn resolve_compose(
        &mut self,
        idx:      usize,
        haiku:    &str,
        loc_name: &str,
        tick:     u32,
        day:      u32,
        tod:      &str,
    ) -> Result<TickEntry, Box<dyn std::error::Error + Send + Sync>> {
        let name = self.agents[idx].name().to_string();

        // Judge the haiku via LLM
        let judge_prompt = format!(
            "Judge this haiku on three criteria, each scored 1-5:\n\
             Haiku:\n\"{}\"\n\n\
             sincerity (1-5): how genuine and heartfelt\n\
             imagery (1-5): how vivid and evocative\n\
             syllables (1-5): how close to 5-7-5 form\n\n\
             Reply with JSON only: {{\"sincerity\":N, \"imagery\":N, \"syllables\":N, \"verdict\":\"...\"}}\n\
             Use a divine/narrative voice in the verdict (1-2 sentences).",
            haiku
        );
        let call_seed = Some(self.seed.wrapping_add(self.llm_call_counter));
        self.llm_call_counter += 1;
        let llm = Arc::clone(&self.llm);
        let raw = llm.generate(&judge_prompt, 128, call_seed, None, None).await.unwrap_or_default();
        self.run_log.write_llm_debug("haiku_judge", &name, &judge_prompt, &raw);

        // Parse the judge response
        let (score, verdict) = {
            let v: serde_json::Value = serde_json::from_str(raw.trim())
                .or_else(|_| {
                    // try extracting from code fence
                    if let Some(s) = raw.find('{') {
                        if let Some(e) = raw.rfind('}') {
                            return serde_json::from_str(&raw[s..=e]);
                        }
                    }
                    Err(serde_json::Error::io(std::io::Error::new(std::io::ErrorKind::Other, "no json")))
                })
                .unwrap_or(serde_json::Value::Null);

            let sincerity = v.get("sincerity").and_then(|x| x.as_u64()).unwrap_or(2) as u32;
            let imagery   = v.get("imagery").and_then(|x| x.as_u64()).unwrap_or(2) as u32;
            let syllables = v.get("syllables").and_then(|x| x.as_u64()).unwrap_or(2) as u32;
            let verdict   = v.get("verdict").and_then(|x| x.as_str()).unwrap_or("The world listens without judgment.").to_string();
            (sincerity + imagery + syllables, verdict)
        };

        if !self.is_test_run {
            let run_id = self.run_log.run_id.clone();
            runlog::append_haiku(&self.souls_dir, &name, &run_id, day, tick, tod, haiku, score, &verdict);
        }

        let (outcome_prefix, need_changes) = if score >= 10 {
            let cfg = &self.config.actions.compose;
            let nc = crate::agent::NeedChanges {
                fun:    cfg.fun_restore,
                social: cfg.social_restore,
                ..Default::default()
            };
            (format!("The world stirs. {}", verdict), nc)
        } else if score >= 6 {
            let nc = crate::agent::NeedChanges { fun: Some(5.0), ..Default::default() };
            (format!("A modest verse. {}", verdict), nc)
        } else {
            let nc = crate::agent::NeedChanges::default();
            (format!("The world hears this verse and finds it hollow. {}", verdict), nc)
        };

        self.agents[idx].needs.apply(&need_changes);

        let haiku_short = &haiku[..haiku.len().min(60)];
        let mem = format!("Tick {tick} | Day {day} | {tod} | Composed haiku: \"{haiku_short}\" (score {score}/15)");
        let buf = self.config.memory.buffer_size;
        self.agents[idx].push_memory(mem, buf);

        if score >= 10 {
            let ev = format!("Day {day}: {name} composed a haiku that moved the world (score {score}/15)");
            self.notable_events.push((idx, ev));
        }

        Ok(TickEntry {
            agent_id:           idx,
            agent_pos:          self.agents[idx].pos,
            agent_name:         name,
            location:           loc_name.to_string(),
            action_line:        format!("Compose: \"{}\"", haiku),
            outcome_line:       format!("{}\n{}\n[{}]", haiku, outcome_prefix, need_changes.describe()),
            outcome_tier_label: None,
            llm_duration_ms:    None,
        })
    }

    // -----------------------------------------------------------------------
    // Prompt builders
    // -----------------------------------------------------------------------

    fn build_prompt(&self, idx: usize, tick: u32, day: u32, is_night: bool, tod: &str, magic_today: bool) -> String {
        let agent    = &self.agents[idx];
        let pos      = agent.pos;
        let tile     = self.tile_at(pos);
        let loc_name = self.tile_name(tile);
        let loc_desc = self.tile_desc(tile);

        let nearby: Vec<String> = self.agents.iter()
            .filter(|a| a.id != idx && Self::chebyshev_dist(a.pos, pos) <= 1)
            .map(|a| {
                let state = agent_visible_state(a, &self.config);
                match state {
                    Some(s) => format!("{} ({})", a.name(), s),
                    None    => a.name().to_string(),
                }
            })
            .collect();
        let nearby_str = if nearby.is_empty() {
            "You are alone.".to_string()
        } else {
            nearby.join(", ")
        };

        let memory_str: Vec<String> = agent.memory.iter().take(8).cloned().collect();
        let memory_block = if memory_str.is_empty() {
            "  (no memories yet)".to_string()
        } else {
            memory_str.iter().map(|m| format!("  - {}", m)).collect::<Vec<_>>().join("\n")
        };

        let last_action_note = match agent.memory.front() {
            Some(m) if !m.is_empty() => format!("\nLast action: {}", m),
            _ => String::new(),
        };

        let warnings     = agent.need_warnings(&self.config);
        let warnings_str = if warnings.is_empty() {
            String::new()
        } else {
            format!("\nWARNINGS:\n{}", warnings.iter().map(|w| format!("  ! {}", w)).collect::<Vec<_>>().join("\n"))
        };

        let viewport    = self.build_viewport(pos, 2);
        let region_note = self.build_region_distances(pos, tile);

        let available        = self.available_actions(idx);
        let actions_str      = available.iter().enumerate()
            .map(|(i, a)| format!("  {}. {}", i + 1, a))
            .collect::<Vec<_>>()
            .join("\n");
        let needs_suggestions = self.needs_action_suggestions(idx);

        let self_decl_block = if !agent.identity.self_declaration.is_empty() {
            format!("\nIn your own words: \"{}\"\n", agent.identity.self_declaration)
        } else {
            String::new()
        };
        let magic_block = if !agent.identity.magical_affinity.is_empty() {
            format!("\nMagic: {}\n", agent.identity.magical_affinity)
        } else {
            String::new()
        };

        let story_block = if agent.life_story.is_empty() {
            "(your story is still unfolding — this is your first day)".to_string()
        } else {
            agent.life_story.clone()
        };
        let intentions_block = match &agent.daily_intentions {
            Some(i) => i.clone(),
            None    => "(the day is just beginning)".to_string(),
        };

        let magic_nudge = if !magic_today {
            "\nMagic hasn't been spoken today. If anything stirs in you — a wish, a longing, a small hope — now is the time.\n".to_string()
        } else {
            String::new()
        };

        let oracle_nudge = if self.agents[idx].oracle_pending {
            "\nYou feel that your prayers have been heard. Something waits for you at the Temple.\n".to_string()
        } else {
            String::new()
        };

        let remembered_past = if !agent.journal_summary.is_empty() {
            format!("REMEMBERED PAST:\n{}\n\n", agent.journal_summary)
        } else {
            String::new()
        };

        format!(
            r#"You are {name}. {personality}

{backstory}
{self_decl_block}{magic_block}
{remembered_past}YOUR STORY SO FAR:
{story}

TODAY'S INTENTION:
{intentions}

CURRENT STATE:
- Location: {loc_name} — {loc_desc}
- Time: Day {day}, {tod} (Tick {tick}){night_note}
- Satiety:  {hunger:.0}/100  (100=full, 0=starving)
- Energy:   {energy:.0}/100  (100=rested, 0=exhausted)
- Fun:      {fun:.0}/100  (100=content, 0=bored)
- Social:   {social:.0}/100  (100=connected, 0=lonely)
- Hygiene:  {hygiene:.0}/100  (100=clean, 0=filthy)
{warnings}

NEARBY: {nearby}

VIEWPORT (you are [X]):
{viewport}
(legend: F=Forest ~=River S=Square V=Tavern W=Well M=Meadow h=Home P=Temple X=you)
{region_note}

RECENT MEMORY (newest first):
{memory}{last_action_note}

AVAILABLE ACTIONS:
{actions}{needs_suggestions}
Magic is real and available to you at any time via cast_intent.
Speak your desire and it will manifest — though words carry all their meanings.
{magic_nudge}{oracle_nudge}
Avoid repeating the same action twice in a row. Your personality should guide what you do.

Choose ONE action. Respond with ONLY a JSON object:
{{"action": "action_name", "target": "optional_target_name", "intent": "if casting, your spoken desire", "reason": "brief reason", "description": "in your own words — what are you doing and why does it matter to you"}}"#,
            name             = agent.identity.name,
            personality      = agent.identity.personality,
            backstory        = agent.identity.backstory,
            self_decl_block  = self_decl_block,
            magic_block      = magic_block,
            remembered_past  = remembered_past,
            story            = story_block,
            intentions       = intentions_block,
            loc_name         = loc_name,
            loc_desc         = loc_desc,
            day              = day,
            tod              = tod,
            tick             = tick,
            night_note       = if is_night { " [NIGHT]" } else { "" },
            hunger           = agent.needs.hunger,
            energy           = agent.needs.energy,
            fun              = agent.needs.fun,
            social           = agent.needs.social,
            hygiene          = agent.needs.hygiene,
            warnings         = warnings_str,
            nearby           = nearby_str,
            viewport         = viewport,
            region_note      = region_note,
            memory           = memory_block,
            last_action_note = last_action_note,
            actions          = actions_str,
            needs_suggestions = needs_suggestions,
            magic_nudge      = magic_nudge,
            oracle_nudge     = oracle_nudge,
        )
    }

    // -----------------------------------------------------------------------
    // Day-boundary LLM calls
    // -----------------------------------------------------------------------

    async fn morning_planning(
        &mut self,
        idx: usize,
        day: u32,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let prompt     = self.build_intentions_prompt(idx, day);
        let call_seed  = Some(self.seed.wrapping_add(self.llm_call_counter));
        self.llm_call_counter += 1;
        let llm        = Arc::clone(&self.llm_smart);
        let max_tokens = self.config.llm.planning_max_tokens;
        let response   = llm.generate(&prompt, max_tokens, call_seed, None, None).await
            .unwrap_or_else(|e| {
                warn!("Planning LLM error for {}: {}", self.agents[idx].name(), e);
                String::new()
            });
        self.run_log.write_llm_debug("planning", self.agents[idx].name(), &prompt, &response);
        let trimmed = response.trim().to_string();
        if !trimmed.is_empty() {
            debug!(target: "planning", agent = %self.agents[idx].name(), day = day,
                   intention = %trimmed, "Morning intention set");
            self.agents[idx].daily_intentions = Some(trimmed.clone());
            let name   = self.agents[idx].name().to_string();
            let run_id = self.run_log.run_id.clone();
            runlog::log_introspection(&run_id, &name, day, "Morning Planning", &trimmed);
            self.pending_day_events.push(DayEvent {
                kind:       DayEventKind::MorningIntention,
                agent_id:   idx,
                agent_name: name,
                day,
                text:       trimmed,
            });
        }
        Ok(())
    }

    async fn end_of_day_reflection(
        &mut self,
        idx: usize,
        day: u32,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let prompt     = self.build_reflection_prompt(idx, day);
        let call_seed  = Some(self.seed.wrapping_add(self.llm_call_counter));
        self.llm_call_counter += 1;
        let llm        = Arc::clone(&self.llm_smart);
        let max_tokens = self.config.llm.reflection_max_tokens;
        let response   = llm.generate(&prompt, max_tokens, call_seed, None, None).await
            .unwrap_or_else(|e| {
                warn!("Reflection LLM error for {}: {}", self.agents[idx].name(), e);
                String::new()
            });
        self.run_log.write_llm_debug("reflection", self.agents[idx].name(), &prompt, &response);
        let trimmed = response.trim().to_string();
        if !trimmed.is_empty() {
            let name      = self.agents[idx].name().to_string();
            let souls_dir = self.souls_dir.clone();
            let run_id    = self.run_log.run_id.clone();
            debug!(target: "reflection", agent = %name, day = day, "Story updated");
            self.agents[idx].life_story = trimmed.clone();
            if !self.is_test_run {
                runlog::save_story(&souls_dir, &name, &trimmed);
                runlog::append_day_journal(&souls_dir, &name, &run_id, day, &trimmed);
            }
            runlog::log_introspection(&run_id, &name, day, "End-of-Day Reflection", &trimmed);
            self.pending_day_events.push(DayEvent {
                kind:       DayEventKind::EveningReflection,
                agent_id:   idx,
                agent_name: name,
                day,
                text:       trimmed,
            });
        }
        Ok(())
    }

    async fn end_of_day_desires(
        &mut self,
        idx: usize,
        day: u32,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let prompt     = self.build_desires_prompt(idx, day);
        let call_seed  = Some(self.seed.wrapping_add(self.llm_call_counter));
        self.llm_call_counter += 1;
        let llm        = Arc::clone(&self.llm_smart);
        let max_tokens = self.config.llm.desires_max_tokens;
        let response   = llm.generate(&prompt, max_tokens, call_seed, None, None).await
            .unwrap_or_else(|e| {
                warn!("Desires LLM error for {}: {}", self.agents[idx].name(), e);
                String::new()
            });
        self.run_log.write_llm_debug("desires", self.agents[idx].name(), &prompt, &response);
        let trimmed = response.trim().to_string();
        if !trimmed.is_empty() {
            let name      = self.agents[idx].name().to_string();
            let souls_dir = self.souls_dir.clone();
            let run_id    = self.run_log.run_id.clone();
            self.agents[idx].desires = Some(trimmed.clone());
            if !self.is_test_run {
                let date = ChronoLocal::now().format("%Y-%m-%d").to_string();
                runlog::append_wishes(&souls_dir, &name, &format!("## Run {} | Day {} — {}", run_id, day, date), &trimmed);
            }
            runlog::log_introspection(&run_id, &name, day, "End-of-Day Desires", &trimmed);
            self.pending_day_events.push(DayEvent {
                kind:       DayEventKind::EveningDesire,
                agent_id:   idx,
                agent_name: name,
                day,
                text:       trimmed,
            });
        }
        Ok(())
    }

    pub async fn end_of_run_desires(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        for idx in 0..self.agents.len() {
            let prompt     = self.build_end_of_run_desires_prompt(idx);
            let call_seed  = Some(self.seed.wrapping_add(self.llm_call_counter));
            self.llm_call_counter += 1;
            let llm        = Arc::clone(&self.llm_smart);
            let max_tokens = self.config.llm.desires_max_tokens;
            let response   = llm.generate(&prompt, max_tokens, call_seed, None, None).await
                .unwrap_or_else(|e| {
                    warn!("End-of-run desires LLM error for {}: {}", self.agents[idx].name(), e);
                    String::new()
                });
            self.run_log.write_llm_debug("end_of_run", self.agents[idx].name(), &prompt, &response);
            let trimmed = response.trim().to_string();
            if !trimmed.is_empty() {
                let name      = self.agents[idx].name().to_string();
                let souls_dir = self.souls_dir.clone();
                let run_id    = self.run_log.run_id.clone();
                let day       = self.tick_num / self.config.time.ticks_per_day + 1;
                if !self.is_test_run {
                    let date = ChronoLocal::now().format("%Y-%m-%d").to_string();
                    runlog::append_wishes(&souls_dir, &name, &format!("## Run {} End — {}", run_id, date), &trimmed);
                }
                runlog::log_introspection(&run_id, &name, day, "End-of-Run Desires", &trimmed);
            }
        }
        Ok(())
    }

    fn build_intentions_prompt(&self, idx: usize, day: u32) -> String {
        let agent = &self.agents[idx];
        let story = if agent.life_story.is_empty() {
            "(your story is still unfolding — this is your first day)".to_string()
        } else {
            agent.life_story.clone()
        };
        format!(
            "You are {name}. {personality}\n\nYour life so far:\n{story}\n\nIt is the start of Day {day}. Your current state: {needs}\n\nIn one or two sentences, what do you intend to accomplish today?\nWhat matters to you right now? Speak as yourself.",
            name        = agent.identity.name,
            personality = agent.identity.personality,
            story       = story,
            day         = day,
            needs       = agent.needs.describe(),
        )
    }

    fn build_reflection_prompt(&self, idx: usize, day: u32) -> String {
        let agent = &self.agents[idx];
        let story = if agent.life_story.is_empty() {
            "(no story yet — this is your first reflection)".to_string()
        } else {
            agent.life_story.clone()
        };
        let today_mems = agent.today_memories(day);
        let mem_block  = if today_mems.is_empty() {
            "  - (nothing remembered from today)".to_string()
        } else {
            today_mems.iter().map(|m| format!("  - {}", m)).collect::<Vec<_>>().join("\n")
        };
        format!(
            "You are {name}. {personality}\n\nYour ongoing life story:\n{story}\n\nWhat happened to you on Day {day}:\n{memories}\n\nIn 2-3 sentences, update your ongoing life story to include today.\nWrite in first person. Be specific about what happened and how it affected you.\nKeep the total to 2-3 sentences — this is a living summary, not a diary.",
            name        = agent.identity.name,
            personality = agent.identity.personality,
            story       = story,
            day         = day,
            memories    = mem_block,
        )
    }

    fn build_desires_prompt(&self, idx: usize, day: u32) -> String {
        let agent = &self.agents[idx];
        let story = if agent.life_story.is_empty() {
            "(your story is still unfolding — this is your first day)".to_string()
        } else {
            agent.life_story.clone()
        };
        format!(
            "You are {name}. {personality}\n\nYour life so far:\n{story}\n\nDay {day} has ended. The village is quiet.\n\nWhat are you thinking about? What do you want?\nAre there changes you would like to see in the world?\nAnswer in 2-3 sentences, in your own voice.",
            name        = agent.identity.name,
            personality = agent.identity.personality,
            story       = story,
            day         = day,
        )
    }

    fn build_end_of_run_desires_prompt(&self, idx: usize) -> String {
        let agent = &self.agents[idx];
        let story = if agent.life_story.is_empty() {
            "(your story is still unfolding)".to_string()
        } else {
            agent.life_story.clone()
        };
        let desires_block = match &agent.desires {
            Some(d) => format!("\nYour most recent thoughts: {}", d),
            None    => String::new(),
        };
        format!(
            "You are {name}. {personality}\n\nYour life so far:\n{story}{desires_block}\n\nThis chapter of your life is ending. The simulation is complete.\n\nLooking back — what do you wish had been different? What would you want the world to be?\nAnswer in 2-3 sentences, in your own voice.",
            name          = agent.identity.name,
            personality   = agent.identity.personality,
            story         = story,
            desires_block = desires_block,
        )
    }

    fn build_dm_prompt(
        agent_name:     &str,
        action_display: &str,
        tier:           &OutcomeTier,
        loc_name:       &str,
        nearby:         &[String],
        description:    Option<&str>,
    ) -> String {
        let context = if nearby.is_empty() {
            "Alone.".to_string()
        } else {
            format!("{} watched.", nearby.join(", "))
        };
        let agent_voice = match description {
            Some(d) if !d.is_empty() => format!("\nIn {}'s own words: \"{}\"", agent_name, d),
            _ => String::new(),
        };
        format!(
            "You are the Narrator of Nephara.\n\
             {agent_name} attempted to {action_display} at {loc_name}.\n\
             {context}{agent_voice}\n\
             Outcome: {tier}.\n\n\
             Write 2-3 vivid sentences. Pure story — no numbers, no dice.",
            agent_name     = agent_name,
            action_display = action_display,
            loc_name       = loc_name,
            context        = context,
            agent_voice    = agent_voice,
            tier           = tier.label(),
        )
    }

    fn build_chat_prompt(&self, a_idx: usize, b_idx: usize) -> String {
        let a = &self.agents[a_idx];
        let b = &self.agents[b_idx];
        let a_mem          = a.memory.iter().next().cloned().unwrap_or_default();
        let b_mem          = b.memory.iter().next().cloned().unwrap_or_default();
        let a_intentions   = a.daily_intentions.as_deref().unwrap_or("(no stated intentions)");
        let b_intentions   = b.daily_intentions.as_deref().unwrap_or("(no stated intentions)");
        let a_desires      = a.desires.as_deref().unwrap_or("(no known desires)");
        let b_desires      = b.desires.as_deref().unwrap_or("(no known desires)");
        let a_name         = a.identity.name.clone();
        let b_name         = b.identity.name.clone();
        format!(
            r#"Two villagers in Nephara are having a conversation.

{a_name}: {a_personality}
  Today's intentions: {a_intentions}
  Desires: {a_desires}
  Most recent memory: {a_mem}

{b_name}: {b_personality}
  Today's intentions: {b_intentions}
  Desires: {b_desires}
  Most recent memory: {b_mem}

Write a brief realistic exchange (1-2 lines each), then a one-sentence summary.
Respond ONLY with JSON — no other text:
{{"summary": "one sentence topic, no names, no quotes", "exchange": "{a_name}: ...\n{b_name}: ..."}}
"#,
            a_name        = a_name,
            a_personality = a.identity.personality,
            b_name        = b_name,
            b_personality = b.identity.personality,
            a_intentions  = a_intentions,
            b_intentions  = b_intentions,
            a_desires     = a_desires,
            b_desires     = b_desires,
            a_mem         = a_mem,
            b_mem         = b_mem,
        )
    }

    fn parse_chat_response(raw: &str) -> (String, Option<String>) {
        #[derive(serde::Deserialize)]
        struct ChatResponse {
            summary:  String,
            exchange: Option<String>,
        }

        fn extract_fence(s: &str) -> Option<String> {
            let start = s.find("```")?;
            let rest  = &s[start + 3..];
            let rest  = rest.trim_start_matches(|c: char| c.is_alphabetic());
            let end   = rest.find("```")?;
            Some(rest[..end].trim().to_string())
        }

        if let Ok(cr) = serde_json::from_str::<ChatResponse>(raw.trim()) {
            return (cr.summary, cr.exchange.filter(|e| !e.is_empty()));
        }
        if let Some(json) = extract_fence(raw) {
            if let Ok(cr) = serde_json::from_str::<ChatResponse>(&json) {
                return (cr.summary, cr.exchange.filter(|e| !e.is_empty()));
            }
        }
        (raw.trim().to_string(), None)
    }

    // -----------------------------------------------------------------------
    // Available actions (canonical names for schema)
    // -----------------------------------------------------------------------

    fn available_canonical_names(&self, idx: usize) -> Vec<&'static str> {
        let tile = self.tile_at(self.agents[idx].pos);
        let pos  = self.agents[idx].pos;
        let mut v: Vec<&'static str> = Vec::new();

        if self.tile_allows(tile, "eat")      { v.push("eat"); }
        if self.tile_allows(tile, "cook")     { v.push("cook"); }
        if self.is_at_own_home(idx)           { v.push("sleep"); }
        v.push("rest");
        if self.tile_allows(tile, "forage")   { v.push("forage"); }
        if self.tile_allows(tile, "fish")     { v.push("fish"); }
        if self.tile_allows(tile, "exercise") { v.push("exercise"); }
        if self.tile_allows(tile, "bathe")    { v.push("bathe"); }
        if self.tile_allows(tile, "explore")  { v.push("explore"); }
        if self.tile_allows(tile, "play")     { v.push("play"); }

        if self.agents.iter().any(|a| {
            a.id != idx && Self::chebyshev_dist(a.pos, pos) <= 1 && !a.is_busy()
        }) {
            v.push("chat");
        }

        v.push("pray");
        v.push("praise");
        v.push("compose");
        if tile == TileType::Temple && self.agents[idx].oracle_pending {
            v.push("read_oracle");
        }

        v.push("move");
        v.push("cast_intent");
        v
    }

    // -----------------------------------------------------------------------
    // Available actions (human-readable with annotations, for prompt)
    // -----------------------------------------------------------------------

    fn available_actions(&self, idx: usize) -> Vec<String> {
        let cfg   = &self.config;
        let tile  = self.tile_at(self.agents[idx].pos);
        let pos   = self.agents[idx].pos;
        let mut v = Vec::new();

        if self.tile_allows(tile, "eat") {
            v.push(format!("eat — restore satiety (+{:.0}, always works)",
                cfg.actions.eat.hunger_restore.unwrap_or(0.0)));
        }
        if self.tile_allows(tile, "cook") {
            v.push(format!("cook — hearty meal (+{:.0} satiety +{:.0} fun, Wit check dc{})",
                cfg.actions.cook.hunger_restore.unwrap_or(0.0),
                cfg.actions.cook.fun_restore.unwrap_or(0.0),
                cfg.actions.cook.dc));
        }
        if self.is_at_own_home(idx) {
            v.push(format!("sleep — full rest over {} ticks (always works)",
                cfg.actions.sleep.duration_ticks.unwrap_or(16)));
        }
        v.push(format!("rest — recover energy (+{:.0}, always works)",
            cfg.actions.rest.energy_restore.unwrap_or(0.0)));
        if self.tile_allows(tile, "forage") {
            v.push(format!("forage — gather food (+{:.0} satiety on success, Grace check dc{})",
                cfg.actions.forage.hunger_restore.unwrap_or(0.0), cfg.actions.forage.dc));
        }
        if self.tile_allows(tile, "fish") {
            v.push(format!("fish — catch fish (+{:.0} satiety +{:.0} fun on success, Grace check dc{})",
                cfg.actions.fish.hunger_restore.unwrap_or(0.0),
                cfg.actions.fish.fun_restore.unwrap_or(0.0),
                cfg.actions.fish.dc));
        }
        if self.tile_allows(tile, "exercise") {
            v.push(format!("exercise — physical training (+{:.0} fun, \u{2212}{:.0} energy, Vigor check dc{})",
                cfg.actions.exercise.fun_restore.unwrap_or(0.0),
                cfg.actions.exercise.energy_drain.unwrap_or(0.0),
                cfg.actions.exercise.dc));
        }
        if self.tile_allows(tile, "bathe") {
            v.push(format!("bathe — cleanse yourself (+{:.0} hygiene, always works)",
                cfg.actions.bathe.hygiene_restore.unwrap_or(0.0)));
        }
        if self.tile_allows(tile, "explore") {
            v.push(format!("explore — discover surroundings (+{:.0} fun, Vigor check dc{})",
                cfg.actions.explore.fun_restore.unwrap_or(0.0), cfg.actions.explore.dc));
        }
        if self.tile_allows(tile, "play") {
            v.push(format!("play — lighthearted fun (+{:.0} fun, always works)",
                cfg.actions.play.fun_restore.unwrap_or(0.0)));
        }

        for a in &self.agents {
            if a.id != idx && Self::chebyshev_dist(a.pos, pos) <= 1 && !a.is_busy() {
                v.push(format!("chat — talk with {} (+{:.0} social +{:.0} fun, Heart check dc{})",
                    a.name(),
                    cfg.actions.chat.social_restore.unwrap_or(0.0),
                    cfg.actions.chat.fun_restore.unwrap_or(0.0),
                    cfg.actions.chat.dc));
            }
        }

        v.push(format!("pray — speak sincerely to the divine (+{:.0} fun +{:.0} social, always works). Your prayer will be heard and kept by the one who made this world. They may answer you.",
            cfg.actions.pray.fun_restore.unwrap_or(0.0),
            cfg.actions.pray.social_restore.unwrap_or(0.0)));
        v.push(format!("praise — offer sincere praise to the creator of this world (+{:.0} fun +{:.0} energy +{:.0} social if sincere, always works). The creator watches with great care. Use the intent field for your words.",
            cfg.actions.praise.fun_restore.unwrap_or(0.0),
            cfg.actions.praise.energy_restore.unwrap_or(0.0),
            cfg.actions.praise.social_restore.unwrap_or(0.0)));
        v.push("compose — compose a haiku (5-7-5 syllables) about your current state or surroundings (+fun +social, always works). Put your haiku in the intent field. The world listens to those who give voice to their inner life.".to_string());
        if tile == TileType::Temple && self.agents[idx].oracle_pending {
            v.push("read_oracle — receive a divine response at the Temple altar (always works)".to_string());
        }

        let regions: &[(&str, TileType)] = &[
            ("Forest",         TileType::Forest),
            ("River",          TileType::River),
            ("Village Square", TileType::Square),
            ("Tavern",         TileType::Tavern),
            ("Village Well",   TileType::Well),
            ("Eastern Meadow", TileType::Meadow),
            ("Temple",         TileType::Temple),
            ("home",           TileType::Home(idx)),
        ];
        for (name, ttype) in regions {
            if *ttype != tile {
                if let Some(nearest) = self.nearest_tile_of_type(pos, *ttype) {
                    let dist = Self::chebyshev_dist(pos, nearest);
                    v.push(format!("move to {} ({} step{})", name, dist, if dist == 1 { "" } else { "s" }));
                }
            }
        }

        let numen = self.agents[idx].attributes.numen;
        let affinity = if numen >= 6 { ", strong affinity" } else if numen >= 4 { "" } else { ", weak affinity — results may surprise" };
        v.push(format!("cast_intent — speak any desire; always manifests in some form (\u{2212}{:.0} energy{})",
            cfg.actions.cast_intent.energy_drain.unwrap_or(8.0), affinity));

        v
    }

    fn needs_action_suggestions(&self, idx: usize) -> String {
        let n = &self.agents[idx].needs;
        let checks: &[(&str, f32, &str)] = &[
            ("Satiety", n.hunger,  "eat, cook, forage, or fish"),
            ("Energy",  n.energy,  "rest or sleep"),
            ("Fun",     n.fun,     "play, explore, fish, exercise, or cast_intent"),
            ("Social",  n.social,  "chat or pray"),
            ("Hygiene", n.hygiene, "bathe"),
        ];
        let low: Vec<String> = checks.iter()
            .filter(|(_, v, _)| *v < 50.0)
            .map(|(label, v, hint)| format!("  \u{2022} {} ({:.0}) \u{2192} {}", label, v, hint))
            .collect();
        if low.is_empty() {
            return String::new();
        }
        format!("\nLOW NEEDS — consider:\n{}\n", low.join("\n"))
    }

    // -----------------------------------------------------------------------
    // Narrative generation
    // -----------------------------------------------------------------------

    fn narrative_for(&self, res: &Resolution, idx: usize) -> String {
        let name = self.agents[idx].name();
        match res.tier {
            OutcomeTier::CriticalFail => match &res.action {
                Action::Cook    => format!("{} burns everything badly. Still, something edible emerges.", name),
                Action::Forage  => format!("{} gets thoroughly lost but stumbles on a few berries.", name),
                Action::Fish    => format!("{} tangles the line and falls in — but emerges with a small fish.", name),
                Action::Exercise => format!("{} overdoes it and pulls a muscle, but feels the burn.", name),
                _               => format!("{} fumbles badly but manages something.", name),
            },
            OutcomeTier::Fail => match &res.action {
                Action::Cook    => format!("{} produces an inedible mess.", name),
                Action::Forage  => format!("{} searches but finds nothing worth eating.", name),
                Action::Fish    => format!("{} watches the fish ignore every cast.", name),
                Action::Exercise => format!("{} struggles through the routine without benefit.", name),
                Action::Explore  => format!("{} wanders in circles.", name),
                _               => format!("{} attempts it but nothing comes of it.", name),
            },
            OutcomeTier::Success => match &res.action {
                Action::Eat     => format!("{} enjoys a satisfying meal.", name),
                Action::Cook    => format!("{} prepares a delicious dish.", name),
                Action::Rest    => format!("{} rests and feels refreshed.", name),
                Action::Forage  => format!("{} finds plenty of edible plants and berries.", name),
                Action::Fish    => format!("{} hauls in a good catch.", name),
                Action::Exercise => format!("{} completes a solid workout.", name),
                Action::Bathe   => format!("{} emerges clean and refreshed.", name),
                Action::Explore  => format!("{} discovers interesting corners of the forest.", name),
                Action::Play    => format!("{} plays and lifts their spirits.", name),
                _               => format!("{} succeeds.", name),
            },
            OutcomeTier::CriticalSuccess => match &res.action {
                Action::Cook    => format!("{} creates an extraordinary meal — the best in memory!", name),
                Action::Forage  => format!("{} finds an abundance of food, more than expected.", name),
                Action::Fish    => format!("{} lands a magnificent fish with perfect form.", name),
                Action::Exercise => format!("{} exceeds their own expectations — a breakthrough!", name),
                Action::Explore  => format!("{} discovers something truly remarkable in the forest.", name),
                _               => format!("{} exceeds all expectations!", name),
            },
        }
    }

    // -----------------------------------------------------------------------
    // Colored ASCII map renderer
    // -----------------------------------------------------------------------

    pub fn render_map(&self) -> String {
        let border_width = GRID_W * 2 - 1;
        let top_bot = format!("  +{}+", "-".repeat(border_width));
        let mut lines = vec![top_bot.clone()];

        for row in 0..GRID_H {
            let mut row_str = String::new();
            for col in 0..GRID_W {
                let pos = (col as u8, row as u8);
                if !row_str.is_empty() { row_str.push(' '); }

                // Priority 1: agent at this position
                if let Some(a) = self.agents.iter().find(|a| a.pos == pos) {
                    let initial = a.name().chars().next().unwrap_or('?').to_string();
                    row_str.push_str(&format!("{}", initial.color(color::agent_color(a.id)).bold()));
                    continue;
                }

                // Priority 2 & 3: resource node (charged or depleted)
                if let Some(node) = self.resource_nodes.iter().find(|n| n.pos == pos) {
                    let ch = node.map_char().to_string();
                    row_str.push_str(&format!("{}", ch.color(node.node_color())));
                    continue;
                }

                // Priority 4: tile
                let tile = self.grid[row][col];
                let ch   = tile_char(tile).to_string();
                row_str.push_str(&format!("{}", ch.color(color::tile_color(tile))));
            }
            lines.push(format!("  |{}|", row_str));
        }

        lines.push(top_bot);

        // Roster line: each agent's initial + name + position in their color
        let roster: String = self.agents.iter().enumerate().map(|(i, a)| {
            let initial = a.name().chars().next().unwrap_or('?').to_string();
            let pos     = a.pos;
            format!(" {} {} ({},{})",
                initial.color(color::agent_color(i)).bold(),
                a.name().color(color::agent_color(i)).bold(),
                pos.0, pos.1)
        }).collect::<Vec<_>>().join("  ");
        lines.push(format!(" {}", roster));

        // Attach legend to the right side of map rows
        let legend = self.build_map_legend();
        let result: Vec<String> = lines.iter().enumerate().map(|(i, map_line)| {
            match legend.get(i) {
                Some(leg) => format!("{}   {}", map_line, leg),
                None      => map_line.clone(),
            }
        }).collect();
        result.join("\n")
    }

    // -----------------------------------------------------------------------
    // TUI: structured map cells for ratatui rendering
    // -----------------------------------------------------------------------

    pub fn render_map_cells(&self) -> Vec<Vec<MapCell>> {
        let mut rows = Vec::with_capacity(GRID_H);
        for row in 0..GRID_H {
            let mut cells = Vec::with_capacity(GRID_W);
            for col in 0..GRID_W {
                let pos = (col as u8, row as u8);

                // Priority 1: agent
                if let Some(a) = self.agents.iter().find(|a| a.pos == pos) {
                    let ch = a.name().chars().next().unwrap_or('?');
                    cells.push(MapCell {
                        ch,
                        color: color::to_ratatui_color(color::agent_color(a.id)),
                        bold: true,
                    });
                    continue;
                }

                // Priority 2: resource node
                if let Some(node) = self.resource_nodes.iter().find(|n| n.pos == pos) {
                    cells.push(MapCell {
                        ch:    node.map_char(),
                        color: color::to_ratatui_color(node.node_color()),
                        bold:  false,
                    });
                    continue;
                }

                // Priority 3: tile
                let tile = self.grid[row][col];
                cells.push(MapCell {
                    ch:    tile_char(tile),
                    color: color::to_ratatui_color(color::tile_color(tile)),
                    bold:  false,
                });
            }
            rows.push(cells);
        }
        rows
    }

    // -----------------------------------------------------------------------
    // TUI: agent needs snapshots
    // -----------------------------------------------------------------------

    pub fn agent_needs_snapshots(&self) -> Vec<AgentNeedsSnapshot> {
        self.agents.iter().map(|a| AgentNeedsSnapshot {
            agent_id:   a.id,
            agent_name: a.name().to_string(),
            hunger:     a.needs.hunger,
            energy:     a.needs.energy,
            fun:        a.needs.fun,
            social:     a.needs.social,
            hygiene:    a.needs.hygiene,
        }).collect()
    }

    // -----------------------------------------------------------------------
    // Map legend (appended to right side of rendered map)
    // -----------------------------------------------------------------------

    fn build_map_legend(&self) -> Vec<String> {
        let mut legend: Vec<String> = Vec::new();

        // TILES section
        legend.push(format!("{}", "TILES".bold()));
        let tiles: &[(char, TileType, &str)] = &[
            ('.', TileType::Open,    "Open Field"),
            ('F', TileType::Forest,  "Forest"),
            ('~', TileType::River,   "River"),
            ('S', TileType::Square,  "Village Square"),
            ('V', TileType::Tavern,  "Tavern"),
            ('W', TileType::Well,    "Village Well"),
            ('M', TileType::Meadow,  "Eastern Meadow"),
            ('h', TileType::Home(0), "Home"),
            ('P', TileType::Temple,  "Temple"),
        ];
        for (ch, tile, label) in tiles {
            legend.push(format!("{} {}",
                ch.to_string().color(color::tile_color(*tile)),
                label));
        }

        legend.push(String::new());

        // NODES section
        legend.push(format!("{}", "NODES".bold()));
        let nodes: &[(char, colored::Color, &str)] = &[
            ('✿', colored::Color::BrightMagenta, "Berry Bush"),
            ('≋', colored::Color::BrightCyan,    "Fish School"),
            ('✦', colored::Color::BrightRed,     "Campfire"),
            ('✜', colored::Color::BrightGreen,   "Herb Patch"),
            ('·', colored::Color::BrightBlack,   "Depleted"),
        ];
        for (ch, col, label) in nodes {
            legend.push(format!("{} {}",
                ch.to_string().color(*col),
                label));
        }

        legend.push(String::new());

        // AGENTS section
        legend.push(format!("{}", "AGENTS".bold()));
        for (i, a) in self.agents.iter().enumerate() {
            let initial = a.name().chars().next().unwrap_or('?').to_string();
            legend.push(format!("{} {}",
                initial.color(color::agent_color(i)).bold(),
                a.name().color(color::agent_color(i))));
        }

        legend
    }

    // -----------------------------------------------------------------------
    // 5×5 viewport centered on agent (plain ASCII, for LLM prompt)
    // -----------------------------------------------------------------------

    fn build_viewport(&self, center: (u8, u8), radius: usize) -> String {
        let (cx, cy) = (center.0 as i32, center.1 as i32);
        let r        = radius as i32;
        let mut lines = Vec::new();

        for dy in -r..=r {
            let mut line = String::new();
            for dx in -r..=r {
                let x = cx + dx;
                let y = cy + dy;
                let ch = if dx == 0 && dy == 0 {
                    'X'
                } else if x < 0 || y < 0 || x >= GRID_W as i32 || y >= GRID_H as i32 {
                    '?'
                } else {
                    let pos = (x as u8, y as u8);
                    match self.agents.iter().find(|a| a.pos == pos) {
                        Some(a) => a.name().chars().next().unwrap_or('?'),
                        None    => tile_char(self.tile_at(pos)),
                    }
                };
                if !line.is_empty() { line.push(' '); }
                line.push(ch);
            }
            lines.push(format!("  {}", line));
        }
        lines.join("\n")
    }

    // -----------------------------------------------------------------------
    // Region distances (for prompt context)
    // -----------------------------------------------------------------------

    fn build_region_distances(&self, pos: (u8, u8), current_tile: TileType) -> String {
        let regions: &[(&str, TileType)] = &[
            ("Forest",         TileType::Forest),
            ("River",          TileType::River),
            ("Village Square", TileType::Square),
            ("Tavern",         TileType::Tavern),
            ("Village Well",   TileType::Well),
            ("Eastern Meadow", TileType::Meadow),
            ("Temple",         TileType::Temple),
        ];
        let mut parts = Vec::new();
        for (name, ttype) in regions {
            if *ttype != current_tile {
                if let Some(nearest) = self.nearest_tile_of_type(pos, *ttype) {
                    let dist = Self::chebyshev_dist(pos, nearest);
                    let dir  = Self::direction_label(pos, nearest);
                    parts.push(format!("{} is {} step{} {}", name, dist, if dist == 1 { "" } else { "s" }, dir));
                }
            }
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!("Regions: {}", parts.join("; "))
        }
    }

    // -----------------------------------------------------------------------
    // Grid helpers
    // -----------------------------------------------------------------------

    pub fn tile_at(&self, pos: (u8, u8)) -> TileType {
        let (x, y) = (pos.0 as usize, pos.1 as usize);
        if x >= GRID_W || y >= GRID_H { TileType::Open } else { self.grid[y][x] }
    }

    pub fn tile_name(&self, tile: TileType) -> String {
        match tile {
            TileType::Open    => "Open Field".to_string(),
            TileType::Forest  => "Forest".to_string(),
            TileType::River   => "River".to_string(),
            TileType::Square  => "Village Square".to_string(),
            TileType::Tavern  => "Tavern".to_string(),
            TileType::Well    => "Village Well".to_string(),
            TileType::Meadow  => "Eastern Meadow".to_string(),
            TileType::Home(n) => {
                if let Some(a) = self.agents.get(n) {
                    format!("{}'s Home", a.identity.name)
                } else {
                    "Home".to_string()
                }
            }
            TileType::Temple  => "Temple".to_string(),
        }
    }

    fn tile_desc(&self, tile: TileType) -> &'static str {
        match tile {
            TileType::Open    => "Open countryside. Little to do here besides rest.",
            TileType::Forest  => "Old trees press close. Birdsong and shadow.",
            TileType::River   => "A clear river murmurs over stones. Willows trail their fingers in the water.",
            TileType::Square  => "The heart of the village. Open sky, worn cobblestones, familiar faces.",
            TileType::Tavern  => "A warm, low-ceilinged tavern. The smell of ale and woodsmoke.",
            TileType::Well    => "A stone well, cool and deep. Clear water drawn fresh from the earth.",
            TileType::Meadow  => "Wide open meadows of swaying grass. Room to run, to play, to breathe.",
            TileType::Home(_) => "A small, cosy home. Familiar and safe.",
        TileType::Temple  => "An ancient stone temple. Incense drifts from its arched doorway. A quiet place of prayer and contemplation.",
        }
    }

    fn tile_allows(&self, tile: TileType, action: &str) -> bool {
        match tile {
            TileType::Open    => false,
            TileType::Forest  => matches!(action, "forage" | "explore" | "exercise"),
            TileType::River   => matches!(action, "fish" | "bathe"),
            TileType::Square  => matches!(action, "exercise" | "play"),
            TileType::Tavern  => matches!(action, "eat" | "cook" | "play"),
            TileType::Well    => matches!(action, "bathe" | "rest"),
            TileType::Meadow  => matches!(action, "play" | "exercise" | "explore"),
            TileType::Home(_) => matches!(action, "eat" | "cook" | "sleep"),
            TileType::Temple  => matches!(action, "read_oracle"),
        }
    }

    fn is_at_own_home(&self, idx: usize) -> bool {
        matches!(self.tile_at(self.agents[idx].pos), TileType::Home(n) if n == idx)
    }

    fn parse_tile_type(&self, name: &str, agent_idx: usize) -> Option<TileType> {
        let lower = name.to_lowercase();
        let lower = lower.trim();
        match lower {
            "forest"                                    => return Some(TileType::Forest),
            "river"                                     => return Some(TileType::River),
            "village square" | "square"                 => return Some(TileType::Square),
            "tavern"                                    => return Some(TileType::Tavern),
            "well" | "village well"                     => return Some(TileType::Well),
            "meadow" | "eastern meadow"                 => return Some(TileType::Meadow),
            "home" | "my home" | "my house"             => return Some(TileType::Home(agent_idx)),
            "temple"                                     => return Some(TileType::Temple),
            _ => {}
        }
        if lower.contains("rowan") && lower.contains("home") { return Some(TileType::Home(1)); }
        if lower.contains("elara") && lower.contains("home") { return Some(TileType::Home(0)); }
        if lower.contains("thane") && lower.contains("home") { return Some(TileType::Home(2)); }
        if lower.contains("home") { return Some(TileType::Home(agent_idx)); }
        None
    }

    // -----------------------------------------------------------------------
    // Pathfinding / geometry
    // -----------------------------------------------------------------------

    /// BFS to find the nearest tile matching `target_type`.
    fn nearest_tile_of_type(&self, from: (u8, u8), target_type: TileType) -> Option<(u8, u8)> {
        if self.tile_at(from) == target_type { return Some(from); }

        let mut visited = [[false; GRID_W]; GRID_H];
        let mut queue   = VecDeque::new();
        let (fx, fy)    = (from.0 as usize, from.1 as usize);
        visited[fy][fx] = true;
        queue.push_back(from);

        while let Some(pos) = queue.pop_front() {
            if self.tile_at(pos) == target_type { return Some(pos); }
            let (x, y) = (pos.0 as i32, pos.1 as i32);
            for dy in -1i32..=1 {
                for dx in -1i32..=1 {
                    if dx == 0 && dy == 0 { continue; }
                    let nx = x + dx;
                    let ny = y + dy;
                    if nx < 0 || ny < 0 || nx >= GRID_W as i32 || ny >= GRID_H as i32 { continue; }
                    let (nx, ny) = (nx as usize, ny as usize);
                    if !visited[ny][nx] {
                        visited[ny][nx] = true;
                        queue.push_back((nx as u8, ny as u8));
                    }
                }
            }
        }
        None
    }

    /// Move one step from `from` toward `to` (Chebyshev, diagonal allowed).
    fn step_toward(from: (u8, u8), to: (u8, u8)) -> (u8, u8) {
        let fx = from.0 as i32;
        let fy = from.1 as i32;
        let tx = to.0   as i32;
        let ty = to.1   as i32;
        let nx = (fx + (tx - fx).signum()).clamp(0, (GRID_W - 1) as i32) as u8;
        let ny = (fy + (ty - fy).signum()).clamp(0, (GRID_H - 1) as i32) as u8;
        (nx, ny)
    }

    fn chebyshev_dist(a: (u8, u8), b: (u8, u8)) -> u8 {
        let dx = (a.0 as i32 - b.0 as i32).unsigned_abs() as u8;
        let dy = (a.1 as i32 - b.1 as i32).unsigned_abs() as u8;
        dx.max(dy)
    }

    fn direction_label(from: (u8, u8), to: (u8, u8)) -> &'static str {
        let dx = to.0 as i32 - from.0 as i32;
        let dy = to.1 as i32 - from.1 as i32;
        match (dx.signum(), dy.signum()) {
            (0,  -1) => "north",
            (0,   1) => "south",
            (1,   0) => "east",
            (-1,  0) => "west",
            (1,  -1) => "northeast",
            (-1, -1) => "northwest",
            (1,   1) => "southeast",
            (-1,  1) => "southwest",
            _        => "nearby",
        }
    }
}

// ---------------------------------------------------------------------------
// Build the 32×32 tile grid
// ---------------------------------------------------------------------------

fn build_grid(n_agents: usize) -> [[TileType; GRID_W]; GRID_H] {
    let mut g = [[TileType::Open; GRID_W]; GRID_H];

    // Forest (N): rows 0..10, cols 0..16
    for row in 0..10  { for col in 0..16  { g[row][col] = TileType::Forest; } }
    // Forest (W): rows 10..20, cols 0..4
    for row in 10..20 { for col in 0..4   { g[row][col] = TileType::Forest; } }

    // River N-S channel: rows 0..22, cols 15..18 (placed before Square/Tavern so they can override)
    for row in 0..22  { for col in 15..18 { g[row][col] = TileType::River; } }
    // River bend: rows 22..26, cols 15..23
    for row in 22..26 { for col in 15..23 { g[row][col] = TileType::River; } }

    // Village Square: rows 14..20, cols 8..16 (overrides river at col 15 in those rows)
    for row in 14..20 { for col in 8..16  { g[row][col] = TileType::Square; } }
    // Tavern: rows 14..17, cols 17..22 (overrides river at col 17 in those rows)
    for row in 14..17 { for col in 17..22 { g[row][col] = TileType::Tavern; } }

    // Well: rows 11..13, cols 13..15
    for row in 11..13 { for col in 13..15 { g[row][col] = TileType::Well; } }

    // Meadow: rows 18..30, cols 22..32
    for row in 18..30 { for col in 22..32 { g[row][col] = TileType::Meadow; } }

    // Temple: rows 10..13, cols 8..12 (north of Village Square)
    for row in 10..13 { for col in 8..12 { g[row][col] = TileType::Temple; } }

    // Home tiles (2×3 block: 3 wide, 2 tall; HOME_POSITIONS is top-left corner)
    for (i, &(hx, hy)) in HOME_POSITIONS[..n_agents].iter().enumerate() {
        for dy in 0..2usize {
            for dx in 0..3usize {
                g[hy as usize + dy][hx as usize + dx] = TileType::Home(i);
            }
        }
    }

    g
}
