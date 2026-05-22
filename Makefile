.PHONY: build release test clean

BIN_DIR := target/x86_64-pc-windows-gnu/release
BIN := $(BIN_DIR)/qscn.exe

build:
	cargo build

release:
	cargo build --release

test:
	cargo test

clean:
	cargo clean
