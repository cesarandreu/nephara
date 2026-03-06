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

use std::sync::Arc;

use clap::Parser;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use llm::{ClaudeBackend, LlmBackend, MockBackend, OllamaBackend};
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

    /// LLM backend: ollama or mock.
    #[arg(long, default_value = "ollama")]
    llm: String,

    /// Override Ollama URL.
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

    /// Write full LLM prompts and responses to runs/{id}/llm_debug.md
    #[arg(long, default_value_t = false)]
    debug_llm: bool,

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
    if let Some(url)   = &cli.llm_url { cfg.llm.ollama_url = url.clone(); }
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
        _ => {
            info!("Using OllamaBackend — model: {}, url: {}", cfg.llm.model, cfg.llm.ollama_url);
            let ollama = OllamaBackend::new(
                cfg.llm.ollama_url.clone(),
                cfg.llm.model.clone(),
                cfg.llm.temperature,
            );
            if let Err(e) = ollama.health_check().await {
                error!("{}", e);
                std::process::exit(1);
            }
            Arc::new(ollama)
        }
    };

    // --- Smart backend ---
    let smart_backend: Arc<dyn LlmBackend> = match cli.llm.as_str() {
        "mock" | "claude" => Arc::clone(&backend),
        _ => match &cfg.llm.smart_model.clone() {
            Some(model) => {
                let smart = OllamaBackend::new(cfg.llm.ollama_url.clone(), model.clone(), cfg.llm.temperature);
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
    let mut run_log = RunLog::new(seed)?;
    run_log.debug_llm = cli.debug_llm;

    // --- Tracing init (deferred so TUI mode can route to file) ---
    let log_filter = if cli.verbose { "debug" } else { "info" };
    if cli.no_tui {
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_filter)))
            .with_target(true)
            .with_writer(std::io::stderr)
            .init();
    } else {
        let trace_path = format!("runs/{}/trace.log", run_log.run_id);
        if let Ok(file) = std::fs::File::create(&trace_path) {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(EnvFilter::new(log_filter))
                .with_writer(move || file.try_clone().unwrap_or_else(|_| std::fs::File::create("/dev/null").unwrap()))
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

    let total_ticks = cli.ticks.unwrap_or(cfg.simulation.default_run_ticks);

    if cli.no_tui {
        run_streaming(world, total_ticks, seed, &cli.llm, &cli.souls, &cfg).await
    } else {
        run_tui(world, total_ticks, seed, &cli.llm, &cli.souls).await
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

    let agent_count   = world.agents.len();
    let backend_owned = backend_name.to_string();
    let souls_owned   = souls_dir.to_string();

    let (tx, rx) = tokio::sync::mpsc::channel::<tui_event::TuiEvent>(512);

    // Spawn simulation
    let sim_handle = tokio::spawn(sim_runner::run_simulation(
        tx, world, total_ticks, seed, backend_owned.clone(), souls_owned,
    ));

    // Run TUI in a blocking thread (crossterm needs blocking I/O)
    let tui_handle = tokio::task::spawn_blocking(move || {
        let mut app = tui::TuiApp::new(agent_count, total_ticks, seed, backend_owned, roster);
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
    // Print banner
    world.run_log.write_line(&format!(
        "Nephara — seed:{} | {} ticks | backend:{}",
        seed, total_ticks, backend_name
    ));
    world.run_log.write_line(&format!(
        "Agents: {}",
        world.agents.iter().map(|a| a.name()).collect::<Vec<_>>().join(", ")
    ));

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
            log::write_state_dump(&world.run_log.run_id, result.tick, &world.agents, seed);
        }
    }

    log::write_state_dump(&world.run_log.run_id, total_ticks, &world.agents, seed);

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
        for (i, agent) in world.agents.iter().enumerate() {
            log::append_journal(
                souls_dir,
                agent.name(),
                &world.run_log.run_id,
                total_ticks,
                &notable_by_agent[i],
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

    info!("Simulation complete. Seed: {}", seed);
    Ok(())
}
