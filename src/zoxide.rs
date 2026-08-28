use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::lazygit;

pub struct RankedDirectory {
    pub path: PathBuf,
    pub label: String,
    pub display_path: String,
    pub score: f64,
    pub alpha_order: usize,
}

pub fn ranked_directories() -> Result<Vec<RankedDirectory>, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set".to_string())?;
    let projects_root = home.join("code/src");
    // zoxide is absent on sandbox hosts and optional elsewhere. When it is
    // missing, or present but yields nothing (an uninitialized db), fall back
    // to a filesystem scan of the project roots so the picker still works.
    let zoxide_entries = match Command::new("zoxide").args(["query", "-ls"]).output() {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8(output.stdout)
                .map_err(|_| "zoxide returned a path that is not valid UTF-8".to_string())?;
            parse_scores(&stdout)
        }
        Ok(_) => Vec::new(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(format!("failed to run zoxide: {error}")),
    };
    let scores = zoxide_entries
        .iter()
        .map(|(score, path)| (path.clone(), *score))
        .collect::<BTreeMap<_, _>>();
    let mut candidates = BTreeMap::new();
    for (score, path) in zoxide_entries {
        if path
            .strip_prefix(&projects_root)
            .is_ok_and(|relative| !relative.as_os_str().is_empty())
        {
            candidates.insert(path, score);
        }
    }
    add_directory(&mut candidates, &scores, home.join(".dot"));
    if let Ok(entries) = fs::read_dir(home.join("tmp")) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                add_directory(&mut candidates, &scores, path);
            }
        }
    }

    if candidates.is_empty() {
        let roots = [projects_root.clone(), PathBuf::from("/workspace/code/src")];
        return fallback_directories(&home, &roots);
    }

    let mut entries = candidates.into_iter().collect::<Vec<_>>();
    entries.sort_by(|(left_path, left_score), (right_path, right_score)| {
        right_score
            .total_cmp(left_score)
            .then_with(|| left_path.cmp(right_path))
    });
    let mut alphabetical = entries
        .iter()
        .map(|(path, _)| path.clone())
        .collect::<Vec<_>>();
    alphabetical.sort();
    let alpha_order = alphabetical
        .into_iter()
        .enumerate()
        .map(|(index, path)| (path, index))
        .collect::<BTreeMap<_, _>>();

    Ok(entries
        .into_iter()
        .map(|(path, score)| RankedDirectory {
            alpha_order: alpha_order.get(&path).copied().unwrap_or(usize::MAX),
            label: directory_label(&home, &projects_root, &path),
            display_path: compact_home(&home, &path),
            path,
            score,
        })
        .collect())
}

/// Filesystem fallback for [`ranked_directories`] when zoxide is absent or has
/// no ranked project directories. Scans each root for git repositories up to
/// [`lazygit::MAX_DEPTH`] levels deep, so a sandbox without zoxide can still
/// create a workspace at a nearby project.
///
/// Roots are `$HOME/code/src` (the tree zoxide already filters to) and
/// `/workspace/code/src` (where sandbox hosts keep their checkout). A
/// directory without a `.git` entry is skipped, matching `lazygit`'s notion of
/// a project root; a non-git directory can still be opened by typing its path
/// in the picker. Scores are zero, so the picker's zoxide/alpha order toggle
/// collapses to alphabetical until zoxide is available.
fn fallback_directories(home: &Path, roots: &[PathBuf]) -> Result<Vec<RankedDirectory>, String> {
    let cancelled = std::sync::atomic::AtomicBool::new(false);
    let mut found: BTreeMap<PathBuf, ()> = BTreeMap::new();
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        lazygit::scan_repositories(root, lazygit::MAX_DEPTH, &cancelled, &mut |path| {
            found.insert(path, ());
        });
    }
    if found.is_empty() {
        return Err(
            "no project directories found under ~/code/src or /workspace/code/src".to_string(),
        );
    }
    let mut paths: Vec<PathBuf> = found.into_keys().collect();
    paths.sort();
    let alpha_order = paths
        .iter()
        .enumerate()
        .map(|(index, path)| (path.clone(), index))
        .collect::<BTreeMap<_, _>>();
    Ok(paths
        .into_iter()
        .map(|path| {
            let root = roots
                .iter()
                .find(|root| path.starts_with(root))
                .map(PathBuf::as_path)
                .unwrap_or(home);
            RankedDirectory {
                label: directory_label(home, root, &path),
                display_path: compact_home(home, &path),
                path: path.clone(),
                score: 0.0,
                alpha_order: alpha_order.get(&path).copied().unwrap_or(usize::MAX),
            }
        })
        .collect())
}

fn add_directory(
    candidates: &mut BTreeMap<PathBuf, f64>,
    scores: &BTreeMap<PathBuf, f64>,
    path: PathBuf,
) {
    if path.is_dir() {
        candidates
            .entry(path.clone())
            .or_insert_with(|| scores.get(&path).copied().unwrap_or(0.0));
    }
}

fn directory_label(home: &Path, projects_root: &Path, path: &Path) -> String {
    let relative = path
        .strip_prefix(projects_root)
        .or_else(|_| path.strip_prefix(home))
        .unwrap_or(path);
    let segments = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>();
    segments
        .iter()
        .skip(segments.len().saturating_sub(2))
        .map(|segment| segment.as_ref())
        .collect::<Vec<_>>()
        .join("/")
}

fn compact_home(home: &Path, path: &Path) -> String {
    path.strip_prefix(home)
        .map(|relative| format!("~/{}", relative.display()))
        .unwrap_or_else(|_| path.display().to_string())
}

fn parse_scores(output: &str) -> Vec<(f64, PathBuf)> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let split = line.find(char::is_whitespace)?;
            let score = line[..split].parse::<f64>().ok()?;
            let path = line[split..].trim_start();
            (!path.is_empty()).then(|| (score, PathBuf::from(path)))
        })
        .collect()
}

/// Mirrors zoxide's keyword matcher (src/db/stream.rs).
///
/// All keywords must appear in order within the path. The last keyword must
/// match the final path component (nothing after it may be a path separator).
/// Matching is case-insensitive.
pub fn keywords_match(path: &str, query: &str) -> bool {
    let keywords: Vec<&str> = query.split_whitespace().collect();
    let (last, keywords) = match keywords.split_last() {
        Some(split) => split,
        None => return true,
    };
    if last.is_empty() {
        return true;
    }

    let path = path.to_lowercase();
    let mut path = path.as_str();
    match path.rfind(&last.to_lowercase()) {
        Some(idx) => {
            if path[idx + last.len()..].contains(std::path::is_separator) {
                return false;
            }
            path = &path[..idx];
        }
        None => return false,
    }

    for keyword in keywords.iter().rev() {
        if keyword.is_empty() {
            continue;
        }
        match path.rfind(&keyword.to_lowercase()) {
            Some(idx) => path = &path[..idx],
            None => return false,
        }
    }
    true
}

pub fn path_query_match(path: &Path, query: &str) -> bool {
    let query = query.trim();
    if query.is_empty() {
        return true;
    }
    let expanded = expand_home(query);
    let path_text = path.to_string_lossy().to_lowercase();
    let compact = std::env::var_os("HOME")
        .map(PathBuf::from)
        .and_then(|home| {
            path.strip_prefix(&home)
                .ok()
                .map(|relative| format!("~/{}", relative.display()).to_lowercase())
        });
    let expanded = expanded.to_string_lossy().to_lowercase();
    path_text.contains(&expanded) || compact.as_deref().is_some_and(|text| text.contains(query))
}

pub fn expand_home(query: &str) -> PathBuf {
    if query == "~" || query.starts_with("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            let mut path = PathBuf::from(home);
            if query.len() > 2 {
                path.push(&query[2..]);
            }
            return path;
        }
    }
    PathBuf::from(query)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scores_and_paths_with_spaces() {
        assert_eq!(
            parse_scores(" 859.7 /tmp/one\n  12.5 /tmp/a project\ninvalid\n"),
            vec![
                (859.7, PathBuf::from("/tmp/one")),
                (12.5, PathBuf::from("/tmp/a project"))
            ]
        );
    }

    #[test]
    fn zoxide_keyword_matcher_matches_in_order_with_last_component_rule() {
        // Case-insensitive substring of the last component.
        assert!(keywords_match("/foo/bar", "ba"));
        assert!(keywords_match("/FOO/BAR", "ba"));
        // Last component must be the final segment.
        assert!(!keywords_match("/bar/foo", "ba"));
        // In-order keywords across components.
        assert!(keywords_match("/foo/bar", "fo ba"));
        assert!(!keywords_match("/bar/foo", "fo ba"));
        // Slash-aware: "foo/" must sit right before a separator.
        assert!(!keywords_match("/foo", "foo/"));
        assert!(keywords_match("/foo/bar", "foo/"));
        assert!(!keywords_match("/foo/bar/baz", "foo/"));
        // "foo /" is equivalent and the trailing slash is optional.
        assert!(!keywords_match("/foo", "foo /"));
        assert!(keywords_match("/foo/bar", "foo /"));
        assert!(keywords_match("/foo/bar/baz", "foo /"));
        // Split components with explicit separators.
        assert!(keywords_match("/foo/bar", "/ fo / ar"));
        // Keyword can span an existing slash.
        assert!(keywords_match("/foo/bar", "oo/ba"));
        // Overlap between adjacent keywords must be real (zoxide tests).
        assert!(!keywords_match("/foo/bar", "foo o bar"));
        assert!(!keywords_match("/foo/bar", "/foo/ /bar"));
        assert!(keywords_match("/foo/baz/bar", "/foo/ /bar"));
    }

    #[test]
    fn labels_projects_and_extra_directories_with_two_segments() {
        let home = Path::new("/Users/example");
        let projects = home.join("code/src");
        assert_eq!(
            directory_label(
                home,
                &projects,
                &projects.join("github.com/aliou/herdr-cast")
            ),
            "aliou/herdr-cast"
        );
        assert_eq!(
            directory_label(home, &projects, &home.join("tmp/repro")),
            "tmp/repro"
        );
        assert_eq!(directory_label(home, &projects, &home.join(".dot")), ".dot");
    }

    fn init_repo(path: &Path) {
        fs::create_dir_all(path).unwrap();
        fs::create_dir_all(path.join(".git")).unwrap();
    }

    #[test]
    fn fallback_finds_git_repos_under_the_project_root() {
        let home = std::env::temp_dir().join(format!("cast-fallback-home-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let projects_root = home.join("code/src");
        init_repo(&projects_root.join("github.com/aliou/one"));
        init_repo(&projects_root.join("github.com/aliou/two"));
        // A directory without `.git` is not a project root and is skipped.
        fs::create_dir_all(projects_root.join("github.com/aliou/not-a-repo")).unwrap();

        let roots = [projects_root.clone()];
        let dirs = fallback_directories(&home, &roots).unwrap();
        let labels: Vec<String> = dirs.iter().map(|d| d.label.clone()).collect();
        assert!(labels.contains(&"aliou/one".to_string()));
        assert!(labels.contains(&"aliou/two".to_string()));
        assert!(!labels.iter().any(|label| label.contains("not-a-repo")));
        // No zoxide scores: every fallback entry scores zero.
        assert!(dirs.iter().all(|d| d.score == 0.0));
        // alpha_order is a dense 0..n so the picker's alpha toggle works.
        let mut orders: Vec<usize> = dirs.iter().map(|d| d.alpha_order).collect();
        orders.sort();
        assert_eq!(orders, (0..dirs.len()).collect::<Vec<_>>());

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn fallback_errors_when_no_repos_are_found() {
        let home = std::env::temp_dir().join(format!("cast-fallback-empty-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap();
        // No roots to scan: nothing can be found, so the picker surfaces the
        // error instead of opening empty.
        assert!(fallback_directories(&home, &[]).is_err());
        let _ = fs::remove_dir_all(&home);
    }
}
