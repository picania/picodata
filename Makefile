.PHONY: default fmt lint test check fat

default: ;

install-cargo:
	curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |\
		sh -s -- -y --profile default --default-toolchain 1.61.0

centos7-cmake3:
	[ -f /usr/bin/cmake ] && sudo rm /usr/bin/cmake
	sudo ln -s /usr/bin/cmake3 /usr/bin/cmake

tarantool-patch:
	cd tarantool-sys && \
	echo "${VER_TNT}" > VERSION

build: tarantool-patch
	. ~/.cargo/env && \
	cargo build --locked

build-release: tarantool-patch
	. ~/.cargo/env && \
		cargo build --release

install:
	mkdir -p $(DESTDIR)/usr/bin
	install -m 0755 target/debug/picodata $(DESTDIR)/usr/bin/picodata

fmt:
	cargo fmt
	pipenv run fmt

lint:
	cargo fmt --check
	cargo check
	cargo clippy -- --deny clippy::all
	pipenv run lint

test:
	cargo test
	pipenv run pytest

check:
	@$(MAKE) lint --no-print-directory
	@$(MAKE) test --no-print-directory

fat:
	@$(MAKE) fmt --no-print-directory
	@$(MAKE) lint --no-print-directory
	@$(MAKE) test --no-print-directory
