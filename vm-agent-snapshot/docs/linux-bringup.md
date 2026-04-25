# Linux Bring-Up

This repository currently separates the runtime contract from the Linux-native implementation work. Use this sequence on Ubuntu 22.04/X11.

## 1. Host prep

- Use Xorg, not Wayland.
- Ensure `/tmp/agent` exists and is writable.
- Load the `uinput` module.
- Run `at-spi-bus-launcher` or confirm the desktop session already provides AT-SPI2.
- Install `Xephyr` for independent parallel task displays.

Or run:

```bash
scripts/ubuntu-setup.sh
```

From this macOS host, use Multipass to provision Ubuntu 22.04 and run the real X11/uinput smoke test end-to-end:

```bash
scripts/multipass-setup.sh
```

## 2. Config

```bash
cp config/agent.example.toml config/agent.toml
export AGENT_MODEL_PROVIDER=groq
export AGENT_MODEL_BASE_URL=https://api.groq.com/openai/v1
export AGENT_MODEL_NAME=<vision-capable model>
export GROQ_API_KEY=...
```

If using xAI instead:

```bash
export AGENT_MODEL_PROVIDER=xai
export AGENT_MODEL_BASE_URL=https://api.x.ai/v1
export XAI_API_KEY=...
```

## 3. Subsystem completion work

### `captured`

The Linux backend is in [apps/captured/src/linux.rs](/Users/jaredstivala/Documents/Codex/2026-04-24/build-spec-general-purpose-os-level/apps/captured/src/linux.rs). It uses:

- `XOpenDisplay`
- `XShmCreateImage` and shared memory attach
- `XDamageCreate` on the root target
- background JPEG quality 75 encoding via `libturbojpeg`

Acceptance gate:

- p50 damage-to-frame-ready under 5ms
- p99 under 20ms

### `a11yd`

The Linux backend is in [apps/a11yd/src/linux.rs](/Users/jaredstivala/Documents/Codex/2026-04-24/build-spec-general-purpose-os-level/apps/a11yd/src/linux.rs). It uses:

- D-Bus session connection through AT-SPI2
- registry event registration for object, focus, and window signals
- signal mapping for `children-changed`, `state-changed`, `text-changed`, `focus`, and `window-activate`
- bounded startup snapshot and incremental in-memory tree mutation

Acceptance gate:

- active-window snapshot under 30ms
- delta processing under 10ms

### `inputd`

The Linux backend is in [apps/inputd/src/linux.rs](/Users/jaredstivala/Documents/Codex/2026-04-24/build-spec-general-purpose-os-level/apps/inputd/src/linux.rs). It implements:

- `/dev/uinput` virtual keyboard and pointer setup
- absolute and relative pointer motion
- button events, scroll events, key up/down, modifiers, drag
- monotonic timestamping at inject completion

Acceptance gate:

- click-to-visible-pixel-change under 50ms on the measurement app

## 4. Runtime order

Start each daemon in its own process:

```bash
make captured
make a11yd
make inputd
make uxd
make reasonerd
make verifyd
make supervise GOAL="Open Firefox and focus the address bar"
```

For scripted smoke tests without the model:

```bash
make smoke-local
```

For a real Linux smoke test that starts a dummy Xorg display, opens a pixel-flipping X11 probe, injects a click through `/dev/uinput`, observes XDamage capture, and verifies the event-driven loop:

```bash
make real-smoke CONFIG=config/agent.example.toml DISPLAY_ID=:99
```

For a Groq-backed reasoning smoke test:

```bash
make model-smoke CONFIG=config/agent.example.toml DISPLAY_ID=:99
```

For independent parallel goals through Xephyr:

```bash
cargo build
cargo run -p taskd -- --config config/agent.toml --goal "draft email" --goal "check calendar"
```

For the full 20-task suite through up to five isolated displays:

```bash
make suite CONFIG=config/agent.toml MAX_PARALLEL=5
```

The general benchmark suite definition is [benchmarks/general_suite.toml](/Users/jaredstivala/Documents/Codex/2026-04-24/build-spec-general-purpose-os-level/benchmarks/general_suite.toml).

## 5. Immediate next engineering work

- Run target-VM benchmarks and tune timeouts/action classes against observed latency.
- Add dashboards and long-run failure taxonomy.
