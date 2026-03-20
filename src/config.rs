use serde::Deserialize;
use std::fs;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub time:       TimeConfig,
    pub needs:      NeedsConfig,
    pub actions:    ActionsConfig,
    pub resolution: ResolutionConfig,
    pub memory:     MemoryConfig,
    pub simulation: SimulationConfig,
    pub llm:        LlmConfig,
    pub world:      WorldConfig,
    pub events:     EventsConfig,
    pub agent:      AgentConfig,
    pub inventory:  InventoryConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct InventoryConfig {
    pub max_slots:           u8,
    pub forage_berry_min:    u8,
    pub forage_berry_max:    u8,
    pub forage_herb_min:     u8,
    pub forage_herb_max:     u8,
    pub fish_min:            u8,
    pub fish_max:            u8,
    pub cook_items_required: u8,
    pub cook_hunger_bonus:   f32,
    pub cook_fun_bonus:      f32,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AgentConfig {
    /// Max rumors stored per agent-about-agent pair.
    pub beliefs_max_per_agent:   usize,
    /// How many beliefs (per agent) to inject into the perception prompt.
    pub beliefs_in_prompt_count: usize,
}

#[derive(Debug, Deserialize, Clone)]
pub struct WorldConfig {
    pub resource_respawn_ticks: u32,
    pub god_name:               String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct DailyPraiseConfig {
    pub penalty:                    f32,
    pub devotion_decay:             f32,
    pub devotion_gain_sincere:      f32,
    pub devotion_gain_heartfelt:    f32,
    pub devotion_gain_transcendent: f32,
    /// Ticks of freedom after transcendent praise.
    pub praise_cooldown_transcendent: u32,
    pub praise_cooldown_heartfelt:    u32,
    pub praise_cooldown_sincere:      u32,
    /// Short cooldown — must praise again soon.
    pub praise_cooldown_hollow:       u32,
    /// How many recent praises to compare against for repetition detection.
    pub praise_repeat_window:         usize,
    /// Jaccard word-overlap threshold above which praise is considered repetitive.
    pub praise_repeat_threshold:      f32,
}

#[derive(Debug, Deserialize, Clone)]
pub struct EventsConfig {
    /// Per-tick probability of a Storm starting (when no event is active).
    pub storm_prob:    f32,
    /// Per-tick probability of a Festival starting (when no event is active).
    pub festival_prob: f32,
    /// Per-tick probability of a ResourceWindfall occurring (independent of active event).
    pub windfall_prob: f32,
    /// Per-tick probability of a MagicResidue appearing (when no event is active).
    pub residue_prob:  f32,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TimeConfig {
    pub ticks_per_day:    u32,
    pub night_start_tick: u32,
}

#[derive(Debug, Deserialize, Clone)]
pub struct NeedsConfig {
    pub decay_per_tick: NeedsValues,
    pub initial:        NeedsValues,
    pub thresholds:     NeedsThresholds,
    pub daily_praise:   DailyPraiseConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct NeedsValues {
    pub hunger:  f32,
    pub energy:  f32,
    pub fun:     f32,
    pub social:  f32,
    pub hygiene: f32,
}

#[derive(Debug, Deserialize, Clone)]
pub struct NeedsThresholds {
    pub penalty_mild:   f32,
    pub penalty_severe: f32,
    pub forced_action:  f32,
}

/// All action configs in one struct — every field in ActionConfig is optional
/// so missing TOML keys are silently skipped.
#[derive(Debug, Deserialize, Clone)]
pub struct ActionsConfig {
    pub eat:         ActionConfig,
    pub cook:        ActionConfig,
    pub sleep:       ActionConfig,
    pub rest:        ActionConfig,
    pub forage:      ActionConfig,
    pub fish:        ActionConfig,
    pub exercise:    ActionConfig,
    pub chat:        ActionConfig,
    pub bathe:       ActionConfig,
    pub explore:     ActionConfig,
    pub play:        ActionConfig,
    pub cast_intent:  ActionConfig,
    pub pray:         ActionConfig,
    pub read_oracle:  ActionConfig,
    pub praise:       ActionConfig,
    pub compose:      ActionConfig,
    pub gossip:       ActionConfig,
    pub meditate:     ActionConfig,
    pub teach:        ActionConfig,
    pub admire:       ActionConfig,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct ActionConfig {
    #[serde(default)] pub hunger_restore:          Option<f32>,
    #[serde(default)] pub hunger_drain:            Option<f32>,
    #[serde(default)] pub energy_restore:          Option<f32>,
    #[serde(default)] pub energy_restore_per_tick: Option<f32>,
    #[serde(default)] pub energy_drain:            Option<f32>,
    #[serde(default)] pub fun_restore:             Option<f32>,
    #[serde(default)] pub social_restore:          Option<f32>,
    #[serde(default)] pub hygiene_restore:         Option<f32>,
    #[serde(default)] pub dc:                      u32,
    #[serde(default)] pub duration_ticks:          Option<u32>,
    #[serde(default)] pub min_duration_ticks:      Option<u32>,
    #[serde(default)] pub max_duration_ticks:      Option<u32>,
    #[serde(default)] pub repeat_window:           Option<usize>,
    #[serde(default)] pub repeat_energy_penalty:   Option<f32>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ResolutionConfig {
    pub crit_fail:      u32,
    pub crit_success:   u32,
    pub night_dc_bonus: i32,
}

#[derive(Debug, Deserialize, Clone)]
pub struct MemoryConfig {
    pub buffer_size:   usize,
    pub journal_n_runs: usize,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SimulationConfig {
    pub default_run_ticks:   u32,
    pub state_dump_interval: u32,
    pub tick_delay_ms:       u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct LlmConfig {
    pub model:                  String,
    pub temperature:            f32,
    pub max_tokens:             u32,
    pub ollama_url:             String,
    pub llamacpp_url:           String,
    pub interpreter_max_tokens: u32,
    pub planning_max_tokens:    u32,
    pub reflection_max_tokens:  u32,
    pub smart_model:            Option<String>,
    pub smart_ollama_url:       Option<String>,
    pub narrator_max_tokens:    u32,
    pub desires_max_tokens:     u32,
    pub oracle_max_tokens:           u32,
    pub journal_summary_max_tokens:  u32,
    /// When Some(false), passes `think: false` to disable chain-of-thought on thinking models.
    /// Leave unset (None) for standard models; set to false for qwen3, deepseek-r1, etc.
    pub think:                  Option<bool>,
    /// Abort the stream if thinking-token accumulation exceeds this many characters.
    /// Prevents runaway chain-of-thought from consuming the entire context window.
    pub thinking_budget_chars:  Option<usize>,
}

pub fn load(path: &str) -> Result<Config, Box<dyn std::error::Error + Send + Sync>> {
    let content = fs::read_to_string(path)
        .map_err(|e| format!("Cannot read config '{}': {}", path, e))?;
    let config = toml::from_str(&content)
        .map_err(|e| format!("Cannot parse config '{}': {}", path, e))?;
    Ok(config)
}

/// Validate loaded config, returning a list of warning strings.
/// Returns an error string if any value is critically invalid.
pub fn validate(config: &Config) -> Vec<String> {
    let mut warnings = Vec::new();

    // Decay rates
    for (name, val) in [
        ("hunger",  config.needs.decay_per_tick.hunger),
        ("energy",  config.needs.decay_per_tick.energy),
        ("fun",     config.needs.decay_per_tick.fun),
        ("social",  config.needs.decay_per_tick.social),
        ("hygiene", config.needs.decay_per_tick.hygiene),
    ] {
        if val < 0.0 {
            warnings.push(format!("needs.decay_per_tick.{} is negative ({})", name, val));
        }
        if val > 10.0 {
            warnings.push(format!("needs.decay_per_tick.{} = {} is unusually high (>10)", name, val));
        }
    }

    // DCs: check all action DCs are 0-30
    for (name, dc) in [
        ("cook",     config.actions.cook.dc),
        ("forage",   config.actions.forage.dc),
        ("fish",     config.actions.fish.dc),
        ("exercise", config.actions.exercise.dc),
        ("chat",     config.actions.chat.dc),
        ("explore",  config.actions.explore.dc),
    ] {
        if dc > 30 {
            warnings.push(format!("actions.{}.dc = {} exceeds max reasonable value of 30", name, dc));
        }
    }

    // thresholds sanity
    let t = &config.needs.thresholds;
    if t.forced_action >= t.penalty_severe {
        warnings.push(format!("forced_action threshold ({}) >= penalty_severe ({}) — needs logic may misbehave",
            t.forced_action, t.penalty_severe));
    }
    if t.penalty_severe >= t.penalty_mild {
        warnings.push(format!("penalty_severe threshold ({}) >= penalty_mild ({}) — needs logic may misbehave",
            t.penalty_severe, t.penalty_mild));
    }

    warnings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_config_succeeds() {
        let cfg = load("config/world.toml").expect("config/world.toml should load");
        assert!(cfg.time.ticks_per_day > 0, "ticks_per_day must be > 0");
        assert!(cfg.simulation.default_run_ticks > 0, "default_run_ticks must be > 0");
    }

    #[test]
    fn validate_config_no_errors() {
        let cfg = load("config/world.toml").expect("config/world.toml should load");
        let warnings = validate(&cfg);
        assert!(warnings.is_empty(), "unexpected config warnings: {:?}", warnings);
    }
}
