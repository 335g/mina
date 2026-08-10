# mina Context

mina is a terminal editor/viewer under construction. Development starts from the text-editing core (UI-agnostic), and the terminal layer comes later.

## Language

**Document**:
The unit of editable text. Holds the text content and is the target of edits.
_Avoid_: Buffer

**Selection**:
The set of active cursor ranges. A single cursor is a Selection with one range.
_Avoid_: cursors, multi-cursor

**Transaction**:
The unit of change applied to a Document. Designed so it can be inverted to support undo.
_Avoid_: edit, change, operation
