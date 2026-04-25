CONFIG ?= config/agent.example.toml
GOAL ?= Open Firefox and focus the address bar
DISPLAY_ID ?= :0
SUITE ?= benchmarks/general_suite.toml
MAX_PARALLEL ?= 5

.DEFAULT_GOAL := run

.PHONY: run task view build check smoke-local model-smoke real-smoke vm-setup vm-run busd captured a11yd inputd reasonerd taskd suite uxd verifyd supervise

run:
	./run.sh

task:
	scripts/vm-task.sh "$(GOAL)"

view:
	scripts/vm-view.sh

build:
	cargo build --workspace

check:
	cargo check

smoke-local:
	CONFIG=$(CONFIG) GOAL="$(GOAL)" scripts/smoke-local.sh

model-smoke:
	CONFIG=$(CONFIG) DISPLAY_ID=$(DISPLAY_ID) scripts/model-smoke.sh

real-smoke:
	CONFIG=$(CONFIG) DISPLAY_ID=$(DISPLAY_ID) scripts/linux-real-smoke.sh

vm-setup:
	scripts/multipass-setup.sh

vm-run:
	scripts/vm-run.sh

busd:
	cargo run -p busd -- --config $(CONFIG)

captured:
	cargo run -p captured -- --config $(CONFIG) --display-id $(DISPLAY_ID)

a11yd:
	cargo run -p a11yd -- --config $(CONFIG) --display-id $(DISPLAY_ID)

inputd:
	cargo run -p inputd -- --config $(CONFIG)

reasonerd:
	cargo run -p reasonerd -- --config $(CONFIG)

taskd:
	cargo run -p taskd -- --config $(CONFIG) --goal "$(GOAL)"

suite: build
	target/debug/taskd --config $(CONFIG) --suite $(SUITE) --max-parallel $(MAX_PARALLEL)

uxd:
	cargo run -p uxd -- --config $(CONFIG)

verifyd:
	cargo run -p verifyd -- --config $(CONFIG)

supervise:
	cargo run -p supervisord -- --config $(CONFIG) --goal "$(GOAL)" --display-id $(DISPLAY_ID)
