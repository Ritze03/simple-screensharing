// Stamps the git commit into the binary (`SSS_GIT_COMMIT`, empty without git) for the header
// of the in-app log dump (`src/logbuf.rs`). No rerun directive: cargo then reruns this
// whenever a file of the package changes, which is when a new commit matters.
fn main() {
    let commit = std::process::Command::new("git")
        .args(["rev-parse", "--short=9", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .unwrap_or_default();
    println!("cargo:rustc-env=SSS_GIT_COMMIT={commit}");
}
