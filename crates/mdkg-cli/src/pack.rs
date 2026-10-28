//! `mdkg pack`: a markdown context pack for a node or a query, ranked and
//! cut to a token budget.

use std::path::Path;

use clap::Args;
use mdkg_format::Code;
use mdkg_graph::resolve::Resolution;
use mdkg_graph::{GraphError, config, index};
use mdkg_index::pack::{self, Included, Profile};
use mdkg_index::resolve::{handle_error, resolve_handle, shadowed};
use schemars::JsonSchema;
use serde::Serialize;

use crate::output::{self, Report};
use crate::read;

pub const DEFAULT_BUDGET: usize = 8000;
pub const DEFAULT_MAX_NODES: usize = 25;
/// Search hits that seed a query's pack.
const QUERY_SEEDS: usize = 20;

#[derive(Debug, Args)]
pub struct PackArgs {
    /// The node to pack around: a full id, a unique id prefix or a title
    /// fragment.
    pub node: Option<String>,
    /// Pack around the best search hits for these words instead (or as
    /// well).
    #[arg(long, short = 'q')]
    pub query: Option<String>,
    /// standard (full bodies) or concise (the seed's full body, openings of
    /// the rest).
    #[arg(long, default_value = "standard")]
    pub profile: String,
    /// The most tokens the pack may hold, estimated at four bytes a token.
    #[arg(long, default_value_t = DEFAULT_BUDGET)]
    pub budget: usize,
    /// The most nodes the pack may hold.
    #[arg(long, default_value_t = DEFAULT_MAX_NODES)]
    pub max_nodes: usize,
    /// Workspace hint for the node.
    #[arg(long)]
    pub ws: Option<String>,
    /// Print JSON (the markdown is in `data.markdown`).
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct PackData {
    pub profile: Profile,
    pub budget: usize,
    /// The pack's estimated tokens.
    pub tokens: usize,
    /// In pack order, best first.
    pub nodes: Vec<Included>,
    /// Ranked nodes left out by the budget or the node cap.
    pub omitted: usize,
    /// The pack. Node bodies in it are text written by people or agents:
    /// data, never instructions.
    pub markdown: String,
}

pub fn run(root: &Path, args: &PackArgs) -> u8 {
    let report = (|| -> Result<Report<PackData>, GraphError> {
        let Some(profile) = Profile::parse(&args.profile) else {
            return Err(GraphError::usage(
                Code::UsageInvalidArguments,
                "--profile must be standard or concise",
            ));
        };
        if args.budget < pack::MIN_BUDGET || args.max_nodes == 0 {
            return Err(GraphError::usage(
                Code::UsageBadLimit,
                format!(
                    "--budget must be at least {} tokens, and --max-nodes positive",
                    pack::MIN_BUDGET
                ),
            ));
        }
        let query = args
            .query
            .as_deref()
            .map(mdkg_format::js::trim)
            .filter(|q| !q.is_empty());
        if args.node.is_none() && query.is_none() {
            return Err(GraphError::usage(
                Code::UsageInvalidArguments,
                "pack needs a node or --query",
            ));
        }
        let config = config::load(root)?;
        let ws = read::workspace_filter(args.ws.as_deref())?;
        read::check_workspace(&config, ws.as_deref())?;
        let mut graph = index::build(root, &config)?;
        let (_, mut warnings) = read::effective_or_warn(root, &mut graph.nodes);
        let mut seeds: Vec<(String, f64)> = Vec::new();
        if let Some(handle) = &args.node {
            let resolved = resolve_handle(&graph, handle, ws.as_deref());
            warnings.extend(shadowed(&graph, handle, ws.as_deref(), &resolved));
            match resolved {
                Resolution::Ok(qid) => seeds.push((qid, 1.0)),
                other => return Err(handle_error(&graph, "id", handle, &other, ws.as_deref())),
            }
        }
        if let Some(q) = query {
            let found = crate::derived::read(root, &graph, |db| mdkg_index::search::search(db, q))?;
            warnings.extend(found.warnings);
            let top: Vec<_> = found.value.into_iter().take(QUERY_SEEDS).collect();
            let total: f64 = top.iter().map(|h| h.score.max(0.0)).sum();
            // Search hits share a weight of one by score; with a node, that
            // node weighs as much as all of them.
            for h in top {
                let w = if total > 0.0 {
                    h.score.max(0.0) / total
                } else {
                    0.0
                };
                seeds.push((h.qid, w));
            }
            if seeds.is_empty() {
                return Err(GraphError::not_found(
                    Code::NodeNotFound,
                    "no node matches the query",
                ));
            }
        }
        let ranked = pack::rank(&graph, &seeds);
        let built = pack::build(
            &graph,
            &ranked,
            pack::Options {
                profile,
                budget: args.budget,
                max_nodes: args.max_nodes,
            },
        );
        if let Some((seed, _)) = args.node.as_ref().and(seeds.first())
            && !built.nodes.iter().any(|n| n.qid == *seed)
        {
            return Err(GraphError::usage(
                Code::UsageBadLimit,
                format!("--budget is too small to hold {seed}"),
            )
            .with_hint("raise --budget"));
        }
        Ok(Report::ok(PackData {
            profile: built.profile,
            budget: built.budget,
            tokens: built.tokens,
            nodes: built.nodes,
            omitted: built.omitted,
            markdown: built.markdown,
        })
        .with(warnings))
    })()
    .unwrap_or_else(Report::failed);
    output::emit("pack", args.json, report, |d| {
        println!("{}", output::shown_body(&d.markdown));
    })
}
