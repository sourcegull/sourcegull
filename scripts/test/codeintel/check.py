#!/usr/bin/env python3
"""API side of the code intelligence test (run by scripts/test/codeintel.sh).

Runs in a container on the dev Compose network, where the dev GitLab is
http://gitlab.localhost:8929 and Sourcegull (the Helm release in k3s) is http://sourcegull-k3s:30080.
Standard library only.

  check.py gitlab-seed     group codeintel, its CI/CD variables, the template project and, per
                           language, codeintel/ci-<lang> (upstream commit + a .gitlab-ci.yml
                           including the template) and codeintel/auto-<lang> (upstream commit
                           only)
  check.py sourcegull-setup    admin user and token, site config, the GitLab connection and an
                           indexing policy for codeintel/auto-*
  check.py run             waits for the repositories, runs the CI pipelines, waits for the
                           auto-indexing jobs and all uploads, then verifies navigation
  check.py verify          only the navigation checks (uploads must be complete)

Results go to $WORK/results.json and a summary to stdout.
"""

import base64
import datetime as dt
import json
import os
import re
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from http.cookiejar import CookieJar

GITLAB = os.environ.get("GITLAB_URL", "http://gitlab.localhost:8929")
GITLAB_TOKEN = os.environ.get("GITLAB_TOKEN", "glpat-sourcegull-dev-root")
GITLAB_HOST = urllib.parse.urlparse(GITLAB).netloc
# git (libcurl) resolves every *.localhost name to the loopback address, so pushes go to the
# Compose service name instead.
GIT_HOST = os.environ.get("GIT_HOST", "gitlab:8929")
SOURCEGULL = os.environ.get("SOURCEGULL_URL", "http://sourcegull-k3s:30080")
SOURCEGULL_IMAGE = os.environ.get("SOURCEGULL_IMAGE", "sourcegull/sourcegull:test-codeintel")
EXECUTOR_TOKEN = os.environ.get("EXECUTOR_TOKEN", "")
WORK = os.environ.get("WORK", "/src/.test-out/codeintel")
ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))
GROUP = "codeintel"
ADMIN = ("admin", "admin@example.com", "codeintel-test-Pw-7x")
TIMEOUT = int(os.environ.get("CODEINTEL_TIMEOUT", "2400"))
# Restricts `run`/`verify` to one path (ci or auto), e.g. while debugging.
KINDS = [k for k in ("ci", "auto") if os.environ.get("CODEINTEL_ONLY", "") in ("", k)]

# One small open-source project per language, pinned to a release commit. A place is (path, a
# substring of the line, the token, which whole-word occurrence of it on the line). At `site`, go
# to definition must land precisely in `definition` (another file) and the precise references
# must span the files in `references`; at `hover_site` (default: `site`) the hover must contain
# `hover` (the indexer's docs).
LANGS = {
    "go": {
        "upstream": "https://github.com/google/uuid.git",
        "release": "v1.6.0",
        "commit": "0f11ee6918f41a04c201eceeadf612a377bc7fbc",
        "indexer": "scip-go",
        "site": ("version4.go", "func NewRandom() (UUID, error) {", "UUID", 0),
        "definition": "uuid.go",
        "hover": "128 bit (16 byte) Universal Unique IDentifier",
        "references": ["uuid.go", "version4.go", "version1.go"],
    },
    "typescript": {
        "upstream": "https://github.com/sindresorhus/p-queue.git",
        "release": "v8.0.1",
        "commit": "47a4f19ab3d4422e06cb1b618f4c45a11b5def30",
        "indexer": "scip-typescript",
        "site": ("source/index.ts", "export default class PQueue<QueueType extends Queue<",
                 "Queue", 0),
        "definition": "source/queue.ts",
        "references": ["source/index.ts", "source/priority-queue.ts", "source/options.ts"],
        # The type's own hover is its signature; the docs are on Options' properties.
        "hover_site": ("source/options.ts", "readonly concurrency?: number;", "concurrency", 0),
        "hover": "Concurrency limit.",
    },
    "java": {
        "upstream": "https://github.com/jhy/jsoup.git",
        "release": "jsoup-1.18.1",
        "commit": "19e8539fbfae80432fc4d8e9b562a39f786f547e",
        "indexer": "scip-java",
        "site": (
            "src/main/java/org/jsoup/nodes/Document.java",
            "public class Document extends Element {",
            "Element",
            0,
        ),
        "definition": "src/main/java/org/jsoup/nodes/Element.java",
        "hover": "An HTML Element consists of a tag name",
        "references": [
            "src/main/java/org/jsoup/nodes/Element.java",
            "src/main/java/org/jsoup/nodes/Document.java",
            "src/main/java/org/jsoup/select/Elements.java",
        ],
    },
    "python": {
        "upstream": "https://github.com/pallets/itsdangerous.git",
        "release": "2.2.0",
        "commit": "096c8d42545d3b68ea21a4f890fb2b2d8979c0bd",
        "indexer": "scip-python",
        "site": ("src/itsdangerous/timed.py", "class TimestampSigner(Signer):", "Signer", 0),
        "definition": "src/itsdangerous/signer.py",
        "hover": "A signer securely signs bytes",
        "references": [
            "src/itsdangerous/__init__.py",
            "src/itsdangerous/timed.py",
            "src/itsdangerous/serializer.py",
        ],
    },
    "rust": {
        "upstream": "https://github.com/dtolnay/semver.git",
        "release": "1.0.23",
        "commit": "69efd3cc770ead273a06ad1788477b3092996d29",
        "indexer": "rust-analyzer",
        "site": ("src/eval.rs", "pub(crate) fn matches_req(req: &VersionReq, ver: &Version)",
                 "Version", 0),
        "definition": "src/lib.rs",
        "hover": "SemVer version",
        "references": ["src/eval.rs", "src/display.rs", "src/parse.rs"],
    },
}

CI_FILE = """# Sourcegull precise code navigation (deploy/gitlab-ci/sourcegull-scip.gitlab-ci.yml). SOURCEGULL_ENDPOINT
# and SOURCEGULL_CLI_IMAGE are CI/CD variables of the group.
include:
  - project: codeintel/sourcegull-ci
    file: /sourcegull-scip.gitlab-ci.yml
"""


def log(msg):
    print(f"[{dt.datetime.now():%H:%M:%S}] {msg}", flush=True)


def fail(msg):
    raise SystemExit(f"FAIL: {msg}")


def now():
    return time.time()


def wait(what, fn, timeout=TIMEOUT, every=5):
    """Calls fn until it returns something truthy; fails after timeout seconds."""
    deadline = now() + timeout
    while True:
        v = fn()
        if v:
            return v
        if now() > deadline:
            fail(f"timed out waiting for {what}")
        time.sleep(every)


def http(method, url, body=None, headers=None, opener=None, raw=False):
    data = None
    headers = dict(headers or {})
    if body is not None:
        data = json.dumps(body).encode()
        headers.setdefault("Content-Type", "application/json")
    req = urllib.request.Request(url, data=data, method=method, headers=headers)
    try:
        with (opener or urllib.request.build_opener()).open(req, timeout=60) as r:
            out = r.read()
            return r.status, (out if raw else (json.loads(out) if out else None))
    except urllib.error.HTTPError as e:
        out = e.read()
        try:
            return e.code, json.loads(out)
        except ValueError:
            return e.code, out.decode(errors="replace")


# ---- GitLab -----------------------------------------------------------------------------


def gl(method, path, body=None, ok=(200, 201, 202, 204)):
    status, out = http(method, f"{GITLAB}/api/v4{path}", body, {"PRIVATE-TOKEN": GITLAB_TOKEN})
    if status not in ok:
        fail(f"GitLab {method} {path}: {status} {out}")
    return out


def gl_get_or_none(path):
    status, out = http("GET", f"{GITLAB}/api/v4{path}", None, {"PRIVATE-TOKEN": GITLAB_TOKEN})
    return out if status == 200 else None


def project_id(path):
    p = gl_get_or_none(f"/projects/{urllib.parse.quote(f'{GROUP}/{path}', safe='')}")
    return p["id"] if p else None


def ensure_project(group_id, path):
    pid = project_id(path)
    if pid:
        return pid
    p = gl("POST", "/projects", {"name": path, "path": path, "namespace_id": group_id,
                                 "visibility": "public", "initialize_with_readme": False})
    log(f"GitLab: created {GROUP}/{path}")
    return p["id"]


def git(*args, cwd=None, env=None):
    e = dict(os.environ, GIT_TERMINAL_PROMPT="0")
    e.update(env or {})
    subprocess.run(["git", *args], cwd=cwd, env=e, check=True, stdout=subprocess.DEVNULL)


def git_out(*args, cwd=None):
    return subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True,
                          text=True).stdout.strip()


def push_url(path):
    return f"http://root:{GITLAB_TOKEN}@{GIT_HOST}/{GROUP}/{path}.git"


def gitlab_seed():
    group = gl_get_or_none(f"/groups/{GROUP}")
    if not group:
        group = gl("POST", "/groups", {"name": GROUP, "path": GROUP, "visibility": "public"})
        log(f"GitLab: created group {GROUP}")
    # The indexer images are the template's defaults (the digests auto-indexing uses); image
    # variables left from older runs are removed so the defaults are what runs.
    variables = {"SOURCEGULL_ENDPOINT": SOURCEGULL, "SOURCEGULL_CLI_IMAGE": SOURCEGULL_IMAGE}
    existing = {v["key"] for v in gl("GET", f"/groups/{group['id']}/variables")}
    for key in sorted(existing):
        if key.endswith("_IMAGE") and key not in variables:
            gl("DELETE", f"/groups/{group['id']}/variables/{key}")
            existing.discard(key)
    for key, value in variables.items():
        body = {"key": key, "value": value, "raw": True}
        if key in existing:
            gl("PUT", f"/groups/{group['id']}/variables/{key}", body)
        else:
            gl("POST", f"/groups/{group['id']}/variables", body)

    # The template, from this checkout, in a project of the instance (`include: project:`).
    tpl_id = ensure_project(group["id"], "sourcegull-ci")
    content = open(os.path.join(ROOT, "deploy/gitlab-ci/sourcegull-scip.gitlab-ci.yml")).read()
    status, current = http("GET", f"{GITLAB}/api/v4/projects/{tpl_id}/repository/files/"
                           "sourcegull-scip.gitlab-ci.yml/raw?ref=main", None,
                           {"PRIVATE-TOKEN": GITLAB_TOKEN}, raw=True)
    if status != 200 or current.decode() != content:
        action = "update" if status == 200 else "create"
        gl("POST", f"/projects/{tpl_id}/repository/commits", {
            "branch": "main", "commit_message": "Sourcegull SCIP CI template",
            "actions": [{"action": action, "file_path": "sourcegull-scip.gitlab-ci.yml",
                         "content": content}]})
        log("GitLab: template updated in codeintel/sourcegull-ci")

    os.makedirs(f"{WORK}/upstream", exist_ok=True)
    seeded = {}
    for lang, spec in LANGS.items():
        repo = f"{WORK}/upstream/{lang}"
        if not os.path.isdir(f"{repo}/.git"):
            log(f"cloning {spec['upstream']}")
            git("clone", "-q", "--no-checkout", spec["upstream"], repo)
        try:
            git("cat-file", "-e", f"{spec['commit']}^{{commit}}", cwd=repo)
        except subprocess.CalledProcessError:
            git("fetch", "-q", "origin", spec["commit"], cwd=repo)
        # The CI commit: the upstream tree plus .gitlab-ci.yml, with fixed dates so its hash is
        # the same on every run.
        tree = git_out("rev-parse", f"{spec['commit']}^{{tree}}", cwd=repo)
        blob = subprocess.run(["git", "hash-object", "-w", "--stdin"], cwd=repo, input=CI_FILE,
                              text=True, capture_output=True, check=True).stdout.strip()
        entries = git_out("ls-tree", tree, cwd=repo).splitlines()
        entries = [e for e in entries if not e.endswith("\t.gitlab-ci.yml")]
        entries.append(f"100644 blob {blob}\t.gitlab-ci.yml")
        new_tree = subprocess.run(["git", "mktree"], cwd=repo, input="\n".join(entries) + "\n",
                                  text=True, capture_output=True, check=True).stdout.strip()
        stamp = "2026-10-01T00:00:00+00:00"
        env = {"GIT_AUTHOR_NAME": "Sourcegull code intel test", "GIT_AUTHOR_EMAIL": "codeintel@example.com",
               "GIT_COMMITTER_NAME": "Sourcegull code intel test",
               "GIT_COMMITTER_EMAIL": "codeintel@example.com", "GIT_AUTHOR_DATE": stamp,
               "GIT_COMMITTER_DATE": stamp}
        ci_commit = subprocess.run(
            ["git", "commit-tree", new_tree, "-p", spec["commit"], "-m",
             "Add Sourcegull precise code navigation to CI"],
            cwd=repo, env=dict(os.environ, **env), capture_output=True, text=True,
            check=True).stdout.strip()
        for kind, commit in (("ci", ci_commit), ("auto", spec["commit"])):
            path = f"{kind}-{lang}"
            ensure_project(group["id"], path)
            # ci.skip: pipelines are started by `run`, once Sourcegull knows the repositories.
            git("push", "-q", "--force", "-o", "ci.skip", push_url(path),
                f"{commit}:refs/heads/main", cwd=repo)
            seeded[path] = commit
        log(f"GitLab: {GROUP}/ci-{lang} at {ci_commit[:10]}, {GROUP}/auto-{lang} at "
            f"{spec['commit'][:10]} ({spec['upstream'].split('/', 3)[3][:-4]} {spec['release']})")
    save("seeded", seeded)


# ---- Sourcegull -----------------------------------------------------------------------------


class Sourcegull:
    def __init__(self, token=None):
        self.token = token
        self.jar = CookieJar()
        self.opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(self.jar))

    def headers(self):
        h = {"X-Requested-With": "Sourcegull"}
        if self.token:
            h["Authorization"] = f"token {self.token}"
        return h

    def post(self, path, body):
        return http("POST", f"{SOURCEGULL}{path}", body, self.headers(), self.opener)

    def gql(self, query, variables=None, allow_errors=False):
        status, out = self.post("/.api/graphql", {"query": query, "variables": variables or {}})
        if status != 200 or not isinstance(out, dict):
            fail(f"GraphQL {status}: {out}")
        if out.get("errors") and not allow_errors:
            fail(f"GraphQL errors: {out['errors']} for {query[:80]}")
        return out if allow_errors else out["data"]


def sourcegull_ready():
    try:
        status, _ = http("GET", f"{SOURCEGULL}/healthz")
        return status == 200
    except OSError:
        return False


def sourcegull_setup():
    wait("Sourcegull at " + SOURCEGULL, sourcegull_ready, timeout=600)
    s = Sourcegull()
    status, out = s.post("/-/sign-up", {"username": ADMIN[0], "email": ADMIN[1],
                                        "password": ADMIN[2]})
    if status != 200:
        status, out = s.post("/-/sign-in", {"username": ADMIN[0], "password": ADMIN[2]})
        if status != 200:
            fail(f"sign-in: {status} {out}")
    viewer = s.gql("{ currentUser { id siteAdmin } }")["currentUser"]
    if not viewer["siteAdmin"]:
        fail("the admin user is not a site admin (was Sourcegull installed fresh?)")
    token = s.gql("mutation($u: ID!) { createAccessToken(user: $u, scopes: [\"user:all\"], "
                  "note: \"codeintel test\") { token } }", {"u": viewer["id"]})
    token = token["createAccessToken"]["token"]
    api = Sourcegull(token)

    conf = api.gql("{ site { configuration { id effectiveContents } } }")["site"]["configuration"]
    site = json.loads(strip_jsonc(conf["effectiveContents"]) or "{}")
    site.update({
        "externalURL": SOURCEGULL,
        "executors.accessToken": EXECUTOR_TOKEN,
        "codeIntel.gitlabJobTokenUploads": True,
        "codeIntelAutoIndexing.enabled": True,
    })
    if not EXECUTOR_TOKEN:
        fail("EXECUTOR_TOKEN is not set")
    api.gql("mutation($id: Int!, $input: String!) { updateSiteConfiguration(lastID: $id, "
            "input: $input) }", {"id": conf["id"], "input": json.dumps(site, indent=2)})
    log("Sourcegull: site config set (externalURL, executors.accessToken, "
        "codeIntel.gitlabJobTokenUploads, codeIntelAutoIndexing.enabled)")

    services = api.gql("{ externalServices { nodes { id displayName } } }")
    if not any(n["displayName"] == "Code intel test GitLab" for n in services["externalServices"]["nodes"]):
        config = {"url": GITLAB, "token": GITLAB_TOKEN, "projectQuery": [f"groups/{GROUP}/projects"]}
        api.gql("mutation($input: AddExternalServiceInput!) { addExternalService(input: $input) "
                "{ id } }", {"input": {"kind": "GITLAB", "displayName": "Code intel test GitLab",
                                      "config": json.dumps(config)}})
        log(f"Sourcegull: GitLab connection to {GITLAB} (groups/{GROUP}/projects)")

    policies = api.gql("{ codeIntelligenceConfigurationPolicies { nodes { name } } }")
    if not any(n["name"] == "Code intel test auto-indexing" for n in
               policies["codeIntelligenceConfigurationPolicies"]["nodes"]):
        api.gql("""mutation($patterns: [String!]) {
            createCodeIntelligenceConfigurationPolicy(repositoryPatterns: $patterns,
                name: "Code intel test auto-indexing", type: GIT_TREE, pattern: "main",
                retentionEnabled: false, retainIntermediateCommits: false,
                indexingEnabled: true, indexIntermediateCommits: false) { id } }""",
                {"patterns": [f"{GITLAB_HOST}/{GROUP}/auto-*"]})
        log(f"Sourcegull: indexing policy for {GITLAB_HOST}/{GROUP}/auto-* (branch main)")
    save("token", token)


def strip_jsonc(text):
    """Drops // and /* */ comments and trailing commas outside strings (site config is JSONC)."""
    out, i, n, in_str = [], 0, len(text), False
    while i < n:
        c = text[i]
        if in_str:
            out.append(c)
            if c == "\\":
                out.append(text[i + 1])
                i += 1
            elif c == '"':
                in_str = False
        elif c == '"':
            in_str = True
            out.append(c)
        elif text.startswith("//", i):
            i = text.find("\n", i)
            i = n if i < 0 else i
            continue
        elif text.startswith("/*", i):
            i = text.find("*/", i) + 2
            continue
        else:
            out.append(c)
        i += 1
    s = "".join(out)
    return re.sub(r",(\s*[}\]])", r"\1", s).strip()


# ---- The two paths ----------------------------------------------------------------------


def repo_name(path):
    return f"{GITLAB_HOST}/{GROUP}/{path}"


def parse_time(s):
    return dt.datetime.fromisoformat(s.replace("Z", "+00:00")).timestamp() if s else None


INDEXES = """query($repo: ID) {
  preciseIndexes(repo: $repo, first: 50) { nodes {
    id state inputCommit inputRoot inputIndexer failure
    queuedAt indexingStartedAt indexingFinishedAt uploadedAt processingStartedAt
    processingFinishedAt
    steps { index { logEntry { exitCode durationMilliseconds } } }
  } } }"""


def wait_repos(api, paths):
    def cloned():
        out = api.gql("""query($q: String) { repositories(first: 100, query: $q) {
            nodes { id name mirrorInfo { cloned } } } }""", {"q": f"{GROUP}/"})
        nodes = {n["name"]: n for n in out["repositories"]["nodes"]}
        missing = [p for p in paths if not nodes.get(repo_name(p), {}).get("mirrorInfo", {})
                   .get("cloned")]
        return None if missing else {p: nodes[repo_name(p)]["id"] for p in paths}
    t0 = now()
    ids = wait(f"the {len(paths)} repositories to be cloned in Sourcegull", cloned, timeout=900)
    log(f"Sourcegull: all repositories cloned ({now() - t0:.0f} s)")
    return ids


def run_pipelines(results):
    """Starts a pipeline on main of each codeintel/ci-<lang> and waits for them."""
    started = {}
    for lang in LANGS:
        pid = project_id(f"ci-{lang}")
        p = gl("POST", f"/projects/{pid}/pipeline", {"ref": "main"})
        started[lang] = (pid, p["id"])
        log(f"GitLab: pipeline {p['id']} started for {GROUP}/ci-{lang}")
    return started


def pipeline_result(pid, pipeline):
    p = gl("GET", f"/projects/{pid}/pipelines/{pipeline}")
    if p["status"] in ("created", "waiting_for_resource", "preparing", "pending", "running"):
        return None
    jobs = gl("GET", f"/projects/{pid}/pipelines/{pipeline}/jobs?per_page=50")
    return p, jobs


def collect_pipelines(started, results):
    for lang, (pid, pipeline) in started.items():
        p, jobs = wait(f"the pipeline of {GROUP}/ci-{lang}",
                       lambda: pipeline_result(pid, pipeline), every=10)
        by_name = {j["name"]: j for j in jobs}
        index_job = by_name.get(f"sourcegull-scip-{lang}")
        upload_job = by_name.get(f"sourcegull-scip-{lang}-upload")
        r = results["ci"][lang]
        r["pipeline"] = {"id": pipeline, "status": p["status"], "duration_s": p.get("duration"),
                         "url": p["web_url"]}
        r["jobs"] = {j["name"]: {"status": j["status"], "duration_s": round(j["duration"] or 0, 1),
                                 "queued_s": round(j.get("queued_duration") or 0, 1)}
                     for j in jobs}
        bad = [j["name"] for j in jobs if j["status"] != "success"]
        if not index_job or not upload_job or bad:
            for j in jobs:
                if j["status"] != "success":
                    status, trace = http("GET", f"{GITLAB}/api/v4/projects/{pid}/jobs/{j['id']}/trace",
                                         None, {"PRIVATE-TOKEN": GITLAB_TOKEN}, raw=True)
                    tail = trace.decode(errors="replace")[-3000:] if status == 200 else ""
                    log(f"job {j['name']} {j['status']}:\n{tail}")
            r["errors"].append(f"pipeline jobs not successful: {bad or 'missing jobs'}")
        log(f"GitLab: {GROUP}/ci-{lang} pipeline {p['status']} in {p.get('duration')} s "
            f"(index {r['jobs'].get(f'sourcegull-scip-{lang}', {}).get('duration_s')} s, upload "
            f"{r['jobs'].get(f'sourcegull-scip-{lang}-upload', {}).get('duration_s')} s)")


def index_id(relay_id):
    """`U:<upload>` or `U:<upload>:I:<job>` (`I:<job>` before the job uploaded)."""
    raw = base64.b64decode(relay_id + "=" * (-len(relay_id) % 4)).decode()
    return json.loads(raw.split(":", 1)[1])


def wait_upload(api, repo_id, kind, lang, results):
    """Waits until the repository has a COMPLETED upload (and, for auto, the index job)."""
    r = results[kind][lang]

    def done():
        nodes = api.gql(INDEXES, {"repo": repo_id})["preciseIndexes"]["nodes"]
        errored = [n for n in nodes if n["state"] in ("INDEXING_ERRORED", "PROCESSING_ERRORED")]
        completed = [n for n in nodes if n["state"] == "COMPLETED"]
        return (completed, errored, nodes) if completed or errored else None
    (completed, errored, nodes) = wait(f"a completed upload for {GROUP}/{kind}-{lang}", done)
    r["indexes"] = [{"id": index_id(x["id"]), "state": x["state"], "indexer": x["inputIndexer"]}
                    for x in nodes]
    if not completed:
        n = errored[0]
        r["errors"].append(f"index {index_id(n['id'])} {n['state']}: {n['failure']}")
        log(f"Sourcegull: {GROUP}/{kind}-{lang} {index_id(n['id'])} {n['state']}: {n['failure']}")
        return
    n = completed[0]
    nid = index_id(n["id"])
    r["upload"] = {
        "id": nid,
        "indexer": n["inputIndexer"],
        "commit": n["inputCommit"],
        "processing_s": span(n["processingStartedAt"], n["processingFinishedAt"]),
        "upload_to_completed_s": span(n["uploadedAt"], n["processingFinishedAt"]),
    }
    if kind == "auto":
        if ":I:" not in nid:
            r["errors"].append(f"upload {nid} is not tied to an index job")
        r["upload"]["job_queued_to_start_s"] = span(n["queuedAt"], n["indexingStartedAt"])
        r["upload"]["job_s"] = span(n["indexingStartedAt"], n["indexingFinishedAt"])
        entry = ((n.get("steps") or {}).get("index") or {}).get("logEntry") or {}
        if entry.get("durationMilliseconds") is not None:
            r["upload"]["indexer_step_s"] = round(entry["durationMilliseconds"] / 1000, 1)
        r["upload"]["queued_to_completed_s"] = span(n["queuedAt"], n["processingFinishedAt"])
    elif ":I:" in nid:
        r["errors"].append(f"CI upload {nid} unexpectedly has an index job")
    if n["inputIndexer"] != LANGS[lang]["indexer"]:
        r["errors"].append(f"indexer {n['inputIndexer']}, want {LANGS[lang]['indexer']}")
    log(f"Sourcegull: {GROUP}/{kind}-{lang} {nid} COMPLETED ({n['inputIndexer']}, processing "
        f"{r['upload']['processing_s']} s" + (f", index job {r['upload'].get('job_s')} s"
                                              if kind == "auto" else "") + ")")


def span(a, b):
    a, b = parse_time(a), parse_time(b)
    return round(b - a, 1) if a and b else None


# ---- Navigation checks ------------------------------------------------------------------


BLOB = """query($repo: String!, $path: String!, $line: Int!, $ch: Int!) {
  repository(name: $repo) { commit(rev: "HEAD") { oid blob(path: $path) {
    content
    symbolInfo(line: $line, character: $ch) {
      definition { repo commit path range { line character length } }
      hover provenance range { start { line character } end { line character } }
    } } } } }"""

USAGES = """query($range: RangeInput!, $after: String) {
  usagesForSymbol(range: $range, first: 1000, after: $after) {
    nodes { provenance usageKind usageRange { repository path
      range { start { line character } end { line character } } } }
    pageInfo { hasNextPage endCursor } } }"""


def locate(api, repo, place):
    """(line, UTF-16 character) of a place, from the file at HEAD."""
    path, needle, token, nth = place
    out = api.gql(BLOB, {"repo": repo, "path": path, "line": 0, "ch": 0})
    lines = out["repository"]["commit"]["blob"]["content"].split("\n")
    line_no = next((i for i, l in enumerate(lines) if needle in l), None)
    if line_no is None:
        fail(f"{repo}: {needle!r} not found in {path}")
    text = lines[line_no]
    matches = [m.start() for m in re.finditer(rf"\b{re.escape(token)}\b", text)
               if m.start() >= text.index(needle)]
    col = matches[nth]
    return line_no, len(text[:col].encode("utf-16-le")) // 2


def symbol_info(api, repo, place):
    line, ch = locate(api, repo, place)
    t0 = now()
    out = api.gql(BLOB, {"repo": repo, "path": place[0], "line": line, "ch": ch})
    ms = round((now() - t0) * 1000)
    where = f"{place[0]}:{line + 1}:{ch + 1} {place[2]}"
    return out["repository"]["commit"]["blob"]["symbolInfo"], where, line, ch, ms


def verify_repo(api, kind, lang, results):
    spec = LANGS[lang]
    repo = repo_name(f"{kind}-{lang}")
    r = results[kind][lang]
    checks = r["checks"]
    info, where, line, ch, ms = symbol_info(api, repo, spec["site"])
    if not info:
        r["errors"].append(f"no symbol at {where}")
        return
    d = info.get("definition") or {}
    checks["site"] = where
    checks["provenance"] = info["provenance"]
    checks["definition"] = f"{d.get('path')}:{(d.get('range') or {}).get('line', -1) + 1}"
    checks["symbolInfo_ms"] = ms
    if info["provenance"] != "PRECISE":
        r["errors"].append(f"{where}: provenance {info['provenance']}, want PRECISE")
    if d.get("path") != spec["definition"]:
        r["errors"].append(f"{where}: definition in {d.get('path')}, want {spec['definition']}")
    elif d.get("path") == spec["site"][0]:
        r["errors"].append(f"{where}: definition not in another file")

    hover_info, hover_where = info, where
    if spec.get("hover_site"):
        hover_info, hover_where, *_ = symbol_info(api, repo, spec["hover_site"])
    hover = (hover_info or {}).get("hover") or ""
    checks["hover_site"] = hover_where
    checks["hover"] = " ".join(hover.split())[:160]
    if (hover_info or {}).get("provenance") != "PRECISE":
        r["errors"].append(f"{hover_where}: hover provenance {(hover_info or {}).get('provenance')}")
    if spec["hover"] not in hover:
        r["errors"].append(f"{hover_where}: hover lacks {spec['hover']!r}: {hover[:200]!r}")

    rng = {"repository": repo, "path": spec["site"][0], "start": {"line": line, "character": ch},
           "end": {"line": line, "character": ch + len(spec["site"][2])}}
    nodes, after = [], None
    t0 = now()
    while True:
        u = api.gql(USAGES, {"range": rng, "after": after})["usagesForSymbol"]
        nodes += u["nodes"]
        if not u["pageInfo"]["hasNextPage"]:
            break
        after = u["pageInfo"]["endCursor"]
    checks["usages_ms"] = round((now() - t0) * 1000)
    # Only this repository's usages prove this path: the other path's copy of the project shares
    # global symbols (same package and version) and shows up as cross-repository usages.
    checks["other_repository_usages"] = sum(n["usageRange"]["repository"] != repo for n in nodes)
    nodes = [n for n in nodes if n["usageRange"]["repository"] == repo]
    precise = [n for n in nodes if n["provenance"] == "PRECISE"]
    refs = [n for n in precise if n["usageKind"] == "REFERENCE"]
    defs = [n for n in precise if n["usageKind"] == "DEFINITION"]
    files = sorted({n["usageRange"]["path"] for n in refs})
    checks["precise_definitions"] = len(defs)
    checks["precise_references"] = len(refs)
    checks["reference_files"] = len(files)
    checks["non_precise_usages"] = len(nodes) - len(precise)
    if not defs or defs[0]["usageRange"]["path"] != spec["definition"]:
        r["errors"].append(f"{where}: precise definitions {[n['usageRange']['path'] for n in defs]}")
    missing = [f for f in spec["references"] if f not in files]
    if missing or len(files) < 2:
        r["errors"].append(f"{where}: precise references miss {missing} (files: {files[:20]})")
    log(f"check {repo}: {where} -> {checks['definition']} ({info['provenance']}); "
        f"{len(refs)} precise references in {len(files)} files; hover at {hover_where} "
        f"{'has' if spec['hover'] in hover else 'LACKS'} {spec['hover']!r}")


# ---- Driver -----------------------------------------------------------------------------


def save(key, value):
    os.makedirs(WORK, exist_ok=True)
    state = load()
    state[key] = value
    with open(f"{WORK}/state.json", "w") as f:
        json.dump(state, f, indent=2)


def load():
    try:
        with open(f"{WORK}/state.json") as f:
            return json.load(f)
    except FileNotFoundError:
        return {}


def new_results():
    return {kind: {lang: {"errors": [], "checks": {}} for lang in LANGS} for kind in ("ci", "auto")}


def run(verify_only=False):
    api = Sourcegull(load()["token"])
    paths = [f"{kind}-{lang}" for kind in ("ci", "auto") for lang in LANGS]
    ids = wait_repos(api, paths)
    results = new_results()
    t_start = now()
    if not verify_only and "ci" in KINDS:
        collect_pipelines(run_pipelines(results), results)
    for kind in KINDS:
        for lang in LANGS:
            wait_upload(api, ids[f"{kind}-{lang}"], kind, lang, results)
    results["wall_s"] = round(now() - t_start)
    for kind in KINDS:
        for lang in LANGS:
            if results[kind][lang].get("upload"):
                verify_repo(api, kind, lang, results)
    with open(f"{WORK}/results.json", "w") as f:
        json.dump(results, f, indent=2)
    return summary(results)


def summary(results):
    print("\nCode intelligence test: precise navigation via CI upload and auto-indexing on Kubernetes")
    print(f"{'path':5} {'language':11} {'indexer':16} {'upload':14} {'pipeline/job':>12} "
          f"{'processing':>10}  {'definition':44} {'refs':>9}  result")
    ok = True
    for kind in KINDS:
        for lang in LANGS:
            r = results[kind][lang]
            u = r.get("upload", {})
            c = r["checks"]
            if kind == "ci":
                dur = (r.get("pipeline") or {}).get("duration_s")
            else:
                dur = u.get("job_s")
            refs = f"{c.get('precise_references', '-')}/{c.get('reference_files', '-')}f"
            good = not r["errors"]
            ok &= good
            print(f"{kind:5} {lang:11} {u.get('indexer', '-'):16} {u.get('id', '-'):14} "
                  f"{(str(dur) + ' s') if dur is not None else '-':>12} "
                  f"{(str(u.get('processing_s')) + ' s') if u.get('processing_s') is not None else '-':>10}  "
                  f"{c.get('definition', '-'):44} {refs:>9}  {'PASS' if good else 'FAIL'}")
            for e in r["errors"]:
                print(f"      - {e}")
    print(f"\nresults: {WORK}/results.json")
    print("PASS" if ok else "FAIL")
    return 0 if ok else 1


def main():
    cmd = sys.argv[1] if len(sys.argv) > 1 else "help"
    if cmd == "gitlab-seed":
        gitlab_seed()
    elif cmd == "sourcegull-setup":
        sourcegull_setup()
    elif cmd == "run":
        sys.exit(run())
    elif cmd == "verify":
        sys.exit(run(verify_only=True))
    else:
        print(__doc__)
        sys.exit(2)


if __name__ == "__main__":
    main()
