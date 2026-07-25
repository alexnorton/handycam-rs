CARGO ?= $(HOME)/.cargo/bin/cargo
PREFIX ?= /usr/local
DESTDIR ?=
INSTALL ?= install

.PHONY: all clean install rust rust-check

all: rust

rust:
	$(CARGO) build --release --workspace --locked

rust-check:
	$(CARGO) fmt --check
	$(CARGO) clippy --workspace --all-targets --locked -- -D warnings
	$(CARGO) test --workspace --locked
	$(CARGO) build -p handycam-core --target wasm32-unknown-unknown --locked

install: rust
	$(INSTALL) -Dm755 target/release/handycam $(DESTDIR)$(PREFIX)/bin/handycam

clean:
	$(CARGO) clean
