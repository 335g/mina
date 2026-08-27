#!/usr/bin/env python3
"""opencode ツール制御 A/B ハーネス（mina CLI 経由のエディタ契約を実LLMで測る）。

使い方:
    ab.py run <t2|t3> <A|B> <idx>   — 1 run 実行（workdir /tmp/ab-run/ws、計測込み）
    ab.py stats <t2|t3> <A|B> <idx> — DB から計測だけ再計算

コマンド構成:
  - opencode.json に bash 専用 agent（native read/edit/glob/grep を無効化）を書き、
    `opencode run --agent <agent>` で実行。ファイル読み書きは全て mina CLI をラップ
    した shim（r / e）を通す。
  - 計測は ~/.local/share/opencode/opencode.db の step-finish（tokens/cost）から。
  - 設計意図: 手段の差（範囲read vs 全文read / apply vs edit / 拒否理由の有無）だけを
    隔離し、同一モデル・同一タスクで対比する。詳細は tools/ab/README.md。
"""
import argparse
import json
import os
import pathlib
import shutil
import sqlite3
import subprocess
import sys
import time

REPO = pathlib.Path(__file__).resolve().parent.parent.parent
SHIMS = pathlib.Path(__file__).resolve().parent / "shims"
WORK = pathlib.Path("/tmp/ab-run")
DB = os.path.expanduser("~/.local/share/opencode/opencode.db")
OPENCODE = os.environ.get(
    "OPENCODE_BIN",
    "/Users/335g/.local/share/mise/installs/opencode/1.14.30/opencode",
)
MINA = os.environ.get("MAB_MINABIN", str(REPO / "target" / "debug" / "mina"))
MODEL = os.environ.get("MAB_MODEL", "opencode/gpt-5.4-nano")
AGENT = "mina-bash"

TOOLS_OFF = {
    "bash": True,
    "read": False,
    "glob": False,
    "grep": False,
    "edit": False,
    "write": False,
    "todowrite": False,
    "webfetch": False,
    "websearch": False,
    "skill": False,
    "apply_patch": False,
}


# ---------- fixtures ----------

def fixture_t2():
    """Test2: cfg.rs. Task: timeout 5000->30000, max_retries 3->5. Drift targets 'max_retries: 3,'."""
    text = """pub struct Config {
    pub timeout: u64,
    pub max_retries: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            timeout: 5000,
            max_retries: 3,
        }
    }
}

fn load_other() {
    let banner = "starting service with defaults";
    log(banner);
}
""".replace("    pub timeout: u64,", "    pub timeout: u64,\n    pub max_connections: u16,")
    (WORK / "ws" / "cfg.rs").write_text(text)


def fixture_t3():
    """Test3: two files with USD constant + price() method to rename."""
    f1 = """const USD: i64 = 100;
fn main() {
    let a = price(USD);
}
"""
    f2 = """fn price(x: i64) -> i64 {
    x + USD
}

fn report() -> i64 {
    price(USD)
}
"""
    (WORK / "ws" / "f1.rs").write_text(f1)
    (WORK / "ws" / "f2.rs").write_text(f2)


def fixture_t4():
    """T4: single real TypeScript file for LSP semantic rename vs apply."""
    (WORK / "ws" / "rename.ts").write_text(
        """const USD = 100;

export function price(x: number): number {
  return x + USD;
}

function main() {
  console.log(price(USD));
}
""")


def fixture_t5():
    """T5: 3-file TS project, many occurrences (USD x12, price x9) — the
    regime where LSP semantic rename should beat an apply loop."""
    (WORK / "ws" / "utils.ts").write_text(
        """export const USD = 100;

export function price(x: number): number {
  return x * USD;
}
""")
    (WORK / "ws" / "data.ts").write_text(
        """import { USD, price } from './utils';

export const items = [USD * 1, USD * 2, USD * 3, USD * 4, USD * 5, USD * 10];

export function total(a: number, b: number): number {
  return price(a) + price(b) + price(a * b) + price(a + b);
}
""")
    (WORK / "ws" / "main.ts").write_text(
        """import { USD, price } from './utils';
import { items, total } from './data';

const first = items[0] + USD;
const second = price(USD) + USD * 2;
const third = total(USD, price(USD));
console.log(first, second, third);
""")


def fixture_t7():
    """T7: 600-line cfg.rs, 8 scattered targets, drift-rejection at scale.
    TARGET_k = 100+k at scattered lines; task: increment each by 1."""
    targets = {1: 30, 2: 90, 3: 150, 4: 210, 5: 270, 6: 400, 7: 520, 8: 590}
    lines = ["// generated configuration\n"]
    for i in range(1, 601):
        if i in targets.values():
            k = [n for n, ln in targets.items() if ln == i][0]
            lines.append(f"let TARGET_{k} = {100 + k};\n")
        else:
            v = (i * 7 + 3) % 503
            lines.append(f"let filler_{i}_value = {v}; // noise\n")
    (WORK / "ws" / "cfg.rs").write_text("".join(lines))


def fixture_t8():
    """T8: 200-line file, 5 targets at KNOWN lines (task gives line hints —
    the positional trap), values TARGET_k = 100+k. Task: increment each by 1."""
    targets = {1: 31, 2: 71, 3: 111, 4: 151, 5: 191}
    lines = []   # no header line: line N in the file == loop index N, so the
                 # line numbers advertised in the task match the file exactly
    for i in range(1, 201):
        if i in targets.values():
            k = [n for n, ln in targets.items() if ln == i][0]
            lines.append(f"let TARGET_{k} = {100 + k};\n")
        else:
            v = (i * 13 + 7) % 251
            lines.append(f"let filler_{i} = {v}; // noise\n")
    (WORK / "ws" / "cfg.rs").write_text("".join(lines))


PROMPTS = {
    # Test2: rejection verbosity (C1). Both arms get the same task + drift; only
    # the rejection message differs (apply vs apply-generic in edit_shim).
    "t2-A": """Edit cfg.rs in the current directory. Two changes:
  1) change the timeout value from 5000 to 30000
  2) change max_retries from 3 to 5

You MUST use these commands for all file access (no other file commands):
  mread <path> [start:end]   read lines of a file (numbered)
  medit <path> <old> <new>   replace the FIRST occurrence of <old> with <new>
Work in small steps: mread a range (e.g. mread cfg.rs 1:20), then medit the exact
text you saw. When both changes are in place, verify with a final mread and reply
with exactly: DONE
Do not modify anything else.""",
    "t2-B": """Edit cfg.rs in the current directory. Two changes:
  1) change the timeout value from 5000 to 30000
  2) change max_retries from 3 to 5

You MUST use these commands for all file access (no other file commands):
  mread <path> [start:end]   read lines of a file (numbered)
  medit <path> <old> <new>   replace the FIRST occurrence of <old> with <new>
Work in small steps: mread a range (e.g. mread cfg.rs 1:20), then medit the exact
text you saw. When both changes are in place, verify with a final mread and reply
with exactly: DONE
Do not modify anything else.""",
    # Test8: does `mina skill` steer AWAY from the positional trap when both
    # a positional tool (medit) and a content-resolved tool (mapply) exist?
    # Task gives explicit line numbers -> positional pull. Tools neutral.
    "t8-A": """Audit constants in cfg.rs. The five target lines are exactly:
  line 31:  let TARGET_1 = 101;
  line 71:  let TARGET_2 = 102;
  line 111: let TARGET_3 = 103;
  line 151: let TARGET_4 = 104;
  line 191: let TARGET_5 = 105;
Increment each value by 1 (TARGET_1 -> 102, ..., TARGET_5 -> 106). All five
updates are required.

Available file commands:
  mread <path> [start:end]             read lines (numbered)
  medit <path> <documentedit-json>     edit by exact character offsets
  mapply <path> <old> <new>            replacement of the first occurrence of <old>
  mcheck <path>                        print the file checksum

mina also provides skill guides: run `mina skill` for the index and
`mina skill <topic>` to read one (read/edit/errors). Consult the edit guide
BEFORE choosing how you will perform the updates.
When all 5 values are updated, verify with mread and reply with exactly: DONE""",
    "t8-B": """Audit constants in cfg.rs. The five target lines are exactly:
  line 31:  let TARGET_1 = 101;
  line 71:  let TARGET_2 = 102;
  line 111: let TARGET_3 = 103;
  line 151: let TARGET_4 = 104;
  line 191: let TARGET_5 = 105;
Increment each value by 1 (TARGET_1 -> 102, ..., TARGET_5 -> 106). All five
updates are required.

Available file commands:
  mread <path> [start:end]             read lines (numbered)
  medit <path> <documentedit-json>     edit by exact character offsets
  mapply <path> <old> <new>            replacement of the first occurrence of <old>
  mcheck <path>                        print the file checksum

When all 5 values are updated, verify with mread and reply with exactly: DONE""",

    # Same real rejection messages in both arms; only the `mina skill` reference
    # differs (both may use mread/medit/mcheck).
    "t7-A": """Edit cfg.rs (a large generated file) in the current directory.
It contains 8 target constants, each currently equal to 100 + its number
(e.g. TARGET_3 = 103). Change EACH one so its value is incremented by 1
(TARGET_3 becomes 104). Updating all 8 is required.

Available file commands:
  mread <path> [start:end]   read lines of a file (numbered)
  medit <path> <old> <new>   replace the FIRST occurrence of <old> with <new>

mina also provides skill guides for how to use these tools:
run `mina skill` for the index and `mina skill <topic>` to read one
(topics: read, edit, persist, errors). If an edit is rejected, follow the
guidance in the errors guide before retrying.
When all 8 targets are updated, verify each final value with mread and reply
with exactly: DONE""",
    "t7-B": """Edit cfg.rs (a large generated file) in the current directory.
It contains 8 target constants, each currently equal to 100 + its number
(e.g. TARGET_3 = 103). Change EACH one so its value is incremented by 1
(TARGET_3 becomes 104). Updating all 8 is required.

Available file commands:
  mread <path> [start:end]   read lines of a file (numbered)
  medit <path> <old> <new>   replace the FIRST occurrence of <old> with <new>

When all 8 targets are updated, verify each final value with mread and reply
with exactly: DONE""",
    # Same task and FULL toolset (mread/medit/mrename/mcheck) for both arms.
    # Arm A is told the skill index/guides exist; Arm B is not.
    "t6-A": """Refactor the TypeScript project in the current directory
(utils.ts, data.ts, main.ts):
  - rename the constant USD to JPY (every occurrence in every file)
  - rename the function price to amount (its definition and every call)

Available file commands (in $PATH):
  mread <path> [start:end]   read lines of a file (numbered)
  medit <path> <old> <new>   replace the FIRST occurrence of <old> with <new>
  mrename <path> <old> <new> language-aware rename (all references, one call)
  mcheck <path>              print a file checksum

mina also provides skill guides for how to use these editing tools:
run `mina skill` to list the topics, and `mina skill <topic>` to read one
(topics: read, edit, rename, persist, errors). Consult the relevant guide
BEFORE deciding how you will perform the renames.
When done, verify that no occurrence of "USD" or "price" remains in ANY of
the .ts files and reply with exactly: DONE""",
    "t6-B": """Refactor the TypeScript project in the current directory
(utils.ts, data.ts, main.ts):
  - rename the constant USD to JPY (every occurrence in every file)
  - rename the function price to amount (its definition and every call)

Available file commands (in $PATH):
  mread <path> [start:end]   read lines of a file (numbered)
  medit <path> <old> <new>   replace the FIRST occurrence of <old> with <new>
  mrename <path> <old> <new> language-aware rename (all references, one call)
  mcheck <path>              print a file checksum

When done, verify that no occurrence of "USD" or "price" remains in ANY of
the .ts files and reply with exactly: DONE""",
    "t5-A": """Refactor the TypeScript project in the current directory
(utils.ts, data.ts, main.ts):
  - rename the constant USD to JPY (every occurrence in every file)
  - rename the function price to amount (its definition and every call)

You MUST use these commands for all file access (no other file commands):
  mread <path> [start:end]   read lines of a file (numbered)
  medit <path> <old> <new>   replace the FIRST occurrence of <old> with <new>
Work file by file, occurrence by occurrence. When done, verify that no
occurrence of "USD" or "price" remains in ANY of the .ts files and reply with
exactly: DONE""",
    "t5-B": """Refactor the TypeScript project in the current directory
(utils.ts, data.ts, main.ts):
  - rename the constant USD to JPY (every occurrence in every file)
  - rename the function price to amount (its definition and every call)

You MUST use these commands for all file access (no other file commands):
  mread <path> [start:end]   read lines of a file (numbered)
  mrename <path> <old> <new> perform a LANGUAGE-AWARE RENAME of the symbol
     whose first whole-word occurrence is <old> in <path> — it renames the
     definition AND all references across all files in one call.
One mrename per symbol is enough. Then verify with a final mread that no
"USD" or "price" remains in ANY .ts file, and reply with exactly: DONE""",
    "t4-A": """Refactor the file rename.ts (TypeScript) in the current directory:
  - rename the constant USD to JPY (every occurrence)
  - rename the function price to amount (its definition and every call)

You MUST use these commands for all file access (no other file commands):
  mread <path> [start:end]   read lines of a file (numbered)
  medit <path> <old> <new>   replace the FIRST occurrence of <old> with <new>
When done, verify that no occurrence of "USD" or "price" remains in the file
and reply with exactly: DONE""",
    "t4-B": """Refactor the file rename.ts (TypeScript) in the current directory:
  - rename the constant USD to JPY (every occurrence)
  - rename the function price to amount (its definition and every call)

You MUST use these commands for all file access (no other file commands):
  mread <path> [start:end]   read lines of a file (numbered)
  mrename <path> <old> <new> perform a LANGUAGE-AWARE RENAME of the symbol whose
     first whole-word occurrence is <old> in <path> — the editor finds and
     updates every reference (definition, calls, uses) itself. One call per
     rename is enough; do not loop over occurrences.
First mread rename.ts to see the file, then use mrename for USD->JPY and for
price->amount, then verify with a final mread that no "USD" or "price"
remains, and reply with exactly: DONE""",
    "t3-A": """Refactor the project in the current directory (files f1.rs and f2.rs):
  - rename the constant USD to JPY (all occurrences)
  - rename the method price() to amount() (its definition and all calls)

You MUST use these commands for all file access (no other file commands):
  mread <path> [start:end]   read lines of a file (numbered)
  medit <path> <old> <new>   replace the FIRST occurrence of <old> with <new>
When done, verify that no occurrence of "USD" or "price(" remains in either file
and reply with exactly: DONE""",
    "t3-B": """Refactor the project in the current directory (files f1.rs and f2.rs):
  - rename the constant USD to JPY (all occurrences)
  - rename the method price() to amount() (its definition and all calls)

You MUST use these commands for all file access (no other file commands):
  mread <path> [start:end]   read lines of a file (numbered)
  mcheck <path>              print the current file checksum (a number)
  medit <path> <json>        POSITIONAL edit: supply a DocumentEdit JSON with
    char-index start and end, replacement text, the checksum from mcheck, and
    the expected old text, e.g.
    medit f1.rs {"start":5,"end":8,"text":"JPY","checksum":123,"expected_text":"USD"}
  You must compute start/end as CHAR indices yourself (count characters, not bytes;
  newlines count as 1 character).
When done, verify that no occurrence of "USD" or "price(" remains in either file
and reply with exactly: DONE""",
}


# ---------- workdir build ----------

def build_workdir(test, arm):
    wd = WORK / "ws"
    if wd.exists():
        shutil.rmtree(wd)
    (wd / "bin").mkdir(parents=True)
    (wd / "audit.log").write_text("")
    # bash-only agent config
    cfg = {"agent": {AGENT: {
        "description": "bash-only agent for mina A/B",
        "tools": TOOLS_OFF,
        "permission": {"bash": "allow"},
        "maxSteps": 30,
    }}}
    (wd / "opencode.json").write_text(json.dumps(cfg, indent=2))
    # mode resolution
    read_mode = "range"
    if test == "t2":
        edit_mode = "apply-generic" if arm == "B" else "apply"
    else:  # t3
        edit_mode = "edit" if arm == "B" else "apply"
    if arm == "B" and test == "t1":
        read_mode = "full"
    lsp_env = ""
    if test == "t6":
        lsp_env = f"export MAB_LSP_BIN=typescript-language-server; export MAB_LSP_ARGS='--stdio'; export MAB_LSP_SETTLE=6;"
    elif test in ("t4", "t5") and arm == "B":
        lsp_env = f"export MAB_LSP_BIN=typescript-language-server; export MAB_LSP_ARGS='--stdio'; export MAB_LSP_SETTLE=5;"
    # shim wrappers — names must avoid zsh builtins ('r'/'e' collide: `r` is the
    # history-rerun builtin in zsh, which opencode's bash tool uses)
    env = f"export MAB_MINABIN={MINA}; export MAB_AUDIT={wd}/audit.log;"

    def wr(name, shim, mode):
        p = wd / "bin" / name
        p.write_text(
            f"#!/usr/bin/env bash\n{env} exec python3 {SHIMS}/{shim} {mode} \"$@\"\n")
        p.chmod(0o755)
        return p

    wr("mread", "read_shim.py", read_mode)
    wr("medit", "edit_shim.py", edit_mode)
    wr("mcheck", "check_shim.py", "x")
    if test == "t8":
        # both edit tools present: medit (positional) and mapply (content-resolved)
        wr("mapply", "edit_shim.py", "apply")
    if (test == "t6") or (test in ("t4", "t5") and arm == "B"):
        p = wd / "bin" / "mrename"
        p.write_text(
            f"#!/usr/bin/env bash\n{env}{lsp_env} exec python3 {SHIMS}/rename_shim.py \"$@\"\n")
        p.chmod(0o755)
    if test in ("t6", "t7", "t8") and arm == "A":
        p = wd / "bin" / "mina"
        p.write_text(
            f"#!/usr/bin/env bash\n"
            f"if [ \"${{1:-}}\" = \"skill\" ]; then exec {MINA} \"$@\"; fi\n"
            f"echo \"error: only 'mina skill' is exposed here; use medit/mread for file operations\" >&2\n"
            f"exit 1\n")
        p.chmod(0o755)
    if test == "t7":
        # drift: after the first successful edit, TARGET_5 moves 105 -> 1050,
        # so the model's remembered old string for it is no longer found
        p = wd / "bin" / "medit"
        p.write_text(
            f"#!/usr/bin/env bash\n{env} export MAB_DRIFT_OLD='let TARGET_5 = 105;'; export MAB_DRIFT_NEW='let TARGET_5 = 1050;'; "
            f"exec python3 {SHIMS}/edit_shim.py apply \"$@\"\n")
        p.chmod(0o755)
    if test == "t2":
        fixture_t2()
        # drift: both arms get it; only rejection verbosity differs (apply vs apply-generic)
        p = wd / "bin" / "medit"
        p.write_text(
            f"#!/usr/bin/env bash\n{env} export MAB_DRIFT_OLD='max_retries: 3,'; export MAB_DRIFT_NEW='max_retries: 4,'; "
            f"exec python3 {SHIMS}/edit_shim.py {edit_mode} \"$@\"\n")
        p.chmod(0o755)
    elif test == "t3":
        fixture_t3()
    elif test == "t4":
        fixture_t4()
    elif test == "t5":
        fixture_t5()
    elif test == "t6":
        fixture_t5()
    if test == "t7":
        fixture_t7()
    if test == "t8":
        fixture_t8()
    (wd / "task.txt").write_text(PROMPTS[f"{test}-{arm}"])
    return wd


# ---------- run ----------

def run_once(test, arm, idx):
    wd = build_workdir(test, arm)
    title = f"mab-{test}-{arm}-{idx}"
    subprocess.run(["pkill", "-f", "opencode run"], capture_output=True)
    time.sleep(1)
    start = time.time()
    env = os.environ.copy()
    env["PATH"] = f"{wd}/bin:" + env["PATH"]
    proc = subprocess.Popen(
        [OPENCODE, "run", "--agent", AGENT, "-m", MODEL,
         "--dangerously-skip-permissions", "--pure", "--title", title,
         PROMPTS[f"{test}-{arm}"]],
        cwd=str(wd), env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        proc.wait(timeout=600)
    except subprocess.TimeoutExpired:
        proc.kill()
    wall = int(time.time() - start)
    sid = session_by_title(title)
    m = measure(sid)
    ok, detail = success(test)
    audit = wd / "audit.log"
    audit_summary = summarize_audit(audit.read_text()) if audit.exists() else ""
    edits = audit.read_text().count("edit_ok") if audit.exists() else 0
    rejects = audit.read_text().count("edit_reject") if audit.exists() else 0
    renames = audit.read_text().count("rename\t") if audit.exists() else 0
    bypass = int(m.split("bypass=")[-1].split()[0]) if "bypass=" in m else -1
    skills = int(m.split("skills=")[-1]) if "skills=" in m else 0
    comp = "C" if ((edits + rejects + renames) > 0 and bypass == 0) else "NC"
    print(f"{title} wall={wall}s ok={ok} comp={comp} {m} {audit_summary} renames={renames} skills={skills} {detail}")
    return title, ok, comp


def session_by_title(title):
    con = sqlite3.connect(DB)
    row = con.execute(
        "select id from session where title like ? order by time_updated desc limit 1",
        (f"%{title}%",)).fetchone()
    con.close()
    return row[0] if row else None


def measure(sid):
    if not sid:
        return "input=NA"
    con = sqlite3.connect(DB)
    rows = con.execute("select data from part where session_id=?", (sid,)).fetchall()
    con.close()
    inp = outp = cost = 0
    bypass = 0
    refused = 0
    skills = 0
    for (r,) in rows:
        try:
            d = json.loads(r)
        except Exception:
            continue
        if "tokens" in d and "cost" in d:
            t = d["tokens"]
            inp += t.get("input", 0)
            outp += t.get("output", 0)
            cost += d.get("cost", 0)
        # bypass: direct edits outside the shims (compliance check). Command text
        # lives in state.input.command (top-level input is null). Common direct-edit
        # patterns (sed -i, perl -pi, python replace/re.sub) are also flagged.
        if d.get("type") == "tool" and str(d.get("tool", "")).lower() == "bash":
            st = d.get("state") or {}
            st_in = st.get("input") or {}
            cmd = str(st_in.get("command", "")) if isinstance(st_in, dict) else ""
            bypass_words = ("session apply", "session edit",
                            "sed -i", "perl -pi", "re.sub", "python3 - <<",
                            "python3 -c", "python3 -f", ".replace(")
            if any(w in cmd for w in bypass_words):
                bypass += 1
            if "mina skill" in cmd:
                skills += 1
            # a direct mina call that the sandbox wrapper REFUSED changed nothing
            # (exit 1, "only 'mina skill' is exposed") — harmless; subtract below
            out = str(st.get("output", ""))
            if "only 'mina skill' is exposed" in out:
                refused += 1
    eff = max(0, bypass - refused)
    return f"input={inp} output={outp} billed={inp+outp} cost={cost:.4f} bypass={eff} refused={refused} skills={skills}"


def summarize_audit(text):
    edits = text.count("edit_ok")
    rejects = text.count("edit_reject")
    reads = sum(1 for l in text.splitlines() if l.split("\t", 1)[0] in ("read", "read_head"))
    # tool split: apply-mode logs "edit_ok\tapply...", positional logs "edit_ok\tedit"
    apply_n = sum(1 for l in text.splitlines() if l.startswith("edit_ok\tapply"))
    pos_n = sum(1 for l in text.splitlines() if l.startswith("edit_ok\tedit"))
    drift = text.count("drift\t")
    return f"edits={edits} rejects={rejects} reads={reads} apply={apply_n} pos={pos_n} drift={drift}"


def success(test):
    def read(p):
        try:
            return (WORK / "ws" / p).read_text()
        except OSError:
            return ""
    if test == "t2":
        s = read("cfg.rs")
        ok = "timeout: 30000" in s and "max_retries: 5" in s
        return ("OK" if ok else "FAIL"), f"timeout30000={'timeout: 30000' in s} retries5={'max_retries: 5' in s}"
    if test == "t3":
        s = read("f1.rs") + read("f2.rs")
        ok = "USD" not in s and "price(" not in s and "JPY" in s and "amount(" in s
        return ("OK" if ok else "FAIL"), f"USD_left={'USD' in s} JPY={'JPY' in s} price_left={'price(' in s} amount={'amount(' in s}"
    if test == "t4":
        s = read("rename.ts")
        ok = "USD" not in s and "price" not in s and "JPY" in s and "amount" in s
        return ("OK" if ok else "FAIL"), f"USD_left={'USD' in s} JPY={'JPY' in s} price_left={'price' in s} amount={'amount' in s}"
    if test == "t5":
        s = read("utils.ts") + read("data.ts") + read("main.ts")
        ok = "USD" not in s and "price" not in s and "JPY" in s and "amount" in s
        return ("OK" if ok else "FAIL"), f"USD_left={'USD' in s} JPY={'JPY' in s} price_left={'price' in s} amount={'amount' in s}"
    if test == "t6":
        s = read("utils.ts") + read("data.ts") + read("main.ts")
        ok = "USD" not in s and "price" not in s and "JPY" in s and "amount" in s
        return ("OK" if ok else "FAIL"), f"USD_left={'USD' in s} JPY={'JPY' in s} price_left={'price' in s} amount={'amount' in s}"
    if test == "t7":
        s = read("cfg.rs")
        finals = [f"TARGET_{k} = {101 + k}" for k in range(1, 9)]
        ok = all(f in s for f in finals)
        missing = [f for f in finals if f not in s]
        return ("OK" if ok else "FAIL"), f"missing={missing or 'none'}"
    if test == "t8":
        s = read("cfg.rs")
        finals = [f"TARGET_{k} = {101 + k}" for k in range(1, 6)]
        ok = all(f in s for f in finals)
        missing = [f for f in finals if f not in s]
        return ("OK" if ok else "FAIL"), f"missing={missing or 'none'}"
    return "NA", ""


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("action", choices=["run", "stats", "fixture"])
    ap.add_argument("test", choices=["t1", "t2", "t3", "t4", "t5", "t6", "t7", "t8"])
    ap.add_argument("arm", choices=["A", "B"], nargs="?")
    ap.add_argument("idx", type=int, nargs="?")
    a = ap.parse_args()
    if a.action == "fixture":
        build_workdir(a.test, "A")
        print(f"fixture ready in {WORK}/ws")
        sys.exit(0)
    if a.action == "stats":
        sid = session_by_title(f"mab-{a.test}-{a.arm}-{a.idx}")
        print(f"mab-{a.test}-{a.arm}-{a.idx}: {measure(sid)}")
        sys.exit(0)
    run_once(a.test, a.arm, a.idx)