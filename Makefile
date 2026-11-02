.PHONY: format lint features docs test ui snapshots quality

format:
	cargo fmt --all -- --check

lint:
	cargo clippy --workspace --all-targets --all-features -- -D warnings
	cargo clippy -p openapi-axum --no-default-features -- -D warnings

# Every feature alone and none at all (needs cargo-hack).
features:
	cargo hack test -p openapi-axum --each-feature
	cargo hack clippy -p openapi-axum --each-feature --all-targets -- -D warnings

docs:
	RUSTDOCFLAGS="-D warnings" cargo doc -p openapi-axum --no-deps --all-features

test:
	cargo test --workspace --all-targets --all-features
	cargo test -p openapi-axum --doc --all-features

# Refresh compile-error snapshots after changing a diagnostic.
ui:
	TRYBUILD=overwrite cargo test -p openapi-axum --test ui --all-features

# Refresh the golden kitchen-sink spec after an intended doc change.
snapshots:
	UPDATE_SNAPSHOTS=1 cargo test -p openapi-axum --test conformance --all-features

quality: format lint features docs test
