# Getting Started

Nephara is a text-based world simulation. Seven AI agents live in a small village, perceiving their surroundings and choosing actions driven by needs, personality, and a freeform magic system inspired by Kabbalah. You don't play a character — you run the simulation and watch emergent stories unfold, like directing a theater troupe of autonomous minds. You can influence the world in limited ways (see [Ways to nudge the world](#ways-to-nudge-the-world) below), but the agents decide what to do.

For the full design spec, see [spec/world-sim-mvp-spec.md](../spec/world-sim-mvp-spec.md).

## Requirements

- [Nix](https://nixos.org/) with flakes enabled (provides Rust toolchain and all dependencies)
- For live LLM runs: an LLM backend — llama.cpp, Ollama, the `llm` CLI, or a Claude API key

Mock mode needs nothing beyond Nix. No network, no GPU, no API key.

## Your first run

Enter the dev shell and start a mock run:

```sh
nix develop
cargo run -- --llm mock
```

This launches a fully deterministic simulation with no LLM required. The default run length is controlled by `default_run_ticks` in `config/world.toml` (currently 96 ticks, about two in-game days).

Useful flags for a first session:

```sh
cargo run -- --llm mock --seed 42           # reproducible output
cargo run -- --llm mock --ticks 48 --seed 42  # shorter run (one in-game day)
```

## What you see: the TUI

By default Nephara launches a fullscreen terminal UI with three panels:

- **Map** (left) — a grid showing agent positions and locations
- **Event log** (right) — a scrolling narrative of each tick's actions and outcomes
- **Needs bars** (bottom) — hunger, energy, fun, social, and hygiene for every agent

Press **`?`** at any time for the in-app keybinding overlay.

## TUI controls

| Key | Action |
|-----|--------|
| `q` | Quit (asks for confirmation) |
| `Tab` | Switch between World view and Agent Detail |
| `j` / `k` | Scroll event log down/up (or cycle agents in Agent Detail) |
| `[` / `]` | Jump to previous / next tick |
| `Space` | Expand the selected log entry |
| `G` | Resume auto-scroll (follow latest events) |
| `l` | Toggle tile legend on the map |
| `1` – `5` | Inspect / jump to a specific agent |
| `?` | Toggle keybinding help overlay |
| `d` | Toggle LLM debug log |
| `p` | Pause / resume the simulation |
| `+` / `-` | Increase / decrease tick speed |
| `g` | Speak as God (send a message to agents) |

## Streaming mode

If you prefer plain scrolling output — or need to pipe logs — pass `--no-tui`:

```sh
cargo run -- --llm mock --no-tui
```

This writes the tick narrative to stdout, which is useful for scripting, `grep`, or redirecting to a file.

## Using a real LLM

Mock mode is great for exploring the interface, but the real magic happens when agents are driven by a language model. Nephara supports several backends; pick whichever you have available:

| Backend | Flag | What it talks to |
|---------|------|------------------|
| llama.cpp (default) | `--llm llamacpp` | OpenAI-compatible server at `localhost:8080` |
| Ollama | `--llm ollama` | Ollama at `localhost:11434` |
| `llm` CLI (preferred) | `--llm llm` | Simon Willison's [`llm`](https://llm.datasette.io/) tool — hundreds of models |
| Claude API | `--llm claude` | Anthropic API (needs `ANTHROPIC_API_KEY`) |
| Claude CLI | `--llm claude-cli` | `claude` CLI tool |

Override the model or URL with `--model <NAME>` and `--llm-url <URL>`. See the [README](../README.md) for detailed examples of each backend.

If you are using a rate-limited API (e.g. Gemini free tier via `llm`), set `rate_limit_rpm` in `config/world.toml` to stay under the limit.

## After a run

Each run creates a timestamped directory under `runs/` containing:

| File | Contents |
|------|----------|
| `tick_log.txt` | Full tick-by-tick narrative |
| `summary.md` | Human-readable run summary (needs, magic cast, notable events, wall time) |
| `state_dump.json` | Latest world snapshot (overwritten periodically) |
| `introspection.md` | Agent desire/intention summaries each tick |
| `trace.log` | Debug trace output (always written in TUI mode) |

Agent-side files in `souls/` are also updated:

- `*.chronicle.md` — new entries appended after each run
- `*.state.md` — consolidated agent state (needs, memory) persisted between runs

## Ways to nudge the world

You are an observer by default, but Nephara gives you two channels for influence.

### Oracle messages (file-based)

Write any text to `souls/<name>.oracle_responses.md`. The next time that agent visits the Temple, they receive your words as an Oracle reading — a private LLM reflection. The file is cleared after delivery and the response is archived to their chronicle.

### Speak as God (TUI)

Press **`g`** in the TUI to open the God overlay. Type a message, choose a target (`0` for all agents, `1`–`5` for a specific agent), and press **Enter** to send. The message is injected into the agent's next perception prompt. Press **Esc** to cancel.

## Tuning the simulation

All parameters — need decay rates, action difficulty classes, restoration amounts, tick counts, LLM settings — live in [`config/world.toml`](../config/world.toml). Edit and re-run; no recompilation needed.

## Adding new agents

Use the summoning script to generate the ritual prompt:

```sh
bash scripts/summon.sh
```

Paste the output into Claude Opus 4.6. The model responds with a complete soul seed. Review it (verify attribute scores sum to 30), then save to `souls/<name>.seed.md`. See [`rituals/summoning.md`](../rituals/summoning.md) for the Archwizard's full notes.
