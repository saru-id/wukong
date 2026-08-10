# The house verbs. `make check` before any commit; `make drill` before
# any ship; `make ci` is exactly what CI runs.

.PHONY: check test drill audit man ci

check:
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings
	cargo nextest run --workspace

test:
	cargo nextest run --workspace

drill:
	bash drills/dotfiles.sh
	bash drills/packages.sh
	bash drills/settings.sh
	bash drills/shared.sh
	bash drills/dayone.sh

audit:
	cargo audit

man:
	cargo run -q -p wukong -- gen-man target/man
	@echo "preview with: man target/man/wukong.1"

ci: check drill audit
