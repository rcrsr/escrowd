# Daily operations for people (APP-LG-4, APP-MONOREPO-STANDARDS §6.1). Code tasks (check, fix,
# build, test) are pnpm scripts; CI and agents never call make. Each recipe is one line.
.DEFAULT_GOAL := help
export PATH := $(shell mise bin-paths 2>/dev/null | tr '\n' ':')$(PATH)

.PHONY: help bootstrap dev dev-stop dev-clean doctor toolchain-lock graph nx-repair codanna

help: ## List targets
	@grep -hE '^[a-z][a-z0-9-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  make %-16s %s\n", $$1, $$2}'
bootstrap: ## Install toolchain and dependencies (idempotent, safe without a terminal)
	@bash scripts/bootstrap.sh
dev: bootstrap ## Run a debug daemon over a scratch project under $XDG_RUNTIME_DIR/escrowd-dev
	@pnpm run dev
dev-stop: ## Stop the dev daemon and unmount its views
	@-pkill -f 'escrow daemon --project $(XDG_RUNTIME_DIR)/escrowd-dev/project'; fusermount3 -uz $(XDG_RUNTIME_DIR)/escrowd-dev/view 2>/dev/null; true
dev-clean: dev-stop ## Remove build outputs, node_modules, .venv and the dev daemon's scratch tree
	@rm -rf target dist node_modules .venv .nx/cache $(XDG_RUNTIME_DIR)/escrowd-dev
doctor: ## Toolchain health, host features and outdated pins
	@bash scripts/doctor.sh
toolchain-lock: ## Resolve mise.lock for every platform
	@mise lock --platform linux-x64,linux-arm64,macos-arm64
graph: ## Nx project graph on port 4211
	@pnpm exec nx graph --port 4211
nx-repair: ## Reset the Nx daemon and cache after a corrupt graph
	@pnpm exec nx reset
codanna: ## Build or refresh the code index
	@codanna index
