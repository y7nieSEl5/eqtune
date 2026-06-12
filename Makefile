# Build-from-source install for eqtune — no code signing or Apple account needed.
.PHONY: build install uninstall test clean

build:
	cargo build --release

# Build, then install + load the LaunchAgent daemon.
# On the first `eqtune on`, macOS asks for audio-capture permission — grant it.
install: build
	./target/release/eqtune install
	@echo
	@echo "Optional: put the CLI on your PATH:"
	@echo "  ln -sf \"$$HOME/Library/Application Support/eqtune/eqtune\" /usr/local/bin/eqtune"

uninstall:
	-./target/release/eqtune uninstall

test:
	cargo test

clean:
	cargo clean
