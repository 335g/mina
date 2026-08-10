# Functional core: Selection lives outside Document

The editing core is functional: primitives transform state instead of mutating it — `(document, selection) → (new_document, new_selection)`. A Document owns its text; the active Selection is not stored on it, it is passed into each operation. This mirrors Helix's decision to keep selections per-View (one document can be shown in several splits). Adopting it from day one avoids an API rewrite when views arrive.
