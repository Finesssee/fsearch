"""fsearch (Windows port) vs ripgrep on content search, end to end per process.

Sides run interleaved per round so cache state and drift hit both alike.
Every run records wall time, exit code and the matched file set; the last
round's sets are diffed so a fast side can't win by finding less.

usage: python bench/win_vs_rg.py <root> <rounds> <out.json> [--with-default-rg]
"""

import json
import os
import re
import statistics
import subprocess
import sys
import time

FSEARCH = os.path.join(os.path.dirname(__file__), "..", "target", "release", "fsearch.exe")
RG = os.environ.get("RG", "rg")

# (label, fsearch query, rg pattern). Lowercase patterns are case-insensitive
# in both (fsearch is smart-case; rg gets -S).
QUERIES = [
    ("rare literal", "grep:apply_dir", "apply_dir"),
    ("common phrase", 'grep:"fn main"', "fn main"),
    ("regex", r"regex:fn\s+\w+_dir", r"fn\s+\w+_dir"),
    ("camelCase word", "grep:useState", "useState"),
    ("very common word", "grep:import", "import"),
]

FS_LINE = re.compile(r"^(.*?):(\d+): ")


def run(argv, env=None, cwd=None):
    t = time.perf_counter()
    p = subprocess.run(argv, capture_output=True, env=env, cwd=cwd)
    return time.perf_counter() - t, p


def fsearch(root, q):
    """Through the daemon (one `fsearch stdio` process per query), with no
    time budget so every match is listed."""
    env = dict(os.environ, FSEARCH_ROOTS=root[:2], FSEARCH_HOME=root)
    req = json.dumps({"q": q, "limit": 10000000, "budget_ms": 0, "per_file": 1}) + "\n"
    t = time.perf_counter()
    p = subprocess.run([FSEARCH, "stdio"], input=req.encode(), capture_output=True, env=env)
    dt = time.perf_counter() - t
    v = json.loads(p.stdout or b"{}")
    prefix = "/" + root.replace("\\", "/").rstrip("/") + "/"
    files = {f["path"].removeprefix(prefix).lower() for f in v.get("files", [])}
    return dt, p.returncode, files, {"complete": v.get("complete"), "took_us": v.get("took_us"), "error": v.get("error")}


def fsearch_first_page(root, q):
    """What the CLI does by default: the interactive time budget and limit."""
    env = dict(os.environ, FSEARCH_ROOTS=root[:2], FSEARCH_HOME=root)
    return run([FSEARCH, q], env=env)[0]


def scoped_globs():
    from parity import SKIP_DIRS, TEXT_EXTS
    return ["-uu", "-g", "*.{" + ",".join(sorted(TEXT_EXTS)) + "}", *(a for d in sorted(SKIP_DIRS) for a in ("-g", f"!{d}/"))]


def rg(root, pat, tuned, scoped=False):
    extra = ["--max-filesize", "1M"] if tuned else []
    if scoped:
        extra += scoped_globs()
    dt, p = run([RG, "-S", "-l", *extra, "--", pat], cwd=root)
    files = {line.replace("\\", "/").lower() for line in p.stdout.decode("utf-8", "replace").splitlines()}
    # rg exits 1 for "no match", 2 for errors (it still prints matches).
    return dt, p.returncode, files, {"stderr_lines": len(p.stderr.splitlines())}


def main():
    root, rounds, out = sys.argv[1], int(sys.argv[2]), sys.argv[3]
    with_default = "--with-default-rg" in sys.argv
    sides = [("fsearch", lambda q: fsearch(root, q[1])), ("fsearch first page", lambda q: (fsearch_first_page(root, q[1]), 0, set(), {})), ("rg --max-filesize 1M", lambda q: rg(root, q[2], True)),
             ("rg scoped like fsearch", lambda q: rg(root, q[2], True, scoped=True))]
    results = {label: {s: [] for s, _ in sides} for label, *_ in QUERIES}
    last = {}
    for r in range(rounds):
        for q in QUERIES:
            order = sides if r % 2 == 0 else sides[::-1]
            for name, fn in order:
                dt, code, files, info = fn(q)
                results[q[0]][name].append({"s": dt, "code": code, "n": len(files), **info})
                last[(q[0], name)] = files
                print(f"round {r} {q[0]:18} {name:22} {dt * 1000:10.1f} ms  {len(files):6} files  exit {code}", flush=True)
    if with_default:
        for q in QUERIES:
            dt, code, files, info = rg(root, q[2], False)
            results[q[0]]["rg (default)"] = [{"s": dt, "code": code, "n": len(files), **info}]
            last[(q[0], "rg (default)")] = files
            print(f"once    {q[0]:18} {'rg (default)':22} {dt * 1000:10.1f} ms  {len(files):6} files  exit {code}", flush=True)
    summary = {}
    for label, *_ in QUERIES:
        fs, rgt = last[(label, "fsearch")], last[(label, "rg --max-filesize 1M")]
        summary[label] = {
            side: {"median_ms": statistics.median(x["s"] for x in runs) * 1000, "min_ms": min(x["s"] for x in runs) * 1000,
                   "max_ms": max(x["s"] for x in runs) * 1000, "runs": len(runs), "files": runs[-1]["n"]}
            for side, runs in results[label].items()
        }
        summary[label]["diff"] = {"both": len(fs & rgt), "only_fsearch": sorted(fs - rgt)[:15], "only_rg": sorted(rgt - fs)[:15],
                                  "only_fsearch_n": len(fs - rgt), "only_rg_n": len(rgt - fs)}
    json.dump({"results": results, "summary": summary}, open(out, "w"), indent=1)
    for label, s in summary.items():
        print(label, json.dumps({k: v for k, v in s.items() if k != "diff"}), "diff:", s["diff"]["both"], s["diff"]["only_fsearch_n"], s["diff"]["only_rg_n"])


if __name__ == "__main__":
    main()
