# options
ignore_output = &> /dev/null

.PHONY: run-dev test

run-dev:
	@CONFIG_FILE_PATH=./debug/config.toml cargo run

test:
	@cargo test -- --nocapture