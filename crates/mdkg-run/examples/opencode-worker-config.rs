//! Print the inline config (`OPENCODE_CONFIG_CONTENT`) this build gives an
//! OpenCode worker turn, without its MCP server, for the live permission
//! check (scripts/check-opencode-hooks.py --live-permissions).
use mdkg_run::profile::{Harness, Role, TurnInput, opencode_config};
use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 2 {
        eprintln!("usage: opencode-worker-config <mdkg binary> <worker checkout>");
        std::process::exit(2);
    }
    let (mdkg, cwd) = (Path::new(&args[0]), Path::new(&args[1]));
    let mut cfg = opencode_config(&TurnInput {
        agent: "live-check",
        mdkg,
        home: None,
        xdg_config: None,
        common_dir: None,
        trusted_worktree: None,
        graph_root: None,
        main_ref: Some("main"),
        harness: Harness::OpenCode,
        role: Role::Worker,
        cwd,
        model: None,
        effort: None,
        sandbox: None,
        approvals: None,
        resume: None,
        new_session: "live-check",
        allow: &[],
        output_file: Path::new("/dev/null"),
        prompt: "",
        api_key: None,
    });
    // The check reads the resolved permissions only; no MCP server starts.
    if let Some(cfg) = cfg.as_object_mut() {
        cfg.remove("mcp");
    }
    println!("{cfg}");
}
