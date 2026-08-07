# scout — build and install.
#
# Two install paths, sharing one config ($XDG_CONFIG_HOME/scout/):
#
#   Plugin (recommended): in Claude Code, run
#       /plugin marketplace add joshcarter/scout
#       /plugin install scout@scout
#   scripts/ensure-binary.sh then installs the binary into
#   $CLAUDE_PLUGIN_DATA on SessionStart and seeds the default config.
#   Add the CLI on top with `make install-cli` (binary + config only —
#   no MCP registration; the plugin already provides the server).
#
#   Standalone (`make install`, no plugin): binary on PATH, config,
#   plus MCP server registered with Claude Code at user scope. Do not
#   combine with the plugin or the server registers twice.

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

MCP_NAME  := scout
MCP_SCOPE := user

.DEFAULT_GOAL := help
.PHONY: help build test install install-cli install-bin install-config register-mcp uninstall

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

install-cli: install-bin install-config ## Binary + config only — CLI alongside the plugin (no MCP registration)
	@echo
	@echo 'scout CLI installed. (MCP server + hooks come from the plugin.)'

install: install-bin install-config register-mcp ## Standalone: binary + config + user-scope MCP server (skip if using the plugin)
	@echo
	@echo 'scout installed.'
	@case ":$$PATH:" in \
	  *":$(BINDIR):"*) ;; \
	  *) echo "  note: $(BINDIR) is not on your PATH — add it to use \`scout\` directly" ;; \
	esac
	@echo '  note: Claude Code picks up the MCP server on the next session,'
	@echo '        not in any session already running.'

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

# `claude mcp add` writes to config files directly, so no session is
# needed. Remove-then-add keeps this idempotent: plain `add` fails when
# the name is already registered.
register-mcp: ## Register the MCP server with Claude Code (user scope)
	@if ! command -v claude >/dev/null 2>&1; then \
	  echo 'claude CLI not found — register scout yourself with:'; \
	  echo '    claude mcp add $(MCP_NAME) -s $(MCP_SCOPE) -- $(BIN) mcp'; \
	elif claude mcp remove $(MCP_NAME) -s $(MCP_SCOPE) >/dev/null 2>&1 || true && \
	     claude mcp add $(MCP_NAME) -s $(MCP_SCOPE) -- $(BIN) mcp >/dev/null 2>&1; then \
	  echo 'registered MCP server: $(MCP_NAME) ($(MCP_SCOPE) scope) -> $(BIN) mcp'; \
	else \
	  echo 'could not register the MCP server — do it yourself with:'; \
	  echo '    claude mcp add $(MCP_NAME) -s $(MCP_SCOPE) -- $(BIN) mcp'; \
	fi

uninstall: ## Remove the installed binary and MCP registration (keeps your config)
	@rm -f $(BIN) && echo 'removed binary: $(BIN)'
	@if command -v claude >/dev/null 2>&1; then \
	  claude mcp remove $(MCP_NAME) -s $(MCP_SCOPE) >/dev/null 2>&1 \
	    && echo 'unregistered MCP server: $(MCP_NAME)' \
	    || echo 'no $(MCP_SCOPE)-scope MCP registration to remove'; \
	fi
	@echo 'left in place: $(CONFIG)'
