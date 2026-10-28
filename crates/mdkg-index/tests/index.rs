//! The derived index on small graphs: syncing by file hash, rebuilding a
//! damaged file, search ranking, handles, graph walks, packs and
//! providers.

#[path = "common/temp.rs"]
mod temp;

use std::fs;
use std::path::{Path, PathBuf};

use mdkg_graph::resolve::Resolution;
use mdkg_graph::{Index, IndexNode, config, index};
use mdkg_index::graph as walk;
use mdkg_index::provider::{self, DerivedProvider, Document, ProviderError, Stored};
use mdkg_index::{Db, pack, resolve, search};

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

fn scratch(name: &str) -> Scratch {
    let dir = temp::dir(&format!("mdkg-index-{name}"));
    Scratch(fs::canonicalize(&dir).expect("canonical"))
}

fn copy_tree(src: &Path, dest: &Path) {
    fs::create_dir_all(dest).expect("mkdir");
    for entry in fs::read_dir(src).expect("read dir") {
        let entry = entry.expect("entry");
        let target = dest.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy");
        }
    }
}

/// A graph from the conformance base config, with these node files.
fn graph_with(name: &str, nodes: &[(&str, &str)]) -> Scratch {
    let s = scratch(name);
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance/base");
    copy_tree(&base, &s.0);
    for (file, text) in nodes {
        let p = s.0.join(".mdkg/work").join(file);
        fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        fs::write(p, text).expect("write node");
    }
    s
}

fn node(id: &str, title: &str, extra: &[&str], body: &str) -> String {
    let mut lines = vec![
        "---".to_string(),
        format!("id: {id}"),
        "type: task".into(),
        format!("title: {title}"),
        "status: todo".into(),
    ];
    lines.extend(extra.iter().map(|s| s.to_string()));
    lines.extend([
        "created: 2026-10-24".into(),
        "updated: 2026-10-24".into(),
        "---".into(),
    ]);
    format!("{}\n{body}", lines.join("\n"))
}

fn read(root: &Path) -> Index {
    let config = config::load(root).expect("config");
    index::build(root, &config).expect("graph builds")
}

fn three() -> Vec<(&'static str, String)> {
    vec![
        (
            "task-1-a.md",
            node(
                "task-1",
                "Parser for frontmatter",
                &["tags: [format]"],
                "# Overview\n\nReads keys.\n",
            ),
        ),
        (
            "task-2-b.md",
            node(
                "task-2",
                "Coordinator leases",
                &["owners: [nick]"],
                "# Overview\n\nThe word frontmatter appears only in this body.\n",
            ),
        ),
        (
            "task-3-c.md",
            node(
                "task-3",
                "Merkle roots",
                &["blocked_by: [task-1]"],
                "# Overview\n\nHashes.\n",
            ),
        ),
    ]
}

fn write_graph(name: &str, files: &[(&str, String)]) -> Scratch {
    let refs: Vec<(&str, &str)> = files.iter().map(|(f, t)| (*f, t.as_str())).collect();
    graph_with(name, &refs)
}

#[test]
fn syncs_by_file_hash() {
    let g = write_graph("sync", &three());
    let db_path = g.0.join("index.v1.sqlite");
    let graph = read(&g.0);
    let (mut db, stats) = Db::open_synced(&db_path, &g.0, &graph).expect("cold");
    assert_eq!((stats.nodes, stats.added, stats.rebuilt), (3, 3, false));

    let stats = db.sync(&g.0, &graph).expect("warm");
    assert_eq!((stats.unchanged, stats.added, stats.updated), (3, 0, 0));
    let hash = db.file_hash("root:task-2").expect("hash").expect("some");
    assert!(hash.starts_with("sha256:"), "{hash}");

    // An edit replaces one row; a removed file drops one; a new one adds.
    let f2 = g.0.join(".mdkg/work/task-2-b.md");
    fs::write(
        &f2,
        node("task-2", "Coordinator leases", &[], "Rewritten body.\n"),
    )
    .expect("edit");
    fs::remove_file(g.0.join(".mdkg/work/task-3-c.md")).expect("rm");
    fs::write(
        g.0.join(".mdkg/work/task-4-d.md"),
        node("task-4", "New work", &[], "Fresh.\n"),
    )
    .expect("add");
    let graph = read(&g.0);
    let stats = db.sync(&g.0, &graph).expect("sync");
    assert_eq!(
        (
            stats.nodes,
            stats.added,
            stats.updated,
            stats.removed,
            stats.unchanged
        ),
        (3, 1, 1, 1, 1)
    );
    assert_ne!(
        db.file_hash("root:task-2").expect("hash").expect("some"),
        hash
    );
    assert!(
        search::search(&db, "frontmatter")
            .expect("search")
            .iter()
            .all(|h| h.qid != "root:task-2")
    );
    assert_eq!(
        search::search(&db, "rewritten").expect("search")[0].qid,
        "root:task-2"
    );

    // A file moved to another name keeps its node, under its new path.
    fs::rename(&f2, g.0.join(".mdkg/work/task-2-renamed.md")).expect("mv");
    let graph = read(&g.0);
    let stats = db.sync(&g.0, &graph).expect("sync");
    assert_eq!((stats.nodes, stats.added, stats.removed), (3, 1, 1));

    // Reopened, nothing changed.
    drop(db);
    let (_, stats) = Db::open_synced(&db_path, &g.0, &graph).expect("reopen");
    assert_eq!(stats.unchanged, 3);
}

#[test]
fn a_touched_file_is_hashed_once_and_kept() {
    let g = write_graph("touch", &three());
    let db_path = g.0.join("index.v1.sqlite");
    let graph = read(&g.0);
    let (mut db, _) = Db::open_synced(&db_path, &g.0, &graph).expect("cold");
    let f1 = g.0.join(".mdkg/work/task-1-a.md");
    let text = fs::read(&f1).expect("read");
    std::thread::sleep(std::time::Duration::from_millis(5));
    fs::write(&f1, &text).expect("touch");
    let stats = db.sync(&g.0, &graph).expect("sync");
    assert_eq!(stats.unchanged, 3, "same content, same row");
}

#[test]
fn a_damaged_or_foreign_file_is_rebuilt() {
    let g = write_graph("damaged", &three());
    let graph = read(&g.0);
    let db_path = g.0.join("index.v1.sqlite");
    fs::write(
        &db_path,
        b"this is not a database, just text that fills a page....",
    )
    .expect("junk");
    let (db, stats) = Db::open_synced(&db_path, &g.0, &graph).expect("rebuilt");
    assert!(stats.rebuilt);
    assert_eq!(stats.added, 3);
    drop(db);

    // Another schema's tables are dropped, not trusted.
    let conn = rusqlite::Connection::open(&db_path).expect("open");
    conn.pragma_update(None, "user_version", 99)
        .expect("version");
    drop(conn);
    let (_, stats) = Db::open_synced(&db_path, &g.0, &graph).expect("rebuilt");
    assert!(stats.rebuilt);
    assert_eq!(stats.added, 3);

    // A file from a build whose schema changed without a version bump (a
    // column missing, no fingerprint, or another one) is replaced too, not
    // used until a query fails.
    let conn = rusqlite::Connection::open(&db_path).expect("open");
    conn.execute_batch("DROP TABLE nodes; CREATE TABLE nodes (qid TEXT); CREATE TABLE stray (x);")
        .expect("old shape");
    conn.execute(
        "UPDATE meta SET value = 'sha256:other' WHERE key = 'schema'",
        [],
    )
    .expect("other fingerprint");
    drop(conn);
    let (db, stats) = Db::open_synced(&db_path, &g.0, &graph).expect("rebuilt");
    assert!(stats.rebuilt);
    assert_eq!(stats.added, 3);
    assert_eq!(db.info().expect("info").nodes, 3);
}

#[test]
fn search_ranks_titles_over_bodies_and_quotes_input() {
    let g = write_graph("search", &three());
    let graph = read(&g.0);
    let mut db = Db::in_memory().expect("memory");
    db.sync(&g.0, &graph).expect("sync");
    let hits = search::search(&db, "FRONTMATTER").expect("search");
    let qids: Vec<&str> = hits.iter().map(|h| h.qid.as_str()).collect();
    assert_eq!(
        qids,
        ["root:task-1", "root:task-2"],
        "title first, body second"
    );
    assert!(hits[0].score > hits[1].score);

    // Prefixes, ids, fields and AND.
    assert_eq!(
        search::search(&db, "pars").expect("search")[0].qid,
        "root:task-1"
    );
    assert_eq!(
        search::search(&db, "task-3").expect("search")[0].qid,
        "root:task-3"
    );
    assert_eq!(
        search::search(&db, "nick").expect("search")[0].qid,
        "root:task-2"
    );
    assert!(
        search::search(&db, "parser leases")
            .expect("search")
            .is_empty()
    );

    // Query syntax is never read from the input.
    for q in [
        "\"",
        "NEAR(a b)",
        "a OR b",
        "title:merkle",
        "*",
        "-x",
        "^a",
        "a AND",
    ] {
        search::search(&db, q).expect("no syntax error");
    }
    assert!(
        search::search(&db, "title:merkle")
            .expect("search")
            .is_empty()
    );

    let snippet = search::snippet(&db, "frontmatter", "root:task-2")
        .expect("snippet")
        .expect("body match");
    assert!(snippet.contains("**frontmatter**"), "{snippet}");
    assert_eq!(
        search::snippet(&db, "parser", "root:task-1").expect("snippet"),
        None
    );
}

/// A node made in memory, for ids the parser doesn't take yet.
fn mem(id: &str, node_type: &str, title: &str) -> IndexNode {
    IndexNode {
        id: id.into(),
        legacy_id: None,
        identity: None,
        qid: format!("root:{id}"),
        ws: "root".into(),
        node_type: node_type.into(),
        title: title.into(),
        status: Some("todo".into()),
        priority: None,
        created: "2026-10-24".into(),
        updated: "2026-10-24".into(),
        tags: vec![],
        owners: vec![],
        links: vec![],
        artifacts: vec![],
        refs: vec![],
        aliases: vec![],
        skills: vec![],
        scope: vec![],
        attributes: Default::default(),
        path: format!(".mdkg/work/{id}.md"),
        edges: Default::default(),
        body: String::new(),
    }
}

fn e_code(graph: &Index, handle: &str) -> u8 {
    let r = resolve::resolve_handle(graph, handle, None);
    resolve::handle_error(graph, "id", handle, &r, None).exit_code()
}

#[test]
fn handles_resolve_by_id_prefix_legacy_id_and_title() {
    let a = "task-0192f3a4-7b2c-7def-8a9b-1c2d3e4f5a6b";
    let b = "task-0192f3a4-9999-7def-8a9b-1c2d3e4f5a6b";
    let c = "goal-01930000-0000-7000-8000-000000000000";
    let d = "task-01940000-0000-7000-8000-000000000000";
    let mut legacy = mem(c, "goal", "Public release");
    legacy.legacy_id = Some("goal-4".into());
    let graph = Index::assemble(
        vec![
            mem(a, "task", "Derived index and search"),
            mem(b, "task", "Derived plane docs"),
            legacy,
            mem(d, "task", "Search"),
            mem("task-7", "task", "Legacy parser"),
        ],
        vec!["root".into()],
    );
    let ok = |q: &str| Resolution::Ok(format!("root:{q}"));
    let r = |h: &str| resolve::resolve_handle(&graph, h, None);
    assert_eq!(r(a), ok(a));
    assert_eq!(r(&format!("root:{a}")), ok(a));
    assert_eq!(r("task-0192f3a4-7b2c"), ok(a));
    assert_eq!(r("0192f3a47b2c"), ok(a));
    assert_eq!(r("0193"), ok(c));
    assert_eq!(r("goal-4"), ok(c), "a migrated node's legacy id");
    assert_eq!(r("index and"), ok(a), "title words, any order or case");
    assert_eq!(r("SEARCH"), ok(d), "a whole title wins over fragments");
    assert_eq!(r("task-7"), ok("task-7"));
    assert_eq!(r("legacy pars"), ok("task-7"), "a 0.6.0 node's title");
    // Titles are matched once across both kinds of id: an exact 0.6.0
    // title wins over a 0.7 fragment, and overlaps stay ambiguous.
    let mixed = Index::assemble(
        vec![
            mem(a, "task", "Search docs"),
            mem("task-7", "task", "Search"),
        ],
        vec!["root".into()],
    );
    assert_eq!(
        resolve::resolve_handle(&mixed, "search", None),
        ok("task-7")
    );
    match resolve::resolve_handle(&mixed, "sea", None) {
        Resolution::Ambiguous(m) => assert_eq!(m.len(), 2),
        other => panic!("{other:?}"),
    }
    assert_eq!(e_code(&graph, "derived"), 7);
    match r("0192f3a4") {
        Resolution::Ambiguous(m) => assert_eq!(m, [format!("root:{a}"), format!("root:{b}")]),
        other => panic!("{other:?}"),
    }
    match r("derived") {
        Resolution::Ambiguous(m) => assert_eq!(m.len(), 2),
        other => panic!("{other:?}"),
    }
    assert_eq!(r("nothing like this"), Resolution::Missing(vec![]));
    // An absent full id is final, even when a title holds it.
    let absent = "task-01990000-0000-7000-8000-000000000000";
    let quoting = Index::assemble(
        vec![mem(a, "task", &format!("Replaces {absent}"))],
        vec!["root".into()],
    );
    assert_eq!(
        resolve::resolve_handle(&quoting, absent, None),
        Resolution::Missing(vec![])
    );
    assert_eq!(r("   "), Resolution::Missing(vec![]));

    let e = resolve::handle_error(&graph, "id", "derived", &r("derived"), None);
    assert_eq!(e.code(), mdkg_format::Code::NodeAmbiguous);
    let hint = e.diagnostic.hint.clone().unwrap_or_default();
    assert!(hint.contains("Derived plane docs"), "{hint}");
}

#[test]
fn ready_set_and_critical_path_follow_blockers() {
    let goal = {
        let mut g = mem("goal-1", "goal", "Ship");
        g.status = None;
        g
    };
    let mut t1 = mem("task-1", "task", "First");
    t1.edges.parent = Some("root:goal-1".into());
    let mut t2 = mem("task-2", "task", "Second");
    t2.edges.parent = Some("root:goal-1".into());
    t2.edges.blocked_by = vec!["root:task-1".into()];
    let mut t3 = mem("task-3", "task", "Third");
    t3.edges.parent = Some("root:goal-1".into());
    t3.edges.blocked_by = vec!["root:task-2".into()];
    let mut t4 = mem("task-4", "task", "Side");
    t4.edges.parent = Some("root:goal-1".into());
    // task-5 blocks task-4 through its `blocks` field, and is done.
    let mut t5 = mem("task-5", "task", "Done already");
    t5.status = Some("done".into());
    t5.edges.blocks = vec!["root:task-4".into()];
    let graph = Index::assemble(vec![goal, t1, t2, t3, t4, t5], vec!["root".into()]);
    let statuses = vec!["todo".to_string()];
    let ready: Vec<&str> = walk::ready_set(&graph, &["task"], &statuses, 9)
        .iter()
        .map(|n| n.qid.as_str())
        .collect();
    assert_eq!(ready, ["root:task-1", "root:task-4"]);

    let g = graph.get("root:goal-1").expect("goal");
    let cp = walk::critical_path(&graph, g);
    assert_eq!(cp.path, ["root:task-1", "root:task-2", "root:task-3"]);
    assert_eq!(cp.remaining, 4);
    assert!(cp.cycle.is_empty());

    let back: Vec<(String, String)> = walk::backrefs(&graph, "root:task-1")
        .into_iter()
        .map(|e| (e.kind, e.qid))
        .collect();
    assert_eq!(
        back,
        [("blocked_by".to_string(), "root:task-2".to_string())]
    );
    let refs = walk::refs(&graph, "root:task-3");
    assert_eq!(refs.len(), 2);
    assert!(refs.iter().all(|e| e.exists));
}

#[test]
fn body_links_are_refs_and_backrefs() {
    let a = "task-0192f3a4-7b2c-7def-8a9b-1c2d3e4f5a6b";
    let b = "goal-01930000-0000-7000-8000-000000000000";
    let gone = "task-01950000-0000-7000-8000-000000000000";
    let mut from = mem(a, "task", "Linker");
    // A stale type still finds the node by its UUID.
    from.body = format!(
        "Serves [the goal]({}) and [a gone one]({gone}).",
        b.replacen("goal", "epic", 1)
    );
    // Links resolve in a format 0.7 graph, which has a graph_id.
    let graph = Index::assemble_in(
        vec![from, mem(b, "goal", "Target")],
        vec!["root".into()],
        mdkg_format::id::Uuid7::parse("0190aaaa-0000-7000-8000-000000000000").ok(),
    );
    let refs: Vec<(String, String, bool)> = walk::refs(&graph, &format!("root:{a}"))
        .into_iter()
        .map(|e| (e.kind, e.qid, e.exists))
        .collect();
    assert_eq!(
        refs,
        [
            ("link".to_string(), format!("root:{b}"), true),
            ("link".to_string(), gone.to_string(), false),
        ]
    );
    let back: Vec<(String, String)> = walk::backrefs(&graph, &format!("root:{b}"))
        .into_iter()
        .map(|e| (e.kind, e.qid))
        .collect();
    assert_eq!(back, [("link".to_string(), format!("root:{a}"))]);
    let ranked = pack::rank(&graph, &[(format!("root:{a}"), 1.0)]);
    assert_eq!(ranked.len(), 2, "the link carries the walk");
}

#[test]
fn a_blocking_cycle_is_reported_not_looped() {
    let goal = mem("goal-1", "goal", "Ship");
    // task-1 waits on task-3, task-3 on task-2, task-2 on task-1; task-4
    // waits on the cycle.
    let mut t1 = mem("task-1", "task", "A");
    t1.edges.parent = Some("root:goal-1".into());
    t1.edges.blocked_by = vec!["root:task-3".into()];
    let mut t2 = mem("task-2", "task", "B");
    t2.edges.parent = Some("root:goal-1".into());
    t2.edges.blocked_by = vec!["root:task-1".into()];
    let mut t3 = mem("task-3", "task", "C");
    t3.edges.parent = Some("root:goal-1".into());
    t3.edges.blocked_by = vec!["root:task-2".into()];
    let mut t4 = mem("task-4", "task", "D");
    t4.edges.parent = Some("root:goal-1".into());
    t4.edges.blocked_by = vec!["root:task-2".into()];
    let graph = Index::assemble(vec![goal, t1, t2, t3, t4], vec!["root".into()]);
    let cp = walk::critical_path(&graph, graph.get("root:goal-1").expect("goal"));
    assert_eq!(
        cp.cycle,
        ["root:task-1", "root:task-2", "root:task-3"],
        "every member"
    );
    assert!(walk::ready_set(&graph, &["task"], &["todo".to_string()], 9).is_empty());

    // Overlapping cycles: task-1 waits on task-2 and task-3, task-2 on
    // task-1, task-3 on task-2. All three are members, whichever is walked
    // first.
    let goal = mem("goal-1", "goal", "Ship");
    let task = |id: &str, blocked: &[&str]| {
        let mut t = mem(id, "task", id);
        t.edges.parent = Some("root:goal-1".into());
        t.edges.blocked_by = blocked.iter().map(|b| format!("root:{b}")).collect();
        t
    };
    let graph = Index::assemble(
        vec![
            goal,
            task("task-1", &["task-2", "task-3"]),
            task("task-2", &["task-1"]),
            task("task-3", &["task-2"]),
        ],
        vec!["root".into()],
    );
    let cp = walk::critical_path(&graph, graph.get("root:goal-1").expect("goal"));
    assert_eq!(cp.cycle, ["root:task-1", "root:task-2", "root:task-3"]);
}

#[test]
fn packs_rank_neighbours_and_keep_the_budget() {
    let mut seed = mem("task-1", "task", "Seed");
    seed.body = "Seed body. ".repeat(40);
    seed.edges.parent = Some("root:epic-1".into());
    let mut epic = mem("epic-1", "epic", "Epic");
    epic.body = "Epic body.".into();
    let mut sib = mem("task-2", "task", "Sibling");
    sib.edges.parent = Some("root:epic-1".into());
    sib.body = "# Overview\n\nSibling opening.\n\n# Details\n\n".to_string() + &"long ".repeat(400);
    let far = mem("task-9", "task", "Unrelated");
    let graph = Index::assemble(vec![seed, epic, sib, far], vec!["root".into()]);
    let ranked = pack::rank(&graph, &[("root:task-1".into(), 1.0)]);
    assert_eq!(ranked[0].0, "root:task-1");
    assert!(
        ranked.iter().all(|(q, _)| q != "root:task-9"),
        "unreachable"
    );
    let at = |q: &str| ranked.iter().position(|(r, _)| r == q).expect("ranked");
    assert!(
        at("root:epic-1") < at("root:task-2"),
        "the parent before a sibling"
    );

    let opts = |profile, budget| pack::Options {
        profile,
        budget,
        max_nodes: 25,
    };
    let full = pack::build(&graph, &ranked, opts(pack::Profile::Standard, 100_000));
    assert_eq!(full.nodes.len(), 3);
    let concise = pack::build(&graph, &ranked, opts(pack::Profile::Concise, 100_000));
    assert!(concise.tokens < full.tokens);
    assert!(concise.markdown.contains("Sibling opening."));
    assert!(!concise.markdown.contains("long long"));
    assert!(
        concise
            .markdown
            .contains(&"Seed body. ".repeat(40).trim().to_string()),
        "the seed stays whole"
    );

    let tight = pack::build(&graph, &ranked, opts(pack::Profile::Standard, 300));
    assert!(tight.tokens <= 300, "{} tokens", tight.tokens);
    assert!(tight.omitted >= 1);
    let tiny = pack::build(&graph, &ranked, opts(pack::Profile::Standard, 120));
    assert_eq!(tiny.nodes[0].qid, "root:task-1");
    assert!(tiny.nodes[0].cut);
    assert!(tiny.tokens <= 120, "{} tokens", tiny.tokens);

    // Every budget holds, down to the smallest, even when the seed's
    // fields alone don't fit.
    let mut wide = mem("task-5", "task", &"A very long title ".repeat(30));
    wide.tags = (0..40).map(|i| format!("tag-{i}")).collect();
    let graph = Index::assemble(vec![wide], vec!["root".into()]);
    let ranked = pack::rank(&graph, &[("root:task-5".into(), 1.0)]);
    for budget in [0, 1, pack::MIN_BUDGET, 100, 200, 400] {
        for profile in [pack::Profile::Standard, pack::Profile::Concise] {
            let p = pack::build(&graph, &ranked, opts(profile, budget));
            assert!(
                p.tokens <= budget.max(pack::MIN_BUDGET),
                "{budget}: {} tokens",
                p.tokens
            );
            assert_eq!(p.nodes.len(), 1, "the seed always goes in");
        }
    }

    // A qid too long for the header at the smallest budget: nothing goes
    // in, and the budget still holds.
    let mut long = mem("task-6", "task", "Long workspace");
    long.ws = "w".repeat(200);
    long.qid = format!("{}:task-6", long.ws);
    let graph = Index::assemble(vec![long.clone()], vec![long.ws.clone()]);
    let ranked = pack::rank(&graph, &[(long.qid.clone(), 1.0)]);
    let p = pack::build(
        &graph,
        &ranked,
        opts(pack::Profile::Standard, pack::MIN_BUDGET),
    );
    assert!(p.tokens <= pack::MIN_BUDGET, "{} tokens", p.tokens);
    assert!(p.nodes.is_empty());
    assert_eq!(p.omitted, 1);
}

/// Counts what it's asked about; stores the title's length.
#[derive(Debug, Default)]
struct Lengths {
    seen: Vec<String>,
}

impl DerivedProvider for Lengths {
    fn name(&self) -> &str {
        "test.lengths"
    }
    fn version(&self) -> u32 {
        1
    }
    fn derive(&mut self, docs: &[Document<'_>]) -> Result<Vec<(String, Vec<u8>)>, ProviderError> {
        self.seen.extend(docs.iter().map(|d| d.qid.to_string()));
        Ok(docs
            .iter()
            .map(|d| (d.qid.to_string(), vec![d.title.len() as u8]))
            .collect())
    }
    fn search(
        &mut self,
        text: &str,
        stored: &[Stored<'_>],
        _limit: usize,
    ) -> Result<Vec<(String, f64)>, ProviderError> {
        Ok(stored
            .iter()
            .filter(|s| usize::from(s.output[0]) == text.len())
            .map(|s| (s.qid.to_string(), 1.0))
            .collect())
    }
}

#[test]
fn providers_see_only_changed_nodes() {
    let g = write_graph("provider", &three());
    let graph = read(&g.0);
    let mut db = Db::in_memory().expect("memory");
    db.sync(&g.0, &graph).expect("sync");
    let mut p = Lengths::default();
    let stats = provider::refresh(&mut db, &g.0, &graph, &mut p).expect("refresh");
    assert_eq!(stats.derived, 3);
    let stats = provider::refresh(&mut db, &g.0, &graph, &mut p).expect("refresh");
    assert_eq!(stats.derived, 0, "nothing changed");

    fs::write(
        g.0.join(".mdkg/work/task-3-c.md"),
        node("task-3", "Merkle roots v2", &[], "Changed.\n"),
    )
    .expect("edit");
    fs::remove_file(g.0.join(".mdkg/work/task-2-b.md")).expect("rm");
    let graph = read(&g.0);
    db.sync(&g.0, &graph).expect("sync");
    p.seen.clear();
    let stats = provider::refresh(&mut db, &g.0, &graph, &mut p).expect("refresh");
    assert_eq!((stats.derived, stats.dropped), (1, 1));
    assert_eq!(p.seen, ["root:task-3"]);
    let found = provider::search(&db, &mut p, "Merkle roots v2", 10).expect("search");
    assert_eq!(found, [("root:task-3".to_string(), 1.0)]);
}

/// Edits a node and syncs it from another connection while it derives,
/// once.
struct Racing {
    root: PathBuf,
    db_path: PathBuf,
    raced: bool,
}

impl std::fmt::Debug for Racing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Racing")
    }
}

impl DerivedProvider for Racing {
    fn name(&self) -> &str {
        "test.racing"
    }
    fn version(&self) -> u32 {
        1
    }
    fn derive(&mut self, docs: &[Document<'_>]) -> Result<Vec<(String, Vec<u8>)>, ProviderError> {
        if !self.raced {
            self.raced = true;
            fs::write(
                self.root.join(".mdkg/work/task-1-a.md"),
                node("task-1", "Parser rewritten", &[], "New.\n"),
            )
            .expect("edit");
            let graph = read(&self.root);
            let (mut other, _) = Db::open_synced(&self.db_path, &self.root, &graph).expect("other");
            other.sync(&self.root, &graph).expect("sync");
        }
        Ok(docs.iter().map(|d| (d.qid.to_string(), vec![1])).collect())
    }
}

#[test]
fn an_output_is_kept_only_for_the_version_it_was_derived_from() {
    let g = write_graph("race", &three());
    let db_path = g.0.join("index.v1.sqlite");
    let graph = read(&g.0);
    let (mut db, _) = Db::open_synced(&db_path, &g.0, &graph).expect("cold");
    let mut p = Racing {
        root: g.0.clone(),
        db_path: db_path.clone(),
        raced: false,
    };
    let stats = provider::refresh(&mut db, &g.0, &graph, &mut p).expect("refresh");
    assert_eq!(stats.derived, 3);
    // task-1 changed under the provider: its output wasn't kept, so the
    // next refresh shows it the new version.
    let stats = provider::refresh(&mut db, &g.0, &read(&g.0), &mut p).expect("refresh");
    assert_eq!(stats.derived, 1);
}

#[test]
fn a_failed_read_rolls_the_sync_back() {
    let g = write_graph("rollback", &three());
    let db_path = g.0.join("index.v1.sqlite");
    let (mut db, _) = Db::open_synced(&db_path, &g.0, &read(&g.0)).expect("cold");
    fs::remove_file(g.0.join(".mdkg/work/task-3-c.md")).expect("rm");
    let graph = read(&g.0);
    let failed = db.sync_then(&g.0, &graph, |_| {
        Err::<(), _>(mdkg_index::IndexError::Io(std::io::Error::other("stop")))
    });
    assert!(failed.is_err());
    assert_eq!(db.info().expect("info").nodes, 3, "nothing applied");
    // A rebuild is one transaction too: a reader in the middle of one
    // keeps its snapshot, and the result is whole.
    let reader = rusqlite::Connection::open(&db_path).expect("reader");
    reader.execute_batch("BEGIN").expect("begin");
    let count = |c: &rusqlite::Connection| -> i64 {
        c.query_row("SELECT count(*) FROM nodes", [], |r| r.get(0))
            .expect("count")
    };
    assert_eq!(count(&reader), 3);
    let stats = db.rebuild(&g.0, &graph).expect("rebuild");
    assert_eq!((stats.added, stats.rebuilt), (2, true));
    assert_eq!(count(&reader), 3, "the reader's snapshot");
    reader.execute_batch("COMMIT").expect("commit");
    assert_eq!(count(&reader), 2);
}

#[test]
fn an_exact_legacy_id_wins_over_a_uuid_prefix_and_says_so() {
    // task-1934 is a migrated node's legacy id, and 1934… starts another
    // task's UUID; task-19 is too short to be a prefix.
    let migrated_id = "task-01920000-0000-7000-8000-000000000000";
    let mut migrated = mem(migrated_id, "task", "Migrated");
    migrated.legacy_id = Some("task-1934".into());
    let mut short = mem("task-01930000-0000-7000-8000-000000000000", "task", "Short");
    short.legacy_id = Some("task-19".into());
    let fresh = "task-1934abcd-0000-7000-8000-000000000000";
    let graph = Index::assemble(
        vec![migrated, short, mem(fresh, "task", "Fresh")],
        vec!["root".into()],
    );
    let r = |h: &str| resolve::resolve_handle(&graph, h, None);
    assert_eq!(
        r("task-1934"),
        Resolution::Ok(format!("root:{migrated_id}"))
    );
    assert_eq!(
        r("TASK-1934"),
        Resolution::Ok(format!("root:{migrated_id}"))
    );
    // A qualified legacy id names nothing (format 0.7, section 6), on a
    // 0.7 graph too.
    assert_eq!(r("root:task-1934"), Resolution::Missing(vec![]));
    let graph07 = Index::assemble_in(
        graph.nodes.clone(),
        vec!["root".into()],
        mdkg_format::id::Uuid7::parse("0190aaaa-0000-7000-8000-000000000000").ok(),
    );
    let r07 = |h: &str| resolve::resolve_handle(&graph07, h, None);
    assert_eq!(
        r07("task-1934"),
        Resolution::Ok(format!("root:{migrated_id}"))
    );
    assert_eq!(r07("root:task-1934"), Resolution::Missing(vec![]));
    assert_eq!(
        r("task-1934a"),
        Resolution::Ok(format!("root:{fresh}")),
        "a longer prefix"
    );
    assert_eq!(
        r("1934"),
        Resolution::Ok(format!("root:{fresh}")),
        "a bare prefix isn't a legacy id"
    );
    assert_eq!(
        r("task-19"),
        Resolution::Ok("root:task-01930000-0000-7000-8000-000000000000".into())
    );

    let note = resolve::shadowed(&graph, "task-1934", None, &r("task-1934")).expect("a note");
    assert_eq!(note.code, mdkg_format::Code::NodeAmbiguous);
    assert_eq!(note.severity, mdkg_format::Severity::Warning);
    assert!(note.message.contains(migrated_id), "{}", note.message);
    let hint = note.hint.unwrap_or_default();
    assert!(hint.contains(fresh) && hint.contains("Fresh"), "{hint}");
    assert!(resolve::shadowed(&graph, "task-19", None, &r("task-19")).is_none());
    assert!(resolve::shadowed(&graph, "1934", None, &r("1934")).is_none());

    // An unmigrated node's 0.6.0 id shadows a prefix the same way.
    let graph = Index::assemble(
        vec![mem("task-1934", "task", "Old"), mem(fresh, "task", "Fresh")],
        vec!["root".into()],
    );
    let res = resolve::resolve_handle(&graph, "task-1934", None);
    assert_eq!(res, Resolution::Ok("root:task-1934".into()));
    assert!(resolve::shadowed(&graph, "task-1934", None, &res).is_some());
}

#[test]
fn a_sync_before_the_refresh_doesnt_pair_old_documents_with_new_hashes() {
    let g = write_graph("race-before", &three());
    let db_path = g.0.join("index.v1.sqlite");
    let old = read(&g.0);
    let (mut db, _) = Db::open_synced(&db_path, &g.0, &old).expect("cold");
    // Another process reads the edited graph and syncs it first.
    fs::write(
        g.0.join(".mdkg/work/task-1-a.md"),
        node("task-1", "Parser rewritten", &[], "New.\n"),
    )
    .expect("edit");
    let new = read(&g.0);
    let (mut other, _) = Db::open_synced(&db_path, &g.0, &new).expect("other");
    other.sync(&g.0, &new).expect("sync");
    // This one still holds the old graph: it derives the old task-1, so
    // the new one must be asked about on the next refresh.
    let mut p = Lengths::default();
    provider::refresh(&mut db, &g.0, &old, &mut p).expect("refresh");
    p.seen.clear();
    let stats = provider::refresh(&mut db, &g.0, &new, &mut p).expect("refresh");
    assert_eq!(p.seen, ["root:task-1"], "{stats:?}");
}

#[test]
fn an_ancestor_never_blocks_its_own_child() {
    // ORR's shape (task-41 check on a migrated graph): a goal whose
    // `blocks` lists its own child task, and a task whose blocked_by names
    // its epic. Neither may keep the child from being ready.
    let mut goal = mem("goal-1", "goal", "Ship");
    goal.edges.blocks = vec!["root:task-1".into()];
    let mut child = mem("task-1", "task", "Child of the goal");
    child.edges.parent = Some("root:goal-1".into());
    let epic = mem("epic-1", "epic", "Epic");
    let mut under_epic = mem("task-2", "task", "Under the epic");
    under_epic.edges.epic = Some("root:epic-1".into());
    under_epic.edges.blocked_by = vec!["root:epic-1".into()];
    // A real blocker still blocks.
    let mut blocked = mem("task-3", "task", "Really blocked");
    blocked.edges.parent = Some("root:goal-1".into());
    blocked.edges.blocked_by = vec!["root:task-1".into()];
    let graph = Index::assemble(
        vec![goal, child, epic, under_epic, blocked],
        vec!["root".into()],
    );
    let ready: Vec<&str> = walk::ready_set(&graph, &["task"], &["todo".to_string()], 9)
        .iter()
        .map(|n| n.qid.as_str())
        .collect();
    assert_eq!(ready, ["root:task-1", "root:task-2"]);
    let warned: Vec<(String, String)> = mdkg_graph::graph_checks::ancestor_blocks(&graph)
        .into_iter()
        .map(|d| (d.code.as_str().to_string(), d.node.unwrap_or_default()))
        .collect();
    assert_eq!(
        warned,
        [
            (
                "edge.blocks_ancestor".to_string(),
                "root:goal-1".to_string()
            ),
            (
                "edge.blocks_ancestor".to_string(),
                "root:task-2".to_string()
            ),
        ]
    );
}

#[test]
fn ancestor_block_warnings_come_in_a_stable_order() {
    // Codex, b8883bf: an epic and the goal above it both list the task in
    // `blocks`; the two warnings come in qid order every time.
    let mut goal = mem("goal-1", "goal", "Ship");
    goal.edges.blocks = vec!["root:task-1".into()];
    let mut epic = mem("epic-1", "epic", "Epic");
    epic.edges.parent = Some("root:goal-1".into());
    epic.edges.blocks = vec!["root:task-1".into()];
    let mut task = mem("task-1", "task", "Task");
    task.edges.epic = Some("root:epic-1".into());
    for _ in 0..20 {
        let graph = Index::assemble(
            vec![goal.clone(), epic.clone(), task.clone()],
            vec!["root".into()],
        );
        let nodes: Vec<String> = mdkg_graph::graph_checks::ancestor_blocks(&graph)
            .into_iter()
            .map(|d| d.node.unwrap_or_default())
            .collect();
        assert_eq!(nodes, ["root:epic-1", "root:goal-1"]);
    }
}

#[test]
fn a_prerequisite_on_another_branch_still_blocks() {
    // RC1 audit I3: in a format 0.7 graph, a blocked_by naming a local id
    // no node here has (its node is on another branch) is an open
    // blocker: the ready set doesn't offer the dependent. A ref into
    // another graph still can't be checked, so it doesn't block.
    let g = "0190aaaa-0000-7000-8000-000000000000";
    let elsewhere = "feat-01950000-0000-7000-8000-000000000000";
    let other_graph = "0191bbbb-0000-7000-8000-000000000000";
    let mut waits = mem("task-01960000-0000-7000-8000-000000000001", "task", "Waits");
    waits.edges.blocked_by = vec![format!("root:{elsewhere}")];
    let mut foreign = mem(
        "task-01960000-0000-7000-8000-000000000002",
        "task",
        "Foreign",
    );
    foreign.edges.blocked_by = vec![format!("mdkg://{other_graph}/{elsewhere}")];
    // An ancestor never blocks its child (edge.blocks_ancestor), missing
    // here or not (Codex, 4925bd9).
    let epic = "epic-01950000-0000-7000-8000-000000000003";
    let mut child = mem("task-01960000-0000-7000-8000-000000000003", "task", "Child");
    child.edges.epic = Some(format!("root:{epic}"));
    child.edges.blocked_by = vec![format!("root:{epic}")];
    let graph = Index::assemble_in(
        vec![waits, foreign, child],
        vec!["root".into()],
        mdkg_format::id::Uuid7::parse(g).ok(),
    );
    let ready: Vec<&str> = walk::ready_set(&graph, &["task"], &["todo".to_string()], 9)
        .iter()
        .map(|n| n.id.as_str())
        .collect();
    assert_eq!(
        ready,
        [
            "task-01960000-0000-7000-8000-000000000002",
            "task-01960000-0000-7000-8000-000000000003"
        ]
    );
}
