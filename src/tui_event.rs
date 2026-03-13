#![allow(dead_code)]

use ratatui::style::Color;

// ---------------------------------------------------------------------------
// Day-boundary events (defined here; imported by world.rs and tui.rs)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum DayEventKind {
    MorningIntention,
    EveningReflection,
    EveningDesire,
    /// A world event (storm, festival, windfall, magic residue) — FEAT-19.
    WorldEvent,
}

#[derive(Debug, Clone)]
pub struct DayEvent {
    pub kind:       DayEventKind,
    pub agent_id:   usize,
    pub agent_name: String,
    pub day:        u32,
    pub text:       String,
}

// ---------------------------------------------------------------------------
// TUI events sent from simulation → TUI thread
// ---------------------------------------------------------------------------

/// A single LLM prompt+response recorded during simulation.
#[derive(Clone)]
pub struct LlmCallRecord {
    pub day:        u32,
    pub call_type:  String,
    pub agent_name: String,
    pub prompt:     String,
    pub response:   String,
}

pub enum TuiEvent {
    TickStart {
        tick:        u32,
        day:         u32,
        time_of_day: &'static str,
    },
    MapUpdate(Vec<Vec<MapCell>>),
    NeedsUpdate(Vec<AgentNeedsSnapshot>),
    AgentAction(TickEntrySnapshot),
    MorningIntention {
        agent_id:   usize,
        agent_name: String,
        day:        u32,
        text:       String,
    },
    EveningDesire {
        agent_id:   usize,
        agent_name: String,
        day:        u32,
        text:       String,
    },
    EveningReflection {
        agent_id:   usize,
        agent_name: String,
        day:        u32,
        text:       String,
    },
    /// A world event (storm, festival, etc.) — FEAT-19.
    WorldEvent {
        day:  u32,
        text: String,
    },
    SimulationComplete {
        total_ticks:    u32,
        magic_count:    u32,
        notable_events: Vec<String>,
    },
    SimulationError(String),
    /// Streaming token from the current LLM call for `agent_id`.
    PartialToken {
        agent_id: usize,
        token:    String,
    },
    /// A completed LLM call for the debug overlay.
    LlmCall(LlmCallRecord),
}

// ---------------------------------------------------------------------------
// Data snapshots
// ---------------------------------------------------------------------------

pub struct TickEntrySnapshot {
    pub tick:               u32,
    pub day:                u32,
    pub agent_id:           usize,
    pub agent_name:         String,
    pub location:           String,
    pub agent_pos:          (u8, u8),
    pub action_line:        String,
    pub outcome_line:       String,
    pub outcome_tier_label: Option<String>,
    /// Text extracted from `Pray: "..."` action lines.
    pub prayer_text:        Option<String>,
    /// Total LLM time in milliseconds for this agent's turn.
    pub llm_duration_ms:    Option<u64>,
}

#[derive(Clone)]
pub struct MapCell {
    pub ch:    char,
    pub color: Color,
    pub bold:  bool,
}

pub struct AgentNeedsSnapshot {
    pub agent_id:   usize,
    pub agent_name: String,
    pub agent_pos:  (u8, u8),
    pub hunger:     f32,
    pub energy:     f32,
    pub fun:        f32,
    pub social:     f32,
    pub hygiene:    f32,
    pub devotion:   f32,
    /// Last N memory entries for the inspect panel.
    pub memories:   Vec<String>,
    /// Top beliefs about others: (about_name, most_recent_rumor).
    pub beliefs:    Vec<(String, String)>,
}
