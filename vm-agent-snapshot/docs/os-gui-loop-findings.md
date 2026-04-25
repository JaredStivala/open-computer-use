# OS GUI Loop Findings

Date: 2026-04-25

Target: `computer-use-agent:99` in the Ubuntu VM.

## Current Fast Path

The VM agent now has a GUI-first fast path:

- framebuffer capture through XDamage/XShm
- exact X11 input through XTest for pointer clicks
- short framebuffer-change verification in the supervisor
- live AT-SPI fallback snapping for click coordinates
- no verified noops for model/parser failures

## Measured Latency

Recent model-driven task steps:

- `model_ms`: ~2200-4800 ms
- `input_ms`: ~0-3 ms
- `verification_ms`: ~370-470 ms

No-model OS probe on Wikipedia:

- cold scene observation: ~660-700 ms
- warm scene observation: ~450-800 ms, with occasional ~1400 ms during page load
- XTest click injection: ~0.8-2.6 ms
- title-change detection after click: ~3-62 ms

## Limiting Factor

After exact XTest input, the main bottlenecks are:

1. Model calls in the current supervisor loop.
2. Scene readiness/extraction from AT-SPI after page transitions.
3. Task-rule filtering over GUI elements.

Click delivery is no longer the limiting factor in the X11 VM.

## Best Next Method

Use a controller-first architecture:

1. Ask the model for strategy only when needed.
2. Let an OS-level controller execute deterministic GUI micro-policies.
3. Maintain a resident scene graph instead of walking AT-SPI from scratch.
4. Wait for action-specific state changes:
   - input accepted
   - framebuffer damage
   - active title/focus change
   - useful scene graph generation
5. Fall back to the model only when the controller cannot resolve the next action.

For the Wikipedia benchmark, the controller should own:

- visible article-link extraction
- first-valid-link filtering
- exact click injection
- navigation/state-change wait
- loop/progress detection

The model should not be called for every link click.
