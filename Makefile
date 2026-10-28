# at-qmi-bridge build helpers.
TARGET   ?= aarch64-unknown-linux-musl
DIST     ?= dist/aarch64-linux-musl
STICK    ?= root@stick
BIN      := at-qmi-bridge

.PHONY: all build release test fmt fmt-check clippy check arm64 arm64-docker deploy clean help

all: check test release

help:
	@echo "build         debug build for the host"
	@echo "release       optimized build for the host"
	@echo "test          run unit tests"
	@echo "fmt           format the code"
	@echo "check         fmt-check + clippy"
	@echo "arm64         static arm64 musl build with the local toolchain (needs: rustup target add $(TARGET))"
	@echo "arm64-docker  same, inside rust:1-alpine (run on an arm64 host); output in $(DIST)"
	@echo "deploy        scp the arm64 binary and OpenWrt files to $(STICK) (override STICK=)"
	@echo "clean         remove target/ and $(DIST)"

build:
	cargo build

release:
	cargo build --release

test:
	cargo test --release

fmt:
	cargo fmt

fmt-check:
	cargo fmt -- --check

clippy:
	cargo clippy --all-targets

check: fmt-check clippy

arm64:
	cargo build --release --target $(TARGET)
	mkdir -p $(DIST)
	cp target/$(TARGET)/release/$(BIN) $(DIST)/$(BIN)

arm64-docker:
	mkdir -p $(DIST)
	docker run --rm -v "$(CURDIR)":/src -w /src rust:1-alpine sh -c \
	  'apk add --no-cache musl-dev && cargo build --release --target-dir /tmp/target && cp /tmp/target/release/$(BIN) $(DIST)/$(BIN)'

deploy: arm64
	scp -O $(DIST)/$(BIN) $(STICK):/usr/sbin/$(BIN).new
	scp -O openwrt/$(BIN).init $(STICK):/etc/init.d/$(BIN)
	ssh $(STICK) 'chmod 755 /usr/sbin/$(BIN).new /etc/init.d/$(BIN) && mv /usr/sbin/$(BIN).new /usr/sbin/$(BIN) && /etc/init.d/$(BIN) restart'

clean:
	cargo clean
	rm -rf $(DIST)
