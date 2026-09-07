# mina Context

mina is a terminal editor/viewer under construction: a UI-agnostic text-editing core, with a resident Daemon owning editor state. The terminal UI (minae) and the headless session CLI for agents are both Clients of that Daemon.

## Language

**Document**:
The unit of editable text. Holds the text content and is the target of edits.
_Avoid_: Buffer

**Selection**:
The set of active cursor ranges. A single cursor is a Selection with one range.
_Avoid_: cursors, multi-cursor

**Range**:
One element of a Selection: a span of text with an anchor (fixed end) and a head (moving end). The head extends the range during selection operations.
_Avoid_: cursor, span

**Transaction**:
The unit of change applied to a Document. Designed so it can be inverted to support undo.
_Avoid_: edit, change, operation

**UndoGroup**:
The unit of undo/redo: a set of Transactions reverted together by one undo. One Insert session is one UndoGroup — it opens on entering Insert mode and closes on leaving Insert (to Normal or Select) or when the owning Client disconnects. A group is owned by exactly one Client (the one that opened it); a write from any other Client closes it first, so a single UndoGroup never contains edits from different Clients.
_Avoid_: change, edit

**Dirty**:
A Document that has been edited since it was last saved to its Path. Cleared by a successful Save only if the document text is unchanged since the save began — an edit landing while the save is writing leaves the flag set, so unsaved edits are never reported as saved. Scoped to the Document, not the View — saving one Document must not clear another's flag.
_Avoid_: unsaved, modified

**Reload**:
Replacing a Document's text with the current contents of its Path after an external change to the file. A Transaction, so undoable; it never changes the Document's identity (ID, Path, LSP association) and clears Dirty, since the text again matches disk.
_Avoid_: refresh, follow

**Deleted**:
A Document whose Path no longer exists on disk, held open pending a Client's acknowledgement (Close). Saving recreates the file and clears the state.
_Avoid_: missing, removed

**Mode**:
The active editing mode — Normal, Insert, or Select. Determines which Keymap resolves key presses.
_Avoid_: state

**View**:
One display region (one split) showing a Document with its own Selection and viewport position. Multiple Views can show the same Document.
_Avoid_: pane, window

**WorkspaceRoot**:
The smallest analysis unit containing an opened file — the nearest directory that carries a project marker (a language's root markers or a recognized manifest, with `.git` as a universal marker), falling back to the file's parent directory. The scope unit of an LSP session. Nearest marker wins, so a Rust workspace member's `Cargo.toml` beats the outer workspace's.
_Avoid_: project root, crate root

**LanguageServer**:
A language analysis process (rust-analyzer, …) spawned by the Daemon per WorkspaceRoot and served over LSP — the provider of Diagnostics, InlayHints, definitions, and Rename/References. Configured by the Languages config; mina ships vetted defaults in its embedded table and treats user-added servers as unvetted (LSP-standard features still work, negotiated via capabilities).
_Avoid_: linter, analyzer, server (when LSP-specific)

**Languages config**:
The daemon-side configuration file (`languages.toml` under the config directory) mapping file types to LanguageServers — each server's command, arguments, initialization options (its `config`), and a language's root markers. Embedded defaults are merged with the user file, and the Daemon re-reads it when it spawns a server. Distinct from Config, which is client-local and never read by the Daemon.
_Avoid_: language settings, server table

**Command**:
A selection-based editor action, resolved from key presses by the Keymap or sent directly by a Client (`session exec`), that reads and may change the Selection.
_Avoid_: action, operation

**DocumentEdit**:
A position-addressed change to a Document — insert, delete, or replace at explicit character ranges — that neither reads nor changes the Selection. Sent by headless Clients (agents); the interactive TUI operates only through Commands.
_Avoid_: patch, edit request

**Keymap**:
A mapping from key sequences to Commands, scoped per Mode and structured as a prefix trie so sequences like `g g` resolve.
_Avoid_: keybindings, keymap table

**Daemon**:
The persistent mina process that owns the editor state — documents, histories, selections, and LSP clients — and serves Clients over a local channel. Clients may come and go; the Daemon and its state remain. The Selection and viewport position are connection-scoped: while an Interactive client is connected they follow that client's navigation, and when the last Interactive client disconnects every View returns to the idle default (a single cursor at the start of the Document, viewport at the first line) unless that client declared `reset_cursor_on_disconnect = false` in its Hello (ADR-0027). Documents, undo histories, and LSP sessions are unaffected by Client disconnects.
_Avoid_: server, backend

**Client**:
A process that connects to the Daemon to send Commands and receive editor state: the terminal UI, a CLI invocation, or an agent.
_Avoid_: frontend, viewer

**Connection**:
A Client's channel to the Daemon, opened by connecting to the daemon socket and sending a Hello (declaring the Client kind). One-shot connections (headless session CLI, agents) are opened per command and carry no pushes; persistent connections (the interactive TUI) subscribe to state broadcasts and live until the Client disconnects (ADR-0013). The socket path is part of the wire contract (`minae-{PROTOCOL_VERSION}.sock`, `minae_protocol::socket_path`).
_Avoid_: link, channel, session

**StateSnapshot**:
The response to every Command: the Daemon's complete editor state — document text, selection, mode, viewport, and diagnostics — serialized for Clients to render.
_Avoid_: frame, update

**Generation**:
A monotonically increasing counter on the Daemon, bumped on every state-changing operation (edits, undo/redo, mode changes, Open/Save), never on pure reads. Activity membership changes (add/remove) also bump it; analysis results (diagnostics, inlay hints) do not (ADR-0028). Clients compare generations to detect that something changed without re-reading the document. Carried in every StateSnapshot.
_Avoid_: version, revision

**Gutter**:
The fixed-width strip at the left of a View showing the absolute LineNumber (1-based) of each visible line, with the line containing the cursor highlighted. Display-only, computed by the Client from the StateSnapshot; never part of the Document text.
_Avoid_: line number column, margin

**Activity**:
A unit of in-progress asynchronous work known to the Daemon — added when the work begins and removed when it ends. Activities ride in the StateSnapshot as a set, so Clients can tell the user that processing is ongoing and headless Clients can detect that it has cleared. An Activity says nothing about its outcome: it ends without reporting success or failure. The animated indicator (spinner) is a Client-side rendering concern; the Activity itself carries only the fact of the work.
_Avoid_: busy flag, loading, progress

**ChangeEvent**:
A record of one state-changing operation, tagged with its source (Interactive, Headless, or External) and kind. The Daemon keeps a bounded ring of recent ChangeEvents and carries it in every StateSnapshot.
_Avoid_: event, log entry

**Diagnostic**:
A problem reported by the language server for a range of text — error, warning, info, or hint. Carried to Clients in the StateSnapshot.
_Avoid_: lint, problem

**InlayHint**:
A read-only annotation rendered inline at a position in the text (inferred types, parameter names), provided by the language server. Display-only: it is never part of the Document text and never participates in Selection, editing, undo, or the checksum. Distinct from `Severity::Hint`, a Diagnostic severity.
_Avoid_: annotation, ghost text

**Peek**:
A transient preview of a symbol's definition shown in a popup, without changing the Selection or jumping to the definition. Triggered by `Space k` (`Command::PeekDefinition`); its result rides in a `StateSnapshot`'s `peek` field and is dismissed by the next key press. Read-only — it never edits or moves. The headless counterpart is `Command::PeekDefinitionAt` (position-addressed, ADR-0025), whose lightweight `ServerMessage::Peek` response carries only the definition snippet, never the full text.
_Avoid_: definition popup, go-to preview

**Rename**:
The semantic replacement of a symbol's name across all its references, resolved by the language server (ADR-0029). Content-addressed for headless Clients (`session rename <path> <old> <new>`): the Daemon resolves `<old>` to the first identifier occurrence (comments and strings excluded) and the server rewrites every reference, possibly across several files. Distinct from apply, which is mechanical text replacement and cannot know a symbol's references. The response is a lightweight impact report (files/edits counts and changed paths), never the full text. Not undoable as a whole; each Document's own edits are still recorded so per-Document undo histories stay consistent.
_Avoid_: rename as a text operation, refactor

**Reference**:
A location where a symbol is used, as reported by the language server (read-only, ADR-0029). Used to learn a symbol's impact before a Rename and to audit a Rename's completeness — the "did we miss any occurrences" check that mechanical replacement fails (T5). The response is a lightweight location list (path + 0-origin line), never the full text.
_Avoid_: usage, impact scope

**Outline**:
The hierarchical list of a Document's symbols — functions, methods, types, impls, and similar — as reported by the language server (read-only). Each entry carries the symbol's name and kind plus two ranges: its span (the whole item) and its selection range (the name token itself), which doubles as the address for position-resolved lookups. The response is a lightweight tree of these entries, never the Document text.
_Avoid_: symbol list, document symbols, outline view

**Colorscheme**:
A named mapping from semantic roles — HighlightGroups and UI elements such as the cursor or status line — to terminal colors and attributes. Built-in schemes ship with mina; user schemes are TOML files in the colorschemes directory and take precedence over built-ins of the same name. A scheme is selected by name from the config file or the `:colorscheme` command.
_Avoid_: theme, palette

**Syntax**:
The parsed syntactic representation of a Document's text, produced from a tree-sitter grammar and kept current with every edit by the Daemon — alongside LSP analysis, for every edit source (interactive, headless, or external reload).
_Avoid_: syntax tree, parse tree

**HighlightGroup**:
A named class of tokens (comment, keyword, string, function, …) that text is assigned to for styling by the Daemon's per-language highlight queries. The unit a Colorscheme maps to colors and attributes.
_Avoid_: scope, token type

**ColorCapability**:
The color depth a terminal supports — truecolor, a 256-color palette, or ANSI16 — detected once from the environment at Client startup. The renderer converts Colorscheme colors to this depth; `NO_COLOR` suppresses color output entirely while keeping attributes (underline, reverse).
_Avoid_: color depth, terminal colors

**Config**:
The user's set of settings, stored as TOML in the `config.toml` file under the XDG config directory and read once at Client startup. Client-local: the Daemon never reads it and changes apply from the next startup, never live. Unknown keys are rejected (typo detection), and a broken file falls back to defaults with a warning rather than blocking startup. Settings that affect the Daemon (e.g. `reset_cursor_on_disconnect`) are declared by the Client at connect time via the Hello message — the Daemon still never reads the config file itself.
_Avoid_: settings, preferences

**Config file**:
The `config.toml` file that stores the Config — `$XDG_CONFIG_HOME/minae/config.toml`, falling back to `~/.config/minae/config.toml`. Manipulated by the user directly or through the `minae config` subcommand.
_Avoid_: config (as a file), rc file

**Base**:
The pinned snapshot that comparison annotates the current text against — fixed at pin time and never changing while pinned. Immutable: edits always target the current text, never the Base.
_Avoid_: baseline, old version, original

**BaseRoot**:
A Base materialized as files and registered with the Daemon. Read-only and lifecycle-managed: the Daemon analyses it (definitions, references, hover) but rejects text changes under it, and tears the analysis session down on unregister.
_Avoid_: snapshot dir, temp root

**ReviewComment**:
A comment anchored to a diff location — side (Base or current), path, line number, the line's content at add time (snippet), and the comment body — held in Daemon-shared storage so both the TUI and headless Clients (`minas review`) can see it. Same-anchor adds overwrite; an empty body deletes that anchor. Listing resolves each entry against the current text (`stale` plus a resolved line) without rewriting the stored value; re-pin (when the base point changes), unregister, or explicit clear drops all entries, while a TUI disconnect keeps them.
_Avoid_: inline comment, annotation
