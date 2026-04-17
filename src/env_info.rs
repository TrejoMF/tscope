use std::fs;
use std::path::{Path, PathBuf};

/// Current branch of the git repo at or above `cwd`. Walks up until it finds
/// a `.git` entry. Handles both regular repos (`.git` directory) and worktrees
/// or submodules (`.git` file containing a `gitdir:` pointer). For detached
/// HEAD, returns the short SHA so the chip is never blank.
pub fn git_branch(cwd: &Path) -> Option<String> {
    let git_dir = find_git_dir(cwd)?;
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    if let Some(rest) = head.strip_prefix("ref: ") {
        Some(rest.rsplit('/').next().unwrap_or(rest).to_string())
    } else {
        Some(head.chars().take(7).collect())
    }
}

fn find_git_dir(start: &Path) -> Option<PathBuf> {
    let mut cur: Option<&Path> = Some(start);
    while let Some(dir) = cur {
        let candidate = dir.join(".git");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if candidate.is_file() {
            if let Ok(s) = fs::read_to_string(&candidate) {
                if let Some(rest) = s.strip_prefix("gitdir: ") {
                    let p = PathBuf::from(rest.trim());
                    return Some(if p.is_absolute() { p } else { dir.join(p) });
                }
            }
        }
        cur = dir.parent();
    }
    None
}
