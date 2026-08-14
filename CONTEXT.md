# mina Context

mina is a terminal editor/viewer under construction. Development starts from the text-editing core (UI-agnostic), and the terminal layer comes later.

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
The smallest analysis unit containing an opened file — the nearest directory with a manifest (`Cargo.toml`) or `.git`, falling back to the file's parent directory. The scope unit of an LSP session.
_Avoid_: project root, crate root

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
The persistent mina process that owns the editor state — documents, histories, selections, and LSP clients — and serves Clients over a local channel. Clients may come and go; the Daemon and its state remain.
_Avoid_: server, backend

**Client**:
A process that connects to the Daemon to send Commands and receive editor state: the terminal UI, a CLI invocation, or an agent.
_Avoid_: frontend, viewer

**StateSnapshot**:
The response to every Command: the Daemon's complete editor state — document text, selection, mode, viewport, and diagnostics — serialized for Clients to render.
_Avoid_: frame, update

**Generation**:
A monotonically increasing counter on the Daemon, bumped on every state-changing operation (edits, undo/redo, mode changes, Open/Save), never on pure reads. Clients compare generations to detect that something changed without re-reading the document. Carried in every StateSnapshot.
_Avoid_: version, revision

**ChangeEvent**:
A record of one state-changing operation, tagged with its source (Interactive, Headless, or External) and kind. The Daemon keeps a bounded ring of recent ChangeEvents and carries it in every StateSnapshot.
_Avoid_: event, log entry

**Diagnostic**:
A problem reported by the language server for a range of text — error, warning, info, or hint. Carried to Clients in the StateSnapshot.
_Avoid_: lint, problem
