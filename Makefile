prog :=download-organiser
debug ?=

ifdef debug
  release :=
  target :=debug
  extension :=debug
else
  release :=--release
  target :=release
  extension :=
endif

.PHONY: nas
nas: install-nas
 
.PHONY: check
check:
	cargo check

.PHONY: build
build:
	cargo build $(release)

.PHONY: build-nas
build-nas:
	cross build $(release) --target aarch64-unknown-linux-gnu

.PHONY: install
install: build
	@echo "exe is at target/$(target)/$(prog)"

.PHONY: install-nas
install-nas: build-nas
	rsync -Phaz target/aarch64-unknown-linux-gnu/$(target)/$(prog) nas:.

.PHONY: all
all: install

.PHONY: help
help:
	@echo "usage: make [install-nas|install] [debug=1]"

.PHONY: clean
clean:
	cargo clean
