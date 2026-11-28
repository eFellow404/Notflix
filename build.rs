//! Build script embedding the short Git commit SHA into the compiled binary.

use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
    println!("cargo:rerun-if-changed=.git/HEAD");

    if let Ok(head) = std::fs::read_to_string(".git/HEAD") {
        if let Some(ref_path) = head.trim().strip_prefix("ref: ") {
            let full_ref = format!(".git/{}", ref_path.trim());
            if Path::new(&full_ref).exists() {
                println!("cargo:rerun-if-changed={full_ref}");
            }
        }
    }

    let sha = std::env::var("GITHUB_SHA")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().chars().take(7).collect::<String>())
        .or_else(|| {
            Command::new("git")
                .args(["rev-parse", "--short=7", "HEAD"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "dev".to_string());

    println!("cargo:rustc-env=NOTFLIX_BUILD_SHA={sha}");
}
