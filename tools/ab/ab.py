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
    # Test3: content-resolved (apply) vs positional (edit)
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
    bypass = int(m.split("bypass=")[-1]) if "bypass=" in m else -1
    comp = "C" if ((edits + rejects) > 0 and bypass == 0) else "NC"
    print(f"{title} wall={wall}s ok={ok} comp={comp} {m} {audit_summary} {detail}")
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
        # bypass: direct mina edit calls outside the shims (compliance check).
        # Command text lives in state.input.command (top-level input is null).
        if d.get("type") == "tool" and str(d.get("tool", "")).lower() == "bash":
            st = d.get("state") or {}
            st_in = st.get("input") or {}
            cmd = str(st_in.get("command", "")) if isinstance(st_in, dict) else ""
            if "session apply" in cmd or "session edit" in cmd:
                bypass += 1
    return f"input={inp} output={outp} billed={inp+outp} cost={cost:.4f} bypass={bypass}"


def summarize_audit(text):
    edits = text.count("edit_ok")
    rejects = text.count("edit_reject")
    reads = sum(1 for l in text.splitlines() if l.split("\t")[0] in ("read", "read_head"))
    drift = text.count("drift\t")
    return f"edits={edits} rejects={rejects} reads={reads} drift={drift}"


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
    return "NA", ""


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("action", choices=["run", "stats", "fixture"])
    ap.add_argument("test", choices=["t1", "t2", "t3"])
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