# User colorschemes as TOML files; built-ins stay compiled-in

Users can define their own Colorschemes as TOML files in `~/.config/minae/colorschemes/`, referenced by filename from `config.toml` (`colorscheme = "..."`) or the `:colorscheme` command. Built-ins deliberately remain Rust consts: DEFAULT must always exist as the fallback even with no config or XDG dir, so schemes have two representations (Rust for built-ins, TOML for users) instead of one unified format.

Resolution is lazy and file-first — a user file shadows a built-in of the same name, and files load only when referenced, not scanned at startup. Scheme files are partial: unspecified roles inherit from the built-in DEFAULT (layering), so a file only writes what it changes. A broken or unknown scheme never blocks startup — it warns and falls back (to the built-in of the same name, else DEFAULT).
