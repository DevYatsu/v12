#!/usr/bin/env python3
"""scripts/gen-array-receiver-table.py — regenerate the ADR-0004 node-differential table.

WHY THIS FILE EXISTS
  `crates/v12-engine/tests/array_generic_receivers.rs` was, until this
  generator landed, a **hand-transcribed** oracle: every `expected` was node's
  output for the same source, copied by hand. A transcribed oracle cannot
  report its own errors, and it did not: Batch 1 marked the `sparse`/`holey`
  receiver columns as matching `node` when `new Array(3)` followed by
  `a[0] = "z"` reported `length` 1 where node reports 3. An instrument that
  cannot detect its own errors moves the goalposts for whoever picks it up
  next, so the expectations are now MECHANICALLY GENERATED: this script reads
  the row source, builds the observation program, runs it under `node`, and
  writes the Rust row literals. Regenerating is a command.

  The program text is emitted into the generated file verbatim, so the Rust
  test executes **byte-for-byte** the program `node` answered — there is no
  second JS harness in Rust that could drift from the one the oracle saw.

  The generated file carries an FNV-1a digest of the row source, and the test
  asserts it still matches, so a row edited without regenerating fails a test
  instead of being silently compared against a stale oracle.

WHAT IT DOES
  1. parses scripts/array-receiver-table.txt (`receiver`/`decl`/`method`)
  2. builds one observation program per receiver from a fixed harness
  3. runs each program under `node`, one process per receiver
  4. writes crates/v12-engine/tests/generated/array_receiver_table.rs

USAGE
  ./scripts/gen-array-receiver-table.py            # regenerate in place
  ./scripts/gen-array-receiver-table.py --check    # exit 1 if stale (CI use)
  ./scripts/gen-array-receiver-table.py --node PATH  # node binary (default: node)

CI DOES NOT RUN THIS. The committed test must pass under plain
`cargo nextest` with no `node` installed, so the generated literals are
committed artifacts; this script is a dev-time tool. `--check` is available
for a developer machine that has node.

WHY PYTHON AND NOT BASH
  The generated Rust needs (a) 64-bit FNV-1a for the staleness digest and
  (b) Rust string-literal escaping. Neither is expressible in POSIX bash
  (the multiply overflows the machine word and there is no escape primitive),
  and doing them in `awk` would be slower to read than the 40 lines of Python
  they replace. Every other script in this directory is bash because it shells
  out to cargo/conformance tooling; this one does string-to-code generation.

THE HARNESS (`_preamble`), and why each piece is here
  * `desc(v)` — a property-path-only description of a return value. It
    deliberately does NOT use `JSON.stringify`: json.rs renders an array from
    its element lattice rather than its `length`, so a JSON-based observer
    charges that one pre-existing defect to every row that returns an array.
    `i in v` is the spec's `HasProperty`, which is the only way to tell a hole
    from a present `undefined`.
  * `snap(o)` — the receiver afterwards, through ordinary property reads only:
    `Object.keys(o).length`, the observed `length`, then per-index
    `hasOwnProperty` + read, so a method's write has to land where a plain
    read sees it. That is the self-consistency property ADR-0004 is about, and
    it is the check that catches a write landing in a store nothing reads.
  * `rec(f)` — wraps a row's callback so the row also reports the invocations:
    arity, value, index. Without it `forEach` is INVISIBLE: it returns
    `undefined` and leaves the receiver untouched, so a harness that observes
    only `ret` and `snap` cannot see that a callback visited a hole that node
    skips. `arguments.length` also pins `reduce`'s four-argument callback.
  * every observation is wrapped in `safe(...)` so a `throw` is recorded as a
    value (`threw:TypeError`) instead of aborting the whole program.
  * rows are `o = <decl>` per method, so each row gets a FRESH receiver; no
    row can observe another's mutations. `VIS` is reset per row for the same
    reason.
  * the program ends with `OUT.join(ROW_SEPARATOR)` and the Rust test splits
    the display string on the same separator, so one `eval` covers a whole
    receiver.
"""

import argparse
import os
import re
import shutil
import subprocess
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TABLE = os.path.join(ROOT, "scripts", "array-receiver-table.txt")
OUT = os.path.join(
    ROOT, "crates", "v12-engine", "tests", "generated", "array_receiver_table.rs"
)

# `;;` cannot occur in this corpus (every observed value is built from letters,
# digits, JSON punctuation, `/`, `,`, `-` and one `|` separator), and it needs
# no escape in either engine's source parser — `\u001f` would be a second
# thing to verify for no gain.
ROW_SEPARATOR = ";;"

# The observation harness. Part of the generated artifact by design: the Rust
# test runs exactly this text, so there is one harness, not two.
PREAMBLE = """var OUT = [];
var VIS = [];
function safe(f) { try { return f(); } catch (e) { return "threw:" + (e && e.name); } }
function rec(f) {
  return function (x, i, a) {
    VIS.push(arguments.length + ":" + String(x) + ":" + String(i));
    return f.apply(this, arguments);
  };
}
function desc(v) {
  if (v === null) { return "null"; }
  if (v === undefined) { return "undefined"; }
  var t = typeof v;
  if (t !== "object" && t !== "function") { return t + ":" + String(v); }
  var L = v.length;
  if (typeof L === "number" && L >= 0 && L <= 64 && typeof v.join === "function") {
    var s = [];
    for (var i = 0; i < L; i++) { s.push(i in v ? String(v[i]) : "-"); }
    return "[" + L + "]" + s.join(",");
  }
  return "obj";
}
function snap(o) {
  var L = o.length;
  if (typeof L !== "number" || L < 0 || L > 64) { L = 0; }
  var s = [];
  for (var i = 0; i < L; i++) {
    s.push(Object.prototype.hasOwnProperty.call(o, i) ? "h:" + String(o[i]) : "-");
  }
  return Object.keys(o).length + "/" + L + "/" + s.join(",");
}
var o; var ret;
"""

ROW_TEMPLATE = """/* {name} */
VIS.length = 0;
o = {decl};
ret = safe(function () {{ return {call}; }});
OUT.push("{name}" + "|" + safe(function () {{ return desc(ret); }}) + "|" + safe(function () {{ return snap(o); }}) + "|" + VIS.join("~"));
"""


def fnv1a(data: bytes) -> int:
    h = 0xCBF29CE484222325
    for b in data:
        h = ((h ^ b) * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return h


def parse(path):
    """Parses the row source into [(receiver_name, decl, [(method, call), …]), …]."""
    receivers = []
    cur = None
    with open(path, encoding="utf-8") as fh:
        for lineno, raw in enumerate(fh, 1):
            line = raw.rstrip("\n")
            if not line.strip() or line.lstrip().startswith("#"):
                continue
            parts = re.split(r"[ \t]+", line.strip(), maxsplit=1)
            if len(parts) != 2:
                sys.exit(f"{path}:{lineno}: expected `<directive> <value>`, got {line!r}")
            key, value = parts[0], parts[1].strip()
            if key == "receiver":
                cur = {"name": value, "decl": None, "methods": []}
                receivers.append(cur)
            elif key == "decl":
                if cur is None or cur["decl"] is not None:
                    sys.exit(f"{path}:{lineno}: `decl` outside a receiver, or twice in one")
                cur["decl"] = value
            elif key == "method":
                bits = re.split(r"[ \t]+", value, maxsplit=1)
                if len(bits) != 2:
                    sys.exit(f"{path}:{lineno}: `method` needs a name and a call")
                cur["methods"].append((bits[0], bits[1]))
            else:
                sys.exit(f"{path}:{lineno}: unknown directive {key!r}")
    for r in receivers:
        if r["decl"] is None or not r["methods"]:
            sys.exit(f"receiver {r['name']!r} has no decl or no methods")
    return receivers


def build_program(recv):
    parts = [PREAMBLE]
    for name, call in recv["methods"]:
        parts.append(ROW_TEMPLATE.format(name=name, decl=recv["decl"], call=call))
    parts.append('OUT.join("%s");\n' % ROW_SEPARATOR)
    return "".join(parts)


def rust_str(value):
    """A Rust string literal. `json.dumps` escaping is a superset of what a
    JS/JSON payload can contain here, and Rust string escapes agree with it for
    every one of those (\\\" \\\\ \\n \\r \\t \\uXXXX)."""
    import json

    return json.dumps(value, ensure_ascii=False)


def run_node(program, node_bin):
    with tempfile.TemporaryDirectory() as tmp:
        js = os.path.join(tmp, "row.js")
        with open(js, "w", encoding="utf-8") as fh:
            fh.write(program)
        proc = subprocess.run(
            [node_bin, "-e", "process.stdout.write(String(require('vm').runInThisContext(require('fs').readFileSync(process.argv[1],'utf8'),{filename:'row.js'})))", js],
            capture_output=True,
            text=True,
        )
    if proc.returncode != 0:
        sys.exit(f"node exited {proc.returncode}:\n{proc.stderr}")
    return proc.stdout


def generate(node_bin):
    with open(TABLE, "rb") as fh:
        raw = fh.read()
    receivers = parse(TABLE)

    out = []
    out.append("// @generated by scripts/gen-array-receiver-table.py — DO NOT EDIT.")
    out.append("//")
    out.append("// Regenerate with:  ./scripts/gen-array-receiver-table.py")
    out.append("//   (needs `node`; the committed test does NOT — that is the point.)")
    out.append("//")
    out.append("// Every `expected` below is `node`'s own output for `program`, and")
    out.append("// `program` is the byte-for-byte text node executed. Edit the row")
    out.append("// source (scripts/array-receiver-table.txt) and re-run the generator;")
    out.append("// the digest test below fails if you edit one and not the other.")
    out.append("")
    out.append("/// Byte length of `scripts/array-receiver-table.txt` at generation time.")
    out.append("const TABLE_SOURCE_BYTES: usize = %d;" % len(raw))
    out.append("/// FNV-1a/64 of the same file. See `fnv1a64` in the parent module.")
    out.append("const TABLE_SOURCE_FNV1A: u64 = 0x%016x;" % fnv1a(raw))
    out.append("")
    out.append("/// The row separator; the Rust test splits the display string on it too.")
    out.append("const ROW_SEPARATOR: &str = %s;" % rust_str(ROW_SEPARATOR))
    out.append("")
    out.append("// One receiver's observation program and the line `node` produced for it.")
    out.append("struct ReceiverTable {")
    out.append("    name: &'static str,")
    out.append("    program: &'static str,")
    out.append("    /// One entry per method, in row-source order, `" + ROW_SEPARATOR + "`-joined.")
    out.append("    expected: &'static str,")
    out.append("}")
    out.append("")

    for recv in receivers:
        program = build_program(recv)
        observed = run_node(program, node_bin)
        lines = observed.split(ROW_SEPARATOR)
        names = [n for n, _ in recv["methods"]]
        if len(lines) != len(names):
            sys.exit(
                "receiver %s: node produced %d observations for %d methods.\n"
                "A method name or an observed value probably contains the %r separator.\n"
                "Observed: %r" % (recv["name"], len(lines), len(names), ROW_SEPARATOR, observed)
            )
        # The method name is re-emitted from the ROW SOURCE, not taken from
        # node's output, so a mismatch between the two is visible as a diff
        # rather than being papered over by trusting whichever side moved.
        for name, line in zip(names, lines):
            if not line.startswith(name + "|"):
                sys.exit(
                    "receiver %s: row %s came back as %r — the program and the row "
                    "source disagree" % (recv["name"], name, line)
                )
        out.append("/// `%s` — %d methods." % (recv["name"], len(names)))
        out.append(
            "const %s: ReceiverTable = ReceiverTable {"
            % re.sub(r"[^A-Za-z0-9]", "_", recv["name"]).upper()
        )
        out.append("    name: %s," % rust_str(recv["name"]))
        out.append("    program: %s," % rust_str(program))
        out.append("    expected: %s," % rust_str(ROW_SEPARATOR.join(lines)))
        out.append("};")
        out.append("")

    out.append("const RECEIVERS: &[ReceiverTable] = &[")
    for recv in receivers:
        out.append("    %s," % re.sub(r"[^A-Za-z0-9]", "_", recv["name"]).upper())
    out.append("];")
    out.append("")
    return "\n".join(out)


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--check", action="store_true", help="exit 1 if the committed file is stale")
    ap.add_argument("--node", default=os.environ.get("V12_NODE", "node"), help="node binary")
    args = ap.parse_args()

    if not shutil.which(args.node):
        sys.exit(
            "gen-array-receiver-table: %r not found on PATH. This is a DEV-TIME tool:\n"
            "  the committed test runs under plain `cargo nextest` with no node.\n"
            "  Set V12_NODE or pass --node PATH." % args.node
        )

    generated = generate(args.node)

    if args.check:
        if not os.path.exists(OUT):
            sys.exit("gen-array-receiver-table: FAIL — %s does not exist" % OUT)
        with open(OUT, encoding="utf-8") as fh:
            current = fh.read()
        if current != generated:
            sys.exit(
                "gen-array-receiver-table: FAIL — %s is stale; run "
                "./scripts/gen-array-receiver-table.py" % OUT
            )
        print("gen-array-receiver-table: OK (%s is in sync)" % os.path.relpath(OUT, ROOT))
        return 0

    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    with open(OUT, "w", encoding="utf-8") as fh:
        fh.write(generated)
    print("gen-array-receiver-table: wrote %s" % os.path.relpath(OUT, ROOT))
    return 0


if __name__ == "__main__":
    sys.exit(main())