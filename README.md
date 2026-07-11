# raft

A personal knowledge graph that grows out of what you already have:
markdown notes, project repos, and git history. Named for what you call
a group of ducks on water.

Install the `raft` executable from crates.io:

```
cargo install raft-kg
```

Point it at directories, it builds the graph:

```toml
# ~/.config/raft/config.toml
[[sources]]
path = "~/notes"
kind = "notes"

[[sources]]
path = "~/dev"
kind = "projects"
```

```
raft init            # write default config
raft index           # scan sources, rebuild the index
raft search <term>   # full-text search across notes
raft about <name>    # everything known about a project or entity
```

Notes are the source of truth; the index (`~/.local/share/raft/raft.db`)
is derived and disposable. Every edge carries provenance: `human`
(you wrote the link), `indexer` (deterministic match), or `agent`
(proposed by an LLM, with rationale).

## Agents

`raft serve` speaks MCP over stdio, exposing `search`, `about`,
`dangling`, `connect`, and `reindex` as tools. Register it and any
MCP-capable agent can walk your graph and refresh it after writing
notes:

```
# Claude Code
claude mcp add raft -- /path/to/raft serve

# claux (config.toml)
[mcp_servers.raft]
command = "/path/to/raft"
args = ["serve"]
```
