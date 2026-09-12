PERSIST — when changes hit the disk

- `minas apply` (and --hunks-stdin) Save for you: after success the file on
  disk is updated.
- Raw `minas edit` does NOT save: the daemon buffer changes but the disk file
  stays old (dirty=true). If you used edit, persist with:
      minas exec "Save"
- When an edit succeeded but the buffer is dirty, minas prints a stderr note:
  "buffer is dirty (not saved); persist with: minas exec '"Save"'".
- Before relying on a file's on-disk content, prefer apply (which re-opens the
  file fresh) over mixing edit + assumptions.
