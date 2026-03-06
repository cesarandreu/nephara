use serde::Deserialize;
use tracing::{debug, warn};

use crate::agent::{Agent, NeedChanges};
use crate::config::Config;

// ---------------------------------------------------------------------------
// Interpreter prompt
// ---------------------------------------------------------------------------

pub fn build_interpreter_prompt(
    agent:        &Agent,
    intent:       &str,
    location_name: &str,
    others_nearby: &[String],
    config:       &Config,
) -> String {
    let others = if others_nearby.is_empty() {
        "None nearby".to_string()
    } else {
        others_nearby.join(", ")
    };

    let world_notes = format!(
        "Hunger: {:.0}/100, Energy: {:.0}/100",
        agent.needs.hunger, agent.needs.energy
    );

    format!(
        r#"You are the Interpreter of Intent in the world of Nephara. A being has spoken
a desire upon reality, and reality must respond.

SPEAKER: {name}
NUMEN (magical clarity, 1-10): {numen}
LOCATION: {location}
NEARBY: {others}
WORLD STATE NOTES: {world_notes}

THE SPOKEN INTENT:
"{intent}"

Your task:
1. Identify the PRIMARY EFFECT — what the speaker most likely meant.
2. Analyze every word for SECONDARY MEANINGS — synonyms, metaphors, double
   meanings, emotional undertones, etymological echoes. List 2-3.
3. Based on Numen score, determine how the intent manifests:
   - Numen 1-3: Secondary meanings DOMINATE. Reality is creative and willful.
   - Numen 4-6: MIXED. Primary effect occurs, but secondary meanings also manifest.
   - Numen 7-9: CLEAN. Primary dominates. Secondary effects are subtle, poetic.
   - Numen 10: MASTERFUL. Almost exactly as meant. Secondary effects are beautiful.
4. Determine duration in ticks (1-4, more ambitious = longer).
5. Determine need changes for the caster (energy always drains by {energy_drain}).

CRITICAL: The spell ALWAYS SUCCEEDS. Never say "nothing happens." Every intent
produces something interesting. Wild misinterpretations should feel like stories,
not punishment.

No direct harm to others. No world-breaking effects. Effects are local and temporary.

Respond with ONLY a JSON object:
{{
  "primary_effect": "What happens as intended",
  "interpretations": ["secondary meaning 1", "secondary meaning 2"],
  "secondary_effect": "What else happens due to the words' other meanings",
  "duration_ticks": 2,
  "need_changes": {{"fun": 10, "energy": -8}},
  "memory_entry": "One-line summary for the caster's memory"
}}"#,
        name        = agent.identity.name,
        numen       = agent.attributes.numen,
        location    = location_name,
        others      = others,
        world_notes = world_notes,
        intent      = intent,
        energy_drain = config.actions.cast_intent.energy_drain.unwrap_or(8.0),
    )
}

// ---------------------------------------------------------------------------
// Interpreter response
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct InterpretedIntent {
    pub primary_effect:   String,
    pub interpretations:  Vec<String>,
    pub secondary_effect: String,
    pub duration_ticks:   u32,
    pub need_changes:     RawNeedChanges,
    pub memory_entry:     String,
}

/// Serde target for the raw need changes map from the LLM.
#[derive(Debug, Deserialize, Default)]
pub struct RawNeedChanges {
    #[serde(default)] pub hunger:  Option<f32>,
    #[serde(default)] pub energy:  Option<f32>,
    #[serde(default)] pub fun:     Option<f32>,
    #[serde(default)] pub social:  Option<f32>,
    #[serde(default)] pub hygiene: Option<f32>,
}

impl InterpretedIntent {
    pub fn to_need_changes(&self, config: &Config) -> NeedChanges {
        // Always apply the configured energy drain; LLM may or may not include it
        let llm_energy = self.need_changes.energy.unwrap_or(0.0);
        let drain      = config.actions.cast_intent.energy_drain.unwrap_or(8.0);
        // Use whichever is more negative
        let energy = if llm_energy < -drain { llm_energy } else { -drain };

        NeedChanges {
            hunger:  self.need_changes.hunger,
            energy:  Some(energy),
            fun:     self.need_changes.fun,
            social:  self.need_changes.social,
            hygiene: self.need_changes.hygiene,
        }
    }

    /// Clamp duration to configured bounds.
    pub fn clamped_duration(&self, config: &Config) -> u32 {
        let min = config.actions.cast_intent.min_duration_ticks.unwrap_or(1);
        let max = config.actions.cast_intent.max_duration_ticks.unwrap_or(4);
        self.duration_ticks.clamp(min, max)
    }
}

// ---------------------------------------------------------------------------
// Response parsing — same cascading approach as action parser
// ---------------------------------------------------------------------------

pub fn parse_interpreter_response(raw: &str) -> Option<InterpretedIntent> {
    let stripped = crate::action::strip_thinking_tags(raw);
    let raw = stripped.as_str();
    debug!(target: "magic", chars = raw.len(), raw = %raw, "Interpreter raw response");

    if let Ok(v) = serde_json::from_str::<InterpretedIntent>(raw.trim()) {
        debug!(target: "magic", primary = %v.primary_effect, duration = v.duration_ticks, "Interpreter parsed");
        return Some(v);
    }

    // Extract from code fence
    if let Some(json) = extract_code_fence(raw) {
        if let Ok(v) = serde_json::from_str::<InterpretedIntent>(&json) {
            debug!(target: "magic", primary = %v.primary_effect, duration = v.duration_ticks, "Interpreter parsed");
            return Some(v);
        }
    }

    warn!("Failed to parse interpreter response. Raw: {}", &raw[..raw.len().min(300)]);
    None
}

fn extract_code_fence(s: &str) -> Option<String> {
    let start = s.find("```")?;
    let rest  = &s[start + 3..];
    let rest  = rest.trim_start_matches(|c: char| c.is_alphabetic());
    let end   = rest.find("```")?;
    Some(rest[..end].trim().to_string())
}

// ---------------------------------------------------------------------------
// Fallback InterpretedIntent when parsing fails
// ---------------------------------------------------------------------------

pub fn fallback_intent(intent: &str, energy_drain: f32) -> InterpretedIntent {
    InterpretedIntent {
        primary_effect:  format!("Something stirs in response to \"{}\".", intent),
        interpretations: vec!["the world listens in its own way".into()],
        secondary_effect: "A faint warmth brushes those nearby.".into(),
        duration_ticks:  1,
        need_changes:    RawNeedChanges { energy: Some(-energy_drain), fun: Some(5.0), ..Default::default() },
        memory_entry:    format!("Cast intent: \"{}\". Reality trembled faintly.", intent),
    }
}
