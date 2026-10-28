#!/usr/bin/env python3
"""Writes a `git fast-import` stream of one commit holding a synthetic source
tree to stdout: about MB megabytes (default 100) of code-like text in files of
1-40 KB across nested directories, built from keywords, identifiers,
numbers, strings and comments (deterministic: seed 1). Each file draws its
identifiers from a few hundred of a 20,000-word vocabulary (camelCase and
snake_case, Zipf-weighted), so trigram density is close to real code. The
pack is rewritten with `git repack -adf` as a server-side clone would be.

    git init --bare /tmp/tree.git
    scripts/bench/synthetic-tree.py 100 | git -C /tmp/tree.git fast-import --quiet

Used by the index build benchmark (scripts/bench/index-build.sh).
"""
import random
import sys

KEYWORDS = (
    "func return if else for while switch case break continue struct impl fn "
    "let const var type interface class public private static void int string "
    "bool nil null true false import package use mod pub self this new"
).split()
EXTS = [".go", ".rs", ".ts", ".py", ".java", ".c", ".txt", ".md"]


def vocab(rnd, n):
    letters = "abcdefghijklmnopqrstuvwxyz"
    syll = ["".join(rnd.choice(letters) for _ in range(rnd.randint(2, 5))) for _ in range(3000)]
    words = set()
    while len(words) < n:
        parts = [rnd.choice(syll) for _ in range(rnd.randint(1, 3))]
        if rnd.random() < 0.6:
            w = parts[0] + "".join(p.capitalize() for p in parts[1:])
        else:
            w = "_".join(parts)
        if rnd.random() < 0.2:
            w = w[0].upper() + w[1:]
        words.add(w)
    return sorted(words)


def local_vocab(rnd, words):
    """A file's identifiers: mostly common ones (Zipf over the vocabulary),
    some of its own, as real source files reuse a small set."""
    n = rnd.randint(40, 300)
    out = []
    for _ in range(n):
        if rnd.random() < 0.7:
            out.append(words[min(int(rnd.paretovariate(0.8)) - 1, len(words) - 1)])
        else:
            out.append(rnd.choice(words))
    return out


def line(rnd, words, depth):
    kind = rnd.random()
    ind = "\t" * depth
    if kind < 0.1:
        return ind + "// " + " ".join(rnd.choice(words) for _ in range(rnd.randint(3, 12)))
    if kind < 0.15:
        return ind + '%s := "%s"' % (rnd.choice(words), " ".join(rnd.choice(words) for _ in range(rnd.randint(1, 6))))
    if kind < 0.2:
        return ind + "%s = %d" % (rnd.choice(words), rnd.randint(0, 1 << 32))
    toks = []
    for _ in range(rnd.randint(2, 10)):
        r = rnd.random()
        if r < 0.25:
            toks.append(rnd.choice(KEYWORDS))
        elif r < 0.9:
            toks.append(rnd.choice(words))
        else:
            toks.append(rnd.choice(["(", ")", "{", "}", "[", "]", ",", ";", ".", "->", "==", "!=", "&&", "+"]))
    return ind + " ".join(toks)


def main():
    target = int(float(sys.argv[1]) * 1e6) if len(sys.argv) > 1 else 100_000_000
    rnd = random.Random(1)
    words = vocab(rnd, 20000)
    rnd.shuffle(words)
    dirs = [""]
    for _ in range(400):
        dirs.append(rnd.choice(dirs) + rnd.choice(words).lower()[:12] + "/")
    out = sys.stdout.buffer
    total = 0
    files = []
    seen = set()
    while total < target:
        size = int(min(40_000, max(1_000, rnd.lognormvariate(8.6, 0.9))))
        lines, n, depth = [], 0, 0
        local = local_vocab(rnd, words)
        while n < size:
            depth = max(0, min(4, depth + rnd.choice([-1, 0, 0, 1])))
            l = line(rnd, local, depth)
            lines.append(l)
            n += len(l) + 1
        data = ("\n".join(lines) + "\n").encode()
        path = rnd.choice(dirs) + rnd.choice(words) + rnd.choice(EXTS)
        if path in seen:
            continue
        seen.add(path)
        mark = len(files) + 1
        out.write(b"blob\nmark :%d\ndata %d\n" % (mark, len(data)))
        out.write(data)
        out.write(b"\n")
        files.append((path, mark))
        total += len(data)
    msg = b"synthetic tree"
    out.write(b"commit refs/heads/main\ncommitter Bench <bench@example.com> 1000000000 +0000\n")
    out.write(b"data %d\n%s\n" % (len(msg), msg))
    for path, mark in files:
        out.write(b"M 100644 :%d %s\n" % (mark, path.encode()))
    out.write(b"\n")


if __name__ == "__main__":
    main()
