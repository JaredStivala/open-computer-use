# General-Purpose OS-Level Computer Use Agent

This repository is a production-oriented runtime scaffold for a Linux-only GUI agent that operates at the OS layer on Ubuntu 22.04/X11.

Implemented in this pass:

- Multi-process architecture over Unix domain sockets with msgpack payloads
- Broker daemon for fan-out across capture, accessibility, verifier, and supervisors
- Shared protocol for capture, accessibility, input, reasoning, verification, and supervisor
- Event-driven verifier contract and action lifecycle
- Verifier arming handshake so verification is subscribed before input injection
- Streaming reasoning client with early tool-call dispatch
- Linux X11 capture backend using XDamage, XShm, and a libturbojpeg worker encoder
- Linux AT-SPI2 event backend with bounded startup snapshot and incremental node updates
- Linux `/dev/uinput` keyboard/pointer backend
- Supervisor that can run a scripted task path before model-driven control
- Xephyr task runner for independent goals with per-task socket namespaces
- UX daemon for GTK/GNOME animation suppression with restore-on-exit
- Linux-only subsystem boundaries isolated behind `cfg(target_os = "linux")`

Not executable on this macOS host:

- X11 `XShm` + `XDamage` capture path
- AT-SPI2 subscriptions
- `/dev/uinput` injection
- Xephyr isolated displays

Those Linux-only integrations type-check for `x86_64-unknown-linux-gnu`, but they still need runtime benchmarking on the target Ubuntu/X11 VM.

## Workspace layout

- `crates/agent-proto`: shared message schema
- `crates/agent-common`: config, Unix socket IPC, logging helpers
- `apps/captured`: capture daemon
- `apps/a11yd`: accessibility daemon
- `apps/inputd`: uinput daemon
- `apps/reasonerd`: Groq/Grok-compatible reasoning daemon
- `apps/smokectl`: Linux real-smoke verifier/input client
- `apps/verifyd`: event-driven verifier
- `apps/supervisord`: task orchestrator
- `apps/busd`: Unix socket event broker
- `apps/uxd`: animation suppression/restoration daemon
- `apps/taskd`: Xephyr-based independent-goal runner
- `benchmarks/general_suite.toml`: 20-task benchmark suite definition
- `scripts/ubuntu-setup.sh`: Ubuntu/X11 dependency and uinput setup
- `scripts/smoke-local.sh`: local no-model smoke runner
- `scripts/model-smoke.sh`: Groq-backed reasoning smoke runner
- `scripts/linux-real-smoke.sh`: real Linux X11/uinput/XDamage smoke test
- `scripts/multipass-setup.sh`: macOS-to-Ubuntu VM provisioning and smoke test
- `tools/x11_pixel_probe.c`: tiny X11 app that flips pixels on click

## Build

Fastest path from this macOS host:

```bash
./run.sh
```

That single command provisions or refreshes the Ubuntu 22.04 VM, syncs this repo and `.env`, installs dependencies if needed, then runs both the real OS-level smoke and the Groq-backed model smoke.

```bash
make build
make check
```

On Linux, install the platform deps needed for the subsystem implementations you complete next:

- X11 dev headers (`libx11-dev`, `libxext-dev`, `libxdamage-dev`, `libxcomposite-dev`)
- AT-SPI2 / D-Bus deps (`libatspi2.0-dev`, `dbus`)
- uinput access (`uinput` kernel module, appropriate group/udev permissions)
- `libjpeg-turbo`
- `xserver-xephyr`

On Ubuntu/X11, `scripts/ubuntu-setup.sh` installs these and configures `/dev/uinput` group access.

## Config

Copy `config/agent.example.toml` to `config/agent.toml` if you want local overrides. The runtime also loads `.env` automatically for model credentials.

Primary env vars:

- `AGENT_MODEL_PROVIDER=groq`
- `AGENT_MODEL_BASE_URL=https://api.groq.com/openai/v1`
- `AGENT_MODEL_NAME=meta-llama/llama-4-scout-17b-16e-instruct`
- `GROQ_API_KEY=...`

If you want actual xAI Grok instead, swap the base URL and auth env vars behind the same `ModelClient` interface.

## Run

Recommended one-command test:

```bash
./run.sh
```

Give the agent a task:

```bash
scripts/vm-task.sh "open up twitter"
```

Task runs start an Ubuntu X11 desktop on display `:99` and open a Chromium browser by default so web tasks have a visible GUI surface. For native-only tasks, disable the browser bootstrap with `START_BROWSER=0`.

Watch the VM display:

```bash
scripts/vm-view.sh
```

On macOS this opens Screen Sharing/VNC for the Ubuntu display `:99`. If prompted, use VNC password `agent`.

No-model smoke loop:

```bash
make smoke-local
```

Groq-backed model smoke loop:

```bash
make model-smoke
```

From macOS, provision the Ubuntu 22.04 VM and run the real Linux smoke test:

```bash
make vm-setup
```

On Linux/X11, run the real capture/input/verifier smoke directly:

```bash
make real-smoke CONFIG=config/agent.example.toml DISPLAY_ID=:99
```

Single supervised goal on the active X display:

```bash
make build
target/debug/busd --config config/agent.example.toml &
target/debug/captured --config config/agent.example.toml --display-id :0 &
target/debug/a11yd --config config/agent.example.toml --display-id :0 &
target/debug/inputd --config config/agent.example.toml &
target/debug/verifyd --config config/agent.example.toml &
target/debug/reasonerd --config config/agent.example.toml &
target/debug/supervisord --config config/agent.example.toml --goal "Open Firefox and focus the address bar"
```

Parallel benchmark suite through Xephyr:

```bash
make suite MAX_PARALLEL=5
```

## Benchmark order

1. `captured`: hit X11 capture targets before touching anything else.
2. `inputd`: measure click-to-pixel latency with an external probe task.
3. `a11yd`: validate tree fetch and delta latency.
4. `supervisord --scripted-task`: close a no-model loop.
5. `reasonerd`: single-step, five-app benchmark.
6. `verifyd`: full multi-step tasks with retries.
7. animation suppression
8. Xephyr task-level parallelism
9. hardening and observability
