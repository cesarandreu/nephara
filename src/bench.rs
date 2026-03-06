use std::io::Write;
use std::time::Instant;

use serde::Serialize;

use crate::action::{build_action_schema, parse_response, Action};
use crate::llm::{LlmBackend, OllamaBackend};
use crate::magic::parse_interpreter_response;

// ---------------------------------------------------------------------------
// Config / result types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct BenchConfig {
    pub models:     Vec<String>,
    pub samples:    usize,
    pub ollama_url: String,
    pub output:     Option<String>,
}

#[derive(Debug, Serialize)]
pub struct BenchResults {
    pub models: Vec<ModelResult>,
}

#[derive(Debug, Serialize)]
pub struct ModelResult {
    pub model:        String,
    pub prompt_types: Vec<PromptResult>,
}

#[derive(Debug, Serialize)]
pub struct PromptResult {
    pub name:           String,
    pub samples:        usize,
    pub parse_success:  usize,
    pub parse_rate:     f64,
    pub avg_latency_ms: f64,
    pub avg_chars:      f64,
}

// ---------------------------------------------------------------------------
// Representative prompts (one per call type used in the simulation)
// ---------------------------------------------------------------------------

static ACTION_PROMPT: &str = r#"You are an agent named Elara in a village simulation. You are feeling hungry.
Choose your next action. Respond with ONLY valid JSON, no other text:
{"action": "eat", "target": null, "intent": null, "reason": "brief reason", "description": "brief description"}
Valid actions: eat, cook, sleep, rest, forage, fish, exercise, bathe, explore, play, wander, chat, move, cast_intent, pray"#;

static NARRATOR_PROMPT: &str = r#"You are the DM Narrator of Nephara. Write 2-3 vivid sentences describing this outcome:
AGENT: Elara  ACTION: Forage  OUTCOME: Success  LOCATION: Forest
Be specific and evocative. No lists. No meta-commentary."#;

static INTERPRETER_PROMPT: &str = r#"You are the Interpreter of Intent in the world of Nephara.
SPEAKER: Elara  NUMEN: 5  LOCATION: Village Square
THE SPOKEN INTENT: "I want warmth to find me"
Respond with ONLY a JSON object:
{"primary_effect": "...", "interpretations": ["...", "..."], "secondary_effect": "...", "duration_ticks": 2, "need_changes": {"fun": 10, "energy": -8}, "memory_entry": "..."}"#;

static PLANNING_PROMPT: &str = r#"You are Elara, an agent in the village of Nephara. What do you intend to accomplish today? Write 1-2 sentences. Be personal and specific."#;

// ---------------------------------------------------------------------------
// Core benchmark runner
// ---------------------------------------------------------------------------

pub async fn run_bench(
    config: BenchConfig,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("Nephara Benchmark");
    println!("Ollama: {}  Samples per type: {}", config.ollama_url, config.samples);
    println!("Models: {}", config.models.join(", "));
    println!();

    let action_schema = build_action_schema(&[
        "eat", "cook", "sleep", "rest", "forage", "fish", "exercise",
        "bathe", "explore", "play", "wander", "chat", "move", "cast_intent", "pray",
    ]);

    let mut all_results: Vec<ModelResult> = Vec::new();

    for model in &config.models {
        println!("── {} ──", model);
        let backend = OllamaBackend::new(config.ollama_url.clone(), model.clone(), 0.7);

        let action_res = bench_prompt_type(
            &backend, ACTION_PROMPT, 150, Some(&action_schema), config.samples, "action",
            |s| { let (a, _, _) = parse_response(s); !matches!(a, Action::Wander) },
        ).await;

        let narrator_res = bench_prompt_type(
            &backend, NARRATOR_PROMPT, 120, None, config.samples, "narrative",
            |s| s.split_whitespace().count() >= 5,
        ).await;

        let interp_res = bench_prompt_type(
            &backend, INTERPRETER_PROMPT, 200, None, config.samples, "interpreter",
            |s| parse_interpreter_response(s).is_some(),
        ).await;

        let plan_res = bench_prompt_type(
            &backend, PLANNING_PROMPT, 100, None, config.samples, "planning",
            |s| s.split_whitespace().count() >= 5,
        ).await;

        for pr in &[&action_res, &narrator_res, &interp_res, &plan_res] {
            println!(
                "  {:<12}  parse: {:>3.0}%  lat: {:>6.0}ms  chars: {:>5.0}",
                pr.name, pr.parse_rate * 100.0, pr.avg_latency_ms, pr.avg_chars,
            );
        }
        println!();

        all_results.push(ModelResult {
            model:        model.clone(),
            prompt_types: vec![action_res, narrator_res, interp_res, plan_res],
        });
    }

    let results = BenchResults { models: all_results };

    if let Some(ref path) = config.output {
        let json = serde_json::to_string_pretty(&results)?;
        std::fs::write(path, &json)?;
        println!("Results saved to {}", path);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Per-prompt-type runner
// ---------------------------------------------------------------------------

async fn bench_prompt_type(
    backend:   &OllamaBackend,
    prompt:    &str,
    max_tokens: u32,
    schema:    Option<&serde_json::Value>,
    samples:   usize,
    name:      &str,
    parse_fn:  impl Fn(&str) -> bool,
) -> PromptResult {
    let mut total_ms    = 0.0f64;
    let mut total_chars = 0.0f64;
    let mut parse_ok    = 0usize;

    print!("  {:<12} ", name);
    let _ = std::io::stdout().flush();

    for _ in 0..samples {
        let start  = Instant::now();
        let result = backend.generate(prompt, max_tokens, None, schema).await;
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        total_ms += elapsed;

        match result {
            Ok(text) => {
                total_chars += text.len() as f64;
                if parse_fn(&text) {
                    parse_ok += 1;
                    print!("✓");
                } else {
                    print!("✗");
                }
            }
            Err(_) => print!("E"),
        }
        let _ = std::io::stdout().flush();
    }
    println!();

    let n = samples.max(1) as f64;
    PromptResult {
        name:           name.to_string(),
        samples,
        parse_success:  parse_ok,
        parse_rate:     parse_ok as f64 / n,
        avg_latency_ms: total_ms   / n,
        avg_chars:      total_chars / n,
    }
}
