# scout — build and install.
#
# Two surfaces, one shared config ($XDG_CONFIG_HOME/scout/):
#
#   Claude Code: the plugin, and only the plugin. In Claude Code, run
#       /plugin marketplace add joshcarter/scout
#       /plugin install scout@scout
#   That brings the MCP server, the hooks, and binary bootstrap in one
#   piece — scripts/ensure-binary.sh installs the binary into
#   $CLAUDE_PLUGIN_DATA on SessionStart and seeds the default config.
#   Nothing here registers an MCP server; there is no `make` path into
#   Claude Code.
#
#   Terminal: `make install` puts the binary on $PREFIX/bin and seeds
#   the same config. Independent of the plugin — run both, or either.
#   (Other MCP clients: point them at `scout mcp` yourself.)

PREFIX ?= $(HOME)/.local
BINDIR := $(PREFIX)/bin
BIN    := $(BINDIR)/scout

# Same resolution as the binary and hooks: XDG_CONFIG_HOME, empty = unset.
ifeq ($(strip $(XDG_CONFIG_HOME)),)
XDG_CONFIG_HOME := $(HOME)/.config
endif
CONFIG_DIR := $(XDG_CONFIG_HOME)/scout
CONFIG     := $(CONFIG_DIR)/config.toml
CONFIG_SRC := config.example.toml

.DEFAULT_GOAL := help
.PHONY: help build test install install-bin install-config uninstall

help: ## Print available targets
	@echo 'scout — make targets:'
	@echo
	@grep -hE '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
	  | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-14s\033[0m %s\n", $$1, $$2}'
	@echo
	@echo 'Variables:'
	@echo '  PREFIX=$(PREFIX)  (binary goes to $$PREFIX/bin/scout)'

build: ## Build the release binary
	cargo build --release

test: ## Run the test suite
	cargo test

install: install-bin install-config ## Install the CLI: binary to $PREFIX/bin, config if missing
	@echo
	@echo 'scout CLI installed.'
	@case ":$$PATH:" in \
	  *":$(BINDIR):"*) ;; \
	  *) echo "  note: $(BINDIR) is not on your PATH — add it to use \`scout\` directly" ;; \
	esac
	@echo '  note: in Claude Code, install the plugin — it carries the MCP'
	@echo '        server and hooks, which this does not.'

install-bin: build ## Install the binary to $PREFIX/bin
	@mkdir -p $(BINDIR)
	@install -m 0755 target/release/scout $(BIN)
	@echo 'installed binary: $(BIN)'

install-config: ## Install the default config, never overwriting an existing one
	@mkdir -p $(CONFIG_DIR)
	@if [ -f "$(CONFIG)" ]; then \
	  echo 'config already exists, left untouched: $(CONFIG)'; \
	else \
	  install -m 0644 $(CONFIG_SRC) "$(CONFIG)"; \
	  echo 'installed default config: $(CONFIG)'; \
	  echo '  edit [llm].endpoint and [llm].model to match your local LLM host'; \
	fi

uninstall: ## Remove the installed CLI binary (keeps your config)
	@rm -f $(BIN) && echo 'removed binary: $(BIN)'
	@echo 'left in place: $(CONFIG)'
	@echo '  note: the plugin manages its own binary copy — remove it with'
	@echo '        /plugin uninstall scout@scout in Claude Code.'
