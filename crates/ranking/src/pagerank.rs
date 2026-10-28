//! Sparse PageRank by power iteration.
//!
//! The graph is stored as CSR over *incoming* edges, with each edge's weight already divided
//! by its source's total out-weight, so one iteration is a parallel pull over the nodes:
//!
//! ```text
//! r'(v) = (1 - d) / N  +  d * (D / N  +  Σ_{u → v} r(u) * w(u, v) / W(u))
//! ```
//!
//! where `D` is the rank held by dangling nodes (no out-edges), spread evenly so ranks keep
//! summing to 1. Ranks are `f32`; sums over all nodes (the dangling mass and the L1 change
//! that decides convergence) are taken in `f64` over fixed chunks and added in order, so
//! results are bit-identical whatever the number of threads.

use rayon::prelude::*;

/// Nodes per parallel chunk.
const CHUNK: usize = 4096;

/// A weighted directed graph, ready for [`pagerank`].
#[derive(Debug, Clone, Default)]
pub struct Graph {
    n: u32,
    /// `offsets[v]..offsets[v + 1]` index `sources` and `weights` for the edges into `v`.
    offsets: Vec<u64>,
    sources: Vec<u32>,
    /// Edge weight over the source's total out-weight.
    weights: Vec<f32>,
    /// Nodes without out-edges.
    dangling: Vec<u32>,
}

impl Graph {
    pub fn nodes(&self) -> u32 {
        self.n
    }

    /// Distinct edges (parallel edges were merged).
    pub fn edges(&self) -> u64 {
        self.sources.len() as u64
    }
}

/// Collects edges for a [`Graph`].
#[derive(Debug, Clone, Default)]
pub struct GraphBuilder {
    n: u32,
    edges: Vec<(u32, u32, f32)>,
}

impl GraphBuilder {
    /// A graph of `n` nodes, `0..n`.
    pub fn new(n: u32) -> Self {
        Self {
            n,
            edges: Vec::new(),
        }
    }

    pub fn reserve(&mut self, edges: usize) {
        self.edges.reserve(edges);
    }

    /// Adds an edge from `src` to `dst` with weight `w`. Self-loops and non-positive weights
    /// are ignored; parallel edges add up. Panics if a node is out of range.
    pub fn add_edge(&mut self, src: u32, dst: u32, w: f32) {
        assert!(
            src < self.n && dst < self.n,
            "edge {src} -> {dst} out of range"
        );
        if src != dst && w > 0.0 && w.is_finite() {
            self.edges.push((src, dst, w));
        }
    }

    pub fn build(mut self) -> Graph {
        let n = self.n as usize;
        // Sorting by (dst, src, weight bits) makes merged weights independent of the order
        // edges were added in.
        self.edges
            .par_sort_unstable_by_key(|&(s, d, w)| (d, s, w.to_bits()));
        let mut merged: Vec<(u32, u32, f32)> = Vec::with_capacity(self.edges.len());
        for (s, d, w) in self.edges {
            match merged.last_mut() {
                Some(last) if last.0 == s && last.1 == d => last.2 += w,
                _ => merged.push((s, d, w)),
            }
        }
        let mut out = vec![0f64; n];
        for &(s, _, w) in &merged {
            out[s as usize] += f64::from(w);
        }
        let mut offsets = vec![0u64; n + 1];
        for &(_, d, _) in &merged {
            offsets[d as usize + 1] += 1;
        }
        for v in 0..n {
            offsets[v + 1] += offsets[v];
        }
        let sources = merged.iter().map(|e| e.0).collect();
        let weights = merged
            .iter()
            .map(|&(s, _, w)| (f64::from(w) / out[s as usize]) as f32)
            .collect();
        let dangling = (0..self.n).filter(|&v| out[v as usize] == 0.0).collect();
        Graph {
            n: self.n,
            offsets,
            sources,
            weights,
            dangling,
        }
    }
}

/// PageRank settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Options {
    /// The probability of following an edge rather than jumping to a random node.
    pub damping: f32,
    /// Stop once ranks change by less than this in total (L1; ranks sum to 1).
    pub tolerance: f64,
    pub max_iterations: u32,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            damping: 0.85,
            tolerance: 1e-6,
            max_iterations: 100,
        }
    }
}

/// The result of [`pagerank`].
#[derive(Debug, Clone, PartialEq)]
pub struct Ranks {
    /// Per node; they sum to 1.
    pub scores: Vec<f32>,
    pub iterations: u32,
    pub converged: bool,
    /// The L1 change of the last iteration.
    pub delta: f64,
}

/// `f64` sum of `f(i)` over `0..n`, chunked so the result does not depend on scheduling.
fn chunked_sum(n: usize, f: impl Fn(usize) -> f64 + Sync) -> f64 {
    let chunks: Vec<f64> = (0..n.div_ceil(CHUNK))
        .into_par_iter()
        .map(|c| (c * CHUNK..((c + 1) * CHUNK).min(n)).map(&f).sum())
        .collect();
    chunks.iter().sum()
}

/// Runs PageRank on `g`.
pub fn pagerank(g: &Graph, opts: &Options) -> Ranks {
    let n = g.n as usize;
    if n == 0 {
        return Ranks {
            scores: Vec::new(),
            iterations: 0,
            converged: true,
            delta: 0.0,
        };
    }
    let d = f64::from(opts.damping);
    let mut rank = vec![(1.0 / n as f64) as f32; n];
    let mut next = vec![0f32; n];
    let mut iterations = 0;
    let mut delta = f64::INFINITY;
    while iterations < opts.max_iterations {
        iterations += 1;
        let dangling = chunked_sum(g.dangling.len(), |i| {
            f64::from(rank[g.dangling[i] as usize])
        });
        let base = ((1.0 - d) / n as f64 + d * dangling / n as f64) as f32;
        let damping = opts.damping;
        let current = &rank;
        next.par_chunks_mut(CHUNK)
            .enumerate()
            .for_each(|(c, chunk)| {
                for (j, out) in chunk.iter_mut().enumerate() {
                    let v = c * CHUNK + j;
                    let (a, b) = (g.offsets[v] as usize, g.offsets[v + 1] as usize);
                    let mut sum = 0f32;
                    for (s, w) in g.sources[a..b].iter().zip(&g.weights[a..b]) {
                        sum += current[*s as usize] * w;
                    }
                    *out = base + damping * sum;
                }
            });
        delta = chunked_sum(n, |i| f64::from((next[i] - rank[i]).abs()));
        std::mem::swap(&mut rank, &mut next);
        if delta < opts.tolerance {
            break;
        }
    }
    Ranks {
        scores: rank,
        iterations,
        converged: delta < opts.tolerance,
        delta,
    }
}

/// Maps PageRank scores to static ranks in `[0, 1]`: on a log scale relative to the lowest
/// score (a node nothing links to, which gets 0) and the highest (which gets 1), so ranks
/// spread over the range however skewed the scores are. All 0 when every score is equal.
pub fn normalize(scores: &[f32]) -> Vec<f32> {
    let n = scores.len() as f64;
    let min = scores.iter().copied().fold(f32::INFINITY, f32::min);
    let max = scores.iter().copied().fold(0f32, f32::max);
    let span = (f64::from(max - min) * n).ln_1p();
    if scores.is_empty() || span <= 0.0 || !span.is_finite() {
        return vec![0.0; scores.len()];
    }
    scores
        .par_iter()
        .map(|&s| ((f64::from(s - min) * n).ln_1p() / span).clamp(0.0, 1.0) as f32)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(n: u32, edges: &[(u32, u32, f32)]) -> Graph {
        let mut b = GraphBuilder::new(n);
        for &(s, d, w) in edges {
            b.add_edge(s, d, w);
        }
        b.build()
    }

    /// Dense f64 PageRank with the same dangling rule, iterated to convergence.
    fn reference(n: usize, edges: &[(u32, u32, f32)], d: f64) -> Vec<f64> {
        let mut out = vec![0f64; n];
        let mut w = vec![vec![0f64; n]; n];
        for &(s, t, x) in edges {
            if s != t {
                w[s as usize][t as usize] += f64::from(x);
                out[s as usize] += f64::from(x);
            }
        }
        let mut r = vec![1.0 / n as f64; n];
        for _ in 0..1000 {
            let dangling: f64 = (0..n).filter(|&u| out[u] == 0.0).map(|u| r[u]).sum();
            let mut next = vec![(1.0 - d) / n as f64 + d * dangling / n as f64; n];
            for u in 0..n {
                for v in 0..n {
                    if w[u][v] > 0.0 {
                        next[v] += d * r[u] * w[u][v] / out[u];
                    }
                }
            }
            r = next;
        }
        r
    }

    fn assert_close(got: &[f32], want: &[f64], eps: f64) {
        assert_eq!(got.len(), want.len());
        for (i, (g, w)) in got.iter().zip(want).enumerate() {
            assert!(
                (f64::from(*g) - w).abs() < eps,
                "node {i}: {g} vs {w} ({got:?} vs {want:?})"
            );
        }
    }

    #[test]
    fn known_small_graphs() {
        // A cycle: every node equal.
        let cycle = graph(4, &[(0, 1, 1.0), (1, 2, 1.0), (2, 3, 1.0), (3, 0, 1.0)]);
        let r = pagerank(&cycle, &Options::default());
        assert!(r.converged);
        assert_close(&r.scores, &[0.25; 4], 1e-6);

        // A star pointing at its center.
        let star = graph(4, &[(1, 0, 1.0), (2, 0, 1.0), (3, 0, 1.0)]);
        let r = pagerank(&star, &Options::default());
        assert!(r.scores[0] > 3.0 * r.scores[1]);
        assert_eq!(r.scores[1], r.scores[2]);

        // The classic four-page example, against a dense reference.
        let edges = [
            (0, 1, 1.0),
            (0, 2, 1.0),
            (1, 2, 1.0),
            (2, 0, 1.0),
            (3, 2, 1.0),
        ];
        let r = pagerank(&graph(4, &edges), &Options::default());
        assert_close(&r.scores, &reference(4, &edges, 0.85), 1e-5);
        // Weights matter: 0 → 2 weighs nine times 0 → 1.
        let weighted = [(0, 1, 1.0), (0, 2, 9.0), (1, 0, 1.0), (2, 0, 1.0)];
        let r = pagerank(&graph(3, &weighted), &Options::default());
        assert!(r.scores[2] > 2.0 * r.scores[1]);
        assert_close(&r.scores, &reference(3, &weighted, 0.85), 1e-5);
    }

    #[test]
    fn dangling_nodes_keep_ranks_summing_to_one() {
        // 2 and 3 have no out-edges; 3 has no edges at all.
        let edges = [(0, 1, 1.0), (1, 2, 1.0), (0, 2, 2.0)];
        let r = pagerank(&graph(4, &edges), &Options::default());
        let sum: f64 = r.scores.iter().map(|&s| f64::from(s)).sum();
        assert!((sum - 1.0).abs() < 1e-5, "{sum}");
        assert_close(&r.scores, &reference(4, &edges, 0.85), 1e-5);
        assert!(r.scores[2] > r.scores[1] && r.scores[1] > r.scores[3]);
        // No edges at all: uniform.
        let r = pagerank(&graph(3, &[]), &Options::default());
        assert_close(&r.scores, &[1.0 / 3.0; 3], 1e-6);
        assert!(
            pagerank(&graph(0, &[]), &Options::default())
                .scores
                .is_empty()
        );
    }

    #[test]
    fn parallel_edges_merge_and_self_loops_drop() {
        let a = graph(2, &[(0, 1, 1.0), (0, 1, 2.0), (1, 1, 5.0), (1, 0, 0.0)]);
        assert_eq!(a.edges(), 1);
        let b = graph(2, &[(0, 1, 3.0)]);
        assert_eq!(
            pagerank(&a, &Options::default()),
            pagerank(&b, &Options::default())
        );
    }

    #[test]
    fn convergence() {
        let edges: Vec<(u32, u32, f32)> = (0..50u32)
            .flat_map(|i| [(i, i * i % 50, 1.0), (i, i / 3, 2.0)])
            .collect();
        let g = graph(50, &edges);
        let r = pagerank(&g, &Options::default());
        assert!(
            r.converged && r.iterations > 1 && r.iterations < 100,
            "{r:?}"
        );
        assert!(r.delta < 1e-6);
        assert_close(&r.scores, &reference(50, &edges, 0.85), 1e-5);
        let capped = pagerank(
            &g,
            &Options {
                max_iterations: 2,
                ..Options::default()
            },
        );
        assert!(!capped.converged);
        assert_eq!(capped.iterations, 2);
    }

    #[test]
    fn deterministic_across_threads_and_edge_order() {
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let mut rnd = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let n = 20_000u32;
        let edges: Vec<(u32, u32, f32)> = (0..100_000)
            .map(|_| {
                let s = (rnd() % u64::from(n)) as u32;
                let d = (rnd() % u64::from(n / 10)) as u32;
                (s, d, (rnd() % 5 + 1) as f32 * 0.3)
            })
            .collect();
        let run = |threads: usize, edges: &[(u32, u32, f32)]| {
            rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap()
                .install(|| pagerank(&graph(n, edges), &Options::default()))
        };
        let one = run(1, &edges);
        let mut reversed = edges.clone();
        reversed.reverse();
        assert_eq!(one, run(4, &edges));
        assert_eq!(one, run(3, &reversed));
        assert_eq!(one, run(1, &edges));
    }

    #[test]
    fn normalized_ranks() {
        assert!(normalize(&[]).is_empty());
        assert_eq!(normalize(&[0.5, 0.5]), [0.0, 0.0]);
        let r = normalize(&[0.1, 0.2, 0.7]);
        assert_eq!(r[0], 0.0);
        assert_eq!(r[2], 1.0);
        assert!(r[1] > 0.0 && r[1] < 1.0);
    }

    /// `cargo test --release -p sourcegull-ranking -- --ignored --nocapture million`
    #[test]
    #[ignore = "performance check; run in release"]
    fn million_nodes() {
        let n = 1_000_000u32;
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let mut rnd = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let start = std::time::Instant::now();
        let mut b = GraphBuilder::new(n);
        b.reserve(10_000_000);
        for s in 0..n {
            for _ in 0..10 {
                // Skewed targets, as references to popular files are.
                let r = rnd();
                let d = ((r % u64::from(n)) * ((r >> 40) % 1024) / 1024) as u32;
                b.add_edge(s, d, (r % 4 + 1) as f32);
            }
        }
        let g = b.build();
        let built = start.elapsed();
        let start = std::time::Instant::now();
        let r = pagerank(&g, &Options::default());
        let ranked = start.elapsed();
        let normalized = normalize(&r.scores);
        println!(
            "pagerank: {} nodes, {} edges: build {:.2}s, {} iterations {:.2}s ({:.0} ms/iteration), \
             converged {}, {} threads",
            g.nodes(),
            g.edges(),
            built.as_secs_f64(),
            r.iterations,
            ranked.as_secs_f64(),
            ranked.as_secs_f64() * 1000.0 / f64::from(r.iterations),
            r.converged,
            rayon::current_num_threads(),
        );
        assert!(r.converged);
        assert_eq!(normalized.len(), n as usize);
    }
}
