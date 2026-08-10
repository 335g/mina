# Workspace crate policy

mina is a Cargo workspace from day one: `mina-*` crate names, flat layout (`mina-core/` at the repo root, mirroring Helix). A new crate is created only when it has content and meets at least one split criterion — a new dependency direction (a layer that only looks downward), independent reuse by another consumer, or test/build isolation. Step 1 ships one populated crate, `mina-core`; `mina-view`, `mina-term`, `mina-lsp`, `mina-loader`, `mina-event` are declared future crates, split off when their work begins.
