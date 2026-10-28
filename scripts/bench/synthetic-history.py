#!/usr/bin/env python3
"""Writes a `git fast-import` stream of a synthetic history to stdout: N
commits with no files, a side branch merged every 200 commits, 50 authors,
subjects and bodies from a fixed word list (deterministic: seed 1).

    git init --bare /tmp/hist.git
    scripts/bench/synthetic-history.py 1000000 | git -C /tmp/hist.git fast-import --quiet

Used by the commit search benchmark (crates/search-exec/examples/commit_bench.rs).
scripts/bench/corpus.py reuses `WORDS` and `commit_message` for the histories of its
synthetic corpora.
"""
import random
import sys

WORDS = (
    "fix add remove update refactor parser lexer index search query cache "
    "commit diff branch merge test docs build release config error handle "
    "memory leak race lock thread async stream buffer shard trigram symbol "
    "rank score path file repo node web api grpc http token session user"
).split()


def commit_message(rnd):
    """A subject of 3-8 words and 0-3 body lines from WORDS, newline-terminated."""
    subject = " ".join(rnd.choice(WORDS) for _ in range(rnd.randint(3, 8)))
    body = "\n".join(
        " ".join(rnd.choice(WORDS) for _ in range(rnd.randint(5, 12)))
        for _ in range(rnd.randint(0, 3))
    )
    return subject + ("\n\n" + body if body else "") + "\n"


def main():
    n = int(sys.argv[1]) if len(sys.argv) > 1 else 1_000_000
    rnd = random.Random(1)
    out = sys.stdout.buffer
    t = 1_000_000_000
    mark = 0
    main_tip = None
    side_tip = None
    for i in range(n):
        mark += 1
        t += rnd.randint(1, 600)
        a = rnd.randrange(50)
        msg = commit_message(rnd).encode()
        on_side = i % 200 in range(150, 199)
        merge = i % 200 == 199 and side_tip is not None
        ref = "refs/heads/side" if on_side else "refs/heads/main"
        parent = side_tip if on_side else main_tip
        if on_side and side_tip is None:
            parent = main_tip
        ident = f"Dev {a} <dev{a}@example.com> {t} +0000"
        out.write(f"commit {ref}\nmark :{mark}\n".encode())
        out.write(f"author {ident}\ncommitter {ident}\n".encode())
        out.write(b"data %d\n" % len(msg) + msg)
        if parent is not None:
            out.write(f"from :{parent}\n".encode())
        if merge:
            out.write(f"merge :{side_tip}\n".encode())
        out.write(b"\n")
        if on_side:
            side_tip = mark
        else:
            main_tip = mark
            if merge:
                side_tip = None
                out.write(f"reset refs/heads/side\nfrom :{main_tip}\n\n".encode())
    out.write(b"done\n")


if __name__ == "__main__":
    main()
