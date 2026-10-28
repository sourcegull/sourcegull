# Sizing the Sourcegull chart

Three presets in [`examples/`](examples/), for installs from a team trial up to a large instance.
Use the closest one as a starting point and change it once you have measured your own load.

**All numbers here are estimates.** They come from the sizing rules and performance targets
below, not from load tests in a cluster.

## The rules the presets apply

| Component | Rule | Status |
|---|---|---|
| repo node disk | about 1.5 × the size of the mirrored repositories on that node (bare repos, index shards, caches) | estimate |
| repo node RAM | about 0.5 × the indexed source on that node (the index's hot set) + 2 GiB | estimate |
| repo node baseline | < 500 MiB RSS + mmap of the index | target |
| index size | ≤ 0.6 × the indexed source | target |
| web | 1 vCPU / 1 GiB per ~50 concurrent searchers; idle RSS < 150 MiB | estimate / target |
| worker | CPU-bound on SCIP processing (≥ 100 MB/s of SCIP input per core) and searches for watches, trends and search jobs; scale on queue depth | target |
| PostgreSQL | everything but git data and the index: metadata, queues, permissions, code graph metadata, trends series | estimate |

"Indexed source" is the size of the indexed branches' files (by default the default branch of
each repository), usually far smaller than the bare repositories, which carry the whole history.
A rough first guess is a quarter to a fifth of the mirrored size.

## Presets

| | [small](examples/small.yaml) | [medium](examples/medium.yaml) | [large](examples/large.yaml) |
|---|---|---|---|
| Repositories | up to ~1,000 | up to ~10,000 | up to ~50,000 |
| Mirrored repositories | ~20 GB | ~300 GB | ~2 TB |
| Indexed source | ~5 GB | ~60 GB | ~400 GB |
| Concurrent searchers | ~25 | ~250 | ~1,000 |
| web | 2–3 × (0.5 vCPU, 0.5–1 GiB) | 3–8 × (1 vCPU, 1–2 GiB) | 4–20 × (2 vCPU, 2–4 GiB) |
| worker | 1 × (0.25 vCPU, 0.25–1 GiB) | KEDA 2–8 × (0.5 vCPU, 0.5–2 GiB) | KEDA 3–20 × (1 vCPU, 1–4 GiB) |
| repo nodes | 1 × (2 vCPU, 5–6 GiB, 50 Gi) | 3 × (4 vCPU, 12–16 GiB, 200 Gi) | 6 × (8 vCPU, 36–48 GiB, 600 Gi) |
| blob store | builtin, 20 Gi | builtin, 100 Gi | S3-compatible bucket |
| PostgreSQL | bundled, 1 vCPU / 2 GiB, 20 Gi | CloudNativePG, 3 × (2 vCPU, 16 GiB, 100 Gi + 20 Gi WAL) | external, ~16 vCPU / 64 GiB |
| executors | off | off | 2 × 8 concurrent jobs |
| Cluster add-ons | none | CloudNativePG operator, KEDA | KEDA |
| Total requests (min replicas, without PostgreSQL) | ~3.5 vCPU, ~6.5 GiB | ~16 vCPU, ~40 GiB | ~59 vCPU, ~227 GiB, plus executor jobs (up to 16 × 2 vCPU / 4 GiB) |

(Requests / limits: the presets set CPU requests and memory requests and limits; there are no CPU
limits, so pods can use idle CPU.)

### Notes

- **Repo nodes can grow.** After `repo.replicaCount` changes and every repo pod runs with the new
  node list, about 1/N of the repositories move to their new node: copied from the old node (no
  load on the code host), checked, switched over, then removed from the old node after a few
  minutes. Searches keep working during a move; Site admin > Repo nodes shows the progress. Moves
  need free space on the new nodes for their share. Scaling in clones the removed pods'
  repositories again from the code host (their volumes are gone). Leave room for growth in the
  PVC size; most storage classes can expand a PVC later, which the StatefulSet's volume template
  cannot.
- **Memory limits of repo nodes** are kept above the requests: index files are memory-mapped,
  and page cache use counts towards the container's memory.
- **Workers** also run the leader-elected singleton tasks, so they never scale below one replica.
  With KEDA (`worker.autoscaling.keda`) the count follows the pgqueue depth: jobs queued and due,
  plus jobs running, over every queue the worker processes, divided by
  `targetJobsPerReplica`. `trigger: prometheus` reads the same depth from the
  `sourcegull_queue_jobs` gauge in Prometheus instead of querying the database (README, "Worker
  autoscaling"). Without KEDA, `worker.autoscaling.hpa` scales on CPU instead.
- **PostgreSQL**: `shared_buffers` about a quarter of the database's memory and
  `effective_cache_size` about three quarters, as in the presets. Use connection limits that fit
  the web, worker and repo pod counts at their maximum replicas (each pod keeps a pool).
- **Search latency targets** (end to end at the web role): indexed literal search p50 < 30 ms
  and p99 < 200 ms to the first result on 10 GB of source with 8 cores, warm; unindexed search of
  the Linux kernel at an arbitrary commit < 2 s. They assume the index's hot set fits in RAM, which
  is what the repo node RAM rule is for.

## Checking a values file

`scripts/test/helm-ct.sh template` runs `helm lint --strict` and `helm template` with every preset,
every `ci/*-values.yaml` and a matrix of options. Run it after changing a preset.
