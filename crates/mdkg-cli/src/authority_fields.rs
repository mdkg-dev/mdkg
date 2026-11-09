//! Refuse authority-field mutations made from a launcher turn. Keep the
//! process/turn check shared with existing outside-turn operations.

use mdkg_graph::GraphError;
use mdkg_run::orchestration::agent::Definition;
use serde_json::Value;

use crate::coord::Repo;

const CODE: &str = "authority.field_in_turn";

fn changed(before: &Value, after: &Value) -> bool {
    ["/env", "/policy/env"]
        .iter()
        .any(|at| before.pointer(at) != after.pointer(at))
        // Definition uses Option<String> for native overrides: JSON null
        // and omission both mean no override, and serialization omits None.
        || ["/sandbox", "/approvals"].iter().any(|at| {
            before.pointer(at).filter(|v| !v.is_null())
                != after.pointer(at).filter(|v| !v.is_null())
        })
}

fn outside(repo: &Repo, actor: &str) -> Result<(), String> {
    let common = mdkg_git::common_dir(&repo.top)
        .map_err(|_| "cannot identify the repository for the turn check".to_owned())?;
    let cwd = std::env::current_dir()
        .map_err(|_| "cannot identify the caller's repository".to_owned())?;
    // A person's terminal remains usable while an unrelated worker runs in
    // the same checkout. Markers, the caller's live actor and ancestry still
    // refuse; changing --as or clearing a marker grants no exception.
    mdkg_run::approval::outside_turn_by_ancestry(
        &common,
        actor,
        &[&repo.top, &cwd],
        std::env::var_os(mdkg_run::profile::TURN_MARK).is_some(),
    )
}

fn refused(why: &str) -> GraphError {
    crate::coord::graph_git::graph_error(
        CODE,
        "env, sandbox and approvals fields cannot be changed inside an agent turn",
    )
    .with_hint(format!(
        "a person changes these fields outside any agent turn, in their own terminal; \
         turn check: {why}"
    ))
}

/// Compare the full proposed config, so replacing or removing any parent
/// cannot evade the check. An ordinary edit or identical value is allowed.
pub(crate) fn config(
    repo: &Repo,
    actor: &str,
    before: &Value,
    after: &Value,
) -> Result<(), GraphError> {
    let agents = |value: &Value| {
        value
            .get("agents")
            .and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default()
    };
    if agents(before)
        .into_iter()
        .chain(agents(after))
        .any(|name| changed(&before["agents"][&name], &after["agents"][&name]))
    {
        outside(repo, actor).map_err(|why| refused(&why))?;
    }
    Ok(())
}

/// Compare against the committed definition, never a mutable edit candidate.
/// A person must still be able to replace an invalid definition from a file.
pub(crate) fn definition(
    repo: &Repo,
    actor: &str,
    name: &str,
    after: Option<&Definition>,
) -> Result<(), GraphError> {
    let Err(why) = outside(repo, actor) else {
        return Ok(());
    };
    let authority = mdkg_run::orchestration::authority(&repo.root).map_err(|_| refused(&why))?;
    let before = authority
        .definitions
        .get(name)
        .map(|definition| &definition.value)
        .unwrap_or(&Value::Null);
    let after = serde_json::to_value(after).map_err(|_| refused(&why))?;
    if changed(before, &after) {
        return Err(refused(&why));
    }
    Ok(())
}
