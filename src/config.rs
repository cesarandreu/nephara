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
    pub cast_intent: ActionConfig,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct ActionConfig {
    #[serde(default)] pub hunger_restore:          Option<f32>,
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
}

#[derive(Debug, Deserialize, Clone)]
pub struct ResolutionConfig {
    pub crit_fail:      u32,
    pub crit_success:   u32,
    pub night_dc_bonus: i32,
}

#[derive(Debug, Deserialize, Clone)]
pub struct MemoryConfig {
    pub buffer_size: usize,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SimulationConfig {
    pub default_run_ticks:   u32,
    pub state_dump_interval: u32,
}

#[derive(Debug, Deserialize, Clone)]
pub struct LlmConfig {
    pub model:                  String,
    pub temperature:            f32,
    pub max_tokens:             u32,
    pub ollama_url:             String,
    pub interpreter_max_tokens: u32,
    pub planning_max_tokens:    u32,
    pub reflection_max_tokens:  u32,
    pub smart_model:            Option<String>,
    pub narrator_max_tokens:    u32,
    pub desires_max_tokens:     u32,
}

pub fn load(path: &str) -> Result<Config, Box<dyn std::error::Error + Send + Sync>> {
    let content = fs::read_to_string(path)
        .map_err(|e| format!("Cannot read config '{}': {}", path, e))?;
    let config = toml::from_str(&content)
        .map_err(|e| format!("Cannot parse config '{}': {}", path, e))?;
    Ok(config)
}
