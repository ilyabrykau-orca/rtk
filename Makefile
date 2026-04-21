# rtk optimized-build entry points. See docs/superpowers/specs/2026-04-21-rtk-pgo-optimized-build-design.md

.PHONY: release-native release-pgo bench-pgo help

help:
	@echo "rtk build targets:"
	@echo "  release-native   cargo install with target-cpu=native (fast, no PGO)"
	@echo "  release-pgo      full PGO build -> target/release/rtk-pgo"
	@echo "  bench-pgo        hyperfine baseline vs PGO binary"

release-native:
	RUSTFLAGS="-C target-cpu=native" cargo install --path . --profile release-native --force

release-pgo:
	./scripts/build-pgo.sh

bench-pgo:
	./scripts/bench-pgo.sh
