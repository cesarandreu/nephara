use rand::rngs::StdRng;
use rand::Rng;
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::agent::{Attributes, NeedChanges, Needs};
use crate::config::{ActionConfig, Config};

// ---------------------------------------------------------------------------
// Structured output schema builder
// ---------------------------------------------------------------------------

/// Build a JSON schema that constrains the LLM's action response to valid
/// canonical action names only. Pass this to Ollama's `format` field.
pub fn build_action_schema(canonical_names: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "required": ["action", "reason", "description"],
        "properties": {
            "action":      { "type": "string", "enum": canonical_names },
            "target":      { "type": ["string", "null"] },
            "intent":      { "type": "string", "default": "" },
            "reason":      { "type": "string" },
            "description": { "type": "string" }
        }
    })
}

// ---------------------------------------------------------------------------
// Action enum
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Action {
    Eat,
    Cook,
    Sleep,
    Rest,
    Forage,
    Fish,
    Exercise,
    Chat { target_name: String },
    Bathe,
    Explore,
    Play,
    Move { destination: String },
    CastIntent { intent: String },
    /// Fallback when requested action fails validation.
    Wander,
}

impl Action {
    pub fn name(&self) -> &'static str {
        match self {
            Action::Eat         => "Eat",
            Action::Cook        => "Cook",
            Action::Sleep       => "Sleep",
            Action::Rest        => "Rest",
            Action::Forage      => "Forage",
            Action::Fish        => "Fish",
            Action::Exercise    => "Exercise",
            Action::Chat { .. } => "Chat",
            Action::Bathe       => "Bathe",
            Action::Explore     => "Explore",
            Action::Play        => "Play",
            Action::Move { .. } => "Move",
            Action::CastIntent { .. } => "Cast Intent",
            Action::Wander      => "Wander",
        }
    }

    pub fn display(&self) -> String {
        match self {
            Action::Chat { target_name }       => format!("Chat with {}", target_name),
            Action::Move { destination }       => format!("Move > {}", destination),
            Action::CastIntent { intent }      => format!("Cast Intent: \"{}\"", intent),
            other => other.name().to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Outcome tier
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum OutcomeTier {
    CriticalFail,
    Fail,
    Success,
    CriticalSuccess,
}

impl OutcomeTier {
    pub fn label(&self) -> &'static str {
        match self {
            OutcomeTier::CriticalFail    => "Critical Fail",
            OutcomeTier::Fail            => "Fail",
            OutcomeTier::Success         => "Success",
            OutcomeTier::CriticalSuccess => "Critical Success",
        }
    }

    /// Multiplier applied to need changes.
    pub fn multiplier(&self) -> f32 {
        match self {
            OutcomeTier::CriticalFail    => 0.5,
            OutcomeTier::Fail            => 0.0,
            OutcomeTier::Success         => 1.0,
            OutcomeTier::CriticalSuccess => 1.5,
        }
    }
}

// ---------------------------------------------------------------------------
// Action resolution result
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Resolution {
    pub action:       Action,
    pub tier:         OutcomeTier,
    pub roll:         u32,
    pub modifier:     i32,
    pub penalty:      i32,
    pub total:        i32,
    pub dc:           u32,
    pub need_changes: NeedChanges,
    pub duration:     u32,
}

impl Resolution {
    pub fn check_line(&self) -> String {
        if self.dc == 0 { return String::new(); }
        let attr = self.attribute_label();
        let mod_val = self.modifier + self.penalty;
        let mod_str = if mod_val > 0 {
            format!("+{}", mod_val)
        } else if mod_val < 0 {
            format!("{}", mod_val)
        } else {
            String::new()
        };
        if attr.is_empty() {
            format!("d20({}){}={} vs DC {} | {}", self.roll, mod_str, self.total, self.dc, self.tier.label())
        } else {
            format!("{} d20({}){}={} vs DC {} | {}", attr, self.roll, mod_str, self.total, self.dc, self.tier.label())
        }
    }

    fn attribute_label(&self) -> String {
        match &self.action {
            Action::Cook     => "Wit".into(),
            Action::Forage   => "Grace".into(),
            Action::Fish     => "Grace".into(),
            Action::Exercise => "Vigor".into(),
            Action::Chat { .. } => "Heart".into(),
            Action::Explore  => "Vigor".into(),
            _ => String::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Resolution logic
// ---------------------------------------------------------------------------

/// Build base NeedChanges from an ActionConfig.
fn base_changes(cfg: &ActionConfig) -> NeedChanges {
    NeedChanges {
        hunger:  cfg.hunger_restore,
        energy:  cfg.energy_restore.map(|v| v).or_else(|| cfg.energy_drain.map(|d| -d)),
        fun:     cfg.fun_restore,
        social:  cfg.social_restore,
        hygiene: cfg.hygiene_restore,
    }
}

/// Resolve a non-magic action. Returns a Resolution.
pub fn resolve(
    action:     &Action,
    attributes: &Attributes,
    needs:      &Needs,
    config:     &Config,
    is_night:   bool,
    rng:        &mut StdRng,
) -> Resolution {
    let (cfg, attr_name) = action_cfg_and_attr(action, config);
    let dc               = effective_dc(action, cfg, is_night, config);
    let base             = base_changes(cfg);

    // Auto-success actions (dc = 0)
    if dc == 0 {
        let duration = cfg.duration_ticks.unwrap_or(1);
        return Resolution {
            action: action.clone(),
            tier: OutcomeTier::Success,
            roll: 0, modifier: 0, penalty: 0, total: 0, dc: 0,
            need_changes: base,
            duration,
        };
    }

    let roll     = rng.gen_range(1u32..=20);
    let modifier = attributes.modifier(attr_name);
    let penalty  = needs.penalty(config, attr_name);
    let total    = roll as i32 + modifier + penalty;

    let tier = if roll == config.resolution.crit_fail {
        OutcomeTier::CriticalFail
    } else if roll == config.resolution.crit_success {
        OutcomeTier::CriticalSuccess
    } else if total >= dc as i32 {
        OutcomeTier::Success
    } else {
        OutcomeTier::Fail
    };

    let need_changes = base.scale(tier.multiplier());

    debug!(target: "action",
        action = %action.name(), roll = roll, modifier = modifier,
        penalty = penalty, total = total, dc = dc, tier = ?tier,
        "d20 resolution");

    Resolution {
        action: action.clone(),
        tier,
        roll, modifier, penalty, total, dc,
        need_changes,
        duration: 1,
    }
}

fn effective_dc(action: &Action, cfg: &ActionConfig, is_night: bool, config: &Config) -> u32 {
    let base = cfg.dc;
    if base == 0 { return 0; }
    let night_bonus = match action {
        Action::Forage | Action::Explore if is_night => config.resolution.night_dc_bonus as u32,
        _ => 0,
    };
    base + night_bonus
}

/// Returns (ActionConfig, attribute_name) for the given action.
pub fn action_cfg_and_attr<'a>(action: &Action, config: &'a Config) -> (&'a ActionConfig, &'static str) {
    match action {
        Action::Eat         => (&config.actions.eat,         ""),
        Action::Cook        => (&config.actions.cook,        "wit"),
        Action::Sleep       => (&config.actions.sleep,       ""),
        Action::Rest        => (&config.actions.rest,        ""),
        Action::Forage      => (&config.actions.forage,      "grace"),
        Action::Fish        => (&config.actions.fish,        "grace"),
        Action::Exercise    => (&config.actions.exercise,    "vigor"),
        Action::Chat { .. } => (&config.actions.chat,        "heart"),
        Action::Bathe       => (&config.actions.bathe,       ""),
        Action::Explore     => (&config.actions.explore,     "vigor"),
        Action::Play        => (&config.actions.play,        ""),
        Action::Move { .. } => (&config.actions.rest,        ""), // placeholder; move has no needs
        Action::CastIntent{ .. } => (&config.actions.cast_intent, "numen"),
        Action::Wander      => (&config.actions.rest,        ""),
    }
}

// ---------------------------------------------------------------------------
// Parse LLM response JSON into an Action
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct ActionResponse {
    action:      Option<String>,
    target:      Option<String>,
    intent:      Option<String>,
    reason:      Option<String>,
    description: Option<String>,
}

/// Cascading parser: JSON → code-fence extraction → regex → Wander default.
/// Returns (action, reason, description).
pub fn parse_response(raw: &str) -> (Action, Option<String>, Option<String>) {
    // 1. Try direct JSON parse
    if let Some(t) = try_parse_json(raw) {
        debug!(target: "action", action = ?t.0, "Action parsed from LLM output");
        return t;
    }
    // 2. Extract from ```json ... ``` code fence
    if let Some(json) = extract_code_fence(raw) {
        if let Some(t) = try_parse_json(&json) {
            debug!(target: "action", action = ?t.0, "Action parsed from LLM output");
            return t;
        }
    }
    // 3. Extract action name with regex-like scan
    if let Some(action_name) = extract_action_field(raw) {
        let a = action_from_name(&action_name, None, None);
        debug!(target: "action", action = ?a, "Action parsed from LLM output");
        return (a, None, None);
    }
    // 4. Default
    tracing::warn!("Could not parse LLM response, defaulting to Wander. Raw: {}", &raw[..raw.len().min(200)]);
    (Action::Wander, None, None)
}

fn try_parse_json(s: &str) -> Option<(Action, Option<String>, Option<String>)> {
    let s = s.trim();
    let parsed: ActionResponse = serde_json::from_str(s).ok()?;
    let name        = parsed.action?;
    let action      = action_from_name(&name, parsed.target.as_deref(), parsed.intent.as_deref());
    let reason      = parsed.reason.filter(|r| !r.is_empty());
    let description = parsed.description.filter(|d| !d.is_empty());
    Some((action, reason, description))
}

fn extract_code_fence(s: &str) -> Option<String> {
    let start = s.find("```")?;
    let rest  = &s[start + 3..];
    // skip optional language tag
    let rest  = rest.trim_start_matches(|c: char| c.is_alphabetic());
    let end   = rest.find("```")?;
    Some(rest[..end].trim().to_string())
}

fn extract_action_field(s: &str) -> Option<String> {
    // Look for "action": "something"
    let key = "\"action\"";
    let pos  = s.find(key)?;
    let rest = &s[pos + key.len()..];
    let colon = rest.find(':')? + 1;
    let rest  = rest[colon..].trim();
    if rest.starts_with('"') {
        let inner = &rest[1..];
        let end   = inner.find('"')?;
        return Some(inner[..end].to_string());
    }
    None
}

pub fn action_from_name(name: &str, target: Option<&str>, intent: Option<&str>) -> Action {
    match name.to_lowercase().replace('_', " ").trim() {
        "eat"                         => Action::Eat,
        "cook"                        => Action::Cook,
        "sleep"                       => Action::Sleep,
        "rest"                        => Action::Rest,
        "forage"                      => Action::Forage,
        "fish"                        => Action::Fish,
        "exercise"                    => Action::Exercise,
        "bathe"                       => Action::Bathe,
        "explore"                     => Action::Explore,
        "play"                        => Action::Play,
        "wander"                      => Action::Wander,
        "chat" => {
            let t = target.unwrap_or("").to_string();
            Action::Chat { target_name: t }
        }
        "move" => {
            let d = target.unwrap_or("Village Square").to_string();
            Action::Move { destination: d }
        }
        "cast intent" | "cast_intent" => {
            let i = intent.unwrap_or("").to_string();
            if i.is_empty() {
                Action::CastIntent { intent: "I seek something I cannot quite name".to_string() }
            } else {
                Action::CastIntent { intent: i }
            }
        }
        other => {
            tracing::warn!("Unknown action '{}', defaulting to Wander", other);
            Action::Wander
        }
    }
}
