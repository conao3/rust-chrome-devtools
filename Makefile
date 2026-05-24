.PHONY: fmt
fmt:
	cargo fmt --all

.PHONY: fmt-check
fmt-check:
	cargo fmt --all -- --check

.PHONY: lint
lint:
	cargo clippy --all-targets -- -D warnings

.PHONY: test
test:
	cargo test
