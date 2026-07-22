.PHONY: build release stage run test fmt clippy check

build:
	python3 tools/build.py build

release:
	python3 tools/build.py build --release

stage:
	python3 tools/build.py stage --release

run:
	python3 tools/build.py run

test:
	python3 tools/host_tests.py

fmt:
	cargo fmt --all -- --check

clippy:
	cargo clippy --locked --all-targets -- -D warnings

check: fmt test build
