mod action;
mod agent;
mod config;
mod llm;
mod log;
mod magic;
mod soul;
mod world;

use std::sync::Arc;

use clap::Parser;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use llm::{LlmBackend, MockBackend, OllamaBackend};
use log::RunLog;
use world::World;

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
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Logging
    let filter = if cli.verbose {
        EnvFilter::new("debug")
    } else {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .init();

    if let Err(e) = run(cli).await {
        error!("Fatal error: {}", e);
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // --- Config ---
    let mut cfg = config::load(&cli.config)?;
    if let Some(url)   = &cli.llm_url { cfg.llm.ollama_url = url.clone(); }
    if let Some(model) = &cli.model   { cfg.llm.model      = model.clone(); }

    info!("Loaded config from '{}'", cli.config);

    // --- Seed ---
    let seed: u64 = cli.seed.unwrap_or_else(|| {
        rand::thread_rng().gen()
    });
    info!("Simulation seed: {}", seed);

    // Top-level RNG seeded deterministically
    let rng = StdRng::seed_from_u64(seed);

    // Mock backend gets its own seeded RNG derived from main seed
    // so its outputs are also deterministic.
    let mock_rng = StdRng::seed_from_u64(seed.wrapping_add(0xDEAD_BEEF));

    // --- LLM backend ---
    let backend: Arc<dyn LlmBackend> = match cli.llm.as_str() {
        "mock" => {
            info!("Using MockBackend (no LLM required)");
            Arc::new(MockBackend::new(mock_rng))
        }
        "ollama" | _ => {
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

    // --- Smart backend (for planning/reflection/desires) ---
    let smart_backend: Arc<dyn LlmBackend> = match cli.llm.as_str() {
        "mock" => Arc::clone(&backend),
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
    info!("Loaded {} soul seeds: {}", souls.len(), souls.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", "));

    // --- Run log ---
    let run_log = RunLog::new(seed)?;
    info!("Run output: runs/{}/", run_log.run_id);

    // --- World ---
    let mut world = World::new(souls, cfg.clone(), seed, rng, backend, smart_backend, run_log, cli.souls.clone());
    world.load_stories().await;

    let total_ticks = cli.ticks.unwrap_or(cfg.simulation.default_run_ticks);

    // Print banner
    world.run_log.write_line(&format!(
        "Nephara — seed:{} | {} ticks | backend:{}",
        seed, total_ticks, cli.llm
    ));
    world.run_log.write_line(&format!(
        "Agents: {}",
        world.agents.iter().map(|a| a.name()).collect::<Vec<_>>().join(", ")
    ));

    // --- Simulation loop ---
    for _t in 0..total_ticks {
        let result = world.tick().await?;

        // Print tick header + map
        let header = log::tick_header(result.tick, result.day, result.time_of_day);
        world.run_log.write_line(&header);
        world.run_log.write_line(&result.map);

        // Print entries
        for entry in &result.entries {
            for line in entry.format() {
                world.run_log.write_line(&line);
            }
        }

        // Print needs footer
        let footer = log::needs_footer(&world.agents);
        world.run_log.write_line(&footer);

        // State dump
        if result.tick > 0 && result.tick % cfg.simulation.state_dump_interval == 0 {
            log::write_state_dump(&world.run_log.run_id, result.tick, &world.agents, seed);
        }
    }

    // --- Final state dump ---
    log::write_state_dump(&world.run_log.run_id, total_ticks, &world.agents, seed);

    // --- End-of-run desires ---
    if let Err(e) = world.end_of_run_desires().await {
        warn!("End-of-run desires failed: {}", e);
    }

    // --- Journal + summary ---
    let notable_by_agent: Vec<Vec<String>> = world.agents.iter().map(|a| {
        world.notable_events.iter()
            .filter(|(id, _)| *id == a.id)
            .map(|(_, ev)| ev.clone())
            .collect()
    }).collect();

    for (i, agent) in world.agents.iter().enumerate() {
        log::append_journal(
            &cli.souls,
            agent.name(),
            &world.run_log.run_id,
            total_ticks,
            &notable_by_agent[i],
        );
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
