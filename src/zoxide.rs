use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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
    let output = Command::new("zoxide")
        .args(["query", "-ls"])
        .output()
        .map_err(|error| format!("failed to run zoxide: {error}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "zoxide query failed with {}: {}",
            output.status,
            detail.trim()
        ));
    }
    let stdout = String::from_utf8(output.stdout)
        .map_err(|_| "zoxide returned a path that is not valid UTF-8".to_string())?;
    let zoxide_entries = parse_scores(&stdout);
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
}
