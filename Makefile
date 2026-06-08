.NOTPARALLEL:
.PHONY: test test-rust test-go proto-lint proto-generate proto-check

test: proto-check test-rust test-go

test-rust:
	cargo test

test-go:
	cd sdk/go && go test -v ./...

proto-lint:
	buf lint

proto-generate:
	./scripts/proto-generate.sh

proto-check: proto-lint proto-generate
	git diff --exit-code -- proto sdk/go/proto
