# scout — build and install.
#
# Two surfaces, one shared config ($XDG_CONFIG_HOME/scout/):
#
#   Plugin (Claude Code, Grok Build): the payload lives in plugins/scout/ and
#   carries the binary at plugins/scout/bin/scout. `make build` puts it there,
#   so the MCP server — declared as ${CLAUDE_PLUGIN_ROOT}/bin/scout, which both
#   harnesses expand — can spawn with nothing bootstrapped first. Install with
#       /plugin marketplace add joshcarter/scout   (Claude Code)
#       /plugin install scout@scout
#       grok plugin marketplace add <path> && grok plugin install scout --trust
#   Nothing here registers an MCP server; the harness does that.
#
#   Terminal: `make install` puts the binary on $PREFIX/bin. Independent of the
#   plugin — run both, or either. (Other MCP clients: point them at
#   `scout mcp` yourself.) The binary seeds its own default config on first
#   run, so there is no install-config step.
#
# Every binary write goes through a temp file and `mv`. Overwriting in place
# fails with ETXTBSY ("Text file busy") whenever an MCP server is running from
# the destination — which, for the plugin payload under Claude's directory
# marketplace, is the common case, not the edge case. Rename is atomic and the
# running process keeps its old inode until it exits.

PREFIX ?= $(HOME)/.local
BINDIR := $(PREFIX)/bin
BIN    := $(BINDIR)/scout

# The plugin payload's copy of the binary. Gitignored; `make build` refreshes
# it so a dev checkout is always a working plugin.
PLUGIN_BIN := plugins/scout/bin/scout

# Same resolution as the binary and hooks: XDG_CONFIG_HOME, empty = unset.
ifeq ($(strip $(XDG_CONFIG_HOME)),)
XDG_CONFIG_HOME := $(HOME)/.config
endif
CONFIG_DIR := $(XDG_CONFIG_HOME)/scout
CONFIG     := $(CONFIG_DIR)/config.toml

.DEFAULT_GOAL := help
.PHONY: help build test install install-bin uninstall

help: ## Print available targets
	@echo 'scout — make targets:'
	@echo
	@grep -hE '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
	  | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-14s\033[0m %s\n", $$1, $$2}'
	@echo
	@echo 'Variables:'
	@echo '  PREFIX=$(PREFIX)  (binary goes to $$PREFIX/bin/scout)'

build: ## Build the release binary and refresh the plugin payload's copy
	cargo build --release
	@mkdir -p $(dir $(PLUGIN_BIN))
	@cp target/release/scout $(PLUGIN_BIN).tmp
	@chmod 0755 $(PLUGIN_BIN).tmp
	@mv -f $(PLUGIN_BIN).tmp $(PLUGIN_BIN)
	@echo 'plugin payload binary: $(PLUGIN_BIN)'

test: ## Run the test suite
	cargo test

install: install-bin ## Install the CLI: binary to $PREFIX/bin
	@echo
	@echo 'scout CLI installed.'
	@case ":$$PATH:" in \
	  *":$(BINDIR):"*) ;; \
	  *) echo "  note: $(BINDIR) is not on your PATH — add it to use \`scout\` directly" ;; \
	esac
	@echo '  note: scout writes a default $(CONFIG) on first run —'
	@echo '        edit [llm].endpoint and [llm].model to match your LLM host.'
	@echo '  note: in a coding agent, install the plugin — it carries the MCP'
	@echo '        server and hooks, which this does not.'

install-bin: build ## Install the binary to $PREFIX/bin
	@mkdir -p $(BINDIR)
	@cp target/release/scout $(BIN).tmp
	@chmod 0755 $(BIN).tmp
	@mv -f $(BIN).tmp $(BIN)
	@echo 'installed binary: $(BIN)'

uninstall: ## Remove the installed CLI binary (keeps your config)
	@rm -f $(BIN) && echo 'removed binary: $(BIN)'
	@echo 'left in place: $(CONFIG)'
	@echo '  note: the plugin carries its own binary in plugins/scout/bin —'
	@echo '        uninstall the plugin in your agent to remove that copy.'
