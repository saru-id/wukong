# The house verbs. `make check` before any commit; `make drill` before
# any ship; `make ci` is exactly what CI runs.

.PHONY: check test drill audit ci

check:
	cargo fmt --all --check
	cargo clippy --workspace --all-targets
	cargo nextest run --workspace

test:
	cargo nextest run --workspace

drill:
	bash drills/dotfiles.sh
	bash drills/packages.sh

audit:
	cargo audit

ci: check drill audit
