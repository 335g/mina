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

**Mode**:
The active editing mode — Normal, Insert, or Select. Determines which Keymap resolves key presses.
_Avoid_: state

**View**:
One display region (one split) showing a Document with its own Selection and viewport position. Multiple Views can show the same Document.
_Avoid_: pane, window

**Command**:
A named editor action that the term layer resolves from key presses and applies to the Editor state.
_Avoid_: action, operation

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

**Diagnostic**:
A problem reported by the language server for a range of text — error, warning, info, or hint. Carried to Clients in the StateSnapshot.
_Avoid_: lint, problem
