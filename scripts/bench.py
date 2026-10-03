"""Builds a generated C project with kiln, GNU Make and Ninja and times common situations.

    python scripts/bench.py [--files 500] [--runs 5] [--dir DIR]

The project: N .c files, each including two of 20 headers, compiled with arm-none-eabi-gcc
(-O2, depfiles), then archived with arm-none-eabi-ar. All three tools run the same commands with
the same parallelism. Needs kiln (release build), make, ninja and the Arm GNU toolchain on PATH.
"""
import argparse
import os
import shutil
import statistics
import subprocess
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
KILN = ROOT / "target" / "release" / ("kiln.exe" if os.name == "nt" else "kiln")
CC = "arm-none-eabi-gcc"
CFLAGS = "-mcpu=cortex-m4 -O2 -Iinclude"


def generate(d: Path, n: int):
    if d.exists():
        shutil.rmtree(d)
    (d / "src").mkdir(parents=True)
    (d / "include").mkdir()
    for h in range(20):
        (d / "include" / f"h{h}.h").write_text(f"/* header {h} */\n#define K{h} {h + 3}\nstatic inline int g{h}(int x) {{ return x * K{h} + {h}; }}\n")
    for i in range(n):
        a, b = i % 20, (i * 7 + 3) % 20
        body = "".join(f"int f{i}_{k}(int x) {{ int s = 0; for (int j = 0; j < x; j++) s += g{a}(j) ^ g{b}(s); return s + {k}; }}\n" for k in range(12))
        (d / "src" / f"m{i}.c").write_text(f'#include "h{a}.h"\n#include "h{b}.h"\n{body}')
    (d / "kiln.toml").write_text(f'''
[vars]
cc = "{CC}"
cflags = "{CFLAGS}"

[[task]]
name = "compile"
foreach = "src/*.c"
output = "build/obj/{{stem}}.o"
depfile = "build/obj/{{stem}}.d"
command = "{{cc}} {{cflags}} -MD -MF {{depfile}} -c {{in}} -o {{out}}"

[[task]]
name = "archive"
inputs = ["@compile"]
output = "build/lib.a"
command = "arm-none-eabi-ar rcs {{out}} {{in}}"
default = true
''')
    objs = " ".join(f"build/obj/m{i}.o" for i in range(n))
    (d / "Makefile").write_text(f'''SRC := $(wildcard src/*.c)
OBJ := $(patsubst src/%.c,build/obj/%.o,$(SRC))
build/lib.a: $(OBJ)
\tarm-none-eabi-ar rcs $@ $^
build/obj/%.o: src/%.c | build/obj
\t{CC} {CFLAGS} -MD -MF $(@:.o=.d) -c $< -o $@
build/obj:
\tmkdir -p $@
-include $(OBJ:.o=.d)
''')
    lines = [f"rule cc\n  command = {CC} {CFLAGS} -MD -MF $out.d -c $in -o $out\n  depfile = $out.d\n  deps = gcc\n",
             "rule ar\n  command = arm-none-eabi-ar rcs $out $in\n"]
    for i in range(n):
        lines.append(f"build build/obj/m{i}.o: cc src/m{i}.c\n")
    lines.append(f"build build/lib.a: ar {objs}\ndefault build/lib.a\n")
    (d / "build.ninja").write_text("".join(lines))


def run(cmd, d):
    t = time.perf_counter()
    r = subprocess.run(cmd, cwd=d, capture_output=True, text=True)
    el = time.perf_counter() - t
    if r.returncode != 0:
        raise SystemExit(f"{cmd} failed:\n{r.stdout}\n{r.stderr}")
    return el, r.stdout


def compiles(tool, out):
    if tool == "kiln":
        return sum(1 for l in out.splitlines() if l.startswith("[") and " compile " in l)
    # Make and Ninja print each command they run.
    return out.count(" -c src/")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--files", type=int, default=500)
    ap.add_argument("--runs", type=int, default=5)
    ap.add_argument("--dir", default=str(ROOT / "target" / "bench-c"))
    ap.add_argument("-j", type=int, default=os.cpu_count())
    a = ap.parse_args()
    d = Path(a.dir)
    j = str(a.j)
    tools = {
        "kiln": [str(KILN), "-j", j],
        "make": ["make", f"-j{j}"],
        "ninja": ["ninja", "-j", j],
    }
    clean = {
        "kiln": lambda: (shutil.rmtree(d / "build", ignore_errors=True), shutil.rmtree(d / ".kiln", ignore_errors=True)),
        "make": lambda: shutil.rmtree(d / "build", ignore_errors=True),
        "ninja": lambda: (shutil.rmtree(d / "build", ignore_errors=True), [p.unlink() for p in d.glob(".ninja_*")]),
    }
    results = {}
    for name, cmd in tools.items():
        rows = {"full": [], "noop": [], "touch header": [], "edit one .c": [], "comment in header": []}
        counts = {}
        for r in range(a.runs):
            generate(d, a.files)
            clean[name]()
            el, out = run(cmd, d)
            rows["full"].append(el)
            el, out = run(cmd, d)
            rows["noop"].append(el)
            # Same bytes, newer modification time.
            h = d / "include" / "h3.h"
            # A second later, so a tool comparing timestamps sees it as newer than the objects.
            time.sleep(1.1)
            now = time.time()
            os.utime(h, (now, now))
            el, out = run(cmd, d)
            rows["touch header"].append(el)
            counts["touch header"] = compiles(name, out)
            c = d / "src" / "m7.c"
            time.sleep(1.1)
            c.write_text(c.read_text() + "int extra(void) { return 7; }\n")
            el, out = run(cmd, d)
            rows["edit one .c"].append(el)
            counts["edit one .c"] = compiles(name, out)
            # A comment-only change: every dependent recompiles to the same object.
            time.sleep(1.1)
            h.write_text(h.read_text().replace("/* header 3 */", "/* header three */"))
            el, out = run(cmd, d)
            rows["comment in header"].append(el)
            counts["comment in header"] = compiles(name, out)
            counts["archive after comment"] = ("archive" in out) if name == "kiln" else None
        results[name] = (rows, counts)
        print(name, {k: f"{statistics.median(v) * 1000:.0f} ms" for k, v in rows.items()}, counts, flush=True)
    print()
    print(f"| Situation ({a.files} files, -j {j}, median of {a.runs}) | kiln | GNU Make | Ninja |")
    print("|---|---|---|---|")
    for k in ["full", "noop", "touch header", "edit one .c", "comment in header"]:
        cells = []
        for name in ["kiln", "make", "ninja"]:
            rows, counts = results[name]
            ms = statistics.median(rows[k]) * 1000
            c = counts.get(k)
            cells.append(f"{ms:.0f} ms" + (f" ({c} compiles)" if c is not None else ""))
        print(f"| {k} | " + " | ".join(cells) + " |")


if __name__ == "__main__":
    main()
