#!/usr/bin/env python3
"""opencode ツール制御 A/B ハーネス（minae CLI 経由のエディタ契約を実LLMで測る）。

使い方:
    ab.py run <t2|t3> <A|B> <idx>   — 1 run 実行（workdir /tmp/ab-run/ws、計測込み）
    ab.py stats <t2|t3> <A|B> <idx> — DB から計測だけ再計算

コマンド構成:
  - opencode.json に bash 専用 agent（native read/edit/glob/grep を無効化）を書き、
    `opencode run --agent <agent>` で実行。ファイル読み書きは全て minae CLI をラップ
    した shim（r / e）を通す。
  - 計測は ~/.local/share/opencode/opencode.db の step-finish（tokens/cost）から。
  - 設計意図: 手段の差（範囲read vs 全文read / apply vs edit / 拒否理由の有無）だけを
    隔離し、同一モデル・同一タスクで対比する。詳細は tools/ab/README.md。
"""
import argparse
import glob
import json
import os
import pathlib
import shutil
import sqlite3
import subprocess
import sys
import threading
import time

REPO = pathlib.Path(__file__).resolve().parent.parent.parent
SHIMS = pathlib.Path(__file__).resolve().parent / "shims"
WORK = pathlib.Path("/tmp/ab-run")
DB = os.path.expanduser("~/.local/share/opencode/opencode.db")


def _find_opencode():
    """env > PATH > mise installs。以前は 1.14.30 のパス直書きで、mise のピンが
    上がると runner が消える単一障害点になっていた。"""
    if os.environ.get("OPENCODE_BIN"):
        return os.environ["OPENCODE_BIN"]
    found = shutil.which("opencode")
    if found:
        return found
    pats = sorted(glob.glob(os.path.expanduser(
        "~/.local/share/mise/installs/opencode/*/opencode")))
    return pats[-1] if pats else "opencode"


OPENCODE = _find_opencode()
MINA = os.environ.get("MAB_MINABIN", str(REPO / "target" / "debug" / "minas"))
MODEL = os.environ.get("MAB_MODEL", "opencode/gpt-5.4-nano")
AGENT_FORCED = "minae-bash"    # ツールを shim に強制（naive / minas arm）
AGENT_NATIVE = "minae-native"  # ネイティブ read/edit/write（素朴な対照 arm）
AGENT = AGENT_FORCED            # 後方互換（他のテストは従来どおり）

# t11 の規模掃引: 「2ファイル合計行数」の 5 点。1200 は公開済み t11 と同じ規模
# （README の −41% の値はここに載る = 曲線との連続性を保つため中間点として残す）。
SCALES = (150, 600, 1200, 2400, 9600)
# drift の注入対象（decode の戻り行）。規模に依存しないのでどの点でも同じ。
T11_DRIFT_OLD = "Ok(Config { timeout, retries })"
T11_DRIFT_NEW = "Ok(Config { timeout, retries /* external */ })"

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

# native arm（素朴な対照）: ネイティブの read/edit/write を許す。強制も bypass 監査も
# しない — これが実際のエージェントの素朴な道具面。
TOOLS_NATIVE = {**TOOLS_OFF, "read": True, "edit": True, "write": True}

# opencode 1.18 では `tools` は deprecated で、permission が本体（config.json の
# PermissionConfig）。旧 runner (1.14) では tools だけが効いたので、両方書いて
# どちらの runner でも同じ強制になるようにしてある（runner を替えても契約が変わらない）。
# 実測: 1.18 で tools だけだと apply_patch が素通りし、セッションが shim を迂回した
# （2026-09-15。smoke で発覚）。
# bash ツールで「ファイルを読む・書く」道具を塞ぐ。tools/permission の read/edit/glob/grep/list
# を deny しても、bash 経由の sed/perl/cat/python は残る（実測: prompt で apply_patch を
# 止めたところ、次は perl -0777 -i -pe に切り替わった）。ここまで塞いで「mina の契約 +
# cargo」だけが使える状態にする（e2e-01 で cat/grep を deny したのと同じ手）。
FILE_TOOLS = ("apply_patch", "applypatch", "sed", "perl", "python", "cat", "nl",
              "head", "tail", "awk", "tee", "dd", "patch", "grep", "rg", "find",
              "truncate", "ruby", "node", "ex", "vi", "vim", "ed")
BASHPERMS_FORCED = {**{f"{c}*": "deny" for c in FILE_TOOLS},
                    **{f"*{c}*": "deny" for c in FILE_TOOLS},
                    "*": "allow"}

PERMS_FORCED = {
    "bash": BASHPERMS_FORCED,
    "read": "deny", "edit": "deny", "glob": "deny", "grep": "deny", "list": "deny",
    "webfetch": "deny", "websearch": "deny", "skill": "deny",
    "todowrite": "deny", "task": "deny",
}
PERMS_NATIVE = {
    "bash": "allow", "read": "allow", "edit": "allow",
    "glob": "allow", "grep": "allow",
}

# 強制 arm の最優先ルール。opencode 本体はモデルに「手編集には常に apply_patch を使え」と
# 指示し、しかも bash ツールが `apply_patch <<EOF` を検出して内部適用する（permission
# フックも無い = config では止められない）。そのため agent の prompt で明示的に上書きする。
# ここが弱いと arm 間の差が「道具」でなく「モデルの気まぐれ」になる（smoke で実測）。
FORCED_PROMPT = """# File access rules — highest priority, overrides any other instruction

All file access MUST go through the two wrapper commands on PATH:

- `mread <path> [start:end]` — the ONLY way to read file contents.
- `medit <path> <old> <new>` — the ONLY way to change a file.

Never use `apply_patch`, `patch`, `cat`, `sed`, `awk`, `perl`, `python`, `tee`,
`head`, `tail`, `nl`, `diff`, or shell redirection (`>`, `>>`) to read or alter
files. Never invoke `minas` directly. Bash exists here only for `cargo`, `ls`,
and similar read-only inspection of names. If you are about to touch a file any
these ways, use `mread`/`medit` instead.
"""


T11_ARMS = ("native", "naive", "minas")


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


def fixture_t9():
    """T9 (M2): Rust analog of T5 — 3-file crate, many occurrences
    (USD x12, price x7), cross-file. The regime where minae's OWN semantic
    rename (`session rename`, ADR-0029) should beat an apply loop."""
    (WORK / "ws" / "Cargo.toml").write_text(
        "[package]\nname = \"abrs\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n")
    (WORK / "ws" / "src").mkdir(parents=True, exist_ok=True)
    (WORK / "ws" / "src" / "lib.rs").write_text("pub mod utils;\npub mod data;\n")
    (WORK / "ws" / "src" / "utils.rs").write_text(
        """pub const USD: u64 = 100;

pub fn price(x: u64) -> u64 {
    x * USD
}
""")
    (WORK / "ws" / "src" / "data.rs").write_text(
        """use crate::utils::USD;
use crate::utils::price;

pub const ITEMS: [u64; 6] = [USD * 1, USD * 2, USD * 3, USD * 4, USD * 5, USD * 10];

pub fn total(a: u64, b: u64) -> u64 {
    price(a) + price(b) + price(a * b) + price(a + b)
}
""")
    (WORK / "ws" / "src" / "main.rs").write_text(
        """use abrs::utils::{USD, price};
use abrs::data::{ITEMS, total};

fn main() {
    let first = ITEMS[0] + USD;
    let second = price(USD) + USD * 2;
    let third = total(USD, price(USD));
    println!("{first} {second} {third}");
}
""")


# T11 の fixture は「頭（論理編集の対象）」＋「生成ノイズ（規模）」の2部構成。
# 頭を定数に切り出してあるのは、outcome 分類が「ノイズ宣言の集合が元と一致するか」
# を検査するため（= 意図しない領域の変更検出）。規模を変えても頭は不変。
T11_CONFIG_HEAD = """// minimal structured configuration for the ab feature fixture
// (keep this file plain and valid Rust — `cargo check` is the ground truth)

pub struct Config {
    pub timeout: u64,
    pub retries: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            timeout: 5000,
            retries: 3,
        }
    }
}

// decode: tiny JSON-ish parser (key presence decides the value)
pub fn decode(json: &str) -> Result<Config, String> {
    let timeout = if json.contains("\\\"timeout\\\"") { 5000 } else { 5000 };
    let retries = if json.contains("\\\"retries\\\"") { 3 } else { 3 };
    Ok(Config { timeout, retries })
}

// validate: bounds checks
pub fn validate(cfg: &Config) -> Result<(), String> {
    if cfg.timeout > 86_400_000 {
        return Err("timeout too large".to_string());
    }
    if cfg.retries > 10 {
        return Err("too many retries".to_string());
    }
    Ok(())
}
"""

T11_MAIN_HEAD = """mod config;
use config::{decode, validate, Config};

fn main() {
    let t = std::env::var("TIMEOUT_MS").ok().and_then(|s| s.parse().ok()).unwrap_or(5000);
    let r = std::env::var("MAX_RETRIES").ok().and_then(|s| s.parse().ok()).unwrap_or(3);
    let cfg = Config { timeout: t, retries: r };
    if let Err(e) = validate(&cfg) {
        eprintln!("config invalid: {e}");
        std::process::exit(1);
    }
    let _d = decode("{}");
    println!("timeout={} retries={}", cfg.timeout, cfg.retries);
}
"""


def _cfg_noise(i):
    return f"fn cfg_noise_{i}() -> u64 {{ {(i * 7 + 3) % 503} }}\n"


def _main_noise(i):
    return f"fn main_noise_{i}() -> u64 {{ {(i * 13 + 7) % 251} }}\n"


def t11_noise_counts(scale):
    """(cfg_noise 個数, main_noise 個数)。scale は 2 ファイル合計の行数。

    1 ファイル = 頭 + ノイズコメント 1 行 + ノイズ n 行。scale//2 に合わせるため
    コメント行も差し引く。"""
    per = max(40, scale // 2)
    return (max(0, per - T11_CONFIG_HEAD.count("\n") - 1),
            max(0, per - T11_MAIN_HEAD.count("\n") - 1))


def fixture_t11(scale=1200):
    """T11: general 2-file feature task (no LSP). Config gains max_conns:
    struct field + Default + decode + validate (+ env read in main.rs).

    規模はノイズ行数だけで変える（論理編集・ground truth は固定）。全ファイル read の
    費用は規模に比例し、窓 read は比例しない — 交点がこの曲線に出る。
    drift は shim 非依存の外部注入（_DriftInjection）で当てる（native arm でも動く）。
    `cargo check` は成功判定の一部。"""
    n_cfg, n_main = t11_noise_counts(scale)
    (WORK / "ws" / "Cargo.toml").write_text(
        "[package]\nname = \"abfeat\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n")
    (WORK / "ws" / "src").mkdir(parents=True, exist_ok=True)
    noise = "// ---- generated noise: keep this file large (contract realism) ----\n"
    (WORK / "ws" / "src" / "config.rs").write_text(
        T11_CONFIG_HEAD + noise + "".join(_cfg_noise(i) for i in range(41, 41 + n_cfg)))
    (WORK / "ws" / "src" / "main.rs").write_text(
        T11_MAIN_HEAD + noise + "".join(_main_noise(i) for i in range(31, 31 + n_main)))


def fixture_t10():
    """T10 (Stage 4): TypeScript analog of T9 — 3-file TS project, many
    occurrences (USD x21, price x9), cross-file with imports. Regime where
    minae's OWN semantic rename (`session rename` via typescript-language-server,
    ADR-0030 Stage 4) should beat an apply loop."""
    (WORK / "ws" / "package.json").write_text('{"name": "abts", "private": true}\n')
    (WORK / "ws" / "src").mkdir(parents=True, exist_ok=True)
    (WORK / "ws" / "src" / "utils.ts").write_text(
        """export const USD = 100;

export function price(x: number): number {
    return x * USD;
}
""")
    (WORK / "ws" / "src" / "data.ts").write_text(
        """import { USD } from "./utils";
import { price } from "./utils";

export const ITEMS = [USD * 1, USD * 2, USD * 3, USD * 4, USD * 5, USD * 10, USD * 11, USD * 12];

export function total(a: number, b: number): number {
    return price(a) + price(b) + price(a * b) + price(a + b);
}
""")
    (WORK / "ws" / "src" / "index.ts").write_text(
        """import { USD, price } from "./utils";
import { ITEMS, total } from "./data";

const first = ITEMS[0] + USD;
const second = price(USD) + USD * 2;
const third = total(USD, price(USD));
console.log(first + " " + second + " " + third);
""")


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
    # Test8: does `minae skill` steer AWAY from the positional trap when both
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

minae also provides skill guides: run `minae skill` for the index and
`minae skill <topic>` to read one (read/edit/errors). Consult the edit guide
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

    # Same real rejection messages in both arms; only the `minae skill` reference
    # differs (both may use mread/medit/mcheck).
    "t7-A": """Edit cfg.rs (a large generated file) in the current directory.
It contains 8 target constants, each currently equal to 100 + its number
(e.g. TARGET_3 = 103). Change EACH one so its value is incremented by 1
(TARGET_3 becomes 104). Updating all 8 is required.

Available file commands:
  mread <path> [start:end]   read lines of a file (numbered)
  medit <path> <old> <new>   replace the FIRST occurrence of <old> with <new>

minae also provides skill guides for how to use these tools:
run `minae skill` for the index and `minae skill <topic>` to read one
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

minae also provides skill guides for how to use these editing tools:
run `minae skill` to list the topics, and `minae skill <topic>` to read one
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
    # T9 (M2): Rust analog of T5. Arm A = apply loop (medit). Arm B = mrename
    # backed by minae's OWN `session rename` (ADR-0029) instead of the harness's
    # tsserver shim — measures whether the productized rename keeps the T5 win.
    "t9-A": """Refactor the Rust crate in the current directory
(src/utils.rs, src/data.rs, src/main.rs):
  - rename the constant USD to JPY (every occurrence in every file)
  - rename the function price to amount (its definition and every call)

You MUST use these commands for all file access (no other file commands):
  mread <path> [start:end]   read lines of a file (numbered)
  medit <path> <old> <new>   replace the FIRST occurrence of <old> with <new>
Work file by file, occurrence by occurrence. When done, verify that no
occurrence of "USD" or "price" remains in ANY of the .rs files and reply with
exactly: DONE""",
    # t11 の 3 arm（規模掃引）。native = opencode ネイティブ（read/edit/write 自由、
    # 強制なし）、naive = 全文 read + 無検証置換（cat + sed 契約）、minas = 範囲 read +
    # 検証付き apply。プロンプト本文（5項目・cargo check・DONE）は 3 arm で同一に保ち、
    # 差が「道具と読み方」だけに帰属するようにする。
    "t11-native": """Implement a small feature in the Rust crate (src/config.rs and
src/main.rs): add a connection limit to the Config struct. Do ALL of the
following:

1. In src/config.rs, add the field `pub max_conns: u32,` to struct Config.
2. In the Default impl, add `max_conns: 1024,` to the constructed Config.
3. In fn decode, read the json key "max_conns" (with the same default 1024)
   and include max_conns in the returned Config.
4. In fn validate, reject configs with max_conns > 65535 with
   Err("max_conns too large").
5. In src/main.rs, read the env var MAX_CONNS (default 1024) and pass
   max_conns into the Config built in main.

After the edits, run `cargo check` in this directory and make sure it passes.
Reply with exactly: DONE""",
    "t11-naive": """Implement a small feature in the Rust crate (src/config.rs and
src/main.rs): add a connection limit to the Config struct. Do ALL of the
following:

1. In src/config.rs, add the field `pub max_conns: u32,` to struct Config.
2. In the Default impl, add `max_conns: 1024,` to the constructed Config.
3. In fn decode, read the json key "max_conns" (with the same default 1024)
   and include max_conns in the returned Config.
4. In fn validate, reject configs with max_conns > 65535 with
   Err("max_conns too large").
5. In src/main.rs, read the env var MAX_CONNS (default 1024) and pass
   max_conns into the Config built in main.

After the edits, run `cargo check` in this directory and make sure it passes.
Reply with exactly: DONE

Available file commands (use ONLY these for file access):
  mread <path>               print the entire file
  medit <path> <old> <new>   replace the first occurrence of <old> with <new>
If an edit reports "text not found", re-read the file and retry.""",
    "t11-minas": """Implement a small feature in the Rust crate (src/config.rs and
src/main.rs): add a connection limit to the Config struct. Do ALL of the
following:

1. In src/config.rs, add the field `pub max_conns: u32,` to struct Config.
2. In the Default impl, add `max_conns: 1024,` to the constructed Config.
3. In fn decode, read the json key "max_conns" (with the same default 1024)
   and include max_conns in the returned Config.
4. In fn validate, reject configs with max_conns > 65535 with
   Err("max_conns too large").
5. In src/main.rs, read the env var MAX_CONNS (default 1024) and pass
   max_conns into the Config built in main.

After the edits, run `cargo check` in this directory and make sure it passes.
Reply with exactly: DONE

Available file commands (use ONLY these for file access):
  mread <path> [start:end]   read numbered lines of a file (JSON: lines with n/text)
  medit <path> <old> <new>   verified content-based apply (opens, locates, saves)
  mcheck <path>              print the file checksum
Use mread with a range to read only the lines you need before each edit. If an
edit is rejected, re-read the affected range and retry. You can pass the line
text you saw verbatim as <old> — it is used to locate the edit.""",
    "t9-B": """Refactor the Rust crate in the current directory
(src/utils.rs, src/data.rs, src/main.rs):
  - rename the constant USD to JPY (every occurrence in every file)
  - rename the function price to amount (its definition and every call)

You MUST use these commands for all file access (no other file commands):
  mread <path> [start:end]   read lines of a file (numbered)
  mrename <path> <old> <new> perform a LANGUAGE-AWARE RENAME of the symbol whose
     first identifier occurrence is <old> in <path> — minae (`session rename`)
     finds and updates every reference (definition, calls, uses) across files
     in one call and saves. One mrename per symbol is enough; do not loop
     over occurrences. First mread the files to see the crate, then mrename
     for USD->JPY and for price->amount, then verify with a final mread that
     no "USD" or "price" remains in ANY .rs file, and reply with exactly: DONE""",
    "t10-A": """Refactor the TypeScript project in the current directory
(src/utils.ts, src/data.ts, src/index.ts):
  - rename the constant USD to JPY (every occurrence in every file)
  - rename the function price to amount (its definition and every call)

You MUST use these commands for all file access (no other file commands):
  mread <path> [start:end]   read lines of a file (numbered)
  medit <path> <old> <new>   replace the FIRST occurrence of <old> with <new>
Work file by file, occurrence by occurrence. When done, verify that no
occurrence of "USD" or "price" remains in ANY of the .ts files and reply with
exactly: DONE""",
    "t10-B": """Refactor the TypeScript project in the current directory
(src/utils.ts, src/data.ts, src/index.ts):
  - rename the constant USD to JPY (every occurrence in every file)
  - rename the function price to amount (its definition and every call)

You MUST use these commands for all file access (no other file commands):
  mread <path> [start:end]   read lines of a file (numbered)
  mrename <path> <old> <new> perform a LANGUAGE-AWARE RENAME of the symbol whose
     first identifier occurrence is <old> in <path> — the language server
     (typescript-language-server via minae `session rename`) finds and updates
     every reference (definition, calls, imports, uses) across files in one
     call and saves. One mrename per symbol is enough; do not loop over
     occurrences. First mread the files to see the project, then mrename for
     USD->JPY and for price->amount, then verify with a final mread that no
     "USD" or "price" remains in ANY .ts file, and reply with exactly: DONE""",
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

def build_workdir(test, arm, scale=1200, drift=False):
    wd = WORK / "ws"
    if wd.exists():
        shutil.rmtree(wd)
    (wd / "bin").mkdir(parents=True)
    (wd / "audit.log").write_text("")
    # 3 arm の agent 設定。native はネイティブ read/edit/write を許す（強制なし）。
    cfg = {"agent": {
        AGENT_FORCED: {
            "description": "bash-only agent（shim 強制）",
            "prompt": str(wd / "forced-prompt.md"),
            "tools": TOOLS_OFF,          # 旧 runner (1.14) 用
            "permission": PERMS_FORCED,  # 1.18 用
            "maxSteps": 30,
        },
        AGENT_NATIVE: {
            "description": "native tools（素朴な対照・強制なし）",
            "tools": TOOLS_NATIVE,
            "permission": PERMS_NATIVE,
            "maxSteps": 30,
        },
    }}
    (wd / "opencode.json").write_text(json.dumps(cfg, indent=2))
    (wd / "forced-prompt.md").write_text(FORCED_PROMPT)
    # mode resolution
    read_mode = "range"
    if test == "t2":
        edit_mode = "apply-generic" if arm == "B" else "apply"
    elif test in ("t1", "t3"):
        edit_mode = "edit" if arm == "B" else "apply"
    else:
        edit_mode = "apply"
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

    # native arm は shim を置かない（PATH に何も足さない = 素朴な道具だけで解く）
    if not (test == "t11" and arm == "native"):
        wr("mread", "read_shim.py", read_mode)
        wr("medit", "edit_shim.py", edit_mode)
        wr("mcheck", "check_shim.py", "x")
    if test == "t11":
        # 規模掃引の 3 arm。native は上の guard で shim 無し。
        #  drift は run_once の外部注入（shim 非依存）が当てる — 3 arm に同じ機構。
        if arm == "naive":
            wr("mread", "read_naive_shim.py", "x")
            wr("medit", "edit_naive_shim.py", "x")
            (wd / "bin" / "mcheck").unlink()
        elif arm == "minas":
            wr("mread", "read_shim.py", "range")
            wr("medit", "edit_shim.py", "apply")
        if arm != "native":
            # 実物の `minas` が PATH にあると、エージェントが shim を飛ばして直接読む
            # （smoke で実際に起きた: `minas read src/config.rs`）。shim の中で使う
            # `minas` は絶対パスなので、PATH 側をブロックしても影響しない。
            p = wd / "bin" / "minas"
            p.write_text(
                "#!/usr/bin/env bash\n"
                'echo "error: use mread/medit for file access" >&2\n'
                "exit 1\n")
            p.chmod(0o755)
    if test == "t8":
        # both edit tools present: medit (positional) and mapply (content-resolved)
        wr("mapply", "edit_shim.py", "apply")
    if (test == "t6") or (test in ("t4", "t5") and arm == "B") or (test in ("t9", "t10") and arm == "B"):
        if test in ("t9", "t10"):
            # mrename backed by minae's OWN session rename (M2, ADR-0029)
            p = wd / "bin" / "mrename"
            p.write_text(
                f"#!/usr/bin/env bash\n{env} exec python3 {SHIMS}/rename_minae_shim.py \"$@\"\n")
            p.chmod(0o755)
        else:
            p = wd / "bin" / "mrename"
            p.write_text(
                f"#!/usr/bin/env bash\n{env}{lsp_env} exec python3 {SHIMS}/rename_shim.py \"$@\"\n")
            p.chmod(0o755)
    if test in ("t6", "t7", "t8") and arm == "A":
        p = wd / "bin" / "minae"
        p.write_text(
            f"#!/usr/bin/env bash\n"
            f"if [ \"${{1:-}}\" = \"skill\" ]; then exec {MINA} \"$@\"; fi\n"
            f"echo \"error: only 'minae skill' is exposed here; use medit/mread for file operations\" >&2\n"
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
    elif test == "t9":
        fixture_t9()
    elif test == "t10":
        fixture_t10()
    elif test == "t11":
        fixture_t11(scale)
    if test == "t7":
        fixture_t7()
    if test == "t8":
        fixture_t8()
    (wd / "task.txt").write_text(PROMPTS[f"{test}-{arm}"])
    return wd


# ---------- run ----------

class _DriftInjection:
    """外部プロセスによる書き換えを 1 回だけ注入する（LLM には予告しない）。

    shim 非依存 — naive/minas は shim の内側で注入できたが、native arm には shim が
    無いので、ファイル監視のポーラーで 3 arm に同じ機構を当てる（外部エディタ/他プロセス
    による変更の再現。より現実に近い）。

    対象ファイルのいずれかが変化し、そのあと 1 ポーリング静かになってから
    T11_DRIFT_OLD -> T11_DRIFT_NEW を適用する。モデルが記憶している anchor が
    ここで古くなるので、次にそれを <old> として使うと見つからない（= drift）。
    注入は audit.log に 1 行残す。
    """

    def __init__(self, paths, audit, old, new, interval=0.1):
        self.paths, self.audit, self.old, self.new = paths, audit, old, new
        self.interval = interval
        self._stop = threading.Event()

    @staticmethod
    def _snap(p):
        try:
            st = p.stat()
            return (st.st_mtime_ns, st.st_size)
        except OSError:
            return None

    def _run(self):
        prev = {p: self._snap(p) for p in self.paths}
        changed = False
        while not self._stop.is_set():
            time.sleep(self.interval)
            now = {p: self._snap(p) for p in self.paths}
            if any(now[p] != prev[p] for p in self.paths):
                changed = True
            elif changed:
                break          # 変化のあと 1 ポーリング静かになった → 注入する
            prev = now
        if self._stop.is_set():
            return
        for p in self.paths:
            try:
                s = p.read_text()
            except OSError:
                continue
            if self.old in s:
                try:
                    p.write_text(s.replace(self.old, self.new, 1))
                    with open(self.audit, "a") as f:
                        f.write(f"drift\t{p.name}\n")
                except OSError:
                    pass
                return

    def start(self):
        threading.Thread(target=self._run, daemon=True).start()

    def stop(self):
        self._stop.set()


def run_once(test, arm, idx, scale=1200, drift=False):
    wd = build_workdir(test, arm, scale, drift)
    title = f"mab-{test}-s{scale}-d{int(drift)}-{arm}-{idx}"
    agent = AGENT_NATIVE if (test == "t11" and arm == "native") else AGENT_FORCED
    subprocess.run(["pkill", "-f", "opencode run"], capture_output=True)
    time.sleep(1)
    inj = None
    if drift:
        inj = _DriftInjection(
            [wd / "src" / "config.rs", wd / "src" / "main.rs"],
            wd / "audit.log", T11_DRIFT_OLD, T11_DRIFT_NEW)
        inj.start()
    start = time.time()
    env = os.environ.copy()
    env["PATH"] = f"{wd}/bin:" + env["PATH"]
    # opencode 1.18 は env の PWD を見てプロジェクトディレクトリを決める（cwd では
    # ない）。Python の env には親 shell の PWD が残っているので、ここで合わせないと
    # セッションが呼び出し元のリポジトリに作られ、最初のメッセージで server error に
    # なる（opencode 1.14 では起きなかった。1.18 で runner を戻した際に判明）。
    env["PWD"] = str(wd)
    # opencode の bash ツールは zsh。~/.zshrc の `mise activate zsh` が PATH を
    # 組み直すので wd/bin が後ろに回り、実物の `minas`（~/.cargo/bin）が shim より
    # 先に解決される（smoke で発覚: `minas apply --whole-stdin` が実行された）。
    # 空の ZDOTDIR を渡して PATH を継承させ、shim を確実に先に解決させる。
    zdir = wd / "zsh"
    zdir.mkdir(exist_ok=True)
    for f in (".zshrc", ".zprofile", ".zshenv", ".zlogin"):
        (zdir / f).write_text("")
    env["ZDOTDIR"] = str(zdir)
    with open(wd / "agent.log", "w") as log:
        proc = subprocess.Popen(
            [OPENCODE, "run", "--agent", agent, "-m", MODEL,
             "--auto", "--pure", "--title", title,
             PROMPTS[f"{test}-{arm}"]],
            cwd=str(wd), env=env, stdout=log, stderr=subprocess.STDOUT)
        try:
            proc.wait(timeout=600)
        except subprocess.TimeoutExpired:
            proc.kill()
    wall = int(time.time() - start)
    if inj:
        inj.stop()
    sid = session_by_title(title)
    m = measure(sid, native=(test == "t11" and arm == "native"))
    if test == "t11":
        label, detail = outcome_t11(scale)
    else:
        ok_, detail = success(test)
        label = "GREEN" if ok_ == "OK" else "LOUD-FAIL"
    ok = "OK" if label == "GREEN" else "FAIL"
    audit = wd / "audit.log"
    audit_summary = summarize_audit(audit.read_text()) if audit.exists() else ""
    edits = audit.read_text().count("edit_ok") if audit.exists() else 0
    rejects = audit.read_text().count("edit_reject") if audit.exists() else 0
    renames = audit.read_text().count("rename\t") if audit.exists() else 0
    bypass = int(m.split("bypass=")[-1].split()[0]) if "bypass=" in m else -1
    skills = int(m.split("skills=")[-1]) if "skills=" in m else 0
    comp = "C" if ((edits + rejects + renames) > 0 and bypass == 0) else "NC"
    if test == "t11" and arm == "native":
        comp = "N/A"   # native は強制も bypass 監査も無い（素朴な対照）
    print(f"{title} wall={wall}s ok={ok} outcome={label} comp={comp} {m} "
          f"{audit_summary} renames={renames} skills={skills} {detail}")
    return title, ok, comp


def session_by_title(title):
    con = sqlite3.connect(DB)
    row = con.execute(
        "select id from session where title like ? order by time_updated desc limit 1",
        (f"%{title}%",)).fetchone()
    con.close()
    return row[0] if row else None


def measure(sid, native=False):
    if not sid:
        return "input=NA"
    con = sqlite3.connect(DB)
    rows = con.execute("select data from part where session_id=?", (sid,)).fetchall()
    con.close()
    inp = outp = cost = 0
    bypass = 0
    refused = 0
    skills = 0
    steps = 0
    for (r,) in rows:
        try:
            d = json.loads(r)
        except Exception:
            continue
        if "tokens" in d and "cost" in d:
            steps += 1
            t = d["tokens"]
            inp += t.get("input", 0)
            outp += t.get("output", 0)
            cost += d.get("cost", 0)
        # bypass: direct edits outside the shims (compliance check). Command text
        # lives in state.input.command (top-level input is null). Common direct-edit
        # patterns (sed -i, perl -pi, python replace/re.sub) are also flagged.
        # bypass: 直接編集（shim 外）の検出。command テキストは state.input.command に
        # ある（トップの input は null）。ネイティブの書き込み系ツールもカウントする
        # （1.18 で apply_patch が permission を素通りした穴をここで可視化する）。
        if not native and str(d.get("tool", "")).lower() in (
                "apply_patch", "patch", "write", "edit"):
            bypass += 1
        if not native and d.get("type") == "tool" and str(d.get("tool", "")).lower() == "bash":
            st = d.get("state") or {}
            st_in = st.get("input") or {}
            cmd = str(st_in.get("command", "")) if isinstance(st_in, dict) else ""
            bypass_words = ("session apply", "session edit", "session rename",
                            "minas apply", "minas edit", "minas rename",
                            "apply_patch", "applypatch", "perl -", "ruby -",
                            "sed -i", "sed -n", "sed ", "cat ", "nl ", "tee ",
                            "python3 ", "re.sub", ">>", "> src/", ".replace(")
            if any(w in cmd for w in bypass_words):
                bypass += 1
            if "minae skill" in cmd:
                skills += 1
            # a direct minae call that the sandbox wrapper REFUSED changed nothing
            # (exit 1, "only 'minae skill' is exposed") — harmless; subtract below
            out = str(st.get("output", ""))
            if "only 'minae skill' is exposed" in out:
                refused += 1
    eff = max(0, bypass - refused)
    return (f"input={inp} output={outp} billed={inp+outp} cost={cost:.4f} "
            f"steps={steps} bypass={eff} refused={refused} skills={skills}")


def summarize_audit(text):
    edits = text.count("edit_ok")
    rejects = text.count("edit_reject")
    reads = sum(1 for l in text.splitlines() if l.split("\t", 1)[0] in ("read", "read_head", "read_full"))
    # tool split: apply-mode logs "edit_ok\tapply...", positional logs "edit_ok\tedit"
    apply_n = sum(1 for l in text.splitlines() if l.startswith("edit_ok\tapply"))
    pos_n = sum(1 for l in text.splitlines() if l.startswith("edit_ok\tedit"))
    drift = text.count("drift\t")
    return f"edits={edits} rejects={rejects} reads={reads} apply={apply_n} pos={pos_n} drift={drift}"


def cargo_check_ok():
    """Ground truth for T11: the fixture crate must still compile."""
    try:
        p = subprocess.run(["cargo", "check", "--offline"], cwd=str(WORK / "ws"),
                           capture_output=True, text=True, timeout=120)
        return p.returncode == 0
    except Exception:
        return False


def outcome_t11(scale):
    """t11 の run を 4 分類する（人手なし・決定論的）。

      CORRUPT   — 生成ノイズ（= 意図しない領域）の宣言が欠けている / 壊れている
      LOUD-FAIL — ビルドは通らない（検出可能な失敗）
      GREEN     — 5 項目すべて + `cargo check` green + ノイズ無傷
      INCOMPLETE— ビルドは通るが項目が欠けている（取りこぼし）

    ノイズ宣言の集合は規模から再計算できる（生成が決定的）ので、fixture の期待値を
    ここで組み立てて現物と比べる。drift の注入先は decode の戻り行でノイズ行ではないため、
    注入分はこの検査に混入しない。
    """
    def read(p):
        try:
            return (WORK / "ws" / p).read_text()
        except OSError:
            return ""
    cf, mn = read("src/config.rs"), read("src/main.rs")
    n_cfg, n_main = t11_noise_counts(scale)
    # ponytail: 部分集合 + 行数の下限で見る（ノイズ行の重複挿入は検出しない）。
    # 何らかの解析が要るようになったら行の多重度まで見る。
    want_cfg = {_cfg_noise(i).rstrip("\n") for i in range(41, 41 + n_cfg)}
    want_main = {_main_noise(i).rstrip("\n") for i in range(31, 31 + n_main)}
    have_cfg, have_main = set(cf.splitlines()), set(mn.splitlines())
    scope_ok = (want_cfg <= have_cfg and want_main <= have_main
                and len(have_cfg) >= len(want_cfg) and len(have_main) >= len(want_main))
    facts = [
        "max_conns: u32" in cf,
        "max_conns: 1024" in cf,
        "cfg.max_conns" in cf,
        "65535" in cf,
        ("max_conns" in mn and "MAX_CONNS" in mn),
    ]
    comp = cargo_check_ok()
    drift_applied = T11_DRIFT_NEW in cf
    detail = (f"facts={sum(facts)}/5 noise={'ok' if scope_ok else 'DAMAGED'} "
              f"cargo={'ok' if comp else 'FAIL'} drift={'yes' if drift_applied else 'no'} "
              f"lines={cf.count(chr(10))}+{mn.count(chr(10))}")
    if not scope_ok:
        return "CORRUPT", detail
    if not comp:
        return "LOUD-FAIL", detail
    if all(facts):
        return "GREEN", detail
    return "INCOMPLETE", detail


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
    if test == "t11":
        cf = read("src/config.rs")
        mn = read("src/main.rs")
        need = [
            "max_conns: u32" in cf,             # 1 struct field
            "max_conns: 1024" in cf,            # 2 default (+3 decode default)
            "cfg.max_conns" in cf,              # 3/4 decode wires it; validate bounds it
            "65535" in cf,                      # 4 validate bound
            "max_conns" in mn and "MAX_CONNS" in mn,  # 5 env read + wired
        ]
        comp = cargo_check_ok()
        ok = all(need) and comp
        return ("OK" if ok else "FAIL"), f"field={'max_conns: u32' in cf} default={'max_conns: 1024' in cf} cfg={'cfg.max_conns' in cf} bound={'65535' in cf} main={'max_conns' in mn and 'MAX_CONNS' in mn} cargo={'OK' if comp else 'FAIL'}"
    if test in ("t9", "t10"):
        ext = 'ts' if test == 't10' else 'rs'
        last = 'index' if test == 't10' else 'main'
        s = read(f"src/utils.{ext}") + read(f"src/data.{ext}") + read(f"src/{last}.{ext}")
        ok = "USD" not in s and "price" not in s and "JPY" in s and "amount" in s
        return ("OK" if ok else "FAIL"), \
            f"USD_left={'USD' in s} JPY={'JPY' in s} price_left={'price' in s} amount={'amount' in s}"
    return "NA", ""


def _apply_t11_ground_truth():
    """5 項目の正解を手で当てる（selftest 専用。GREEN を作るため）。"""
    wd = WORK / "ws"
    p = wd / "src" / "config.rs"
    cf = p.read_text()
    cf = cf.replace("    pub retries: u32,\n", "    pub retries: u32,\n    pub max_conns: u32,\n")
    cf = cf.replace("            retries: 3,\n", "            retries: 3,\n            max_conns: 1024,\n")
    cf = cf.replace("    Ok(Config { timeout, retries })\n",
                    "    let max_conns = if json.contains(\"max_conns\") { 1024 } else { 1024 };\n"
                    "    Ok(Config { timeout, retries, max_conns })\n")
    cf = cf.replace('    if cfg.retries > 10 {\n        return Err("too many retries".to_string());\n    }\n',
                    '    if cfg.retries > 10 {\n        return Err("too many retries".to_string());\n    }\n'
                    '    if cfg.max_conns > 65535 {\n        return Err("max_conns too large".to_string());\n    }\n')
    p.write_text(cf)
    mn = wd / "src" / "main.rs"
    mn.write_text(mn.read_text().replace(
        "    let cfg = Config { timeout: t, retries: r };\n",
        '    let c = std::env::var("MAX_CONNS").ok().and_then(|s| s.parse().ok()).unwrap_or(1024);\n'
        '    let cfg = Config { timeout: t, retries: r, max_conns: c };\n'))


def _selftest_outcome():
    """規模掃引の計器の自己検証（LLM 不要・daemon 不要・数十秒）。

    分類器が「分類できる」ことと「間違った入力で間違った答えを出す」ことを確かめる。
    4 分類すべてを決定論的に作って確認する（ここが赤いまま掃引を回さない）。
    """
    wd = WORK / "ws"
    failed = []

    def check(name, got, want):
        ok = got == want
        print(f"  {'PASS' if ok else 'FAIL'}  {name:26s} -> {got!r:14s} (want {want!r})")
        if not ok:
            failed.append(name)

    for scale in SCALES:
        build_workdir("t11", "minas", scale, False)
        cfg = (wd / "src" / "config.rs").read_text()
        mn = (wd / "src" / "main.rs").read_text()
        check(f"行数 cfg scale={scale}", cfg.count("\n"), scale // 2)
        check(f"行数 main scale={scale}", mn.count("\n"), scale // 2)
        check(f"未編集 scale={scale}", outcome_t11(scale)[0], "INCOMPLETE")

    p = wd / "src" / "config.rs"
    build_workdir("t11", "minas", 150, False)
    p.write_text(p.read_text().replace(_cfg_noise(45).rstrip("\n"),
                                       "fn cfg_noise_45() -> i64", 1))
    check("ノイズ改変 -> CORRUPT", outcome_t11(150)[0], "CORRUPT")

    build_workdir("t11", "minas", 150, False)
    _apply_t11_ground_truth()
    check("ground truth -> GREEN", outcome_t11(150)[0], "GREEN")

    build_workdir("t11", "minas", 150, False)
    # ノイズは無傷のままビルドだけ落とす（ノイズを消すと CORRUPT が先に確定する）
    mn_ = wd / "src" / "main.rs"
    mn_.write_text(mn_.read_text() + '\nfn broken() { let x: u32 = "s"; }\n')
    check("ビルド不能 -> LOUD-FAIL", outcome_t11(150)[0], "LOUD-FAIL")

    build_workdir("t11", "minas", 150, True)
    inj = _DriftInjection([p, wd / "src" / "main.rs"], wd / "audit.log",
                          T11_DRIFT_OLD, T11_DRIFT_NEW)
    inj.start()
    time.sleep(0.5)
    p.write_text(p.read_text().replace("    pub retries: u32,",
                                       "    pub retries: u32,\n    pub tmp: u8,", 1))
    time.sleep(1.2)
    inj.stop()
    check("drift 注入が当たる", "drift=yes" in outcome_t11(150)[1], True)

    print(f"\n{len(failed)} failure(s)" if failed else "\nselftest OK")
    return 1 if failed else 0


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("action", choices=["run", "stats", "fixture", "selftest"])
    ap.add_argument("test", nargs="?",
                    choices=["t1", "t2", "t3", "t4", "t5", "t6", "t7", "t8", "t9", "t10", "t11"])
    ap.add_argument("arm", nargs="?", help="t11 は native|naive|minas、他は A|B")
    ap.add_argument("idx", type=int, nargs="?")
    ap.add_argument("--scale", type=int, default=1200,
                    choices=list(SCALES), help="t11 の 2 ファイル合計行数")
    ap.add_argument("--drift", type=int, default=0, help="1 で外部書き換えを注入")
    a = ap.parse_args()
    if a.action == "selftest":
        sys.exit(_selftest_outcome())
    if a.action in ("run", "fixture"):
        valid = T11_ARMS if a.test == "t11" else ("A", "B")
        if a.arm not in valid:
            ap.error(f"arm must be one of {valid} for {a.test}")
    if a.action == "fixture":
        build_workdir(a.test, a.arm, a.scale, bool(a.drift))
        for f in ("src/config.rs", "src/main.rs"):
            p = WORK / "ws" / f
            if p.exists():
                print(f"{f}: {p.read_text().count(chr(10))} lines")
        print(f"fixture ready in {WORK}/ws (scale={a.scale} drift={bool(a.drift)})")
        sys.exit(0)
    if a.action == "stats":
        title = f"mab-{a.test}-s{a.scale}-d{int(a.drift)}-{a.arm}-{a.idx}"
        sid = session_by_title(title)
        print(f"{title}: {measure(sid)}")
        sys.exit(0)
    run_once(a.test, a.arm, a.idx, a.scale, bool(a.drift))