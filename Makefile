# options
ignore_output = &> /dev/null

.PHONY: run-dev

run-dev:
	@CONFIG_FILE_PATH=./debug/config.toml cargo run
