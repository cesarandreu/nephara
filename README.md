# Nephara

A text-based world simulation where AI agents (small local LLMs via Ollama) inhabit a shared village, perceive their surroundings, and take actions driven by needs, personality, and a Kabbalistic freeform magic system.

Seven entities — Elara, Rowan, Thane, Mira, Sael, Kael, and Lyra — live out their days in a tick-based loop. Each tick an agent perceives the world, decides on an action, and the world resolves it with d20 rolls and narrative. Spells always succeed, but words carry all their meanings.

**New here?** See the [Getting Started guide](docs/getting-started.md).

## Requirements

- [Nix](https://nixos.org/) with flakes enabled
- For live runs: [Ollama](https://ollama.com/) (included in the dev shell)

## Quick Start

Enter the dev shell:

```sh
nix develop
```

### Mock run (no LLM required)

Fully deterministic, no Ollama needed:

```sh
cargo run -- --llm mock
```

With a fixed seed for reproducible output:

```sh
cargo run -- --llm mock --seed 42
```

Short test run:

```sh
cargo run -- --llm mock --ticks 48 --seed 42
```

### Live run (llama.cpp — default backend)

The default backend is `llamacpp`, which expects an OpenAI-compatible server (e.g., llama.cpp) at `http://localhost:8080`. Start your server, then:

```sh
cargo run
```

Override the model or URL:

```sh
cargo run -- --model my-model-name
cargo run -- --llm-url http://other-host:8080
```

### Live run (Ollama)

Ollama is still fully supported via `--llm ollama`:

```sh
ollama serve
ollama pull gemma3:4b
cargo run -- --llm ollama
```

Override the model or Ollama URL:

```sh
cargo run -- --llm ollama --model gemma3:12b
cargo run -- --llm ollama --llm-url http://other-host:11434
```

### Live run (llm CLI — preferred)

The `llm` backend shells out to Simon Willison's [`llm`](https://llm.datasette.io/) CLI tool, which supports hundreds of models (Gemini, OpenAI, Anthropic, local via plugins) through a single interface. This is the preferred backend for live runs.

Install it and add a model key, then:

```sh
cargo run -- --llm llm
cargo run -- --llm llm --model gemini-2.0-flash:free
cargo run -- --llm llm --model gpt-4o-mini
```

For free-tier APIs (e.g. Gemini free tier), set `rate_limit_rpm` in `config/world.toml` to stay under the rate limit:

```toml
rate_limit_rpm = 15   # requests per minute; 0 = unlimited
```

### Live run (Claude CLI)

Shells out to the `claude` CLI for each agent turn. Requires the Claude CLI to be installed and authenticated:

```sh
cargo run -- --llm claude-cli
cargo run -- --llm claude-cli --model claude-sonnet-4-6
```

## CLI Reference

```
nephara [OPTIONS]

Options:
  --ticks <N>       Ticks to simulate (default: 96, i.e. 2 in-game days)
  --llm <BACKEND>   LLM backend: llamacpp (default), llm, ollama, claude, claude-cli, mock
  --llm-url <URL>   Override backend URL (default: http://localhost:8080 for llamacpp, http://localhost:11434 for ollama)
  --model <MODEL>   Override model name (default: gemma3:4b)
  --config <PATH>   Config file (default: config/world.toml)
  --souls <DIR>     Soul seeds directory (default: souls/)
  --seed <N>        Deterministic seed (random if omitted, logged at startup)
  --no-tui          Use streaming terminal output instead of fullscreen TUI
  --verbose         Enable debug logging

nephara bench [OPTIONS]

  --models <m1,m2,...>   Comma-separated model names to benchmark
  --samples <N>          Prompts per model per type (default: 5)
  --llm-url <URL>        Override backend URL
  --output <FILE>        Write JSON report (default: stdout)
```

## TUI Mode

By default, Nephara runs in a fullscreen terminal UI (ratatui). The screen is split into three panels: a live map (left), a tick event log (right), and a needs bar for each agent (bottom).

Key bindings:

| Key | Action |
|-----|--------|
| `j` / `k` | Scroll event log down / up |
| `[` / `]` | Jump to previous / next tick |
| `Space` | Expand selected entry |
| `q` | Quit |

Use `--no-tui` for the original streaming output (useful for piping or scripting).

## Output

Each run creates a directory under `runs/` containing:

- `tick_log.txt` — the full scrolling narrative
- `summary.md` — human-readable run summary (agent needs, magic cast, notable events, wall time)
- `state_dump.json` — latest world snapshot (overwritten every N ticks, configurable)
- `introspection.md` — agent desire/intention summaries each tick

Agent chronicles are appended to `souls/*.chronicle.md` after each run. Consolidated agent state (needs, memory, inventory) is persisted to `souls/*.state.md` and loaded at startup.

## Configuration

All tunable parameters live in `config/world.toml` — need decay rates, action DCs, restoration amounts, tick counts, LLM settings. No recompilation needed to tweak values.

## Development Commands

These commands run inside the `nix develop` shell, which provides `cargo` and all dependencies. Non-NixOS users with Rust installed can run `cargo` directly, but must also install `pkg-config` and OpenSSL dev headers (`libssl-dev` on Debian/Ubuntu, `openssl-devel` on Fedora).

```sh
# Build
cargo build

# Run tests (no LLM or network required — MockBackend only)
cargo test

# Check for warnings/errors without producing a binary
cargo check

# Format code
cargo fmt

# Lint
cargo clippy

# Verbose logging (debug level)
cargo run -- --llm mock --verbose
```

### Log Categories

`RUST_LOG` filters against named targets. The following categories are available:

| Target | What it covers |
|--------|----------------|
| `llm` | Ollama health check, every request (model, tokens, prompt chars), every raw response |
| `action` | Raw LLM response per agent, parsed action, d20 roll details, outcome tier |
| `magic` | Interpreter prompt built, raw Interpreter response, parsed InterpretedIntent |
| `narrate` | GM Narrator prompt sent, raw narrative response |

Examples:

```sh
# General info + all LLM traffic
RUST_LOG=info,llm=debug cargo run -- --llm mock --ticks 6 --seed 42

# Only d20 rolls and action parsing
RUST_LOG=off,action=debug cargo run -- --llm mock --ticks 6 --seed 42

# Full firehose
RUST_LOG=debug cargo run -- --llm mock --ticks 6 --seed 42

# Live run: confirm Ollama is ready, then watch LLM round-trips
RUST_LOG=info,llm=debug cargo run -- --ticks 6
```

## Adding New Agents

Use the summoning script to generate the ritual prompt:

```sh
bash scripts/summon.sh
```

Copy the output and paste it into Claude Opus 4.6. The model will respond with a complete soul seed. Review it (verify attribute scores sum to 30), then save it to `souls/<name>.seed.md`. See `rituals/summoning.md` for full Archwizard's notes.

## Interacting with Agents

To send a message to an agent, write any text to `souls/<name>.oracle_responses.md`. The next time the agent is at the Temple, they will receive it as an Oracle reading (a private LLM reflection), the file is cleared, and the response is archived to their `souls/<name>.chronicle.md`.

## Project Structure

```
src/
  main.rs        CLI, initialization, run loop
  world.rs       World struct, 32×32 grid, tick cycle, GM Narrator, prompts
  agent.rs       Agent struct, needs, attributes, memory buffer, beliefs
  action.rs      Action enum, d20 resolution, outcome tiers
  magic.rs       Cast Intent flow, Interpreter prompt, response parsing
  llm.rs         LlmBackend trait; OllamaBackend, OpenAICompatBackend (llamacpp),
                 ClaudeBackend, ClaudeCliBackend, MockBackend
  config.rs      TOML deserialization into typed config struct
  soul.rs        Soul seed parser (YAML frontmatter + markdown body)
  log.rs         Tick log formatting, chronicle/state writing, state dumps
  color.rs       ANSI + ratatui color mappings
  bench.rs       Benchmark subcommand (nephara bench)
  sim_runner.rs  Extracted tick loop for TUI mode; sends TuiEvents over mpsc
  tui.rs         Fullscreen TUI (ratatui); map, event log, needs bar panels
  tui_event.rs   Pure data types for TUI events

souls/         Entity definitions (*.seed.md), chronicles (*.chronicle.md),
               state files (*.state.md), oracle responses (*.oracle_responses.md)
config/        world.toml — all tunable world parameters
spec/          Full design specification
rituals/       The summoning prompt used to create the founding entities
runs/          Simulation output (gitignored)
```

## Determinism

Given the same `--seed`, `--ticks`, and `--llm mock`, the output is byte-for-byte identical. Useful for regression testing:

```sh
cargo run -- --llm mock --ticks 144 --seed 42 > out1.txt
cargo run -- --llm mock --ticks 144 --seed 42 > out2.txt
diff out1.txt out2.txt   # empty — identical
```

Live Ollama runs are deterministic on the same model version and hardware (seed is passed to Ollama's generate options with `temperature: 0`).
