#!/usr/bin/env python3
"""Benchmark corpora for scripts/bench/search.sh.

    scripts/bench/corpus.py list
    scripts/bench/corpus.py fetch <corpus> [--repos N] [--seed S] [--budget-gib G] [--jobs J]
    scripts/bench/corpus.py verify <corpus>
    scripts/bench/corpus.py path <corpus>

Corpora live under $SOURCEGULL_CORPUS_DIR (default ~/.cache/sourcegull-corpora), one directory each:

    <corpus>/repos/<name>.git   bare repositories, HEAD -> refs/heads/main
    <corpus>/manifest.tsv       name, commit, tree, files, bytes, source (one line per repo)
    <corpus>/manifest.sha256    the SHA-256 of manifest.tsv
    <corpus>/corpus.json        parameters, totals, size distribution, and `stream_args`
                                for search-stream.py
    <corpus>/queries.json       the default queries for search-stream.py

`fetch` is idempotent and resumable: each repository is built or fetched in `<name>.git.partial`
and renamed into place when it is complete and checked, so a rerun skips finished repositories
and restarts unfinished ones. Public repositories are pinned to a tag and the commit it must
resolve to; synthetic repositories are generated deterministically (fixed seed, fixed
timestamps), so the same parameters always give the same commit IDs and the same manifest.
`verify` rereads every repository's HEAD commit and tree and compares them with the manifest.

Only git and the Python standard library are needed: run it in the toolbox (`scripts/dev corpus
...`) or on any machine with git.
"""

import argparse
import concurrent.futures
import hashlib
import importlib.util
import json
import math
import os
import random
import shutil
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.dont_write_bytecode = True  # no __pycache__ in scripts/ from the import below
_spec = importlib.util.spec_from_file_location("synthetic_history", HERE / "synthetic-history.py")
synthetic_history = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(synthetic_history)
WORDS = synthetic_history.WORDS
commit_message = synthetic_history.commit_message

# Bumped when the generator's output changes, so an old synthetic corpus is not mistaken for a
# new one.
GENERATOR_VERSION = 1
GIB = 1024**3

# Public repositories: (name, URL, tag, commit the tag must resolve to). Shallow (depth 1)
# unless the corpus asks for full history.
LINUX = ("linux", "https://github.com/torvalds/linux", "v6.12",
         "adc218676eef25575469234709c2d87185ca223a")
# The mixed corpus: taken in this order until the tracked bytes reach the budget (10 GiB by
# default). The pins make the cut deterministic.
MIXED = [
    LINUX,
    ("llvm-project", "https://github.com/llvm/llvm-project", "llvmorg-19.1.0",
     "a4bf6cd7cfb1a1421ba92bca9d017b49936c55e4"),
    ("go", "https://github.com/golang/go", "go1.23.0",
     "6885bad7dd86880be6929c02085e5c7a67ff2887"),
    ("rust", "https://github.com/rust-lang/rust", "1.82.0",
     "f6e511eec7342f59a25f7c0534f1dbea00d01b14"),
    ("kubernetes", "https://github.com/kubernetes/kubernetes", "v1.31.0",
     "9edcffcde5595e8a5b1a35f88c421764e575afce"),
    ("gitlab", "https://gitlab.com/gitlab-org/gitlab.git", "v17.5.0-ee",
     "173959793c1163052d16c2158773ea9504aa712e"),
    ("tensorflow", "https://github.com/tensorflow/tensorflow", "v2.17.0",
     "ad6d8cc177d0c868982e39e0823d0efbfb95f04c"),
    ("node", "https://github.com/nodejs/node", "v22.9.0",
     "4631be0311fbde7b77723757e15d025727399e63"),
    ("cpython", "https://github.com/python/cpython", "v3.13.0",
     "60403a5409ff2c3f3b07dd2ca91a7a3e096839c7"),
    ("postgres", "https://github.com/postgres/postgres", "REL_17_0",
     "d7ec59a63d745ba74fba0e280bbf85dc6d1caa3e"),
    ("react", "https://github.com/facebook/react", "v18.3.1",
     "f1338f8080abd1386454a10bbf93d67bfe37ce85"),
    ("vscode", "https://github.com/microsoft/vscode", "1.94.0",
     "d78a74bcdfad14d5d3b1b782f87255d802b57511"),
    ("elasticsearch", "https://github.com/elastic/elasticsearch", "v8.15.0",
     "1a77947f34deddb41af25e6f0ddb8e830159c179"),
    ("runtime", "https://github.com/dotnet/runtime", "v8.0.10",
     "81cabf2857a01351e5ab578947c7403a5b128ad1"),
    ("swift", "https://github.com/apple/swift", "swift-6.0-RELEASE",
     "f79ab15f11ffea6cad859da22b17da6cd9252167"),
    ("godot", "https://github.com/godotengine/godot", "4.3-stable",
     "77dcf97d82cbfe4e4615475fa52ca03da645dbd8"),
    ("chromium", "https://github.com/chromium/chromium", "130.0.6723.58",
     "3a50e012e4c9b8a410a4e2b12bf577e69ee8f755"),
    ("intellij-community", "https://github.com/JetBrains/intellij-community",
     "idea/242.23339.11", "213249ff9b4378985ab68391ab186596e604e247"),
    ("firefox", "https://github.com/mozilla-firefox/firefox", "FIREFOX_131_0_RELEASE",
     "600819446ed92e6794c1a44b9d2229924b6da43d"),
]

SYNTHETIC_QUERIES = {
    "selective": "func main index:only",
    "common": "reconcile index:only",
    "regex": "func\\s+main patterntype:regexp index:only",
    "unindexed": "func main index:no",
}
LINUX_QUERIES = {
    "selective": "kmalloc_array index:only",
    "common": "spin_lock_irqsave index:only",
    "regex": "kmalloc_array\\(.*GFP_ATOMIC patterntype:regexp index:only",
    "unindexed": "kmalloc_array index:no",
}
MIXED_QUERIES = {
    "selective": "kmalloc_array index:only",
    "common": "func main index:only",
    "regex": "func\\s+main patterntype:regexp index:only",
    "unindexed": "kmalloc_array index:no",
}

# Synthetic corpora. Repository sizes (bytes of source at HEAD) and history lengths are drawn
# from log-normal distributions: median and sigma (of the natural log). `min`/`max` clamp.
#
# - tiny: a dozen small repositories, for CI and smoke runs (seconds to build).
# - small-repos: many small repositories (20,000 by default).
# - gitlab-dist: a long-tailed mix like a self-managed GitLab instance: most projects are small
#   (median 64 KiB of source and 5 commits), a few percent pass 10 MiB and the largest reach
#   hundreds of MiB.
SYNTHETIC = {
    "tiny": dict(repos=12, size=(20_000, 1.0, 2_000, 200_000), commits=(3, 0.5, 1, 8),
                 # The manifest of the default parameters; `fetch` and `verify` check it.
                 expected_sha256="cf507672bc5ee08c8d82a053d4d9ad16eb06cb5291eb87cfc88e61d4c4d25ff8"),
    "small-repos": dict(repos=20_000, size=(8_000, 0.8, 500, 200_000), commits=(3, 0.7, 1, 30)),
    "gitlab-dist": dict(repos=2_000, size=(64 * 1024, 2.2, 1_000, 512 * 1024**2),
                        commits=(5, 1.5, 1, 400)),
}


def corpus_root():
    return Path(os.environ.get("SOURCEGULL_CORPUS_DIR") or
                Path.home() / ".cache" / "sourcegull-corpora").resolve()


def git(*args, cwd=None, input=None, capture=True):
    result = subprocess.run(["git", *args], cwd=cwd, input=input, check=True,
                            stdout=subprocess.PIPE if capture else None)
    return result.stdout.decode().strip() if capture else ""


def log(msg):
    print(msg, file=sys.stderr, flush=True)


# --- Synthetic source -----------------------------------------------------------------------

def _ident(rnd):
    return rnd.choice(WORDS) + rnd.choice(WORDS).capitalize()


def _go_line(rnd):
    a, b = _ident(rnd), rnd.choice(WORDS)
    n = rnd.randrange(100_000)
    return rnd.choice([
        f"func {a}(ctx context.Context, {b} int) error {{",
        f"\tif err := {b}.{a.capitalize()}(ctx, {n}); err != nil {{",
        f"\t\treturn fmt.Errorf(\"{a} {b} {n}: %w\", err)",
        "\t}",
        "}",
        f"// {a} will {rnd.choice(WORDS)} the {b} before reconcile ({n}).",
        f"\t{b}{n % 97} := make(map[string]int, {n % 1000})",
        f"\tfor _, {b} := range {a}.{rnd.choice(WORDS)}s {{",
        "",
    ])


def _py_line(rnd):
    a, b = _ident(rnd), rnd.choice(WORDS)
    n = rnd.randrange(100_000)
    return rnd.choice([
        f"def {a.lower()}_{b}(self, {b}, limit={n % 1000}):",
        f"    \"\"\"{a} the {b} ({n}).\"\"\"",
        f"    if {b} is None or len({b}) > {n}:",
        f"        raise ValueError(\"{a}: bad {b}\")",
        f"    return self.{b}.{rnd.choice(WORDS)}({b}, {n})",
        f"# TODO: reconcile {b} with {a}",
        f"class {a.capitalize()}{n % 100}({rnd.choice(WORDS).capitalize()}Base):",
        "",
    ])


def _ts_line(rnd):
    a, b = _ident(rnd), rnd.choice(WORDS)
    n = rnd.randrange(100_000)
    return rnd.choice([
        f"export function {a}({b}: {rnd.choice(WORDS).capitalize()}, n = {n}): boolean {{",
        f"  const {b}{n % 97} = await this.{a}({b}, {n});",
        f"  if (!{b}) throw new Error('{a}: {b} {n}');",
        f"  return {b}.{rnd.choice(WORDS)}.length > {n % 1000};",
        "}",
        f"// {a}: {rnd.choice(WORDS)} {b} then reconcile",
        f"import {{ {a} }} from './{b}';",
        "",
    ])


def _rs_line(rnd):
    a, b = _ident(rnd), rnd.choice(WORDS)
    n = rnd.randrange(100_000)
    snake = "".join("_" + c.lower() if c.isupper() else c for c in a)
    return rnd.choice([
        f"pub fn {snake}(&self, {b}: &str) -> Result<u64, Error> {{",
        f"    let {b}_{n % 97} = self.{b}.get({n}).ok_or(Error::{a.capitalize()})?;",
        f"    tracing::debug!({b}, \"{snake} {n}\");",
        f"    Ok({b}.len() as u64 + {n})",
        "}",
        f"/// {a.capitalize()} the {b}; see reconcile ({n}).",
        f"impl {a.capitalize()} for {rnd.choice(WORDS).capitalize()} {{",
        "",
    ])


def _md_line(rnd):
    return " ".join(rnd.choice(WORDS) for _ in range(rnd.randint(4, 14))) + "."


LANGS = [
    ("go", _go_line, "package {pkg}\n\nimport (\n\t\"context\"\n\t\"fmt\"\n)\n\n"),
    ("py", _py_line, "import os\nimport sys\n\n"),
    ("ts", _ts_line, "// {pkg}\n"),
    ("rs", _rs_line, "//! {pkg}\nuse crate::error::Error;\n\n"),
    ("md", _md_line, "# {pkg}\n\n"),
]
_POOLS = {}


def line_pool(lang):
    """A fixed pool of lines per language, so generation is fast and deterministic."""
    if lang not in _POOLS:
        fn = next(f for name, f, _ in LANGS if name == lang)
        rnd = random.Random(f"pool-{lang}")
        _POOLS[lang] = [fn(rnd) + "\n" for _ in range(30_000)]
    return _POOLS[lang]


def lognormal(rnd, spec):
    median, sigma, lo, hi = spec
    return max(lo, min(hi, int(median * math.exp(rnd.gauss(0, sigma)))))


def make_file(rnd, lang, size, pkg):
    head = next(h for name, _, h in LANGS if name == lang).format(pkg=pkg)
    parts = [head]
    total = len(head)
    pool = line_pool(lang)
    while total < size:
        line = pool[rnd.randrange(len(pool))]
        parts.append(line)
        total += len(line)
    return "".join(parts).encode()


def synthetic_repo(args):
    """Generates one repository with `git fast-import`; returns its manifest line."""
    corpus, params, index, path = args
    rnd = random.Random(f"{corpus}-{params['seed']}-{index}")
    size = lognormal(rnd, params["size"])
    commits = lognormal(rnd, params["commits"])
    pkg = rnd.choice(WORDS)
    files = {}
    # main.go gives the default queries a match in every repository.
    files["cmd/main.go"] = (b"package main\n\nimport \"fmt\"\n\nfunc main() {\n"
                            b"\tfmt.Println(\"" + pkg.encode() + b"\")\n}\n")
    remaining = size - len(files["cmd/main.go"])
    n = 0
    while remaining > 0:
        lang = rnd.choice(LANGS)[0]
        fsize = min(remaining, max(200, int(4096 * math.exp(rnd.gauss(0, 1.0)))))
        directory = "/".join(rnd.choice(WORDS) for _ in range(rnd.randint(0, 3)))
        name = f"{directory}/{rnd.choice(WORDS)}_{n}.{lang}".lstrip("/")
        files[name] = make_file(rnd, lang, fsize, pkg)
        remaining -= len(files[name])
        n += 1
    names = sorted(files)
    # History: the first commit adds everything, later ones rewrite or add a file.
    stream = []
    t = 1_600_000_000 + index * 1000
    for c in range(commits):
        author = rnd.randrange(50)
        ident = f"Dev {author} <dev{author}@example.com> {t + c * 3600} +0000"
        msg = commit_message(rnd).encode()
        stream.append(f"commit refs/heads/main\nauthor {ident}\ncommitter {ident}\n".encode())
        stream.append(b"data %d\n%s" % (len(msg), msg))
        if c == 0:
            changed = names
        elif rnd.random() < 0.3:
            lang = rnd.choice(LANGS)[0]
            new = f"{rnd.choice(WORDS)}/{rnd.choice(WORDS)}_c{c}.{lang}"
            files[new] = make_file(rnd, lang, rnd.randint(200, 4096), pkg)
            names = sorted(files)
            changed = [new]
        else:
            changed = [rnd.choice(names)]
            files[changed[0]] += make_file(rnd, changed[0].rsplit(".", 1)[-1], 300, pkg)
        for name in changed:
            data = files[name]
            stream.append(f"M 100644 inline {name}\n".encode())
            stream.append(b"data %d\n%s\n" % (len(data), data))
        stream.append(b"\n")
    stream.append(b"done\n")
    partial = Path(str(path) + ".partial")
    if partial.exists():
        shutil.rmtree(partial)
    partial.parent.mkdir(parents=True, exist_ok=True)
    git("init", "--bare", "--quiet", "--template=", "--initial-branch=main", str(partial))
    # One pack per repo instead of loose objects (fast-import unpacks fewer than 100).
    git("-c", "fastimport.unpackLimit=1", "fast-import", "--quiet", "--done", cwd=partial,
        input=b"".join(stream))
    return finish_repo(partial, path, f"synthetic:{corpus}")


# --- Public repositories ---------------------------------------------------------------------

def fetch_repo(name, url, tag, commit, path, full):
    partial = Path(str(path) + ".partial")
    if not (partial / "HEAD").exists():
        partial.mkdir(parents=True, exist_ok=True)
        git("init", "--bare", "--quiet", "--template=", "--initial-branch=main", str(partial))
    depth = [] if full else ["--depth=1"]
    log(f"fetching {name} {tag} from {url}{' (full history)' if full else ''}")
    # A rerun restarts an interrupted fetch; objects from finished fetches are kept. Hosts
    # under load fail fetches now and then, so each is tried a few times.
    for attempt in range(1, 5):
        try:
            git("fetch", "--no-tags", "--progress", *depth, url,
                f"+refs/tags/{tag}:refs/tags/{tag}", cwd=partial, capture=False)
            break
        except subprocess.CalledProcessError:
            if attempt == 4:
                raise SystemExit(f"{name}: fetching {tag} from {url} failed; rerun to resume")
            log(f"{name}: fetch failed, retrying in {30 * attempt}s")
            time.sleep(30 * attempt)
    got = git("rev-parse", f"refs/tags/{tag}^{{commit}}", cwd=partial)
    if got != commit:
        raise SystemExit(f"{name}: {tag} resolves to {got}, pinned {commit}")
    git("update-ref", "refs/heads/main", commit, cwd=partial)
    git("symbolic-ref", "HEAD", "refs/heads/main", cwd=partial)
    git("update-ref", "-d", f"refs/tags/{tag}", cwd=partial)
    return finish_repo(partial, path, f"{url}@{tag}")


def finish_repo(partial, path, source):
    """Records the manifest line in the repository and moves it into place."""
    commit = git("rev-parse", "HEAD", cwd=partial)
    tree = git("rev-parse", "HEAD^{tree}", cwd=partial)
    files = size = 0
    out = subprocess.run(["git", "ls-tree", "-r", "-l", "-z", "HEAD"], cwd=partial,
                         check=True, stdout=subprocess.PIPE).stdout
    for entry in out.split(b"\0"):
        if not entry:
            continue
        meta = entry.split(b"\t", 1)[0].split()
        if meta[1] == b"blob":
            files += 1
            size += int(meta[3])
    # The name column is filled in from the path by entry_line.
    line = "\t".join([path.name, commit, tree, str(files), str(size), source])
    (partial / "sourcegull-corpus-entry").write_text(line + "\n")
    os.replace(partial, path)
    return line


def entry_line(repos_dir, path):
    line = (path / "sourcegull-corpus-entry").read_text().rstrip("\n")
    fields = line.split("\t")
    fields[0] = str(path.relative_to(repos_dir))[: -len(".git")]
    return "\t".join(fields)


# --- Corpus ---------------------------------------------------------------------------------

def write_manifest(cdir, lines, info, queries):
    manifest = "".join(line + "\n" for line in lines)
    digest = hashlib.sha256(manifest.encode()).hexdigest()
    sizes = sorted(int(line.split("\t")[4]) for line in lines)
    files = sum(int(line.split("\t")[3]) for line in lines)

    def pct(p):
        return sizes[min(len(sizes) - 1, int(p / 100 * len(sizes)))]

    # Broad queries on the larger corpora pass the default display limit, as in the UI.
    info.update(stream_args=["--allow-display-limit"], repos=len(lines), files=files, bytes=sum(sizes), manifest_sha256=digest,
                size_percentiles={f"p{p}": pct(p) for p in (10, 50, 90, 99, 100)},
                finished_utc=time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()))
    (cdir / "manifest.tsv.tmp").write_text(manifest)
    os.replace(cdir / "manifest.tsv.tmp", cdir / "manifest.tsv")
    (cdir / "manifest.sha256").write_text(f"{digest}  manifest.tsv\n")
    (cdir / "queries.json").write_text(json.dumps(queries, indent=2) + "\n")
    (cdir / "corpus.json").write_text(json.dumps(info, indent=2) + "\n")
    return digest


def check_params(cdir, params):
    """Refuses to mix repositories built with different parameters in one directory."""
    pfile = cdir / "params.json"
    if pfile.exists():
        old = json.loads(pfile.read_text())
        if old != params:
            raise SystemExit(f"{cdir} was built with {old}; requested {params}. "
                             "Delete it or set SOURCEGULL_CORPUS_DIR to another directory.")
    else:
        cdir.mkdir(parents=True, exist_ok=True)
        pfile.write_text(json.dumps(params, indent=2) + "\n")


def fetch_synthetic(name, opts):
    spec = SYNTHETIC[name]
    params = {"generator": GENERATOR_VERSION, "repos": opts.repos or spec["repos"],
              "seed": opts.seed, "size": list(spec["size"]), "commits": list(spec["commits"])}
    cdir = corpus_root() / name
    check_params(cdir, params)
    repos_dir = cdir / "repos"
    width = len(str(params["repos"] - 1))
    paths = [repos_dir / f"r{i:0{width}d}.git" for i in range(params["repos"])]
    todo = [(name, params, i, p) for i, p in enumerate(paths) if not p.exists()]
    log(f"{name}: {len(paths) - len(todo)} of {len(paths)} repositories present, "
        f"generating {len(todo)}")
    started = time.monotonic()
    with concurrent.futures.ProcessPoolExecutor(max_workers=opts.jobs) as pool:
        for done, _ in enumerate(pool.map(synthetic_repo, todo, chunksize=8), 1):
            if done % 500 == 0 or done == len(todo):
                log(f"{name}: {done}/{len(todo)} generated "
                    f"({time.monotonic() - started:.0f}s)")
    lines = [entry_line(repos_dir, p) for p in paths]
    digest = hashlib.sha256("".join(line + "\n" for line in lines).encode()).hexdigest()
    expected = spec.get("expected_sha256")
    if expected and params["repos"] == spec["repos"] and opts.seed == 1 and digest != expected:
        raise SystemExit(f"{name}: manifest {digest} differs from the pinned {expected}")
    write_manifest(cdir, lines, {"corpus": name, "kind": "synthetic", **params},
                   SYNTHETIC_QUERIES)
    return cdir


def fetch_public(name, opts):
    if name in ("linux", "linux-full"):
        entries, budget, queries = [LINUX], None, LINUX_QUERIES
    else:
        entries, budget, queries = MIXED, int(opts.budget_gib * GIB), MIXED_QUERIES
    full = name == "linux-full"
    params = {"repos": [list(e) for e in entries], "full_history": full, "budget": budget}
    cdir = corpus_root() / name
    check_params(cdir, params)
    repos_dir = cdir / "repos"
    lines, total = [], 0
    for repo, url, tag, commit in entries:
        if budget is not None and total >= budget:
            break
        path = repos_dir / f"{repo}.git"
        if not path.exists():
            fetch_repo(repo, url, tag, commit, path, full)
        line = entry_line(repos_dir, path)
        if line.split("\t")[1] != commit:
            raise SystemExit(f"{path}: HEAD is not the pinned {commit}")
        lines.append(line)
        total += int(line.split("\t")[4])
        log(f"{repo}: {int(line.split(chr(9))[4]) / 1e6:.1f} MB, total {total / GIB:.2f} GiB")
    write_manifest(cdir, lines, {"corpus": name, "kind": "public", "full_history": full,
                                 "budget_bytes": budget}, queries)
    return cdir


def verify(name):
    cdir = corpus_root() / name
    manifest = (cdir / "manifest.tsv").read_text()
    want = (cdir / "manifest.sha256").read_text().split()[0]
    if hashlib.sha256(manifest.encode()).hexdigest() != want:
        raise SystemExit(f"{name}: manifest.tsv does not match manifest.sha256")
    bad = 0
    for line in manifest.splitlines():
        repo, commit, tree = line.split("\t")[:3]
        path = cdir / "repos" / f"{repo}.git"
        got = git("rev-parse", "HEAD", "HEAD^{tree}", cwd=path).split()
        if got != [commit, tree]:
            log(f"{repo}: HEAD {got} differs from the manifest {commit} {tree}")
            bad += 1
    if bad:
        raise SystemExit(f"{name}: {bad} repositories differ from the manifest")
    log(f"{name}: {len(manifest.splitlines())} repositories match manifest {want}")


def main():
    names = sorted(SYNTHETIC) + ["linux", "linux-full", "mixed-10g"]
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("command", choices=("list", "fetch", "verify", "path"))
    parser.add_argument("corpus", nargs="?", choices=names)
    parser.add_argument("--repos", type=int, help="synthetic corpora: number of repositories")
    parser.add_argument("--seed", type=int, default=1, help="synthetic corpora: random seed")
    parser.add_argument("--budget-gib", type=float, default=10.0,
                        help="mixed-10g: stop adding repositories at this many GiB of source")
    parser.add_argument("--jobs", type=int, default=os.cpu_count(),
                        help="synthetic corpora: parallel generators")
    opts = parser.parse_args()
    if opts.command == "list":
        for name in names:
            cdir = corpus_root() / name
            state = "missing"
            if (cdir / "corpus.json").exists():
                info = json.loads((cdir / "corpus.json").read_text())
                state = f"{info['repos']} repos, {info['bytes'] / 1e6:.1f} MB"
            elif cdir.exists():
                state = "incomplete"
            print(f"{name:12} {state}")
        print(f"(in {corpus_root()})")
        return
    if not opts.corpus:
        parser.error("a corpus name is required")
    if opts.command == "path":
        print(corpus_root() / opts.corpus)
    elif opts.command == "verify":
        verify(opts.corpus)
    elif opts.corpus in SYNTHETIC:
        print(fetch_synthetic(opts.corpus, opts))
    else:
        print(fetch_public(opts.corpus, opts))


if __name__ == "__main__":
    main()
