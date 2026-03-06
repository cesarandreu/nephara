use async_trait::async_trait;
use rand::rngs::StdRng;
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use tracing::{debug, info, warn};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait LlmBackend: Send + Sync {
    /// Generate a completion for the given prompt.
    /// `seed`   — when Some, passes the seed to the backend for deterministic output.
    /// `schema` — when Some, passed as Ollama's `format` field to constrain output.
    async fn generate(
        &self,
        prompt:    &str,
        max_tokens: u32,
        seed:      Option<u64>,
        schema:    Option<&serde_json::Value>,
    ) -> Result<String>;
}

// ---------------------------------------------------------------------------
// Ollama backend
// ---------------------------------------------------------------------------

pub struct OllamaBackend {
    pub url:         String,
    pub model:       String,
    pub temperature: f32,
    client:          reqwest::Client,
}

impl OllamaBackend {
    pub fn new(url: String, model: String, temperature: f32) -> Self {
        OllamaBackend {
            url,
            model,
            temperature,
            client: reqwest::Client::new(),
        }
    }
}

#[derive(Serialize)]
struct OllamaRequest<'a> {
    model:   &'a str,
    prompt:  &'a str,
    stream:  bool,
    options: OllamaOptions,
    #[serde(skip_serializing_if = "Option::is_none")]
    format:  Option<&'a serde_json::Value>,
}

#[derive(Serialize)]
struct OllamaOptions {
    temperature: f32,
    num_predict: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    seed: Option<i64>,
}

#[derive(Deserialize)]
struct OllamaResponse {
    response: String,
}

#[derive(Deserialize)]
struct OllamaTagsResponse {
    models: Vec<OllamaModelEntry>,
}

#[derive(Deserialize)]
struct OllamaModelEntry {
    name: String,
}

impl OllamaBackend {
    pub async fn health_check(&self) -> Result<()> {
        let url = format!("{}/api/tags", self.url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("Ollama not running at {}: {}", self.url, e))?;

        if !resp.status().is_success() {
            return Err(format!("Ollama health check failed ({})", resp.status()).into());
        }

        let tags: OllamaTagsResponse = resp
            .json()
            .await
            .map_err(|e| format!("Ollama tags parse error: {}", e))?;

        let names: Vec<&str> = tags.models.iter().map(|m| m.name.as_str()).collect();
        if names.iter().any(|n| *n == self.model || n.starts_with(&format!("{}:", self.model))) {
            info!(target: "llm", model = %self.model, "Ollama ready: model available");
        } else {
            warn!(target: "llm", model = %self.model, available = ?names, "Model not found in Ollama list — will try anyway");
        }
        Ok(())
    }
}

#[async_trait]
impl LlmBackend for OllamaBackend {
    async fn generate(
        &self,
        prompt:    &str,
        max_tokens: u32,
        seed:      Option<u64>,
        schema:    Option<&serde_json::Value>,
    ) -> Result<String> {
        let url  = format!("{}/api/generate", self.url);
        let body = OllamaRequest {
            model:  &self.model,
            prompt,
            stream: false,
            options: OllamaOptions {
                temperature: seed.map(|_| 0.0).unwrap_or(self.temperature),
                num_predict: max_tokens,
                seed: seed.map(|s| s as i64),
            },
            format: schema,
        };

        debug!(target: "llm", model = %self.model, max_tokens = max_tokens,
               prompt_chars = prompt.len(), has_schema = schema.is_some(), "LLM request");
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Ollama HTTP error: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text   = resp.text().await.unwrap_or_default();
            return Err(format!("Ollama returned {}: {}", status, text).into());
        }

        let ollama_resp: OllamaResponse = resp
            .json()
            .await
            .map_err(|e| format!("Ollama JSON parse error: {}", e))?;

        let raw = ollama_resp.response;
        debug!(target: "llm", chars = raw.len(), response = %raw, "LLM response");
        Ok(raw)
    }
}

// ---------------------------------------------------------------------------
// Mock backend — fully deterministic, returns plausible JSON actions
// ---------------------------------------------------------------------------

pub struct MockBackend {
    rng: Mutex<StdRng>,
}

impl MockBackend {
    pub fn new(rng: StdRng) -> Self {
        MockBackend { rng: Mutex::new(rng) }
    }
}

// Vivid 2-3 sentence outcomes for the DM Narrator
static MOCK_NARRATIVES: &[&str] = &[
    "The effort shows in her hands — rough work, honest result. She doesn't stop to admire it; there is no need. Something settles in her that had been restless all morning.",
    "He moves through it like someone who has done this a thousand times before. There is no hesitation, no wasted motion. When it's done, he doesn't look back.",
    "Something shifts in the air around her, subtle but real. She pauses mid-motion, head tilted, as though listening to a sound no one else can hear. Then she continues, and the moment passes.",
    "It goes badly, and he knows it before it's finished. He sets it aside without ceremony and stands very still for a moment. Then he starts again.",
    "She finds exactly what she was looking for, and it surprises her. She holds it up to the light, turns it once, then tucks it away carefully. Some luck deserves to be kept quiet.",
    "The moment passes without ceremony, leaving only the quiet satisfaction of having tried. Nothing dramatic — just the small, honest weight of a thing attempted. That is enough for now.",
    "He stumbles once, catches himself, and carries on with quiet dignity. No one saw it, or if they did, they say nothing. By the time he reaches the end, he has already forgiven himself.",
    "A small triumph, the kind no one else will notice but her. She allows herself one moment of stillness to mark it. Then the world resumes, indifferent and continuing.",
];

// Valid InterpretedIntent JSON for mock Interpreter calls
static MOCK_INTERPRETER_RESPONSES: &[&str] = &[
    r#"{"primary_effect":"A warmth settles in the bones, like sun through thin cloth.","interpretations":["warmth as belonging","warmth as memory"],"secondary_effect":"Those nearby feel briefly, inexplicably welcome.","duration_ticks":2,"need_changes":{"fun":10,"energy":-8,"social":5},"memory_entry":"Cast intent: warmth. It answered, in its own way."}"#,
    r#"{"primary_effect":"The light bends strangely for a moment, then settles.","interpretations":["light as clarity","light as attention"],"secondary_effect":"A crow lands nearby and watches with unusual focus.","duration_ticks":1,"need_changes":{"fun":8,"energy":-8},"memory_entry":"Cast intent: light. The world blinked."}"#,
    r#"{"primary_effect":"The sound of the village seems to quiet, just slightly.","interpretations":["quiet as peace","quiet as absence"],"secondary_effect":"Someone, somewhere, stops what they were saying mid-sentence.","duration_ticks":2,"need_changes":{"fun":6,"energy":-8,"social":-3},"memory_entry":"Cast intent: stillness. The world half-listened."}"#,
    r#"{"primary_effect":"A smell of rain arrives before any rain does.","interpretations":["rain as change","rain as cleansing"],"secondary_effect":"Three birds take flight from the same tree at once.","duration_ticks":1,"need_changes":{"fun":12,"energy":-8,"hygiene":5},"memory_entry":"Cast intent: rain. The air agreed before the sky did."}"#,
];

// Preset intents for cast_intent actions
static MOCK_CHAT_SUMMARIES: &[&str] = &[
    "They discuss the strange weather and share a laugh about it",
    "One mentions a dream they had; the other nods with quiet recognition",
    "They trade observations about the forest and what they've heard lately",
    "A short exchange about food, and what might be good for dinner",
    "They talk about the river, and whether the fish are biting",
    "One asks the other how they are; the answer is honest and brief",
    "They speak of small things — the quality of the light, the smell of the air",
    "They notice they are both going nowhere in particular, and feel better for it",
];

static MOCK_INTENTS: &[&str] = &[
    "I want the morning light to be gentler on my eyes",
    "Let the air carry the smell of fresh bread",
    "May my steps feel lighter today",
    "I wish for clarity of mind and purpose",
    "Let the river remember my name",
    "I want the wind to bring news from far away",
    "May warmth find those who are cold",
    "Let the shadows keep their secrets a little longer",
];

// All possible action JSON templates the mock can return
fn mock_actions(rng: &mut StdRng) -> &'static str {
    let choices: &[&str] = &[
        r#"{"action":"eat","target":null,"intent":null,"reason":"hungry"}"#,
        r#"{"action":"cook","target":null,"intent":null,"reason":"will make something tasty"}"#,
        r#"{"action":"rest","target":null,"intent":null,"reason":"feeling tired"}"#,
        r#"{"action":"sleep","target":null,"intent":null,"reason":"very tired"}"#,
        r#"{"action":"forage","target":null,"intent":null,"reason":"looking for food"}"#,
        r#"{"action":"fish","target":null,"intent":null,"reason":"want to fish"}"#,
        r#"{"action":"bathe","target":null,"intent":null,"reason":"need to clean up"}"#,
        r#"{"action":"exercise","target":null,"intent":null,"reason":"keeping fit"}"#,
        r#"{"action":"explore","target":null,"intent":null,"reason":"curious about the forest"}"#,
        r#"{"action":"play","target":null,"intent":null,"reason":"want some fun"}"#,
        r#"{"action":"move","target":"Village Square","intent":null,"reason":"going to the square"}"#,
        r#"{"action":"move","target":"Tavern","intent":null,"reason":"heading to the tavern"}"#,
        r#"{"action":"move","target":"Forest","intent":null,"reason":"wandering into the forest"}"#,
        r#"{"action":"move","target":"River","intent":null,"reason":"going to the river"}"#,
        r#"{"action":"move","target":"home","intent":null,"reason":"going home"}"#,
        r#"{"action":"chat","target":"Elara","intent":null,"reason":"want to talk"}"#,
        r#"{"action":"chat","target":"Rowan","intent":null,"reason":"want to talk"}"#,
        r#"{"action":"chat","target":"Thane","intent":null,"reason":"want to talk"}"#,
    ];
    let idx = rng.gen_range(0..choices.len());
    choices[idx]
}

#[async_trait]
impl LlmBackend for MockBackend {
    async fn generate(
        &self,
        prompt:    &str,
        _max_tokens: u32,
        _seed:     Option<u64>,
        _schema:   Option<&serde_json::Value>,
    ) -> Result<String> {
        let mut rng = self.rng.lock().expect("mock rng poisoned");

        // Detect prompt type by content — order matters (most specific first)
        if prompt.contains("This chapter of your life is ending") {
            let choices = [
                "I wish I had spent more time at the river — just sitting, not fishing, not thinking. I want the world to be quieter, slower. Less urgency and more willingness to simply be.",
                "Looking back, I think I was too afraid to say what I wanted out loud. I'd want a world where speaking your desires didn't feel like a risk. More honesty, less pretending.",
                "I regret not talking to the others more. I kept to myself when I didn't need to. Let the world be a little warmer — easier to approach and easier to be approached.",
                "I wanted more magic. Not power — just strangeness, wonder, the sense that the ordinary could shift at any moment. I'd ask the world to stay surprising.",
            ];
            return Ok(choices[rng.gen_range(0..choices.len())].to_string());
        }

        if prompt.contains("Are there changes you would like to see in the world") {
            let choices = [
                "I keep thinking about the forest — how it holds its secrets so carefully. I want more time there, and maybe a little less noise from my own thoughts.",
                "I want to understand magic better. Not just use it, but understand why it answers the way it does. The world could be more forthcoming about such things.",
                "Today I found myself wishing for better company — not because anyone was unkind, but because I think I'm ready for it. I want the world to offer more chances to connect.",
                "I want the river to run cleaner and the food to be more plentiful. Small wishes, but they weigh on me more than I'd like.",
                "I've been thinking about what I'm here for. I don't have an answer yet, and I think I'd like the world to give me a little more time to find one.",
            ];
            return Ok(choices[rng.gen_range(0..choices.len())].to_string());
        }

        if prompt.contains("intend to accomplish today") {
            let choices = [
                "I intend to forage and rest by the river today.",
                "Today I will seek company and perhaps cook a proper meal.",
                "I mean to explore the forest and clear my head.",
                "I want to practice fishing and spend time alone.",
                "Today I'll rest and tend to myself.",
            ];
            return Ok(choices[rng.gen_range(0..choices.len())].to_string());
        }

        if prompt.contains("update your ongoing life story") {
            let choices = [
                "I wandered and foraged, finding small comforts in familiar places. The day passed quietly, as many do.",
                "I spent time near the river, fishing without much luck. The stillness was welcome nonetheless.",
                "I sought company today and found it briefly. The conversation lifted something in me.",
                "I pushed myself too hard and felt it by evening. Rest came as a relief.",
                "Today held small magic — I spoke a wish aloud and felt the world shift slightly.",
            ];
            return Ok(choices[rng.gen_range(0..choices.len())].to_string());
        }

        if prompt.contains("primary_effect") {
            let idx = rng.gen_range(0..MOCK_INTERPRETER_RESPONSES.len());
            return Ok(MOCK_INTERPRETER_RESPONSES[idx].to_string());
        }
        if prompt.contains("brief conversation") {
            let idx = rng.gen_range(0..MOCK_CHAT_SUMMARIES.len());
            return Ok(MOCK_CHAT_SUMMARIES[idx].to_string());
        }
        if prompt.contains("Narrator of Nephara") {
            let idx = rng.gen_range(0..MOCK_NARRATIVES.len());
            return Ok(MOCK_NARRATIVES[idx].to_string());
        }

        // Action prompt — 25% chance of cast_intent
        if rng.gen_ratio(1, 4) {
            let intent_idx = rng.gen_range(0..MOCK_INTENTS.len());
            let intent     = MOCK_INTENTS[intent_idx];
            warn!("MockBackend chose cast_intent");
            return Ok(format!(
                r#"{{"action":"cast_intent","target":null,"intent":"{}","reason":"felt a stirring in the world"}}"#,
                intent
            ));
        }

        let response = mock_actions(&mut rng).to_string();
        Ok(response)
    }
}
