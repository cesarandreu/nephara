mod action;
mod agent;
mod bench;
mod color;
mod config;
mod llm;
mod log;
mod magic;
mod sim_runner;
mod soul;
mod tui;
mod tui_event;
mod world;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex};

use clap::Parser;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use llm::{ClaudeBackend, ClaudeCliBackend, LlmBackend, MockBackend, OllamaBackend, OpenAICompatBackend};
use log::RunLog;
use world::World;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Bench subcommand types
// ---------------------------------------------------------------------------

#[derive(clap::Subcommand, Debug)]
enum Commands {
    /// Benchmark one or more Ollama models across all prompt types.
    Bench(BenchArgs),
}

#[derive(Parser, Debug)]
struct BenchArgs {
    /// Comma-separated list of Ollama model names to benchmark.
    #[arg(long)]
    models: String,

    /// Number of samples per prompt type per model.
    #[arg(long, default_value_t = 3)]
    samples: usize,

    /// Ollama base URL.
    #[arg(long, default_value = "http://localhost:11434")]
    ollama_url: String,

    /// Write results to this JSON file (optional).
    #[arg(long)]
    output: Option<String>,
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(name = "nephara", about = "Nephara — AI World Simulation")]
struct Cli {
    /// Number of ticks to simulate (default: from config).
    #[arg(long)]
    ticks: Option<u32>,

    /// LLM backend: llamacpp, ollama, claude, mock.
    #[arg(long, default_value = "llamacpp")]
    llm: String,

    /// Override LLM backend URL (applies to llamacpp and ollama).
    #[arg(long)]
    llm_url: Option<String>,

    /// Override model name.
    #[arg(long)]
    model: Option<String>,

    /// Config file path.
    #[arg(long, default_value = "config/world.toml")]
    config: String,

    /// Soul seeds directory.
    #[arg(long, default_value = "souls")]
    souls: String,

    /// Deterministic seed. If omitted, a random seed is generated and logged.
    #[arg(long)]
    seed: Option<u64>,

    /// Enable debug logging.
    #[arg(long)]
    verbose: bool,

    /// Use streaming terminal output instead of the fullscreen TUI.
    #[arg(long)]
    no_tui: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    if let Err(e) = run(cli).await {
        eprintln!("Fatal error: {}", e);
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // --- Bench subcommand (early exit, no config/souls needed) ---
    if let Some(Commands::Bench(args)) = cli.command {
        let models = args.models.split(',').map(|s| s.trim().to_string()).collect();
        return bench::run_bench(bench::BenchConfig {
            models,
            samples:    args.samples,
            ollama_url: args.ollama_url,
            output:     args.output,
        }).await;
    }

    // --- Config ---
    let mut cfg = config::load(&cli.config)?;
    for warning in config::validate(&cfg) {
        warn!("Config warning: {}", warning);
    }
    if let Some(url) = &cli.llm_url {
        cfg.llm.ollama_url   = url.clone();
        cfg.llm.llamacpp_url = url.clone();
    }
    if let Some(model) = &cli.model   { cfg.llm.model      = model.clone(); }

    info!("Loaded config from '{}'", cli.config);

    // --- Seed ---
    let seed: u64 = cli.seed.unwrap_or_else(|| rand::thread_rng().gen());
    info!("Simulation seed: {}", seed);

    let rng      = StdRng::seed_from_u64(seed);
    let mock_rng = StdRng::seed_from_u64(seed.wrapping_add(0xDEAD_BEEF));

    // --- LLM backend ---
    let backend: Arc<dyn LlmBackend> = match cli.llm.as_str() {
        "mock" => {
            info!("Using MockBackend (no LLM required)");
            Arc::new(MockBackend::new(mock_rng))
        }
        "claude" => {
            let model = cli.model.as_deref()
                .unwrap_or("claude-haiku-4-5-20251001")
                .to_string();
            info!("Using ClaudeBackend — model: {}", model);
            Arc::new(ClaudeBackend::new(model).map_err(|e| { error!("{}", e); e })?)
        }
        "claude-cli" => {
            let model = cli.model.as_deref()
                .unwrap_or("claude-haiku-4-5-20251001")
                .to_string();
            info!("Using ClaudeCliBackend — model: {}", model);
            Arc::new(ClaudeCliBackend::new(model))
        }
        "ollama" => {
            info!("Using OllamaBackend — model: {}, url: {}", cfg.llm.model, cfg.llm.ollama_url);
            let ollama = OllamaBackend::new(
                cfg.llm.ollama_url.clone(),
                cfg.llm.model.clone(),
                cfg.llm.temperature,
                cfg.llm.think,
                cfg.llm.thinking_budget_chars,
            );
            if let Err(e) = ollama.health_check().await {
                error!("{}", e);
                std::process::exit(1);
            }
            Arc::new(ollama)
        }
        _ => {
            // "llamacpp" | "openai" | anything else → OpenAI-compatible backend
            info!("Using OpenAICompatBackend — model: {}, url: {}", cfg.llm.model, cfg.llm.llamacpp_url);
            let llamacpp = OpenAICompatBackend::new(
                cfg.llm.llamacpp_url.clone(),
                cfg.llm.model.clone(),
                cfg.llm.temperature,
                cfg.llm.think,
                cfg.llm.thinking_budget_chars,
            );
            llamacpp.health_check().await;  // warn but don't abort
            Arc::new(llamacpp)
        }
    };

    // --- Smart backend ---
    let smart_backend: Arc<dyn LlmBackend> = match cli.llm.as_str() {
        "mock" | "claude" | "claude-cli" | "llamacpp" | "openai" => Arc::clone(&backend),
        _ => match &cfg.llm.smart_model.clone() {
            Some(model) => {
                let smart_url = cfg.llm.smart_ollama_url.clone()
                    .unwrap_or_else(|| cfg.llm.ollama_url.clone());
                let smart = OllamaBackend::new(smart_url, model.clone(), cfg.llm.temperature, cfg.llm.think, cfg.llm.thinking_budget_chars);
                match smart.health_check().await {
                    Ok(_)  => {
                        info!("Smart model '{}' available for planning/reflection/desires", model);
                        Arc::new(smart) as Arc<dyn LlmBackend>
                    }
                    Err(e) => {
                        warn!("Smart model '{}' unavailable: {}. Falling back to main model.", model, e);
                        Arc::clone(&backend)
                    }
                }
            }
            None => Arc::clone(&backend),
        }
    };

    // --- Soul seeds ---
    let souls = soul::load_all(&cli.souls)?;
    info!("Loaded {} soul seeds: {}", souls.len(),
        souls.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", "));

    // --- Run log ---
    let run_log = RunLog::new(seed)?;

    // --- Tracing init (deferred so TUI mode can route to file) ---
    let log_filter = if cli.verbose { "debug" } else { "info" };
    let trace_path = format!("runs/{}/trace.log", run_log.run_id);
    if cli.no_tui {
        if let Ok(file) = std::fs::File::create(&trace_path) {
            use tracing_subscriber::prelude::*;
            let file_layer = tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_writer(std::sync::Mutex::new(file));
            let stderr_layer = tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_writer(std::io::stderr);
            let _ = tracing_subscriber::registry()
                .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_filter)))
                .with(file_layer)
                .with(stderr_layer)
                .try_init();
        } else {
            tracing_subscriber::fmt()
                .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_filter)))
                .with_target(true)
                .with_writer(std::io::stderr)
                .init();
        }
    } else {
        if let Ok(file) = std::fs::File::create(&trace_path) {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(EnvFilter::new(log_filter))
                .with_writer(std::sync::Mutex::new(file))
                .try_init();
        }
    }

    info!("Run output: runs/{}/", run_log.run_id);

    // --- World ---
    let is_test_run = cli.llm == "mock";
    let mut world = World::new(
        souls, cfg.clone(), seed, rng,
        backend, smart_backend, run_log, cli.souls.clone(), is_test_run,
    )?;
    world.load_stories().await;
    world.summarize_journal_memories().await;

    let total_ticks = cli.ticks.unwrap_or(cfg.simulation.default_run_ticks);

    if cli.no_tui {
        run_streaming(world, total_ticks, seed, &cli.llm, &cli.souls, &cfg).await
    } else {
        let god_queue = Arc::new(Mutex::new(VecDeque::new()));
        run_tui(world, total_ticks, seed, &cli.llm, &cli.souls, &cfg, god_queue).await
    }
}

// ---------------------------------------------------------------------------
// TUI mode
// ---------------------------------------------------------------------------

async fn run_tui(
    mut world:    World,
    total_ticks:  u32,
    seed:         u64,
    backend_name: &str,
    souls_dir:    &str,
    cfg:          &config::Config,
    god_queue:    Arc<Mutex<VecDeque<tui_event::GodMessage>>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Suppress stdout from run_log in TUI mode
    world.run_log.tui_mode = true;

    // Build roster for TUI (name + color)
    let roster: Vec<(String, ratatui::style::Color)> = world.agents.iter()
        .map(|a| (
            a.name().to_string(),
            color::to_ratatui_color(color::agent_color(a.id)),
        ))
        .collect();

    let agent_count       = world.agents.len();
    let backend_owned     = backend_name.to_string();
    let model_owned       = world.config.llm.model.clone();
    let souls_owned       = souls_dir.to_string();
    let ticks_per_day     = world.config.time.ticks_per_day;
    let night_start_tick  = world.config.time.night_start_tick;
    let god_name_owned    = world.config.world.god_name.clone();

    // A1: pause/resume + tick speed atomics
    let paused       = Arc::new(AtomicBool::new(false));
    let tick_delay   = Arc::new(AtomicU64::new(cfg.simulation.tick_delay_ms));

    let (tx, rx) = tokio::sync::mpsc::channel::<tui_event::TuiEvent>(512);

    // Wire up TUI streaming (FEAT-13)
    world.tui_tx = Some(tx.clone());

    let god_queue_sim = Arc::clone(&god_queue);
    let god_queue_tui = god_queue;

    // Spawn simulation
    let sim_handle = tokio::spawn(sim_runner::run_simulation(
        tx, world, total_ticks, seed, backend_owned.clone(), souls_owned,
        Arc::clone(&paused), Arc::clone(&tick_delay), god_queue_sim,
    ));

    // Run TUI in a blocking thread (crossterm needs blocking I/O)
    let tui_handle = tokio::task::spawn_blocking(move || {
        let mut app = tui::TuiApp::new(agent_count, total_ticks, ticks_per_day, night_start_tick, seed, backend_owned, model_owned, roster, god_name_owned, paused, tick_delay, god_queue_tui);
        app.run(rx)
    });

    // Wait for both
    if let Err(e) = sim_handle.await? {
        error!("Simulation error: {}", e);
    }
    if let Err(e) = tui_handle.await? {
        error!("TUI error: {}", e);
    }

    info!("Run complete. Seed: {}", seed);
    Ok(())
}

// ---------------------------------------------------------------------------
// Streaming (--no-tui) mode — original behavior
// ---------------------------------------------------------------------------

async fn run_streaming(
    mut world:    World,
    total_ticks:  u32,
    seed:         u64,
    backend_name: &str,
    souls_dir:    &str,
    cfg:          &config::Config,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Enable token streaming to stdout (FEAT-13)
    world.token_echo = true;

    // Print banner
    world.run_log.write_line(&format!(
        "Nephara — seed:{} | {} ticks | backend:{} | model:{}",
        seed, total_ticks, backend_name, world.config.llm.model
    ));
    world.run_log.write_line(&format!(
        "Agents: {}",
        world.agents.iter().map(|a| a.name()).collect::<Vec<_>>().join(", ")
    ));

    let initial_needs: Vec<(String, crate::agent::Needs)> = world.agents.iter()
        .map(|a| (a.name().to_string(), a.needs.clone()))
        .collect();
    let run_start = std::time::Instant::now();

    for _t in 0..total_ticks {
        let result = world.tick().await?;

        let header = log::tick_header(result.tick, result.day, result.time_of_day);
        world.run_log.write_line(&header);
        world.run_log.write_line(&result.map);

        for entry in &result.entries {
            for line in entry.format() {
                world.run_log.write_line(&line);
            }
        }

        let footer = log::needs_footer(&world.agents);
        world.run_log.write_line(&footer);

        if result.tick > 0 && result.tick % cfg.simulation.state_dump_interval == 0 {
            log::write_state_dump(&world.run_log.run_id, &world.agents, seed);
        }
    }

    log::write_state_dump(&world.run_log.run_id, &world.agents, seed);

    if let Err(e) = world.end_of_run_desires().await {
        warn!("End-of-run desires failed: {}", e);
    }

    let notable_by_agent: Vec<Vec<String>> = world.agents.iter().map(|a| {
        world.notable_events.iter()
            .filter(|(id, _)| *id == a.id)
            .map(|(_, ev)| ev.clone())
            .collect()
    }).collect();

    if !world.is_test_run {
        let run_id = world.run_log.run_id.clone();
        for (i, agent) in world.agents.iter().enumerate() {
            let journal_content = if notable_by_agent[i].is_empty() {
                "A quiet run. Nothing of great note occurred.".to_string()
            } else {
                notable_by_agent[i].iter().map(|e| format!("- {}", e)).collect::<Vec<_>>().join("\n")
            };
            let day = total_ticks / cfg.time.ticks_per_day + 1;
            let tod = log::time_of_day(total_ticks % cfg.time.ticks_per_day, cfg.time.night_start_tick);
            log::append_chronicle(
                souls_dir, agent.name(), &run_id, day, total_ticks, tod, "journal", &journal_content,
            );
            log::save_state(
                souls_dir, agent.name(), &run_id,
                &agent.life_story, &agent.attributes, &agent.attribute_xp,
                &agent.affinity, &agent.beliefs, &agent.inventory,
            );
        }
    }

    let all_notable: Vec<String> = world.notable_events.iter().map(|(_, e)| e.clone()).collect();
    log::print_run_summary(
        &world.run_log,
        total_ticks,
        &world.agents,
        world.magic_count,
        &all_notable,
        seed,
    );

    // Post-run summary markdown (FEAT-11)
    let run_duration_ms = run_start.elapsed().as_millis() as u64;
    let llm_url = match backend_name {
        "ollama" => cfg.llm.ollama_url.clone(),
        _        => cfg.llm.llamacpp_url.clone(),
    };
    log::write_run_summary(
        &world.run_log.run_id,
        seed,
        total_ticks,
        &world.agents,
        &initial_needs,
        world.magic_count,
        &all_notable,
        run_duration_ms,
        world.is_test_run,
        backend_name,
        &cfg.llm.model,
        cfg.llm.smart_model.as_deref(),
        &llm_url,
    );

    info!("Simulation complete. Seed: {}", seed);
    Ok(())
}
