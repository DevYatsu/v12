#!/usr/bin/env python3
"""Independent divergence counter for the ADR-0004 table.

Rebuilds the same observation programs the generator emits (importing it, so
there is ONE builder), runs each under `node` and under the v12 CLI, and prints
the differing rows. This exists so a batch can measure the divergence set
WITHOUT going through `KNOWN_DIVERGENCES` — i.e. without the ledger telling the
truth — which is the only way the before/after numbers can be trusted.

Usage:  ./scripts/table-divergence.py [--node PATH] [--v12 PATH] [--only recv]
"""
import argparse
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import importlib.util  # noqa: E402

# The generator's filename is not an identifier, so load it by path.
_spec = importlib.util.spec_from_file_location(
    "gen_art", os.path.join(HERE, "gen-array-receiver-table.py")
)
G = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(G)

ROOT = G.ROOT


def run_file(prog, argv, tmpdir, tag):
    """Both engines observe the SAME thing: the value of the program's final
    expression, coerced with `String` and written to stdout.

    `node -e` is not enough for v12 (the CLI takes a file, not `-e`), and
    `console.log` is how the generator's own harness gets its answer out of
    node, so both sides go through a file and one `console.log`.
    """
    js = os.path.join(tmpdir, "%s.js" % tag)
    with open(js, "w", encoding="utf-8") as fh:
        fh.write(prog)
        fh.write('\nconsole.log(String(OUT.join("%s")));\n' % G.ROW_SEPARATOR)
    p = subprocess.run(argv + [js], capture_output=True, text=True)
    if p.returncode != 0:
        return "<<ERROR rc=%d>> %s" % (p.returncode, p.stderr.strip().splitlines()[-1:])
    return p.stdout.rstrip("\n")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--node", default=os.environ.get("V12_NODE", "node"))
    ap.add_argument("--v12", default=os.path.join(ROOT, "target", "debug", "v12"))
    ap.add_argument("--only", default=None, help="restrict to one receiver")
    ap.add_argument("--quiet", action="store_true", help="counts only")
    a = ap.parse_args()

    recvs = G.parse(G.TABLE)
    total = diff_rows = 0
    methods = {}
    import tempfile

    with tempfile.TemporaryDirectory() as tmp:
      for r in recvs:
        if a.only and r["name"] != a.only:
            continue
        prog = G.build_program(r)
        n = run_file(prog, [a.node], tmp, "node").split(G.ROW_SEPARATOR)
        v = run_file(prog, [a.v12], tmp, "v12").split(G.ROW_SEPARATOR)
        names = [nm for nm, _ in r["methods"]]
        total += len(names)
        for name, want, got in zip(names, n, v):
            if want != got:
                diff_rows += 1
                methods.setdefault(name, []).append(r["name"])
                if not a.quiet:
                    print("=== %s / %s" % (r["name"], name))
                    print("  node: %s" % want)
                    print("  v12 : %s" % got)
    diff_methods = len(methods)
    print()
    print("DIVERGENT: %d of %d rows, %d methods" % (diff_rows, total, diff_methods))
    for m in sorted(methods):
        print("  %-18s %2d  %s" % (m, len(methods[m]), ",".join(methods[m])))
    return 0


if __name__ == "__main__":
    sys.exit(main())