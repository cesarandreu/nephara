use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use std::collections::VecDeque;
use std::sync::Arc;
use tracing::{debug, warn};

use crate::action::{self, Action, OutcomeTier, Resolution};
use crate::agent::Agent;
use crate::config::Config;
use crate::llm::LlmBackend;
use crate::log::{self as runlog, RunLog, TickEntry};
use crate::magic;
use crate::soul::SoulSeed;

// ---------------------------------------------------------------------------
// Grid constants
// ---------------------------------------------------------------------------

pub const GRID_W: usize = 24;
pub const GRID_H: usize = 12;

/// Home positions per agent index (x=col, y=row).
/// Agents are sorted alphabetically: Elara=0, Rowan=1, Thane=2.
pub const HOME_POSITIONS: &[(u8, u8)] = &[
    (1, 4),   // agent 0
    (1, 7),   // agent 1
    (16, 8),  // agent 2
];

fn home_pos_for(idx: usize) -> (u8, u8) {
    HOME_POSITIONS.get(idx).copied().unwrap_or((0, 0))
}

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
    Home(usize),
}

fn tile_char(tile: TileType) -> char {
    match tile {
        TileType::Open    => '.',
        TileType::Forest  => 'F',
        TileType::River   => '~',
        TileType::Square  => 'S',
        TileType::Tavern  => 'T',
        TileType::Home(_) => 'h',
    }
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
}

// ---------------------------------------------------------------------------
// World
// ---------------------------------------------------------------------------

pub struct World {
    pub tick_num:       u32,
    pub agents:         Vec<Agent>,
    pub seed:           u64,
    pub config:         Config,
    pub run_log:        RunLog,
    pub notable_events: Vec<(usize, String)>,
    pub magic_count:    u32,
    grid:               [[TileType; GRID_W]; GRID_H],
    rng:                StdRng,
    llm:                Arc<dyn LlmBackend>,
    llm_call_counter:   u64,
}

impl World {
    // -----------------------------------------------------------------------
    // Construction
    // -----------------------------------------------------------------------

    pub fn new(
        seeds:   Vec<SoulSeed>,
        config:  Config,
        seed:    u64,
        rng:     StdRng,
        llm:     Arc<dyn LlmBackend>,
        run_log: RunLog,
    ) -> Self {
        let agents = seeds.iter().enumerate()
            .map(|(i, s)| Agent::from_soul(i, s, &config, home_pos_for(i)))
            .collect();
        let grid = build_grid();
        World {
            tick_num: 0,
            agents,
            seed,
            config,
            run_log,
            notable_events: Vec::new(),
            magic_count: 0,
            grid,
            rng,
            llm,
            llm_call_counter: 0,
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

        // Randomise agent order each tick
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

        self.tick_num += 1;

        let map = self.render_map();
        Ok(TickResult { tick, day, time_of_day: tod, entries, map })
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
                agent_name:  self.agents[idx].name().to_string(),
                location:    loc_name,
                action_line: format!("(busy — {} tick{} remaining)", ticks_left, if ticks_left == 1 { "" } else { "s" }),
                outcome_line: String::new(),
            });
        }

        // --- Forced sleep if energy < forced_action threshold ---
        let action = if self.agents[idx].needs.energy < self.config.needs.thresholds.forced_action
            && self.is_at_own_home(idx)
        {
            Action::Sleep
        } else if self.agents[idx].needs.energy < self.config.needs.thresholds.forced_action {
            Action::Move { destination: "home".to_string() }
        } else {
            // Build schema from available canonical names
            let canonical = self.available_canonical_names(idx);
            let canonical_strs: Vec<&str> = canonical.iter().copied().collect();
            let schema = action::build_action_schema(&canonical_strs);

            // Build prompt and ask LLM
            let prompt    = self.build_prompt(idx, tick, day, is_night, tod);
            let call_seed = Some(self.seed.wrapping_add(self.llm_call_counter));
            self.llm_call_counter += 1;
            let llm = Arc::clone(&self.llm);
            let raw = llm
                .generate(&prompt, self.config.llm.max_tokens, call_seed, Some(&schema))
                .await
                .unwrap_or_else(|e| {
                    warn!("LLM error for {}: {}", self.agents[idx].name(), e);
                    String::new()
                });
            debug!(target: "action", agent = %self.agents[idx].name(), raw = %raw, "Agent action response");
            action::parse_response(&raw)
        };

        // --- Validate and resolve ---
        let action   = self.validate(idx, action);
        let tile     = self.tile_at(self.agents[idx].pos);
        let loc_name = self.tile_name(tile);
        let entry    = self.resolve_and_apply(idx, action, &loc_name, tick, day, tod, is_night).await?;

        Ok(entry)
    }

    // -----------------------------------------------------------------------
    // Validate action — returns the action unchanged or wander
    // -----------------------------------------------------------------------

    fn validate(&self, idx: usize, action: Action) -> Action {
        let pos  = self.agents[idx].pos;
        let tile = self.tile_at(pos);

        match action {
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
        // Pick a destination that isn't the current tile type
        let options = [
            ("Forest",         TileType::Forest),
            ("River",          TileType::River),
            ("Village Square", TileType::Square),
            ("Tavern",         TileType::Tavern),
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
        idx:      usize,
        action:   Action,
        loc_name: &str,
        tick:     u32,
        day:      u32,
        tod:      &str,
        is_night: bool,
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
                        agent_name:   self.agents[idx].name().to_string(),
                        location:     loc_name.to_string(),
                        action_line:  format!("Move > {} (arrived)", arrived),
                        outcome_line: format!("{} is already at {}.", self.agents[idx].name(), arrived),
                    });
                }

                if let Some(nearest) = self.nearest_tile_of_type(pos, target_type) {
                    let next_pos = Self::step_toward(pos, nearest);
                    self.agents[idx].pos = next_pos;
                    let mem = format!("Tick {tick} | Day {day} | {tod} | Moving toward {destination}");
                    let buf = self.config.memory.buffer_size;
                    self.agents[idx].push_memory(mem, buf);
                    Ok(TickEntry {
                        agent_name:   self.agents[idx].name().to_string(),
                        location:     loc_name.to_string(),
                        action_line:  format!("Move → {}", destination),
                        outcome_line: format!("{} moves toward {}.", self.agents[idx].name(), destination),
                    })
                } else {
                    Ok(TickEntry {
                        agent_name:   self.agents[idx].name().to_string(),
                        location:     loc_name.to_string(),
                        action_line:  format!("Move → {} (unreachable)", destination),
                        outcome_line: format!("{} wanders, unable to find {}.", self.agents[idx].name(), destination),
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
                    agent_name:   self.agents[idx].name().to_string(),
                    location:     loc_name.to_string(),
                    action_line:  "Sleep".to_string(),
                    outcome_line: format!("{} falls into a deep sleep.", self.agents[idx].name()),
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
                self.agents[idx].needs.apply(&res.need_changes);

                let nearby: Vec<String> = self.agents.iter()
                    .filter(|a| a.id != idx && Self::chebyshev_dist(a.pos, self.agents[idx].pos) <= 1)
                    .map(|a| a.name().to_string())
                    .collect();
                let agent_name_str = self.agents[idx].name().to_string();
                let gm_prompt = Self::build_gm_prompt(
                    &agent_name_str, &res.action.display(), &res.tier, loc_name, &nearby,
                );
                let call_seed = Some(self.seed.wrapping_add(self.llm_call_counter));
                self.llm_call_counter += 1;
                let llm = Arc::clone(&self.llm);
                debug!(target: "narrate", agent = %agent_name_str, action = %res.action.display(),
                       tier = %res.tier.label(), "GM Narrator prompt sent");
                let narrative = match llm.generate(&gm_prompt, 80, call_seed, None).await {
                    Ok(n) if !n.trim().is_empty() => {
                        let n = n.trim().to_string();
                        debug!(target: "narrate", narrative = %n, "GM Narrator response");
                        n
                    },
                    _ => self.narrative_for(&res, idx),
                };

                let check_line   = res.check_line();
                let action_line  = if check_line.is_empty() {
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

                let needs_note = res.need_changes.describe();
                let mem = format!("Tick {tick} | Day {day} | {tod} | {} — {} [{}]",
                    res.action.name(), res.tier.label(), needs_note);
                let buf = self.config.memory.buffer_size;
                self.agents[idx].push_memory(mem, buf);

                Ok(TickEntry {
                    agent_name:  self.agents[idx].name().to_string(),
                    location:    loc_name.to_string(),
                    action_line,
                    outcome_line: narrative,
                })
            }
        }
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
                    agent_name:  self.agents[idx].name().to_string(),
                    location:    loc_name.to_string(),
                    action_line: format!("Chat with {}", target),
                    outcome_line: format!("{} looks around for {} but finds no one.", self.agents[idx].name(), target),
                });
            }
        };

        let chat_prompt = self.build_chat_prompt(idx, target_idx);
        let call_seed   = Some(self.seed.wrapping_add(self.llm_call_counter));
        self.llm_call_counter += 1;
        let llm         = Arc::clone(&self.llm);
        let summary     = llm
            .generate(&chat_prompt, 80, call_seed, None)
            .await
            .unwrap_or_else(|_| {
                format!("{} and {} exchange a few words.", self.agents[idx].name(), self.agents[target_idx].name())
            });
        let summary = summary.trim().trim_matches('"').to_string();

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

        let check_line = res.check_line();
        Ok(TickEntry {
            agent_name:  self.agents[idx].name().to_string(),
            location:    loc_name.to_string(),
            action_line: format!("Chat with {} | {}", self.agents[target_idx].name(), check_line),
            outcome_line: format!("{} [{}]", summary, changes.describe()),
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
            .generate(&prompt, self.config.llm.interpreter_max_tokens, call_seed, None)
            .await
            .unwrap_or_default();

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
        let full_outcome = if interpreted.secondary_effect.is_empty() {
            format!("{}\n{}", interpreted.memory_entry, meta)
        } else {
            format!(
                "{}\n{}\n(secondary: {})",
                interpreted.memory_entry, meta, interpreted.secondary_effect
            )
        };

        Ok(TickEntry {
            agent_name:  self.agents[idx].name().to_string(),
            location:    loc_name.to_string(),
            action_line: format!("Cast Intent: \"{}\"", intent),
            outcome_line: full_outcome,
        })
    }

    // -----------------------------------------------------------------------
    // Prompt builders
    // -----------------------------------------------------------------------

    fn build_prompt(&self, idx: usize, tick: u32, day: u32, is_night: bool, tod: &str) -> String {
        let agent    = &self.agents[idx];
        let pos      = agent.pos;
        let tile     = self.tile_at(pos);
        let loc_name = self.tile_name(tile);
        let loc_desc = self.tile_desc(tile);

        // Nearby agents (Chebyshev distance ≤ 1)
        let nearby: Vec<String> = self.agents.iter()
            .filter(|a| a.id != idx && Self::chebyshev_dist(a.pos, pos) <= 1)
            .map(|a| {
                if a.is_busy() {
                    format!("{} (busy)", a.name())
                } else {
                    a.name().to_string()
                }
            })
            .collect();
        let nearby_str = if nearby.is_empty() {
            "You are alone.".to_string()
        } else {
            nearby.join(", ")
        };

        // Recent memory (newest first, up to 5)
        let memory_str: Vec<String> = agent.memory.iter().take(5).cloned().collect();
        let memory_block = if memory_str.is_empty() {
            "  (no memories yet)".to_string()
        } else {
            memory_str.iter().map(|m| format!("  - {}", m)).collect::<Vec<_>>().join("\n")
        };

        // Last action note for anti-repetition
        let last_action_note = match agent.memory.front() {
            Some(m) if !m.is_empty() => format!("\nLast action: {}", m),
            _ => String::new(),
        };

        // Need warnings
        let warnings     = agent.need_warnings(&self.config);
        let warnings_str = if warnings.is_empty() {
            String::new()
        } else {
            format!("\nWARNINGS:\n{}", warnings.iter().map(|w| format!("  ! {}", w)).collect::<Vec<_>>().join("\n"))
        };

        // 5×5 viewport
        let viewport = self.build_viewport(pos, 2);

        // Region distances
        let region_note = self.build_region_distances(pos, tile);

        // Available actions (human-readable, annotated)
        let available   = self.available_actions(idx);
        let actions_str = available.iter().enumerate()
            .map(|(i, a)| format!("  {}. {}", i + 1, a))
            .collect::<Vec<_>>()
            .join("\n");

        format!(
            r#"You are {name}. {personality}

{backstory}

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
{region_note}

RECENT MEMORY (newest first):
{memory}{last_action_note}

AVAILABLE ACTIONS:
{actions}
(You may also cast_intent — speak a desire upon reality. It will manifest,
though perhaps not as you expect.)

Avoid repeating the same action twice in a row. Your personality should guide what you do.

Choose ONE action. Respond with ONLY a JSON object:
{{"action": "action_name", "target": "optional_target_name", "intent": "if casting, your spoken desire", "reason": "brief reason"}}"#,
            name             = agent.identity.name,
            personality      = agent.identity.personality,
            backstory        = agent.identity.backstory,
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
        )
    }

    fn build_gm_prompt(
        agent_name:     &str,
        action_display: &str,
        tier:           &OutcomeTier,
        loc_name:       &str,
        nearby:         &[String],
    ) -> String {
        let context = if nearby.is_empty() {
            "Alone.".to_string()
        } else {
            format!("{} watched.", nearby.join(", "))
        };
        format!(
            "You are the Narrator of Nephara.\n\
             {agent_name} attempted to {action_display} at {loc_name}.\n\
             {context}\n\
             Outcome: {tier}.\n\n\
             Write ONE vivid sentence (15-25 words). Pure story — no numbers, no dice.",
            agent_name     = agent_name,
            action_display = action_display,
            loc_name       = loc_name,
            context        = context,
            tier           = tier.label(),
        )
    }

    fn build_chat_prompt(&self, a_idx: usize, b_idx: usize) -> String {
        let a = &self.agents[a_idx];
        let b = &self.agents[b_idx];
        let a_mem = a.memory.iter().next().cloned().unwrap_or_default();
        let b_mem = b.memory.iter().next().cloned().unwrap_or_default();
        format!(
            r#"Two villagers in Nephara are having a brief conversation.

{a_name}: {a_personality}
  Recent memory: {a_mem}

{b_name}: {b_personality}
  Recent memory: {b_mem}

Write ONE sentence that summarises what they talk about or say to each other.
Do not use quotation marks. Do not use names in the sentence. Just the summary."#,
            a_name        = a.identity.name,
            a_personality = a.identity.personality,
            b_name        = b.identity.name,
            b_personality = b.identity.personality,
            a_mem         = a_mem,
            b_mem         = b_mem,
        )
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

        v.push("move");
        v.push("cast_intent");
        v
    }

    // -----------------------------------------------------------------------
    // Available actions (human-readable with annotations, for prompt)
    // -----------------------------------------------------------------------

    fn available_actions(&self, idx: usize) -> Vec<String> {
        let tile    = self.tile_at(self.agents[idx].pos);
        let pos     = self.agents[idx].pos;
        let mut v   = Vec::new();

        if self.tile_allows(tile, "eat")      { v.push("eat (always works)".to_string()); }
        if self.tile_allows(tile, "cook")     { v.push("cook (Wit check)".to_string()); }
        if self.is_at_own_home(idx)           { v.push("sleep (always works)".to_string()); }
        v.push("rest (always works)".to_string());
        if self.tile_allows(tile, "forage")   { v.push("forage (Grace check)".to_string()); }
        if self.tile_allows(tile, "fish")     { v.push("fish (Grace check)".to_string()); }
        if self.tile_allows(tile, "exercise") { v.push("exercise (Vigor check)".to_string()); }
        if self.tile_allows(tile, "bathe")    { v.push("bathe (always works)".to_string()); }
        if self.tile_allows(tile, "explore")  { v.push("explore (Vigor check)".to_string()); }
        if self.tile_allows(tile, "play")     { v.push("play (always works)".to_string()); }

        // Chat: nearby non-busy agents (Chebyshev ≤ 1)
        for a in &self.agents {
            if a.id != idx && Self::chebyshev_dist(a.pos, pos) <= 1 && !a.is_busy() {
                v.push(format!("chat (Heart check) — target: {}", a.name()));
            }
        }

        // Move: named regions with distances
        let regions: &[(&str, TileType)] = &[
            ("Forest",         TileType::Forest),
            ("River",          TileType::River),
            ("Village Square", TileType::Square),
            ("Tavern",         TileType::Tavern),
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

        v
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
    // ASCII map renderer
    // -----------------------------------------------------------------------

    pub fn render_map(&self) -> String {
        let top_bot = format!("  +{}+", "-".repeat(GRID_W * 2 - 1));
        let mut lines = vec![top_bot.clone()];

        for row in 0..GRID_H {
            let mut row_chars: Vec<char> = Vec::with_capacity(GRID_W);
            for col in 0..GRID_W {
                let pos = (col as u8, row as u8);
                let here: Vec<char> = self.agents.iter()
                    .filter(|a| a.pos == pos)
                    .map(|a| a.name().chars().next().unwrap_or('?'))
                    .collect();
                let ch = if !here.is_empty() {
                    here[0]
                } else {
                    tile_char(self.grid[row][col])
                };
                row_chars.push(ch);
            }
            let row_str = row_chars.iter()
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
                .join(" ");
            lines.push(format!("  |{}|", row_str));
        }

        lines.push(top_bot);
        lines.join("\n")
    }

    // -----------------------------------------------------------------------
    // 5×5 viewport centered on agent
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
            TileType::Home(n) => {
                if let Some(a) = self.agents.get(n) {
                    format!("{}'s Home", a.identity.name)
                } else {
                    "Home".to_string()
                }
            }
        }
    }

    fn tile_desc(&self, tile: TileType) -> &'static str {
        match tile {
            TileType::Open    => "Open countryside. Little to do here besides rest.",
            TileType::Forest  => "Old trees press close. Birdsong and shadow.",
            TileType::River   => "A clear river murmurs over stones. Willows trail their fingers in the water.",
            TileType::Square  => "The heart of the village. Open sky, worn cobblestones, familiar faces.",
            TileType::Tavern  => "A warm, low-ceilinged tavern. The smell of ale and woodsmoke.",
            TileType::Home(_) => "A small, cosy home. Familiar and safe.",
        }
    }

    fn tile_allows(&self, tile: TileType, action: &str) -> bool {
        match tile {
            TileType::Open    => false,
            TileType::Forest  => matches!(action, "forage" | "explore" | "exercise"),
            TileType::River   => matches!(action, "fish" | "bathe"),
            TileType::Square  => matches!(action, "exercise" | "play"),
            TileType::Tavern  => matches!(action, "eat" | "cook" | "play"),
            TileType::Home(_) => matches!(action, "eat" | "cook" | "sleep"),
        }
    }

    fn is_at_own_home(&self, idx: usize) -> bool {
        matches!(self.tile_at(self.agents[idx].pos), TileType::Home(n) if n == idx)
    }

    fn parse_tile_type(&self, name: &str, agent_idx: usize) -> Option<TileType> {
        let lower = name.to_lowercase();
        let lower = lower.trim();
        match lower {
            "forest"                          => return Some(TileType::Forest),
            "river"                           => return Some(TileType::River),
            "village square" | "square"       => return Some(TileType::Square),
            "tavern"                          => return Some(TileType::Tavern),
            "home" | "my home" | "my house"   => return Some(TileType::Home(agent_idx)),
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
        // If already on a matching tile, return immediately.
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
// Build the 24×12 tile grid
// ---------------------------------------------------------------------------

fn build_grid() -> [[TileType; GRID_W]; GRID_H] {
    let mut g = [[TileType::Open; GRID_W]; GRID_H];

    // Forest: cols 4-9, rows 0-2
    for row in 0..3  { for col in 4..10 { g[row][col] = TileType::Forest; } }
    // River:  cols 12-14, rows 1-2
    for row in 1..3  { for col in 12..15 { g[row][col] = TileType::River; } }
    // Square: cols 4-8, rows 4-6
    for row in 4..7  { for col in 4..9  { g[row][col] = TileType::Square; } }
    // Tavern: cols 10-13, rows 5-6
    for row in 5..7  { for col in 10..14 { g[row][col] = TileType::Tavern; } }

    // Home tiles
    for (i, &(hx, hy)) in HOME_POSITIONS.iter().enumerate() {
        g[hy as usize][hx as usize] = TileType::Home(i);
    }

    g
}
