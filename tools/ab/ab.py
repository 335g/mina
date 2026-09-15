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
import re
import shutil
import sqlite3
import subprocess
import sys
import threading
import time

REPO = pathlib.Path(__file__).resolve().parent.parent.parent
SHIMS = pathlib.Path(__file__).resolve().parent / "shims"
WORK = pathlib.Path("/tmp/ab-run")
# 測定の履歴。1 run = 1 行の JSONL で git 管理下に置く（数字は binary と runner に
# 依存するので、commit / generation / runner 版を各業に入れておかないと比較できない）。
HISTORY = pathlib.Path(__file__).resolve().parent.parent.parent / "docs" / "benchmarks" / "l2" / "history.jsonl"
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

# --- t11 の 3 arm は「bash を外す」で強制する（2026-09-15 の方針変更）---
#
# 理由（すべて実測）: opencode 1.18 では
#   (1) `apply_patch` は PATH のコマンドではなく bash ツール内部の特例で、permission
#       フックを持たない。しかも opencode 自身が「手編集には常に apply_patch を使え」と
#       モデルに指示している。
#   (2) bash の permission deny パターンが効かない（`{"cat":"deny"}` も
#       `{"cat*":"deny"}` も、--auto の有無に関わらず `cat` が実行された）。
#   (3) prompt で apply_patch を禁じると `perl -0777 -i -pe` に切り替わった。
# つまり bash が 1 つでもあると mina の道具面は強制できず、測れるのは「モデルの気まぐれ」
# になる（6 run 中 5 run が何らかの形で迂回）。そこで bash を tool ごと落とし、ファイル
# アクセスは opencode の custom tool（.opencode/tools/*.ts）だけにする。custom tool は
# 同名の組み込み tool を上書きでき、`tools: {bash: false}` は実測で効いた。
STRICT_OFF = {**TOOLS_OFF, "bash": False, "task": False}
# native arm: ネイティブの read/edit/write/glob/grep だけ。bash も task も無し
# （= 素朴な道具面からシェルの万能さだけを除いた対照。厳密さを優先し、3 arm すべてで
# bash を使わない）。
TOOLS_NATIVE = {**STRICT_OFF, "read": True, "edit": True, "write": True,
                "glob": True, "grep": True}
# 強制 arm: ファイル系のネイティブ tool を全部落とし、custom tool だけ残す。
TOOLS_STRICT = STRICT_OFF

# 旧 runner (1.14) / 旧テスト (t1-t10) 用。t11 ではもう使わない。
PERMS_NATIVE = {
    "bash": "deny", "task": "deny",
    "read": "allow", "edit": "allow", "glob": "allow", "grep": "allow",
}
PERMS_STRICT = {
    "bash": "deny", "task": "deny",
    "read": "deny", "edit": "deny", "glob": "deny", "grep": "deny",
    "list": "deny", "webfetch": "deny", "websearch": "deny",
    "skill": "deny", "todowrite": "deny",
}
PERMS_FORCED = {**PERMS_STRICT, "bash": "allow"}   # 旧テスト用（bash + shim）

# 強制 arm の補足 prompt。opencode 本体はモデルに「手編集には常に apply_patch を使え」と
# 指示するが、強制 arm には bash も apply_patch も無い。そのままだと存在しない道具を
# 探しに行くので、使える道具をここで明示する。
STRICT_PROMPT = """# File access in this project

There is no shell in this session. All file access goes through the two tools
`mread` and `medit` — use them; do not attempt anything else.
"""


T11_ARMS = ("native", "naive", "minas")

# ---------- t11 の custom tool ----------
#
# .opencode/tools/<name>.ts のファイル名が tool 名になる。ここでは shim を subprocess で
# 呼ぶだけの薄いラッパ（実装は既存の python shim 1 本のまま = audit.log もそのまま使える）。
# shim が非ゼロで終わったら throw して tool error にする — 却下が「見える」ことが mina の
# 契約の一部なので、黙って失敗させない。
TOOL_TS_HEAD = """import { tool } from "@opencode-ai/plugin"

const MINA = %(mina)r
const AUDIT = %(audit)r
const SHIM = %(shim)r
const PY = %(py)r

function run(argv: string[], cwd: string) {
  const p = Bun.spawnSync({
    cmd: [PY, SHIM, ...argv],
    cwd,
    env: { ...process.env, MAB_MINABIN: MINA, MAB_AUDIT: AUDIT },
  })
  const out = (p.stdout?.toString() ?? "") + (p.stderr?.toString() ?? "")
  if (p.exitCode !== 0) {
    throw new Error(out.trim() || `failed with exit ${p.exitCode}`)
  }
  return out
}
"""


#: arm -> (read shim, read mode, edit shim, edit mode, tool descriptions)
T11_TOOLS = {
    "naive": {
        "read": ("read_naive_shim.py", "x"),
        "edit": ("edit_naive_shim.py", "x"),
        "read_desc": "Read a file. Returns the whole file, as-is.",
        "edit_desc": ("Replace the first occurrence of `old` with `new` in `path`. "
                      "Both are literal text."),
    },
    "minas": {
        "read": ("read_shim.py", "range"),
        "edit": ("edit_shim.py", "apply"),
        "read_desc": ("Read lines of a file. Returns numbered lines (n: text). "
                      "Give `start`/`end` to read only the lines you need; "
                      "omitting both returns just the head of the file."),
        "edit_desc": ("Apply a content-based edit: replace `old` with `new` in "
                      "`path`. `old` must be text you actually saw in the file; "
                      "the edit is verified against the current file contents and "
                      "is rejected if it no longer matches. If it is rejected, "
                      "re-read the affected lines and retry."),
    },
}


def write_t11_tools(wd, arm):
    """強制 arm の custom tool を生成する。native arm はネイティブ tool を使うので何もしない。"""
    if arm not in T11_TOOLS:
        return
    spec = T11_TOOLS[arm]
    tdir = wd / ".opencode" / "tools"
    tdir.mkdir(parents=True, exist_ok=True)
    base = dict(mina=str(MINA), audit=str(wd / "audit.log"), py=sys.executable,
                shim=str(SHIMS))
    rshim, rmode = spec["read"]
    eshim, emode = spec["edit"]
    (tdir / "mread.ts").write_text(TOOL_TS_HEAD % {**base, "shim": str(SHIMS / rshim)} + f"""
export default tool({{
  description: {spec["read_desc"]!r},
  args: {{
    path: tool.schema.string().describe("file path relative to the project root"),
    start: tool.schema.number().optional().describe("first line (1-based)"),
    end: tool.schema.number().optional().describe("last line, inclusive"),
  }},
  async execute(args, ctx) {{
    const argv = [{rmode!r}, args.path]
    if (args.start !== undefined || args.end !== undefined) {{
      argv.push(`${{args.start ?? 1}}:${{args.end ?? ""}}`)
    }}
    return run(argv, ctx.directory)
  }},
}})
""")
    (tdir / "medit.ts").write_text(TOOL_TS_HEAD % {**base, "shim": str(SHIMS / eshim)} + f"""
export default tool({{
  description: {spec["edit_desc"]!r},
  args: {{
    path: tool.schema.string().describe("file path relative to the project root"),
    old: tool.schema.string().describe("exact text currently in the file"),
    new: tool.schema.string().describe("replacement text"),
  }},
  async execute(args, ctx) {{
    return run([{emode!r}, args.path, args.old, args.new], ctx.directory)
  }},
}})
""")


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


# t11 のタスク本文。3 arm で完全に同一。
# 旧版は「cargo check を走らせて通ることを確かめよ」と書いていたが、3 arm とも bash を
# 持たない（strict）ので検証は実行できない。合否は harness 側が cargo check で判定する。
T11_BODY = """Implement a small feature in the Rust crate (src/config.rs and
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

The result must compile. Reply with exactly: DONE
"""

# arm ごとの道具の案内 — ここだけが 3 arm で違う。
T11_TOOL_HINT = {
    "native": "",
    "naive": """
Use `mread` to read a file and `medit` to replace text in it. This session has
no shell.""",
    "minas": """
Use `mread` to read lines of a file (pass `start`/`end` to read only the lines
you need) and `medit` to apply a content-based edit. An edit is rejected when
its `old` text no longer matches the file — re-read the affected lines and
retry. This session has no shell.""",
}

T11_PROMPTS = {f"t11-{arm}": T11_BODY + T11_TOOL_HINT[arm] for arm in T11_ARMS}

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
    # t11 の 3 arm（規模掃引）。プロンプト本文（5項目・DONE）は 3 arm で完全に同一に保ち、
    # 差が「道具と読み方」だけに帰属するようにする（T11_PROMPTS を参照）。
    **T11_PROMPTS,
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
    # 3 arm の agent 設定。t11 は bash を外して custom tool だけで解かせる（strict）。
    # t1-t10 は従来どおり bash + PATH shim（強制はできず監査除外に依存する旧方式）。
    strict = (test == "t11")
    cfg = {"agent": {
        AGENT_FORCED: {
            "description": ("mina contract agent（custom tool のみ）" if strict
                            else "bash-only agent（shim 強制）"),
            "tools": TOOLS_STRICT if strict else TOOLS_OFF,          # 旧 runner (1.14) 用
            "permission": PERMS_STRICT if strict else PERMS_FORCED,  # 1.18 用
            "maxSteps": 30,
        },
        AGENT_NATIVE: {
            "description": "native tools（素朴な対照）",
            "tools": TOOLS_NATIVE if strict else {**TOOLS_OFF, "read": True,
                                                  "edit": True, "write": True},
            "permission": PERMS_NATIVE if strict else {
                "bash": "allow", "read": "allow", "edit": "allow",
                "glob": "allow", "grep": "allow"},
            "maxSteps": 30,
        },
    }}
    if strict:
        cfg["agent"][AGENT_FORCED]["prompt"] = str(wd / "strict-prompt.md")
        (wd / "strict-prompt.md").write_text(STRICT_PROMPT)
    (wd / "opencode.json").write_text(json.dumps(cfg, indent=2))
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
    if test != "t11":
        wr("mread", "read_shim.py", read_mode)
        wr("medit", "edit_shim.py", edit_mode)
        wr("mcheck", "check_shim.py", "x")
    if test == "t11":
        # 3 arm。native はネイティブ tool のみ、naive/minas は custom tool のみ。
        # どの arm も bash を持たないので PATH shim も `minas` ブロッカーも不要。
        # drift は run_once の外部注入が当てる（shim 非依存の同じ機構）。
        write_t11_tools(wd, arm)
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
    if test != "t11":
        env["PATH"] = f"{wd}/bin:" + env["PATH"]
    # opencode 1.18 は env の PWD を見てプロジェクトディレクトリを決める（cwd では
    # ない）。Python の env には親 shell の PWD が残っているので、ここで合わせないと
    # セッションが呼び出し元のリポジトリに作られ、最初のメッセージで server error に
    # なる（opencode 1.14 では起きなかった。1.18 で runner を戻した際に判明）。
    env["PWD"] = str(wd)
    if test != "t11":
        # opencode の bash ツールは zsh。~/.zshrc の `mise activate zsh` が PATH を
        # 組み直すので wd/bin が後ろに回り、実物の `minas`（~/.cargo/bin）が shim より
        # 先に解決される（smoke で発覚: `minas apply --whole-stdin` が実行された）。
        # 空の ZDOTDIR を渡して PATH を継承させ、shim を確実に先に解決させる。
        # t11 は bash を持たないので不要。
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
    m = measure(sid, native=(test == "t11"))
    if test == "t11":
        label, detail = outcome_t11(scale)
        # strict: arm ごとに許した tool 以外が 1 回でも呼ばれたら、その run は
        # 「契約の測定」として使えない（NC）。bash を外したので、ここが本当の意味での
        # 遵守チェックになる。
        used = tool_names(sid)
        stray = ",".join(sorted(used - T11_ALLOWED[arm]))
    else:
        ok_, detail = success(test)
        label = "GREEN" if ok_ == "OK" else "LOUD-FAIL"
        stray = ""
    ok = "OK" if label == "GREEN" else "FAIL"
    audit = wd / "audit.log"
    audit_summary = summarize_audit(audit.read_text()) if audit.exists() else ""
    edits = audit.read_text().count("edit_ok") if audit.exists() else 0
    rejects = audit.read_text().count("edit_reject") if audit.exists() else 0
    renames = audit.read_text().count("rename\t") if audit.exists() else 0
    bypass = int(m.split("bypass=")[-1].split()[0]) if "bypass=" in m else -1
    skills = int(m.split("skills=")[-1]) if "skills=" in m else 0
    comp = "C" if ((edits + rejects + renames) > 0 and bypass == 0) else "NC"
    if test == "t11":
        comp = "C" if not stray else "NC"
    print(f"{title} wall={wall}s ok={ok} outcome={label} comp={comp} stray={stray or '-'} {m} "
          f"{audit_summary} renames={renames} skills={skills} {detail}")
    _record({**_meta(), "ts": time.strftime("%Y-%m-%dT%H:%M:%S"),
             "task": test, "scale": scale, "drift": int(drift), "arm": arm,
             "idx": idx, "wall_s": wall, "steps": _num(m, "steps"),
             "input": _num(m, "input"), "output": _num(m, "output"),
             "cost": _num(m, "cost", float), "outcome": label, "comp": comp,
             "stray": stray,
             "facts": detail.split("facts=")[-1].split()[0] if "facts=" in detail else "",
             "noise": "DAMAGED" if "noise=DAMAGED" in detail else "ok",
             "cargo": "ok" if "cargo=ok" in detail else "FAIL",
             "edits": edits, "rejects": rejects, "renames": renames,
             "reads": _num(audit_summary, "reads")})
    return title, ok, comp


def _num(text, key, cast=int):
    """`m` / audit summary は "key=value" の並び。Number を取り出す。"""
    if f"{key}=" not in text:
        return None
    try:
        return cast(text.split(f"{key}=")[-1].split()[0])
    except ValueError:
        return None


# t11 の各 arm に許した tool。これ以外が 1 回でも呼ばれた run は NC（測定に使わない）。
# native の apply_patch: opencode 1.18 では apply_patch tool に permission フックが無く、
# `tools: {apply_patch: false}` も無視される（実測: bash を外しても呼ばれた）。一方
# apply_patch は opencode の既定のファイル tool 一式に含まれるので、native arm の
# 「素朴な道具面」としてはむしろこれを許すのが正しい（弱い対照にしない）。
T11_ALLOWED = {
    "native": {"read", "edit", "write", "glob", "grep", "apply_patch"},
    "naive": {"mread", "medit"},
    "minas": {"mread", "medit"},
}


def tool_names(sid):
    """そのセッションで実際に呼ばれた tool 名の集合（遵守監査）。"""
    con = sqlite3.connect(DB)
    rows = con.execute("select data from part where session_id=?", (sid,)).fetchall()
    con.close()
    names = set()
    for (r,) in rows:
        try:
            d = json.loads(r)
        except Exception:
            continue
        if d.get("type") == "tool" and d.get("tool"):
            names.add(str(d["tool"]))
    return names


def _record(row):
    """1 run = 1 行を履歴に追記する（同じ key の再走も追記し、集計側が「同一 key の
    最後の行」を採用する。履歴は消さない）。"""
    HISTORY.parent.mkdir(parents=True, exist_ok=True)
    with open(HISTORY, "a") as f:
        f.write(json.dumps(row, ensure_ascii=False, sort_keys=True) + "\n")


def _meta():
    """commit / minas の build generation / runner 版を 1 run ごとに固定する。"""
    def sh(*cmd):
        try:
            return subprocess.run(cmd, capture_output=True, text=True,
                                  timeout=20).stdout.strip()
        except Exception:
            return ""
    gen = ""
    try:
        gen = str(json.loads(subprocess.run([MINA, "info"], capture_output=True,
                                            text=True, timeout=20).stdout)
                            .get("generation", ""))
    except Exception:
        pass
    return {"commit": sh("git", "rev-parse", "--short", "HEAD"),
            "generation": gen,
            "build": "debug" if "/debug/" in str(MINA) else "release",
            "model": MODEL, "runner": sh(OPENCODE, "--version")}


def sweep(arms=T11_ARMS, scales=SCALES, drift_scales=None, n=5):
    """決定済みの掃引をそのまま回す。

    規模点 × arm × n を idx に沿って **arm 交互**（paired 設計）に走らせる。
    drift は最小・最大の 2 点だけで on/off を取る。1 run が失敗しても続行する
    （失敗 run は NC として履歴に残り、後で差し替えられる）。
    """
    drift_scales = drift_scales or (SCALES[0], SCALES[-1])
    plan = []
    for scale in scales:
        for dr in (0, 1):
            if dr == 1 and scale not in drift_scales:
                continue
            for idx in range(1, n + 1):
                for arm in arms:
                    plan.append((arm, idx, scale, bool(dr)))
    print(f"sweep: {len(plan)} runs -> {HISTORY}", flush=True)
    for k, (arm, idx, scale, dr) in enumerate(plan, 1):
        print(f"[{k}/{len(plan)}] {arm} scale={scale} drift={int(dr)} idx={idx}", flush=True)
        try:
            run_once("t11", arm, idx, scale, dr)
        except Exception as e:                      # 1 run の失敗で掃引を止めない
            print(f"  run failed: {type(e).__name__}: {e}", flush=True)
    print("sweep done")


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
    cf_c = re.sub(r"\s+", " ", cf)   # 空白だけ潰す（識別子はそのまま）
    cf_num = cf_c.replace("_", "")    # Rust の数値は `65_535` とも書ける
    facts = [
        "max_conns: u32" in cf_c,
        "max_conns: 1024" in cf_c,
        "cfg.max_conns" in cf_c,
        ("65535" in cf_num and "max_conns too large" in cf_c),
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


def _apply_t11_ground_truth(underscore=False):
    """5 項目の正解を手で当てる（selftest 専用。GREEN を作るため）。

    underscore=True で `65_535` と書く — nano が実際にこう書いて分類器が
    誤って INCOMPLETE にしたので、その綴りでも GREEN になることを検証する。
    """
    wd = WORK / "ws"
    p = wd / "src" / "config.rs"
    cf = p.read_text()
    cf = cf.replace("    pub retries: u32,\n", "    pub retries: u32,\n    pub max_conns: u32,\n")
    cf = cf.replace("            retries: 3,\n", "            retries: 3,\n            max_conns: 1024,\n")
    cf = cf.replace("    Ok(Config { timeout, retries })\n",
                    "    let max_conns = if json.contains(\"max_conns\") { 1024 } else { 1024 };\n"
                    "    Ok(Config { timeout, retries, max_conns })\n")
    limit = "65_535" if underscore else "65535"
    cf = cf.replace('    if cfg.retries > 10 {\n        return Err("too many retries".to_string());\n    }\n',
                    '    if cfg.retries > 10 {\n        return Err("too many retries".to_string());\n    }\n'
                    f'    if cfg.max_conns > {limit} {{\n        return Err("max_conns too large".to_string());\n    }}\n')
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
    _apply_t11_ground_truth(underscore=True)
    check("ground truth 65_535 -> GREEN", outcome_t11(150)[0], "GREEN")

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
    ap.add_argument("action", choices=["run", "stats", "fixture", "selftest", "sweep"])
    ap.add_argument("test", nargs="?",
                    choices=["t1", "t2", "t3", "t4", "t5", "t6", "t7", "t8", "t9", "t10", "t11"])
    ap.add_argument("arm", nargs="?", help="t11 は native|naive|minas、他は A|B")
    ap.add_argument("idx", type=int, nargs="?")
    ap.add_argument("--scale", type=int, default=1200,
                    choices=list(SCALES), help="t11 の 2 ファイル合計行数")
    ap.add_argument("--drift", type=int, default=0, help="1 で外部書き換えを注入")
    ap.add_argument("--n", type=int, default=5, help="sweep: arm ごとの反復数")
    ap.add_argument("--scales", default=",".join(map(str, SCALES)),
                    help="sweep: 規模点をカンマ区切りで")
    a = ap.parse_args()
    if a.action == "selftest":
        sys.exit(_selftest_outcome())
    if a.action == "sweep":
        sc = tuple(int(x) for x in a.scales.split(",") if x.strip())
        sweep(scales=sc, n=a.n)
        sys.exit(0)
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