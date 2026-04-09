.PHONY: check test lint ci

CLIPPY_FLAGS = --all-targets --all-features -- -D warnings \
	-A dead_code \
	-A clippy::manual_is_multiple_of \
	-A clippy::collapsible_if \
	-A clippy::useless_conversion \
	-A clippy::useless_format \
	-A clippy::collapsible_match \
	-A clippy::useless_vec

check:
	cargo check

test:
	cargo test

lint:
	cargo clippy $(CLIPPY_FLAGS)

ci: check test lint
