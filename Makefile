SHELL := /usr/bin/env bash
.DEFAULT_GOAL := help
.NOTPARALLEL: release version-bump

.PHONY: help version version-bump release build test clippy fmt fmt-check lint package clean install install-hooks

# Calendar version: YYYYMMDD.0.X. The final component increments when more
# than one release is cut on the same day. Override with VERSION=... when
# preparing a historical or otherwise explicit release.
#
# The next version is always strictly higher than BOTH the latest v<today>
# tag AND the version currently in Cargo.toml. Keying off tags alone can
# collide with a version already committed to Cargo.toml (e.g. a release that
# bumped the version but failed before tagging), which would make the bump a
# no-op and silently abort the release. `make release` should always release.
define get_next_version
$(shell \
	TODAY=$$(date +%Y%m%d); \
	TAG_PATCH=$$(git tag -l "v$$TODAY.0.*" 2>/dev/null | sort -V | tail -1 | sed -n 's/.*\.0\.\([0-9]*\)$$/\1/p'); \
	CUR=$$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -1); \
	CUR_PATCH=$$(echo "$$CUR" | sed -n "s/^$$TODAY\.0\.\([0-9]*\)$$/\1/p"); \
	NEXT=0; \
	if [ -n "$$TAG_PATCH" ]; then NEXT=$$((TAG_PATCH + 1)); fi; \
	if [ -n "$$CUR_PATCH" ] && [ "$$CUR_PATCH" -ge "$$NEXT" ]; then NEXT=$$((CUR_PATCH + 1)); fi; \
	echo "$$TODAY.0.$$NEXT"; \
)
endef

VERSION ?= $(get_next_version)
RELEASE_BRANCH := release/v$(VERSION)

help:
	@echo "raft Makefile"
	@echo
	@echo "Usage:"
	@echo "  make version                         Show current and next versions"
	@echo "  make version-bump                    Create a release branch and version commit"
	@echo "  make release                         Bump, validate, merge, tag, and push"
	@echo "  make release VERSION=20260711.0.1    Release an explicit version"
	@echo "  make lint                            Run the same local checks as CI"
	@echo "  make package                         Verify the crates.io package"
	@echo "  make install                         Install the local raft binary"
	@echo
	@echo "Next version: $(VERSION)"

version:
	@CURRENT=$$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -1); \
	echo "Current version: $$CURRENT"; \
	echo "Next version:    $(VERSION)"

# Start from a clean main branch, update Cargo.toml and Cargo.lock, and leave
# the release commit on its own branch for inspection before release.
version-bump:
	@test "$$(git branch --show-current)" = "main" || { echo "error: version-bump must start on main" >&2; exit 1; }
	@test -z "$$(git status --porcelain)" || { echo "error: working tree is not clean" >&2; exit 1; }
	@if git show-ref --verify --quiet "refs/heads/$(RELEASE_BRANCH)"; then echo "error: branch $(RELEASE_BRANCH) already exists" >&2; exit 1; fi
	@echo "Creating $(RELEASE_BRANCH)"
	@git switch -c "$(RELEASE_BRANCH)"
	@sed -i 's/^version = .*/version = "$(VERSION)"/' Cargo.toml
	@cargo check --quiet
	@git add Cargo.toml Cargo.lock
	@git diff --cached --quiet && { echo "error: version $(VERSION) is already current - nothing to release" >&2; git switch main; git branch -D "$(RELEASE_BRANCH)"; exit 1; } || true
	@git commit -m "chore: bump version to $(VERSION)"
	@echo "Created release commit on $(RELEASE_BRANCH)"

# The pushed tag starts .github/workflows/release.yml, which builds release
# archives and publishes raft-kg to crates.io. Publishing is intentionally not
# performed locally.
release: version-bump lint package
	@echo "Merging $(RELEASE_BRANCH) into main"
	@git switch main
	@git merge --no-ff "$(RELEASE_BRANCH)" -m "Merge branch '$(RELEASE_BRANCH)'"
	@git tag -a "v$(VERSION)" -m "Release v$(VERSION)"
	@git push origin main
	@git push origin "v$(VERSION)"
	@echo "Released v$(VERSION); GitHub Actions will publish binaries and raft-kg"

build:
	cargo build --release --locked

test:
	cargo test --all-targets --locked

clippy:
	cargo clippy --all-targets --all-features --locked -- -D warnings

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

lint: fmt-check test clippy

package:
	cargo publish --dry-run --locked

clean:
	cargo clean

install:
	cargo install --path . --locked

install-hooks:
	@mkdir -p .git/hooks
	@printf '#!/usr/bin/env bash\nset -e\nexec make lint\n' > .git/hooks/pre-push
	@chmod +x .git/hooks/pre-push
	@echo "Installed pre-push hook -> make lint"
