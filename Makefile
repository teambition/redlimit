# options
ignore_output = &> /dev/null

.PHONY: run-dev test build

run-dev:
	@CONFIG_FILE_PATH=./debug/config.toml cargo run

test:
	@cargo test -- --nocapture

build:
	@rustup target add x86_64-unknown-linux-gnu
	@rustup target add aarch64-unknown-linux-gnu
	@cargo build --target x86_64-unknown-linux-gnu --release
	@cargo build --target aarch64-unknown-linux-gnu --release
