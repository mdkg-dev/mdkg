//! Timings on a real graph (task-41's speed criterion): a cold build under
//! 2 s, queries under 100 ms.
//!
//! Point `MDKG_BENCH_GRAPH` at a read-only copy of a graph (never a live
//! repository) and run:
//!
//! ```text
//! MDKG_BENCH_GRAPH=/path/to/copy cargo test --release -p mdkg-index \
//!     --test bench -- --ignored --nocapture
//! ```
//!
//! The graph is only read; the index goes to a temporary folder.

#[path = "common/temp.rs"]
mod temp;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use mdkg_graph::{config, index};
use mdkg_index::graph as walk;
use mdkg_index::{Db, pack, resolve, search};

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// The median of `runs` timings of `f`.
fn median<T>(runs: usize, mut f: impl FnMut() -> T) -> Duration {
    let mut times: Vec<Duration> = (0..runs)
        .map(|_| {
            let t = Instant::now();
            std::hint::black_box(f());
            t.elapsed()
        })
        .collect();
    times.sort();
    times[runs / 2]
}

#[test]
#[ignore = "needs MDKG_BENCH_GRAPH; run with --release"]
fn real_graph_timings() {
    let Some(root) = std::env::var_os("MDKG_BENCH_GRAPH").map(PathBuf::from) else {
        eprintln!("MDKG_BENCH_GRAPH is unset: nothing to measure");
        return;
    };
    let tmp = temp::dir("mdkg-index-bench");
    let path = tmp.join("bench.v1.sqlite");

    let config = config::load(&root).expect("config");
    let t = Instant::now();
    let graph = index::build(&root, &config).expect("graph builds");
    let read = t.elapsed();

    let t = Instant::now();
    let (mut db, stats) = Db::open_synced(&path, &root, &graph).expect("cold build");
    let cold = t.elapsed();
    assert_eq!(stats.added, graph.nodes.len());

    let warm = median(5, || db.sync(&root, &graph).expect("warm sync"));
    let reopen = median(5, || {
        Db::open_synced(&path, &root, &graph).expect("reopen").1
    });

    let queries = [
        "lease",
        "room membership",
        "migration rollback",
        "stream redis latency",
    ];
    let mut worst_query = Duration::ZERO;
    for q in queries {
        let d = median(5, || search::search(&db, q).expect("search"));
        let hits = search::search(&db, q).expect("search");
        let snip = median(5, || {
            hits.iter()
                .take(50)
                .filter_map(|h| search::snippet(&db, q, &h.qid).expect("snippet"))
                .count()
        });
        eprintln!(
            "search {q:?}: {} hits, {:.1} ms, 50 snippets {:.1} ms",
            hits.len(),
            ms(d),
            ms(snip)
        );
        worst_query = worst_query.max(d + snip);
    }

    let first = graph.nodes[graph.nodes.len() / 2].clone();
    let resolve_t = median(5, || resolve::resolve_handle(&graph, &first.title, None));
    let refs_t = median(5, || walk::backrefs(&graph, &first.qid));
    let statuses: Vec<String> = config.next_status_preference.clone();
    let ready_t = median(5, || {
        walk::ready_set(
            &graph,
            &["task", "bug", "feat", "test", "spike"],
            &statuses,
            9,
        )
        .len()
    });
    let goal = graph.nodes.iter().find(|n| n.node_type == "goal").cloned();
    let path_t = goal
        .as_ref()
        .map(|g| median(5, || walk::critical_path(&graph, g)));
    let pack_t = median(5, || {
        let ranked = pack::rank(&graph, &[(first.qid.clone(), 1.0)]);
        pack::build(
            &graph,
            &ranked,
            pack::Options {
                profile: pack::Profile::Concise,
                budget: 8000,
                max_nodes: 25,
            },
        )
        .tokens
    });

    eprintln!("nodes: {}", graph.nodes.len());
    eprintln!("read graph (mdkg_graph::index::build): {:.1} ms", ms(read));
    eprintln!("cold index build: {:.1} ms", ms(cold));
    eprintln!("warm sync (nothing changed): {:.1} ms", ms(warm));
    eprintln!("reopen + sync: {:.1} ms", ms(reopen));
    eprintln!("worst search + 50 snippets: {:.1} ms", ms(worst_query));
    eprintln!("resolve a title: {:.1} ms", ms(resolve_t));
    eprintln!("backrefs: {:.1} ms", ms(refs_t));
    eprintln!("ready set: {:.1} ms", ms(ready_t));
    if let Some(p) = path_t {
        eprintln!("critical path: {:.1} ms", ms(p));
    }
    eprintln!("pack (rank + build): {:.1} ms", ms(pack_t));
    std::fs::remove_dir_all(&tmp).ok();

    assert!(cold < Duration::from_secs(2), "cold build {cold:?}");
    assert!(
        worst_query < Duration::from_millis(100),
        "query {worst_query:?}"
    );
}
