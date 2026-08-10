# Clean-room implementation modeled on Helix

mina is modeled on the Helix editor (MPL-2.0), but every line of implementation is written from scratch. We study Helix's concepts and type design — anchor/head ranges, OT-style transactions, workspace crate layout — and reimplement them in our own code; we never copy source files. This keeps mina free of MPL-2.0's file-level obligations and forces us to actually understand the design we borrow.
