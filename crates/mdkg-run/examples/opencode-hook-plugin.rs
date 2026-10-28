//! Print this build's canonical OpenCode plugin for the live-check fixture.
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 1 {
        eprintln!("usage: opencode-hook-plugin <mdkg binary or observer wrapper>");
        std::process::exit(2);
    }
    print!("{}", mdkg_run::hooks::opencode_plugin(&args[0]));
}
