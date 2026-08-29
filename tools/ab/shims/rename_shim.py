#!/usr/bin/env python3
"""minae A/B shim — LSP semantic rename (T4).

    mrename <path> <old> <new>
        Runs rust-analyzer (LSP over stdio) in the project containing <path>,
        renames the symbol at the first whole-word occurrence of <old> in
        <path> to <new>, and applies the returned WorkspaceEdit directly to
        the files. This is *semantic* (language-aware) renaming: the model
        supplies the symbol name, not coordinates.

    Output (compact, for the agent):
        renamed: <old> -> <new> (N files, M edits)
    On failure (symbol not found / LSP error): a one-line error with reason,
    exit 2 (retryable).

Env:
    MAB_LSP_BIN   path to the LSP server binary (default: rust-analyzer)
    MAB_AUDIT     audit log path
"""
import json
import os
import pathlib
import re
import subprocess
import sys
import threading
import time

LSP_BIN = os.environ.get("MAB_LSP_BIN", "rust-analyzer")
LSP_ARGS = [a for a in os.environ.get("MAB_LSP_ARGS", "").split() if a]
AUDIT = os.environ.get("MAB_AUDIT", "")

MAX_WAIT = int(os.environ.get("MAB_LSP_WAIT", "45"))  # seconds


def audit(kind, extra=""):
    if AUDIT:
        with open(AUDIT, "a") as f:
            f.write(f"{kind}\t{extra}\n")


class Lsp:
    def __init__(self, cwd):
        # rust-analyzer >=1.98 removed the --stdio flag (bare invocation is the
        # LSP-over-stdio default); typescript-language-server needs --stdio.
        # MAB_LSP_ARGS supplies per-server flags.
        self.proc = subprocess.Popen(
            [LSP_BIN] + LSP_ARGS,
            cwd=str(cwd),
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )
        self.next_id = 1
        self.msgs = {}
        self._reader = threading.Thread(target=self._read_loop, daemon=True)
        self._reader.start()

    def _read_loop(self):
        while True:
            try:
                header = b""
                while b"\r\n\r\n" not in header:
                    chunk = self.proc.stdout.read(1)
                    if not chunk:
                        return
                    header += chunk
                m = re.search(rb"Content-Length: (\d+)", header)
                if not m:
                    return
                body = self.proc.stdout.read(int(m.group(1)))
                if not body:
                    return
                try:
                    msg = json.loads(body)
                except Exception:
                    continue
                if "id" in msg:
                    self.msgs[msg["id"]] = msg
                # notifications are dropped (indexing progress etc.)
            except Exception:
                return

    def _send(self, obj):
        body = json.dumps(obj).encode()
        self.proc.stdin.write(
            b"Content-Length: " + str(len(body)).encode() + b"\r\n\r\n" + body)
        self.proc.stdin.flush()

    def request(self, method, params):
        rid = self.next_id
        self.next_id += 1
        self._send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        deadline = time.time() + MAX_WAIT
        while time.time() < deadline:
            msg = self.msgs.pop(rid, None)
            if msg is not None:
                return msg
            time.sleep(0.05)
        return {"error": {"message": "LSP request timed out"}}

    def notify(self, method, params):
        self._send({"jsonrpc": "2.0", "method": method, "params": params})

    def close(self):
        try:
            self.proc.terminate()
        except Exception:
            pass


def uri(path):
    return pathlib.Path(path).resolve().as_uri()


def find_position(text, name):
    """First whole-word occurrence of `name` -> (line, character) 0-origin."""
    for i, line in enumerate(text.splitlines()):
        for m in re.finditer(r"\b" + re.escape(name) + r"\b", line):
            return i, m.start()
    return None


def apply_edits(root, edits):
    """edits: {uri: [{range, newText}...]} — apply bottom-up per file."""
    from urllib.parse import unquote, urlparse
    n = 0
    for u, ch in edits.items():
        # some servers return file:/abs/path (no authority); parse as URI,
        # never treat the whole string as a relative path
        parsed = urlparse(u)
        raw = unquote(parsed.path) if parsed.scheme == "file" else u
        path = pathlib.Path(raw)
        text = path.read_text()
        # sort by range start descending, apply in reverse
        for e in sorted(ch, key=lambda e: (e["range"]["start"]["line"],
                                           e["range"]["start"]["character"]), reverse=True):
            r = e["range"]
            s = r["start"]
            en = r["end"]
            start = text_index(text, s)
            end = text_index(text, en)
            text = text[:start] + e["newText"] + text[end:]
            n += 1
        path.write_text(text)
    return n


def text_index(text, pos):
    lines = text.splitlines(keepends=True)
    return sum(len(l) for l in lines[:pos["line"]]) + pos["character"]


def main():
    if len(sys.argv) < 4:
        print("usage: mrename <path> <old> <new>"); sys.exit(1)
    path_arg, old, new = sys.argv[1], sys.argv[2], sys.argv[3]
    path = pathlib.Path(path_arg).resolve()
    if not path.exists():
        print(f"mrename: file not found: {path_arg}"); sys.exit(2)
    cwd = path.parent.parent if (path.parent.parent / "Cargo.toml").exists() else path.parent
    lsp = Lsp(cwd)
    try:
        root_uri = pathlib.Path(cwd).resolve().as_uri()
        lsp.request("initialize", {
            "processId": None,
            "rootUri": root_uri,
            "workspaceFolders": [{"uri": root_uri, "name": "root"}],
            "capabilities": {
                "textDocument": {"rename": {"prepareSupport": False}},
            },
        })
        lsp.notify("initialized", {})
        # open the target file and every source file of the same language so
        # cross-file renames resolve (extension follows the target file)
        ext = path.suffix
        for f in sorted(pathlib.Path(cwd).rglob(f"*{ext}")):
            txt = f.read_text()
            lang = "rust" if ext == ".rs" else ("typescript" if ext in (".ts", ".tsx") else "plaintext")
            lsp.notify("textDocument/didOpen", {
                "textDocument": {"uri": f.as_uri(), "languageId": lang,
                                 "version": 1, "text": txt},
            })
        # let the server load/parse before asking for a rename
        time.sleep(int(os.environ.get("MAB_LSP_SETTLE", "4")))
        pos = find_position(path.read_text(), old)
        if pos is None:
            print(f"mrename: '{old}' not found in {path_arg}"); sys.exit(2)
        resp = lsp.request("textDocument/rename", {
            "textDocument": {"uri": path.as_uri()},
            "position": {"line": pos[0], "character": pos[1]},
            "newName": new,
        })
        if "error" in resp:
            print(f"mrename: LSP error: {resp['error'].get('message')}"); sys.exit(2)
        result = resp.get("result")
        if not result:
            print(f"mrename: no edits returned for '{old}'"); sys.exit(2)
        changes = {}
        if result.get("changes"):
            changes = dict(result["changes"])
        for dc in result.get("documentChanges") or []:
            if "textDocument" in dc and "edits" in dc:
                changes.setdefault(dc["textDocument"]["uri"], []).extend(dc["edits"])
        if not changes:
            print(f"mrename: no edits returned for '{old}'".replace("\x27", "'")); sys.exit(2)
        n = apply_edits(cwd, changes)
        audit("rename", f"{old}->{new} files={len(changes)} edits={n}")
        print(f"renamed: {old} -> {new} ({len(changes)} files, {n} edits)")
    finally:
        lsp.close()


if __name__ == "__main__":
    main()