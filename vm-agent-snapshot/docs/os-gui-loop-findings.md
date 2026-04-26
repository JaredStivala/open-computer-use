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

Controller policy run after the BiDi coordinate helper:

- task: Bean to Philosophy Wikipedia first-link walk
- result: succeeded
- path length: 15 clicks, then finish on step 16
- model latency: `0 ms` for every policy step
- XTest input: ~0-5 ms per click
- initial successful run: ~44 s total, mostly due to legacy post-navigation waits
- optimized hot-path run: ~5.46 s total
- verification/title wait after optimization: mostly ~80-180 ms per click
- observed route:
  `Bean -> Genus -> Taxonomic rank -> Taxonomy (biology) -> Biology -> Scientific study -> Scientific theory -> Universe -> Existence -> Reality -> Everything -> Antithesis -> Proposition -> Meaning (philosophy) -> Philosophy of language -> Philosophy`

## Limiting Factor

After exact XTest input, the main bottlenecks are:

1. Browser/page rendering and title-commit latency.
2. Keeping helper-provided coordinates aligned with the rendered GUI.
3. Model calls when no deterministic controller policy applies.

Click delivery and model latency are no longer the limiting factors for the
Wikipedia policy path in the X11 VM.

AT-SPI exposed stale hidden Firefox content after navigation. Firefox WebDriver
BiDi was added as a context helper for visible DOM bounding boxes, while all
actions still execute as GUI clicks through XTest.

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
