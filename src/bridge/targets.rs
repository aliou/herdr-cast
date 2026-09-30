//! Which machines the host should bridge to: every target of a running
//! `herdr --remote <target>` client on this Mac. Nothing is stored; a remote
//! is bridged exactly while a client is attached to it.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::process::{Command, Stdio};

/// Running remote clients, grouped by the machine ssh would reach.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Wanted {
    /// The first target string seen for this machine; used for ssh.
    pub target: String,
    /// Pids of the `herdr --remote` clients for this machine.
    pub pids: BTreeSet<u32>,
}

/// Resolves and caches target strings to ssh's effective destination.
#[derive(Default)]
pub struct Resolver {
    keys: HashMap<String, String>,
}

impl Resolver {
    pub fn scan(&mut self) -> Result<BTreeMap<String, Wanted>, String> {
        let output = Command::new("/bin/ps")
            .args(["-axo", "pid=,args="])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(|error| format!("failed to run ps: {error}"))?;
        if !output.status.success() {
            return Err(format!("ps failed with {}", output.status));
        }
        let mut wanted: BTreeMap<String, Wanted> = BTreeMap::new();
        for (pid, target) in String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(parse_line)
        {
            let key = self.key(&target);
            let entry = wanted.entry(key).or_default();
            if entry.target.is_empty() {
                entry.target = target;
            }
            entry.pids.insert(pid);
        }
        Ok(wanted)
    }

    /// Aliases for one machine (`factorial`, `user@192.168.1.16`) share a
    /// key. Falls back to the raw target when `ssh -G` fails.
    fn key(&mut self, target: &str) -> String {
        if let Some(key) = self.keys.get(target) {
            return key.clone();
        }
        let key = Command::new("ssh")
            .args(["-G", target])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| parse_ssh_config(&String::from_utf8_lossy(&output.stdout)))
            .unwrap_or_else(|| target.to_string());
        self.keys.insert(target.to_string(), key.clone());
        key
    }
}

/// `(pid, target)` from one `ps -axo pid=,args=` line, when it is a herdr
/// client started with `--remote <target>` or `--remote=<target>`.
pub fn parse_line(line: &str) -> Option<(u32, String)> {
    let mut words = line.split_whitespace();
    let pid = words.next()?.parse().ok()?;
    let program = words.next()?;
    if Path::new(program).file_name()? != "herdr" {
        return None;
    }
    while let Some(word) = words.next() {
        let target = match word.strip_prefix("--remote") {
            Some("") => words.next(),
            Some(rest) => rest.strip_prefix('='),
            None => continue,
        };
        return target
            .filter(|target| !target.is_empty() && !target.starts_with('-'))
            .map(|target| (pid, target.to_string()));
    }
    None
}

/// `user@hostname:port` from `ssh -G` output.
pub fn parse_ssh_config(output: &str) -> Option<String> {
    let value = |name: &str| {
        output.lines().find_map(|line| {
            let (key, value) = line.split_once(' ')?;
            (key == name).then(|| value.trim().trim_end_matches('.').to_string())
        })
    };
    Some(format!(
        "{}@{}:{}",
        value("user")?,
        value("hostname")?,
        value("port")?
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_remote_clients_and_their_targets() {
        assert_eq!(
            parse_line(
                "47424 herdr --remote aliou.freelancer@192.168.1.16 --remote-keybindings server"
            ),
            Some((47424, "aliou.freelancer@192.168.1.16".to_string()))
        );
        assert_eq!(
            parse_line(" 11609 /etc/profiles/per-user/me/bin/herdr --remote=donut"),
            Some((11609, "donut".to_string()))
        );
    }

    #[test]
    fn ignores_everything_else() {
        for line in [
            "2242 /etc/profiles/per-user/me/bin/herdr server",
            "2241 herdr",
            "47465 /etc/profiles/per-user/me/bin/herdr client",
            "8547 herdr session attach factorial",
            "9000 herdr remote-client-bridge",
            "9001 herdr --remote-keybindings server",
            "9002 herdr --remote",
            "9003 herdr --remote --remote-keybindings server",
            "9004 herdr-cast bridge-send --remote donut",
            "9005 ssh -C -T donut herdr --remote donut",
            "10916 node sbxctl.cjs herdr stellar-lee-adama",
            "not-a-pid herdr --remote donut",
        ] {
            assert_eq!(parse_line(line), None, "matched {line:?}");
        }
    }

    #[test]
    fn keys_machines_by_effective_ssh_destination() {
        let output = "user alioudiallo\nhostname donut.tetra-albacore.ts.net.\nport 22\nidentityfile ~/.ssh/id\n";
        assert_eq!(
            parse_ssh_config(output).as_deref(),
            Some("alioudiallo@donut.tetra-albacore.ts.net:22")
        );
        assert_eq!(parse_ssh_config("user x\n"), None);
    }
}
