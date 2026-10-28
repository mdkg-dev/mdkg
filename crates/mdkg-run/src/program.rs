//! Executable identity shared by PATH selection and diagnostics.

use std::path::{Path, PathBuf};

/// When `p` is the launcher that cargo-dist's npm package puts on PATH,
/// the folder holding the binary it starts and that binary's name. The
/// launcher is `run-<name>.js`, whose body is `run("<name>")` from
/// `./binary`, and it keeps the binaries in `node_modules/.bin_real`
/// beside it. The package's other scripts (`install.js`) don't count.
pub fn npm_launcher(p: &Path) -> Option<(PathBuf, String)> {
    use std::io::Read;
    let real = std::fs::canonicalize(p).ok()?;
    let name = real
        .file_name()?
        .to_str()?
        .strip_prefix("run-")?
        .strip_suffix(".js")?
        .to_string();
    let mut text = String::new();
    std::fs::File::open(&real)
        .ok()?
        .take(4096)
        .read_to_string(&mut text)
        .ok()?;
    let call = format!("run({});", serde_json::to_string(&name).ok()?);
    let launches = text.lines().any(|l| l.trim() == call)
        && text
            .lines()
            .any(|l| l.trim() == r#"const { run } = require("./binary");"#);
    let dir = real.parent()?.join("node_modules").join(".bin_real");
    (launches && dir.is_dir()).then_some((dir, name))
}

/// Whether running `p` runs the program `exe`: it is `exe`, or the npm
/// package's launcher for it.
pub fn runs_program(p: &Path, exe: &Path) -> bool {
    if same_program(p, exe) {
        return true;
    }
    let (Some((dir, name)), Some(file)) = (npm_launcher(p), exe.file_name()) else {
        return false;
    };
    let stem = file.to_string_lossy();
    stem.strip_suffix(".exe").unwrap_or(&stem) == name && same_program(&dir.join(file), exe)
}

fn same_program(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

/// Whether two PATH selections invoke the same file, directly or through
/// cargo-dist's npm launcher. No byte/build equality or arbitrary wrappers.
pub fn same_invocation(a: &Path, b: &Path) -> bool {
    let target = npm_launcher(b).map(|(dir, name)| dir.join(name));
    runs_program(a, target.as_deref().unwrap_or(b))
}
