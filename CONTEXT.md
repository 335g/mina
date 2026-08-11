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
