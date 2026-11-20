# durable-quickstart

The smallest durable host: one backend, one adapter, two turns. It uses no model and no
network; the adapter commits one fixed answer so the durable loop is the only thing on screen.

```bash
# file store
cargo run -p durable-quickstart -- ./durable-quickstart
cargo run -p durable-quickstart -- ./durable-quickstart

# the same code on SQLite
cargo run -p durable-quickstart -- ./durable-sqlite --sqlite
```

The first run admits `run-1`, settles it and prints the committed turn. The second run finds
`run-1` in the store, admits `run-2` and settles that too. Delete the directory and the next run
starts from an empty session again.
