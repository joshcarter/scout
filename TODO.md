# CLI vs. MCP

When invoked in a terminal, scout should print human-readable output
with color. Output of `grep` should be similar to `ack`

Note: one place where we don't want to mimic `ack` is with giant
JSON files that completely fill the terminal window. Need some
mechanism for not returning those unless absolutely necessary. Ditto
context pollution when used as a MCP server.

# Claude guidance to use appropriate tools

Make sure Claude gets what it needs to know about scout's features,
and guidance to favor using them. Builds, unit test runs, etc. should
be going through scout with some consistency.
