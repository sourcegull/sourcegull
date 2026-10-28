#!/usr/bin/env python3
"""API side of the batch changes scale test (run by scripts/test/batches-scale.sh).

Runs in a container on the dev Compose network, where the dev GitLab is
http://gitlab.localhost:8929, Sourcegull (the production image) is http://sourcegull-batches-scale:7080
and the dev Postgres is postgres:5432 (Sourcegull's database: $BATCHES_SCALE_DB). Standard library only.

  check.py gitlab-seed   group batches-scale with $BATCHES_SCALE_REPOS projects
                         batches-scale/repo-NNNN (README.md and config.txt on main, created in parallel through the API; idempotent)
                         and a system hook for merge request and push events pointed at
                         Sourcegull; deletes earlier runs' merge requests and branches
  check.py sourcegull-setup  admin user and token, site config (executors.accessToken), the GitLab
                         connection with the webhook secret, the root token as the site
                         credential for batch changes
  check.py campaign      deletes leftover merge requests, waits for the mirrors and for
                         search to match every repository, then the server-side campaign:
                         resolution, execution on the executor, auto-apply, publication as
                         merge requests (checked in GitLab: branch, title, diff), bulk comment
                         and merge, merge requests closed in GitLab (webhook), a narrower
                         re-apply (closed and archived), the burndown
  check.py local-spec    writes the spec for `gull bulk apply` (the local path) to $WORK/local
  check.py local         checks the local run's changesets and merge requests, then prints the
                         summary
  check.py verify        re-checks the last run's batch change (states, GitLab, burndown)

Results go to $WORK/results.json and a summary to stdout.
"""

import base64
import concurrent.futures as cf
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
SOURCEGULL = os.environ.get("SOURCEGULL_URL", "http://sourcegull-batches-scale:7080")
EXECUTOR_TOKEN = os.environ.get("EXECUTOR_TOKEN", "")
WORK = os.environ.get("WORK", "/src/.test-out/batches-scale")
DB_URL = f"postgres://sourcegull:sourcegull@postgres:5432/{os.environ.get('BATCHES_SCALE_DB', 'sourcegull_batches_scale')}"
GROUP = "batches-scale"
REPOS = int(os.environ.get("BATCHES_SCALE_REPOS", "1000"))
TIMEOUT = int(os.environ.get("BATCHES_SCALE_TIMEOUT", "3600"))
ADMIN = ("admin", "admin@example.com", "batches-scale-test-Pw-7x")
HOOK_SECRET = "batches-scale-test-hook-secret"
HOOK_URL = f"{SOURCEGULL}/.api/gitlab-webhooks"
STEP_IMAGE = "alpine:3.20"
# GitLab API calls in flight while checking, and while seeding (project creation is heavy:
# more at once only makes Gitaly time out on the dev GitLab's single Puma process).
PARALLEL = 16
SEED_PARALLEL = int(os.environ.get("BATCHES_SCALE_SEED_PARALLEL", "4"))

# The campaign's slices of the repositories (sorted by name): a bulk comment on the first ones,
# a bulk merge of the next, merge requests closed in GitLab after those, and the last ones
# dropped by the narrower re-apply. The local path runs on its own small subset.
N_COMMENT = min(50, max(1, REPOS // 20))
N_MERGE = min(10, max(1, REPOS // 100))
N_CLOSE = min(5, max(1, REPOS // 200))
N_DROP = min(50, max(1, REPOS // 20))
N_LOCAL = min(20, max(1, REPOS // 50))


def log(msg):
    print(f"[{dt.datetime.now():%H:%M:%S}] {msg}", flush=True)


def fail(msg):
    raise SystemExit(f"FAIL: {msg}")


def now():
    return time.time()


def wait(what, fn, timeout=TIMEOUT, every=2.0):
    """Calls fn until it returns something truthy; fails after timeout seconds."""
    deadline = now() + timeout
    while True:
        v = fn()
        if v:
            return v
        if now() > deadline:
            fail(f"timed out waiting for {what}")
        time.sleep(every)


def http(method, url, body=None, headers=None, opener=None, raw=False, timeout=120):
    data = None
    headers = dict(headers or {})
    if body is not None:
        data = json.dumps(body).encode()
        headers.setdefault("Content-Type", "application/json")
    req = urllib.request.Request(url, data=data, method=method, headers=headers)
    try:
        with (opener or urllib.request.build_opener()).open(req, timeout=timeout) as r:
            out = r.read()
            return r.status, (out if raw else (json.loads(out) if out else None))
    except urllib.error.HTTPError as e:
        out = e.read()
        try:
            return e.code, json.loads(out)
        except ValueError:
            return e.code, out.decode(errors="replace")


def parse_time(s):
    return dt.datetime.fromisoformat(s.replace("Z", "+00:00")).timestamp() if s else None


def pct(values, p):
    """The p-th percentile (nearest rank) of values, or None."""
    if not values:
        return None
    v = sorted(values)
    return v[min(len(v) - 1, max(0, int(round(p / 100 * len(v) + 0.5)) - 1))]


def parallel(fn, items, workers=PARALLEL):
    with cf.ThreadPoolExecutor(workers) as pool:
        return list(pool.map(fn, items))


# ---- GitLab -----------------------------------------------------------------------------


def gl(method, path, body=None, ok=(200, 201, 202, 204), retries=5):
    for attempt in range(retries):
        try:
            status, out = http(method, f"{GITLAB}/api/v4{path}", body,
                               {"PRIVATE-TOKEN": GITLAB_TOKEN})
        except OSError as e:
            status, out = 0, str(e)
        if status in ok:
            return out
        # GitLab answers 502 while Puma restarts a worker, 429 when throttled, and 400 with
        # Gitaly's "Deadline Exceeded" when overloaded.
        transient = status in (0, 429, 500, 502, 503, 504) or (
            status == 400 and "Deadline Exceeded" in str(out))
        if transient and attempt + 1 < retries:
            time.sleep(2 * (attempt + 1))
            continue
        fail(f"GitLab {method} {path}: {status} {str(out)[:300]}")


def gl_get_or_none(path):
    status, out = http("GET", f"{GITLAB}/api/v4{path}", None, {"PRIVATE-TOKEN": GITLAB_TOKEN})
    return out if status == 200 else None


def gl_all(path):
    """Every page of a GitLab list (offset pagination, 100 per page)."""
    out, page = [], 1
    sep = "&" if "?" in path else "?"
    while True:
        items = gl("GET", f"{path}{sep}per_page=100&page={page}")
        out.extend(items)
        if len(items) < 100:
            return out
        page += 1


def repo_path(i):
    return f"repo-{i:04d}"


def seed_files(path):
    return {
        "README.md": f"# {path}\n\nA project of the Sourcegull batch changes scale test.\n",
        "config.txt": f"name = {path}\nversion = 1\n",
    }


def group_id():
    g = gl_get_or_none(f"/groups/{GROUP}")
    return g["id"] if g else None


def gitlab_seed():
    t0 = now()
    gid = group_id()
    if not gid:
        gid = gl("POST", "/groups", {"name": GROUP, "path": GROUP, "visibility": "public"})["id"]
        log(f"GitLab: created group {GROUP}")
    existing = {p["path"]: p for p in gl_all(f"/groups/{gid}/projects?simple=false")}
    wanted = [repo_path(i) for i in range(REPOS)]

    def ensure(path):
        # Retried by hand: a request GitLab timed out on may still have succeeded.
        p = existing.get(path)
        created = False
        for attempt in range(8):
            if p:
                break
            status, out = http("POST", f"{GITLAB}/api/v4/projects", {
                "name": path, "path": path, "namespace_id": gid, "visibility": "public",
                "initialize_with_readme": False}, {"PRIVATE-TOKEN": GITLAB_TOKEN})
            if status == 201:
                p, created = out, True
            else:
                p = gl_get_or_none(f"/projects/{urllib.parse.quote(f'{GROUP}/{path}', safe='')}")
                if not p:
                    time.sleep(2 * (attempt + 1))
        if not p:
            fail(f"GitLab: could not create {GROUP}/{path}")
        if not (created or p.get("empty_repo")):
            return created
        for attempt in range(8):
            status, out = http("POST", f"{GITLAB}/api/v4/projects/{p['id']}/repository/commits", {
                "branch": "main", "commit_message": "Initial commit",
                "author_name": "Sourcegull batch changes scale test", "author_email": "batches-scale@example.com",
                "actions": [{"action": "create", "file_path": f, "content": c}
                            for f, c in seed_files(path).items()]},
                {"PRIVATE-TOKEN": GITLAB_TOKEN})
            if status == 201 or gl_get_or_none(f"/projects/{p['id']}/repository/branches/main"):
                return created
            time.sleep(2 * (attempt + 1))
        fail(f"GitLab: could not commit to {GROUP}/{path}: {status} {out}")

    missing = [p for p in wanted if p not in existing or existing[p].get("empty_repo")]
    if missing:
        log(f"GitLab: creating {len(missing)} of {REPOS} projects in {GROUP} "
            f"({SEED_PARALLEL} at once)")
        created = sum(parallel(ensure, missing, SEED_PARALLEL))
        log(f"GitLab: {created} projects created in {now() - t0:.0f} s")
    else:
        log(f"GitLab: all {REPOS} projects exist")

    # One system hook for every project's merge request and push events (GitLab CE has no
    # group hooks).
    hooks = gl("GET", "/hooks")
    hook = next((h for h in hooks if h["url"] == HOOK_URL), None)
    body = {"url": HOOK_URL, "token": HOOK_SECRET, "merge_requests_events": True,
            "push_events": True, "tag_push_events": False, "repository_update_events": False,
            "enable_ssl_verification": False}
    if hook:
        gl("PUT", f"/hooks/{hook['id']}", body)
    else:
        gl("POST", "/hooks", body)
        log(f"GitLab: system hook for merge request events → {HOOK_URL}")
    # Earlier runs' merge requests go before Sourcegull starts, so deleting them (slow on GitLab)
    # does not slow the mirroring down.
    t1 = now()
    deleted = cleanup_gitlab()
    if deleted:
        log(f"GitLab: deleted {deleted} merge requests of earlier runs ({now() - t1:.0f} s)")
    save("seed", {"repos": REPOS, "seconds": round(t1 - t0, 1), "created": len(missing)})


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
        for attempt in range(5):
            try:
                status, out = self.post("/.api/graphql",
                                        {"query": query, "variables": variables or {}})
                break
            except OSError as e:
                if attempt == 4:
                    fail(f"GraphQL: {e}")
                time.sleep(2)
        if status != 200 or not isinstance(out, dict):
            fail(f"GraphQL {status}: {str(out)[:500]}")
        if out.get("errors") and not allow_errors:
            fail(f"GraphQL errors: {out['errors']} for {query[:100]}")
        return out if allow_errors else out["data"]


def sourcegull_ready():
    try:
        status, _ = http("GET", f"{SOURCEGULL}/healthz", timeout=5)
        return status == 200
    except OSError:
        return False


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
    return re.sub(r",(\s*[}\]])", r"\1", "".join(out)).strip()


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
                  "note: \"batches-scale test\") { token } }", {"u": viewer["id"]})
    token = token["createAccessToken"]["token"]
    api = Sourcegull(token)

    if not EXECUTOR_TOKEN:
        fail("EXECUTOR_TOKEN is not set")
    conf = api.gql("{ site { configuration { id effectiveContents } } }")["site"]["configuration"]
    site = json.loads(strip_jsonc(conf["effectiveContents"]) or "{}")
    site.update({"externalURL": SOURCEGULL, "executors.accessToken": EXECUTOR_TOKEN})
    api.gql("mutation($id: Int!, $input: String!) { updateSiteConfiguration(lastID: $id, "
            "input: $input) }", {"id": conf["id"], "input": json.dumps(site, indent=2)})
    log("Sourcegull: site config set (externalURL, executors.accessToken)")

    config = {"url": GITLAB, "token": GITLAB_TOKEN, "projectQuery": [f"groups/{GROUP}/projects"],
              "webhooks": [{"secret": HOOK_SECRET}]}
    api.gql("mutation($input: AddExternalServiceInput!) { addExternalService(input: $input) "
            "{ id } }", {"input": {"kind": "GITLAB", "displayName": "Batch changes scale test GitLab",
                                  "config": json.dumps(config)}})
    log(f"Sourcegull: GitLab connection to {GITLAB} (groups/{GROUP}/projects, webhook secret)")

    hosts = api.gql("{ batchChangesCodeHosts { nodes { externalServiceKind externalServiceURL "
                    "credential { id } } } }")["batchChangesCodeHosts"]["nodes"]
    host = next((h for h in hosts if h["externalServiceKind"] == "GITLAB"), None)
    if not host:
        fail(f"batchChangesCodeHosts has no GitLab: {hosts}")
    api.gql("""mutation($url: String!, $token: String!) {
        createBatchChangesCredential(externalServiceKind: GITLAB, externalServiceURL: $url,
            credential: $token) { id } }""", {"url": host["externalServiceURL"],
                                               "token": GITLAB_TOKEN})
    log(f"Sourcegull: site credential for {host['externalServiceURL']} (the root token)")
    save("token", token)
    save("user", viewer["id"])
    save("connected_at", now())


# ---- Campaign ---------------------------------------------------------------------------

REPO_RE = rf"^{re.escape(GITLAB_HOST)}/{GROUP}/repo-[0-9]+$"


def spec_yaml(run, name, query, branch, title):
    return f"""version: 2
name: {name}
description: Bumps the version in config.txt (batch changes scale test, run {run}).
on:
  - repositoriesMatchingQuery: '{query}'
steps:
  - run: sed -i 's/^version = .*/version = {run}/' config.txt
    container: {STEP_IMAGE}
changesetTemplate:
  title: '{title}'
  body: Sets `version = {run}` in config.txt (Sourcegull batch changes scale test).
  branch: {branch}
  commit:
    message: Set the version to {run}
    author:
      name: Sourcegull batch changes scale test
      email: batches-scale@example.com
  published: true
"""


def query_for(exclude=()):
    q = f"repo:{REPO_RE} file:^config\\.txt$ version"
    if exclude:
        alts = "|".join(exclude)
        q += f" -repo:^{re.escape(GITLAB_HOST)}/{GROUP}/({alts})$"
    return q


def repo_name(path):
    return f"{GITLAB_HOST}/{GROUP}/{path}"


def db(sql):
    """Rows of a query on Sourcegull's database (tab-separated)."""
    out = subprocess.run(["psql", DB_URL, "-At", "-F", "\t", "-v", "ON_ERROR_STOP=1", "-c", sql],
                         check=True, capture_output=True, text=True).stdout
    return [line.split("\t") for line in out.splitlines() if line]


def cleanup_gitlab():
    """Deletes the merge requests and branches of earlier runs, so each run starts clean."""
    gid = group_id()
    mrs = gl_all(f"/groups/{gid}/merge_requests?state=all&scope=all")
    if not mrs:
        return 0

    def delete(mr):
        gl("DELETE", f"/projects/{mr['project_id']}/merge_requests/{mr['iid']}", ok=(204, 404))
        if mr["source_branch"] != "main":
            b = urllib.parse.quote(mr["source_branch"], safe="")
            gl("DELETE", f"/projects/{mr['project_id']}/repository/branches/{b}",
               ok=(204, 404))
    parallel(delete, mrs)
    return len(mrs)


def wait_mirrored(api, results):
    t_conn = load().get("connected_at") or now()

    def mirrored():
        out = api.gql("""query($q: String) { repositories(first: 1, query: $q,
            cloneStatus: CLONED) { totalCount } }""", {"q": f"{GROUP}/repo-"})
        return out["repositories"]["totalCount"] >= REPOS
    wait(f"{REPOS} repositories to be mirrored", mirrored, every=5)
    # From the repo node's queue: the clones queued by discovery (the time from the connection
    # to the last clone, wherever this check started waiting).
    row = db("SELECT extract(epoch FROM max(finished_at)), count(*), "
             "extract(epoch FROM percentile_cont(0.95) WITHIN GROUP "
             "(ORDER BY finished_at - started_at)) FROM repo_update_jobs "
             "WHERE reason = 'discovered' AND state = 'completed'")[0]
    results["mirror_s"] = round(float(row[0]) - t_conn, 1)
    results["clone_p95_s"] = round(float(row[2]), 2)
    log(f"Sourcegull: {REPOS} repositories mirrored, the last {results['mirror_s']} s after the "
        f"connection was added (clone p95 {results['clone_p95_s']} s)")


def wait_searchable(api, results, spec):
    t0 = now()
    t_conn = load().get("connected_at") or t0

    def matched():
        ws = api.gql("query($s: String!) { resolveWorkspacesForBatchSpec(batchSpec: $s) "
                     "{ repository { name } } }", {"s": spec})["resolveWorkspacesForBatchSpec"]
        return len({w["repository"]["name"] for w in ws}) >= REPOS
    wait(f"search to match all {REPOS} repositories", matched, every=5)
    results["searchable_s"] = round(now() - t_conn, 1)
    log(f"Sourcegull: search matches all {REPOS} repositories (checked {results['searchable_s']} s "
        "after the connection was added)")


SPEC_STATE = """query($id: ID!) { node(id: $id) { ... on BatchSpec {
    state startedAt finishedAt failureMessage
    workspaceResolution { state failureMessage startedAt finishedAt
        workspaces(first: 1) { totalCount stats { queued processing completed errored
            ignored } } }
    appliesToBatchChange { id }
} } }"""

STATS = """query($id: ID!) { node(id: $id) { ... on BatchChange {
    currentSpec { id }
    changesetsStats { unpublished draft open merged closed deleted retrying failed scheduled
        processing archived total }
} } }"""

CHANGESETS = """query($id: ID!, $after: String, $archived: Boolean) { node(id: $id) {
  ... on BatchChange { changesets(first: 100, after: $after, onlyArchived: $archived) {
    pageInfo { hasNextPage endCursor }
    nodes { ... on ExternalChangeset { id state externalID title error
        repository { name } externalURL { url } currentSpec { description {
          ... on GitBranchChangesetDescription { headRef } } } } }
} } } }"""


def run_spec(api, ns, bc_id, spec, results, key, expect_workspaces):
    """createBatchSpecFromRaw, resolution, executeBatchSpec with auto-apply; returns the spec ID."""
    t0 = now()
    bs = api.gql("""mutation($s: String!, $ns: ID!, $bc: ID!) {
        createBatchSpecFromRaw(batchSpec: $s, namespace: $ns, batchChange: $bc) { id } }""",
                 {"s": spec, "ns": ns, "bc": bc_id})["createBatchSpecFromRaw"]["id"]

    def resolved():
        n = api.gql(SPEC_STATE, {"id": bs})["node"]["workspaceResolution"]
        if n and n["state"] in ("FAILED", "ERRORED"):
            fail(f"workspace resolution {n['state']}: {n['failureMessage']}")
        return n if n and n["state"] == "COMPLETED" else None
    res = wait("the workspace resolution", resolved, every=1)
    total = res["workspaces"]["totalCount"]
    r = results[key] = {"spec": bs, "workspaces": total,
                        "resolution_s": round(now() - t0, 1)}
    log(f"{key}: {total} workspaces resolved in {r['resolution_s']} s")
    if total != expect_workspaces:
        fail(f"{key}: {total} workspaces, expected {expect_workspaces}")

    t1 = now()
    api.gql("mutation($id: ID!) { executeBatchSpec(batchSpec: $id, autoApplyEnabled: true) "
            "{ state } }", {"id": bs})
    last = [None]

    def executed():
        n = api.gql(SPEC_STATE, {"id": bs})["node"]
        st = n["workspaceResolution"]["workspaces"]["stats"]
        line = f"{n['state']} {st}"
        if line != last[0] and int(now() - t1) % 30 < 3:
            log(f"{key}: {line}")
            last[0] = line
        if n["state"] in ("FAILED", "CANCELED"):
            fail(f"{key}: execution {n['state']}: {n['failureMessage']}")
        return n if n["state"] == "COMPLETED" else None
    n = wait("the execution", executed, every=2)
    r["execution_s"] = round(now() - t1, 1)
    log(f"{key}: executed in {r['execution_s']} s")

    def applied():
        cur = api.gql(STATS, {"id": bc_id})["node"]["currentSpec"]["id"]
        return cur == bs
    wait("the auto-apply", applied, timeout=600, every=1)
    r["applied_s"] = round(now() - t1, 1)
    log(f"{key}: auto-applied {r['applied_s']} s after executeBatchSpec")
    return bs


def changesets(api, bc_id, archived=False):
    out, after = [], None
    while True:
        c = api.gql(CHANGESETS, {"id": bc_id, "after": after, "archived": archived})
        c = c["node"]["changesets"]
        out.extend(c["nodes"])
        if not c["pageInfo"]["hasNextPage"]:
            return out
        after = c["pageInfo"]["endCursor"]


def stats(api, bc_id):
    return api.gql(STATS, {"id": bc_id})["node"]["changesetsStats"]


def wait_published(api, bc_id, n, key, results, t0):
    last = [None]

    def done():
        s = stats(api, bc_id)
        if s["failed"]:
            bad = [c for c in changesets(api, bc_id) if c.get("state") == "FAILED"][:3]
            fail(f"{key}: {s['failed']} changesets failed, e.g. {bad}")
        line = (f"open {s['open']}/{n}, processing {s['processing']}, retrying "
                f"{s['retrying']}, scheduled {s['scheduled']}, unpublished {s['unpublished']}")
        if line != last[0] and int(now()) % 15 < 2:
            log(f"{key}: {line}")
            last[0] = line
        return s if s["open"] >= n else None
    wait(f"{n} open changesets", done, every=2)
    results[key]["published_s"] = round(now() - t0, 1)
    log(f"{key}: {n} changesets open {results[key]['published_s']} s after executeBatchSpec")


def gitlab_mrs(branch, state="opened"):
    gid = group_id()
    b = urllib.parse.quote(branch, safe="")
    return gl_all(f"/groups/{gid}/merge_requests?state={state}&scope=all&source_branch={b}")


def check_gitlab_mrs(run, branch, title, paths, errors, check_diffs=True):
    """Every path has exactly one open MR for branch with title and the version diff."""
    mrs = gitlab_mrs(branch)
    by_path = {}
    for mr in mrs:
        path = mr["references"]["full"].split("!")[0].split("/")[-1]
        by_path.setdefault(path, []).append(mr)
    missing = [p for p in paths if p not in by_path]
    extra = sorted(set(by_path) - set(paths))
    dupes = [p for p, v in by_path.items() if len(v) > 1]
    if missing:
        errors.append(f"{len(missing)} repositories without an open MR on {branch}, e.g. "
                      f"{missing[:5]}")
    if extra:
        errors.append(f"open MRs on {branch} in unexpected repositories: {extra[:5]}")
    if dupes:
        errors.append(f"several open MRs on {branch}: {dupes[:5]}")
    wrong = [p for p, v in by_path.items() if v[0]["title"] != title
             or v[0]["target_branch"] != "main"]
    if wrong:
        errors.append(f"MRs with a wrong title or target: {wrong[:5]}")
    bad_diff = []
    if check_diffs:
        def diff_ok(mr):
            # GitLab computes a new MR's diff asynchronously: empty until then.
            for _ in range(30):
                d = gl("GET", f"/projects/{mr['project_id']}/merge_requests/{mr['iid']}/diffs")
                if d:
                    break
                time.sleep(2)
            return (len(d) == 1 and d[0]["new_path"] == "config.txt"
                    and "-version = " in d[0]["diff"]
                    and f"+version = {run}\n" in d[0]["diff"])
        oks = parallel(diff_ok, [v[0] for v in by_path.values()])
        bad_diff = [p for p, ok in zip(by_path, oks) if not ok]
        if bad_diff:
            errors.append(f"MRs with an unexpected diff: {bad_diff[:5]}")
    return {"open_mrs": len(mrs), "missing": len(missing), "wrong_title": len(wrong),
            "bad_diff": len(bad_diff), "diffs_checked": len(by_path) if check_diffs else 0}, mrs


def publish_metrics(bc_db_id, mrs, t_exec):
    """Reconcile and publish timings from Sourcegull's database and the MRs' creation times."""
    rows = db(f"""SELECT extract(epoch FROM finished_at - started_at),
                         extract(epoch FROM finished_at - queued_at),
                         extract(epoch FROM started_at), extract(epoch FROM finished_at)
                  FROM changesets WHERE owned_by_batch_change_id = {bc_db_id}
                  AND publication_state = 'PUBLISHED' AND finished_at IS NOT NULL""")
    run_s = [float(r[0]) for r in rows]
    queue_to_done = [float(r[1]) for r in rows]
    created = sorted(parse_time(m["created_at"]) for m in mrs)
    m = {
        "reconcile_p50_s": round(pct(run_s, 50), 3) if run_s else None,
        "reconcile_p95_s": round(pct(run_s, 95), 3) if run_s else None,
        "reconcile_max_s": round(max(run_s), 3) if run_s else None,
        "queued_to_published_p95_s": round(pct(queue_to_done, 95), 1) if run_s else None,
        "reconciled": len(rows),
    }
    if len(created) > 1:
        span = created[-1] - created[0]
        m["publish_span_s"] = round(span, 1)
        m["publish_per_min"] = round(len(created) / span * 60, 1) if span else None
        m["first_mr_after_execute_s"] = round(created[0] - t_exec, 1)
    return m


def execution_metrics(spec_db_id):
    rows = db(f"""SELECT extract(epoch FROM j.finished_at - j.started_at),
                         extract(epoch FROM j.started_at), extract(epoch FROM j.finished_at)
                  FROM batch_spec_workspace_execution_jobs j
                  JOIN batch_spec_workspaces w ON w.id = j.batch_spec_workspace_id
                  WHERE w.batch_spec_id = {spec_db_id} AND j.state = 'completed'""")
    if not rows:
        return {"jobs": 0}
    d = [float(r[0]) for r in rows]
    start = min(float(r[1]) for r in rows)
    end = max(float(r[2]) for r in rows)
    return {"jobs": len(rows), "job_p50_s": round(pct(d, 50), 2),
            "job_p95_s": round(pct(d, 95), 2), "jobs_wall_s": round(end - start, 1),
            "jobs_per_min": round(len(rows) / (end - start) * 60, 1) if end > start else None}


def spec_db_id(spec_gql_id):
    rand = re.sub(r"[^0-9a-zA-Z]", "", decode_id(spec_gql_id))
    return int(db(f"SELECT id FROM batch_specs WHERE rand_id = '{rand}'")[0][0])


def decode_id(gql_id):
    """The key of a GraphQL ID (base64url of `Kind:key`, string keys JSON-quoted)."""
    raw = base64.urlsafe_b64decode(gql_id + "=" * (-len(gql_id) % 4)).decode()
    return raw.split(":", 1)[1].strip('"')


def bulk(api, bc_id, mutation, ids, extra="", what=""):
    t0 = now()
    op = api.gql(f"mutation($bc: ID!, $cs: [ID!]!) {{ {mutation}(batchChange: $bc, "
                 f"changesets: $cs{extra}) {{ id }} }}", {"bc": bc_id, "cs": ids})[mutation]["id"]

    def finished():
        n = api.gql("query($id: ID!) { node(id: $id) { ... on BulkOperation { state progress "
                    "errors { error } changesetCount } } }", {"id": op})["node"]
        return n if n["state"] != "PROCESSING" else None
    n = wait(f"the {what} bulk operation", finished, every=1)
    secs = round(now() - t0, 1)
    log(f"bulk {what}: {n['state']} for {n['changesetCount']} changesets in {secs} s"
        + (f", errors {n['errors'][:3]}" if n["errors"] else ""))
    return n, secs


def mr_ref(c):
    """(project path, iid) of a changeset's merge request."""
    path = c["repository"]["name"].split("/")[-1]
    return path, int(c["externalID"])


def project_ids():
    gid = group_id()
    return {p["path"]: p["id"] for p in gl_all(f"/groups/{gid}/projects?simple=true")}


CURRENT = {}


def campaign():
    results = load_results()
    CURRENT["results"] = results
    st = load()
    api = Sourcegull(st["token"])
    errors = results["errors"]
    run = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%d%H%M%S")
    branch = f"batches-scale/{run}"
    title = f"Batch changes scale test: version {run}"
    name = f"batches-scale-{run}"
    paths = [repo_path(i) for i in range(REPOS)]
    results.update({"run": run, "repos": REPOS, "branch": branch})
    save("run", {"run": run, "branch": branch, "title": title, "name": name})

    t0 = now()
    deleted = cleanup_gitlab()
    if deleted:
        log(f"GitLab: deleted {deleted} merge requests of earlier runs ({now() - t0:.0f} s)")

    spec = spec_yaml(run, name, query_for(), branch, title)
    wait_mirrored(api, results)
    wait_searchable(api, results, spec)

    ns = st["user"]
    bc = api.gql("mutation($ns: ID!, $n: String!) { createEmptyBatchChange(namespace: $ns, "
                 "name: $n) { id url } }", {"ns": ns, "n": name})["createEmptyBatchChange"]
    bc_id = bc["id"]
    save("run", {"run": run, "branch": branch, "title": title, "name": name, "bc": bc_id,
                 "url": bc["url"]})
    bc_db = int(db(f"SELECT id FROM batch_changes WHERE name = '{name}'")[0][0])

    # 1. Server-side: resolution, execution, auto-apply, publication.
    t_exec_start = now()
    bs = run_spec(api, ns, bc_id, spec, results, "server", REPOS)
    t_exec = now() - results["server"]["execution_s"]
    results["server"].update(execution_metrics(spec_db_id(bs)))
    wait_published(api, bc_id, REPOS, "server", results, t_exec)
    t_pub = now()
    gl_check, mrs = check_gitlab_mrs(run, branch, title, paths, errors)
    results["server"]["gitlab"] = gl_check
    results["server"].update(publish_metrics(bc_db, mrs, t_exec))
    results["server"]["total_s"] = round(t_pub - t_exec_start, 1)
    log(f"GitLab: {gl_check['open_mrs']} open MRs on {branch}, {gl_check['diffs_checked']} diffs "
        f"checked, {gl_check['missing']} missing")

    cs = {c["repository"]["name"].split("/")[-1]: c for c in changesets(api, bc_id)}
    bad = [p for p, c in cs.items() if c["state"] != "OPEN" or c["title"] != title
           or c["currentSpec"]["description"]["headRef"] != branch]
    if len(cs) != REPOS or bad:
        errors.append(f"{len(cs)} changesets, {len(bad)} not OPEN with the title and branch: "
                      f"{bad[:5]}")
    pids = project_ids()

    # 2. Bulk operations: a comment on the first N_COMMENT, a merge of the next N_MERGE.
    comment_paths = paths[:N_COMMENT]
    merge_paths = paths[N_COMMENT:N_COMMENT + N_MERGE]
    body = f"Bulk comment from the batch changes scale test ({run})."
    n, secs = bulk(api, bc_id, "createChangesetComments", [cs[p]["id"] for p in comment_paths],
                   ', body: "' + body + '"', "comment")
    results["bulk_comment"] = {"changesets": len(comment_paths), "state": n["state"],
                               "seconds": secs}

    def has_comment(p):
        notes = gl("GET", f"/projects/{pids[p]}/merge_requests/{cs[p]['externalID']}/notes"
                   "?per_page=100")
        return any(x["body"] == body for x in notes)
    commented = sum(parallel(has_comment, comment_paths))
    results["bulk_comment"]["found_in_gitlab"] = commented
    if n["state"] != "COMPLETED" or commented != len(comment_paths):
        errors.append(f"bulk comment: {n['state']}, {commented}/{len(comment_paths)} in GitLab")

    n, secs = bulk(api, bc_id, "mergeChangesets", [cs[p]["id"] for p in merge_paths], "",
                   "merge")
    results["bulk_merge"] = {"changesets": len(merge_paths), "state": n["state"], "seconds": secs}

    def merged_in_gitlab(p):
        return gl("GET", f"/projects/{pids[p]}/merge_requests/{cs[p]['externalID']}")["state"]
    gl_states = parallel(merged_in_gitlab, merge_paths)
    t_m = now()
    wait("merged changesets", lambda: stats(api, bc_id)["merged"] >= len(merge_paths),
         timeout=120, every=1)
    results["bulk_merge"]["merged_in_gitlab"] = gl_states.count("merged")
    results["bulk_merge"]["state_after_s"] = round(now() - t_m, 1)
    if n["state"] != "COMPLETED" or gl_states.count("merged") != len(merge_paths):
        errors.append(f"bulk merge: {n['state']}, GitLab states {gl_states}")

    # 3. Webhooks: MRs closed in GitLab turn CLOSED in Sourcegull through the merge_request hook.
    close_paths = paths[N_COMMENT + N_MERGE:N_COMMENT + N_MERGE + N_CLOSE]
    closed_at = {}
    t_close = dt.datetime.now(dt.timezone.utc).isoformat()
    for p in close_paths:
        gl("PUT", f"/projects/{pids[p]}/merge_requests/{cs[p]['externalID']}",
           {"state_event": "close"})
        closed_at[p] = now()
    latency = {}

    def all_closed():
        for p in close_paths:
            if p in latency:
                continue
            n = api.gql("query($id: ID!) { node(id: $id) { ... on ExternalChangeset { state } } }",
                        {"id": cs[p]["id"]})["node"]
            if n["state"] == "CLOSED":
                latency[p] = round(now() - closed_at[p], 2)
        return len(latency) == len(close_paths)
    try:
        wait("changesets closed in GitLab to turn CLOSED", all_closed, timeout=120, every=0.25)
    except SystemExit as e:
        errors.append(f"webhook: {e}")
    ids = ",".join(str(int(decode_id(cs[p]["id"]))) for p in close_paths)
    # Evidence that the hook did it (a sync would also find the state, but not within
    # seconds: connections with webhooks sync hourly at most): the hook's timeline events, or,
    # if a sync replaced those meanwhile, the deliveries in the webhook log.
    hook_events = int(db(f"SELECT count(*) FROM changeset_events WHERE changeset_id IN ({ids}) "
                         "AND key LIKE 'hook:%' AND kind = 'CLOSED'")[0][0])
    deliveries = int(db(f"SELECT count(*) FROM webhook_logs WHERE received_at >= '{t_close}' "
                        "AND status_code < 300 AND request::text LIKE '%merge_request%' "
                        "AND request::text LIKE '%close%'")[0][0])
    results["webhook_close"] = {"changesets": len(close_paths), "latency_s": latency,
                                "max_latency_s": max(latency.values()) if latency else None,
                                "hook_close_events": hook_events,
                                "close_deliveries": deliveries}
    log(f"webhook: {len(latency)}/{len(close_paths)} CLOSED, latencies {latency}, "
        f"{hook_events} hook close events, {deliveries} close deliveries logged")
    if max(hook_events, deliveries) < len(close_paths):
        errors.append(f"webhook: only {hook_events} close events / {deliveries} deliveries "
                      "came from the hook")
    if latency and max(latency.values()) > 30:
        errors.append(f"webhook: closing took up to {max(latency.values())} s")

    # 4. Re-apply with a narrower query: the last N_DROP repositories drop out; their
    # changesets are closed on GitLab and archived. The merged ones' repositories now have the
    # change on main, so once Sourcegull fetched them (push hook) their workspaces produce no diff
    # and those changesets are archived too, staying MERGED. Changesets closed in GitLab but
    # still in the spec are reopened on every apply.
    drop = paths[-N_DROP:]
    spec2 = spec_yaml(run, name, query_for(drop), branch, title)
    t_re = now()
    run_spec(api, ns, bc_id, spec2, results, "reapply", REPOS - N_DROP)

    def archived_and_closed():
        s = stats(api, bc_id)
        if s["archived"] < N_DROP or s["processing"] or s["retrying"]:
            return None
        arch = changesets(api, bc_id, archived=True)
        done = sum(1 for c in arch if c["state"] in ("CLOSED", "MERGED"))
        return (s, arch) if done == len(arch) else None
    s, arch = wait(f"{N_DROP} closed and archived changesets", archived_and_closed,
                   timeout=900, every=2)
    arch_paths = sorted(c["repository"]["name"].split("/")[-1] for c in arch)
    arch_merged = sorted(c["repository"]["name"].split("/")[-1] for c in arch
                         if c["state"] == "MERGED")
    not_closed = [c["repository"]["name"] for c in arch
                  if c["state"] != "CLOSED" and c["repository"]["name"].split("/")[-1] in drop]
    results["reapply"].update({"archived": s["archived"], "archived_s": round(now() - t_re, 1),
                               "archived_not_closed": len(not_closed),
                               "archived_merged": len(arch_merged)})

    def gl_state(p):
        return gl("GET", f"/projects/{pids[p]}/merge_requests/{cs[p]['externalID']}")["state"]
    dropped_states = parallel(gl_state, drop)
    results["reapply"]["closed_in_gitlab"] = dropped_states.count("closed")
    log(f"reapply: {s['archived']} archived, {dropped_states.count('closed')}/{N_DROP} closed in "
        f"GitLab, {round(now() - t_re, 1)} s")
    if (sorted(set(arch_paths) - set(arch_merged)) != sorted(drop)
            or not set(arch_merged) <= set(merge_paths) or not_closed
            or dropped_states.count("closed") != N_DROP):
        errors.append(f"reapply: archived {arch_paths[:5]}…, not CLOSED {not_closed[:5]}, "
                      f"GitLab states {set(dropped_states)}")
    reopened = parallel(gl_state, close_paths)
    results["reapply"]["reopened_in_gitlab"] = reopened.count("opened")
    if reopened.count("opened") != N_CLOSE:
        errors.append(f"reapply: MRs closed in GitLab not reopened: {reopened}")
    final = stats(api, bc_id)
    results["final_stats"] = final
    expect_open = REPOS - N_MERGE - N_DROP
    if final["open"] != expect_open or final["merged"] + len(arch_merged) != N_MERGE:
        errors.append(f"final stats {final}, expected open {expect_open}, merged {N_MERGE}")

    # 5. Burndown.
    results["burndown"] = burndown(api, bc_id, errors)
    save_results(results)


def burndown(api, bc_id, errors):
    pts = api.gql("query($id: ID!) { node(id: $id) { ... on BatchChange { "
                  "changesetCountsOverTime(includeArchived: true) { date total merged closed "
                  "open draft } } } }", {"id": bc_id})["node"]["changesetCountsOverTime"]
    last = pts[-1] if pts else {}
    out = {"points": len(pts), "last": last}
    log(f"burndown: {len(pts)} points, last {last}")
    if (not pts or last.get("total", 0) < REPOS or last.get("merged", 0) < N_MERGE
            or last.get("closed", 0) < N_DROP):
        errors.append(f"burndown: {len(pts)} points, last {last}")
    return out


# ---- Local path (`gull bulk apply`) ----------------------------------------------------


def local_spec():
    st = load()
    run = st["run"]["run"]
    paths = [repo_path(i) for i in range(N_LOCAL)]
    q = f"repo:^{re.escape(GITLAB_HOST)}/{GROUP}/({'|'.join(paths)})$ file:^config\\.txt$ version"
    spec = spec_yaml(run, f"batches-scale-local-{run}", q, f"batches-scale-local/{run}",
                     f"Batch changes scale test (local): version {run}")
    os.makedirs(f"{WORK}/local", exist_ok=True)
    with open(f"{WORK}/local/spec.yml", "w") as f:
        f.write(spec)
    with open(f"{WORK}/local/env", "w") as f:
        f.write(f"SRC_ENDPOINT={SOURCEGULL}\nSRC_ACCESS_TOKEN={st['token']}\n")
    save("local_started", now())


def local_check():
    results = load_results()
    st = load()
    run = st["run"]["run"]
    api = Sourcegull(st["token"])
    errors = results["errors"]
    paths = [repo_path(i) for i in range(N_LOCAL)]
    t0 = now()
    user = api.gql("{ currentUser { username } }")["currentUser"]["username"]
    ns = st["user"]
    bc = api.gql("query($ns: ID!, $n: String!) { batchChange(namespace: $ns, name: $n) { id } }",
                 {"ns": ns, "n": f"batches-scale-local-{run}"})["batchChange"]
    r = results["local"] = {"repos": N_LOCAL, "gull_s": load().get("local_seconds")}
    if not bc:
        errors.append(f"local: no batch change batches-scale-local-{run} for {user}")
        save_results(results)
        return
    wait_published(api, bc["id"], N_LOCAL, "local", results, t0)
    branch = f"batches-scale-local/{run}"
    gl_check, _ = check_gitlab_mrs(run, branch, f"Batch changes scale test (local): version {run}", paths,
                                   errors)
    r["gitlab"] = gl_check
    log(f"local: {gl_check['open_mrs']} open MRs on {branch}")
    save_results(results)


# ---- Verify (re-check the last run) -----------------------------------------------------


def verify():
    results = load_results()
    st = load()
    rs = st["run"]
    api = Sourcegull(st["token"])
    errors = results["errors"] = []
    paths = [repo_path(i) for i in range(REPOS)]
    open_paths = paths[:N_COMMENT] + paths[N_COMMENT + N_MERGE:REPOS - N_DROP]
    gl_check, _ = check_gitlab_mrs(rs["run"], rs["branch"], rs["title"], open_paths, errors)
    results["verify"] = {"gitlab": gl_check, "stats": stats(api, rs["bc"])}
    results["burndown"] = burndown(api, rs["bc"], errors)
    save_results(results)


# ---- State and results ------------------------------------------------------------------


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


def load_results():
    try:
        with open(f"{WORK}/results.json") as f:
            r = json.load(f)
    except FileNotFoundError:
        r = {}
    r.setdefault("errors", [])
    return r


def save_results(results):
    results["seed"] = load().get("seed")
    with open(f"{WORK}/results.json", "w") as f:
        json.dump(results, f, indent=2)


def summary():
    r = load_results()
    s = r.get("server", {})
    re_ = r.get("reapply", {})
    loc = r.get("local", {})
    wh = r.get("webhook_close", {})

    def v(x, unit=" s"):
        return "-" if x is None else f"{x}{unit}"
    print(f"\nBatch changes scale test: a {r.get('repos')}-repository campaign against GitLab CE "
          f"(run {r.get('run')})")
    rows = [
        ("GitLab seed", v((r.get("seed") or {}).get("seconds"))),
        ("mirrored (after the connection)", v(r.get("mirror_s"))),
        ("searchable (all repositories matched)", v(r.get("searchable_s"))),
        ("workspace resolution", v(s.get("resolution_s"))),
        ("execution (executeBatchSpec → COMPLETED)", v(s.get("execution_s"))),
        ("  jobs: count / p50 / p95 / per min",
         f"{s.get('jobs')} / {v(s.get('job_p50_s'))} / {v(s.get('job_p95_s'))} / "
         f"{s.get('jobs_per_min')}"),
        ("auto-apply (after executeBatchSpec)", v(s.get("applied_s"))),
        ("all open (after executeBatchSpec)", v(s.get("published_s"))),
        ("publish throughput", v(s.get("publish_per_min"), " MRs/min")),
        ("reconcile p50 / p95 / max",
         f"{v(s.get('reconcile_p50_s'))} / {v(s.get('reconcile_p95_s'))} / "
         f"{v(s.get('reconcile_max_s'))}"),
        ("GitLab: open MRs / diffs checked",
         f"{(s.get('gitlab') or {}).get('open_mrs')} / {(s.get('gitlab') or {}).get('diffs_checked')}"),
        ("bulk comment", f"{(r.get('bulk_comment') or {}).get('changesets')} in "
         f"{v((r.get('bulk_comment') or {}).get('seconds'))}"),
        ("bulk merge", f"{(r.get('bulk_merge') or {}).get('changesets')} in "
         f"{v((r.get('bulk_merge') or {}).get('seconds'))}"),
        ("closed in GitLab → CLOSED (webhook), max", v(wh.get("max_latency_s"))),
        ("re-apply: resolution / execution / archived",
         f"{v(re_.get('resolution_s'))} / {v(re_.get('execution_s'))} / "
         f"{re_.get('archived')} in {v(re_.get('archived_s'))}"),
        ("burndown points", f"{(r.get('burndown') or {}).get('points')}"),
        ("local: gull bulk apply / all open",
         f"{loc.get('repos')} repos, {v(loc.get('gull_s'))} / {v(loc.get('published_s'))}"),
    ]
    for k, val in rows:
        print(f"  {k:46} {val}")
    print(f"\nresults: {WORK}/results.json")
    for e in r["errors"]:
        print(f"  - {e}")
    print("FAIL" if r["errors"] else "PASS")
    return 1 if r["errors"] else 0


def main():
    cmd = sys.argv[1] if len(sys.argv) > 1 else "help"
    if cmd == "gitlab-seed":
        gitlab_seed()
    elif cmd == "sourcegull-setup":
        sourcegull_setup()
    elif cmd == "campaign":
        with open(f"{WORK}/results.json", "w") as f:
            json.dump({"errors": []}, f)
        try:
            campaign()
        except SystemExit as e:
            # Keep what was measured, with the failure, for the summary.
            r = CURRENT.get("results") or load_results()
            r["errors"].append(str(e))
            save_results(r)
            raise
    elif cmd == "local-spec":
        local_spec()
    elif cmd == "local":
        if len(sys.argv) > 2:
            save("local_seconds", float(sys.argv[2]))
        local_check()
        sys.exit(summary())
    elif cmd == "verify":
        verify()
        sys.exit(summary())
    elif cmd == "summary":
        sys.exit(summary())
    else:
        print(__doc__)
        sys.exit(2)


if __name__ == "__main__":
    main()
