use tokio::sync::mpsc;
use tracing::warn;

use crate::log as runlog;
use crate::tui_event::{DayEventKind, TickEntrySnapshot, TuiEvent};
use crate::world::World;

// ---------------------------------------------------------------------------
// Prayer text extraction
// ---------------------------------------------------------------------------

fn extract_prayer_text(action_line: &str) -> Option<String> {
    // Matches: Pray: "some text" or Pray: some text
    let rest = action_line.strip_prefix("Pray:")?;
    let rest = rest.trim();
    if rest.starts_with('"') && rest.ends_with('"') && rest.len() >= 2 {
        Some(rest[1..rest.len() - 1].to_string())
    } else if !rest.is_empty() {
        Some(rest.to_string())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Simulation runner
// ---------------------------------------------------------------------------

pub async fn run_simulation(
    tx:           mpsc::Sender<TuiEvent>,
    mut world:    World,
    total_ticks:  u32,
    seed:         u64,
    backend_name: String,
    souls_dir:    String,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Banner to file
    world.run_log.write_line(&format!(
        "Nephara — seed:{} | {} ticks | backend:{}",
        seed, total_ticks, backend_name
    ));
    world.run_log.write_line(&format!(
        "Agents: {}",
        world.agents.iter().map(|a| a.name()).collect::<Vec<_>>().join(", ")
    ));

    for _t in 0..total_ticks {
        let tick_num = world.tick_num;
        let tpd      = world.config.time.ticks_per_day;
        let day      = tick_num / tpd + 1;
        let tod      = runlog::time_of_day(tick_num % tpd, world.config.time.night_start_tick);

        // Send tick start (fire-and-forget; TUI may be slow)
        let _ = tx.send(TuiEvent::TickStart { tick: tick_num, day, time_of_day: tod }).await;

        let result = world.tick().await?;

        // Send day-boundary events first
        for ev in &result.day_events {
            let tui_ev = match ev.kind {
                DayEventKind::MorningIntention => TuiEvent::MorningIntention {
                    agent_id:   ev.agent_id,
                    agent_name: ev.agent_name.clone(),
                    day:        ev.day,
                    text:       ev.text.clone(),
                },
                DayEventKind::EveningReflection => TuiEvent::EveningReflection {
                    agent_id:   ev.agent_id,
                    agent_name: ev.agent_name.clone(),
                    day:        ev.day,
                    text:       ev.text.clone(),
                },
                DayEventKind::EveningDesire => TuiEvent::EveningDesire {
                    agent_id:   ev.agent_id,
                    agent_name: ev.agent_name.clone(),
                    day:        ev.day,
                    text:       ev.text.clone(),
                },
            };
            let _ = tx.send(tui_ev).await;
        }

        // Send map update
        let cells = world.render_map_cells();
        let _ = tx.send(TuiEvent::MapUpdate(cells)).await;

        // Send needs update
        let needs = world.agent_needs_snapshots();
        let _ = tx.send(TuiEvent::NeedsUpdate(needs)).await;

        // Send per-agent actions
        for entry in &result.entries {
            let prayer_text = if entry.action_line.starts_with("Pray:") {
                extract_prayer_text(&entry.action_line)
            } else {
                None
            };
            let snapshot = TickEntrySnapshot {
                tick:               result.tick,
                day:                result.day,
                agent_id:           entry.agent_id,
                agent_name:         entry.agent_name.clone(),
                location:           entry.location.clone(),
                agent_pos:          entry.agent_pos,
                action_line:        entry.action_line.clone(),
                outcome_line:       entry.outcome_line.clone(),
                outcome_tier_label: entry.outcome_tier_label.clone(),
                prayer_text,
            };
            let _ = tx.send(TuiEvent::AgentAction(snapshot)).await;
        }

        // File logging (tui_mode suppresses stdout)
        let header = runlog::tick_header(result.tick, result.day, result.time_of_day);
        world.run_log.write_line(&header);
        world.run_log.write_line(&result.map);
        for entry in &result.entries {
            for line in entry.format() {
                world.run_log.write_line(&line);
            }
        }
        let footer = runlog::needs_footer(&world.agents);
        world.run_log.write_line(&footer);

        // State dump
        if result.tick > 0 && result.tick % world.config.simulation.state_dump_interval == 0 {
            runlog::write_state_dump(&world.run_log.run_id, result.tick, &world.agents, seed);
        }
    }

    // Final state dump
    runlog::write_state_dump(&world.run_log.run_id, total_ticks, &world.agents, seed);

    // End-of-run desires
    if let Err(e) = world.end_of_run_desires().await {
        warn!("End-of-run desires failed: {}", e);
    }

    // Journal
    let notable_by_agent: Vec<Vec<String>> = world.agents.iter().map(|a| {
        world.notable_events.iter()
            .filter(|(id, _)| *id == a.id)
            .map(|(_, ev)| ev.clone())
            .collect()
    }).collect();

    if !world.is_test_run {
        for (i, agent) in world.agents.iter().enumerate() {
            runlog::append_journal(
                &souls_dir,
                agent.name(),
                &world.run_log.run_id,
                total_ticks,
                &notable_by_agent[i],
            );
        }
    }

    // Summary to file
    let all_notable: Vec<String> = world.notable_events.iter().map(|(_, e)| e.clone()).collect();
    runlog::print_run_summary(
        &world.run_log,
        total_ticks,
        &world.agents,
        world.magic_count,
        &all_notable,
        seed,
    );

    // Send completion event
    let _ = tx.send(TuiEvent::SimulationComplete {
        total_ticks,
        magic_count:    world.magic_count,
        notable_events: all_notable,
    }).await;

    Ok(())
}
