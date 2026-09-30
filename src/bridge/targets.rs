//! Which machines the host should bridge to: every target of a running
//! `herdr --remote <target>` client on this Mac. Nothing is stored; a remote
//! is bridged exactly while a client is attached to it.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::process::{Command, Stdio};

/// One running `herdr --remote` client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Client {
    pub pid: u32,
    pub ppid: u32,
    /// Seconds since the client started.
    pub age: u64,
    pub target: String,
    /// `--session <name>`; absent for the remote's default session.
    pub session: Option<String>,
}

/// Running remote clients, grouped by the machine ssh would reach.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Wanted {
    /// The first target string seen for this machine; used for ssh.
    pub target: String,
    pub clients: Vec<Client>,
}

impl Wanted {
    pub fn pids(&self) -> BTreeSet<u32> {
        self.clients.iter().map(|client| client.pid).collect()
    }

    /// Pids that may own the Ghostty tab of a client attached to `session`,
    /// best first: clients of that session before any other client of this
    /// machine, newest first within each group. Each client offers its own
    /// pid (Ghostty reports a tab's foreground process) and then its
    /// parent's (a wrapper such as `sbxctl herdr` in the foreground).
    pub fn tab_pids(&self, session: Option<&str>) -> Vec<u32> {
        let mut clients: Vec<&Client> = self.clients.iter().collect();
        clients.sort_by_key(|client| (client.session.as_deref() != session, client.age));
        let mut pids = Vec::new();
        for client in clients {
            for pid in [client.pid, client.ppid] {
                if pid > 1 && !pids.contains(&pid) {
                    pids.push(pid);
                }
            }
        }
        pids
    }
}

/// Resolves and caches target strings to ssh's effective destination.
#[derive(Default)]
pub struct Resolver {
    keys: HashMap<String, String>,
}

impl Resolver {
    pub fn scan(&mut self) -> Result<BTreeMap<String, Wanted>, String> {
        let output = Command::new("/bin/ps")
            .args(["-axo", "pid=,ppid=,etime=,args="])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(|error| format!("failed to run ps: {error}"))?;
        if !output.status.success() {
            return Err(format!("ps failed with {}", output.status));
        }
        let mut wanted: BTreeMap<String, Wanted> = BTreeMap::new();
        for client in String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(parse_line)
        {
            let key = self.key(&client.target);
            let entry = wanted.entry(key).or_default();
            if entry.target.is_empty() {
                entry.target = client.target.clone();
            }
            entry.clients.push(client);
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

/// A herdr client started with `--remote <target>` (or `--remote=<target>`)
/// from one `ps -axo pid=,ppid=,etime=,args=` line.
pub fn parse_line(line: &str) -> Option<Client> {
    let mut words = line.split_whitespace();
    let pid = words.next()?.parse().ok()?;
    let ppid = words.next()?.parse().ok()?;
    let age = parse_elapsed(words.next()?)?;
    let program = words.next()?;
    if Path::new(program).file_name()? != "herdr" {
        return None;
    }
    let arguments: Vec<&str> = words.collect();
    let target = flag_value(&arguments, "--remote")?;
    Some(Client {
        pid,
        ppid,
        age,
        target,
        session: flag_value(&arguments, "--session"),
    })
}

/// The value of `--flag value` or `--flag=value`, when it is not another
/// flag.
fn flag_value(arguments: &[&str], flag: &str) -> Option<String> {
    let mut iterator = arguments.iter();
    while let Some(argument) = iterator.next() {
        let value = match argument.strip_prefix(flag) {
            Some("") => iterator.next().copied(),
            Some(rest) => rest.strip_prefix('='),
            None => continue,
        };
        return value
            .filter(|value| !value.is_empty() && !value.starts_with('-'))
            .map(str::to_string);
    }
    None
}

/// Seconds from `ps` elapsed time: `[[dd-]hh:]mm:ss`.
fn parse_elapsed(value: &str) -> Option<u64> {
    let (days, clock) = match value.split_once('-') {
        Some((days, clock)) => (days.parse::<u64>().ok()?, clock),
        None => (0, value),
    };
    let mut seconds = 0;
    for part in clock.split(':') {
        seconds = seconds * 60 + part.parse::<u64>().ok()?;
    }
    Some(days * 86_400 + seconds)
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

    fn client(pid: u32, ppid: u32, age: u64, session: Option<&str>) -> Client {
        Client {
            pid,
            ppid,
            age,
            target: "donut".into(),
            session: session.map(str::to_string),
        }
    }

    #[test]
    fn finds_remote_clients_with_their_session_and_age() {
        assert_eq!(
            parse_line(
                "47424 16508 01:02:03 herdr --remote aliou.freelancer@192.168.1.16 --remote-keybindings server"
            ),
            Some(Client {
                pid: 47424,
                ppid: 16508,
                age: 3723,
                target: "aliou.freelancer@192.168.1.16".into(),
                session: None,
            })
        );
        assert_eq!(
            parse_line(" 11609 11505 2-00:00:05 /etc/profiles/per-user/me/bin/herdr --remote=donut --session work"),
            Some(Client {
                pid: 11609,
                ppid: 11505,
                age: 2 * 86_400 + 5,
                target: "donut".into(),
                session: Some("work".into()),
            })
        );
        assert_eq!(
            parse_line("1 2 00:07 herdr --session=a --remote donut")
                .and_then(|client| client.session),
            Some("a".into())
        );
    }

    #[test]
    fn ignores_everything_else() {
        for line in [
            "2242 1 10:00 /etc/profiles/per-user/me/bin/herdr server",
            "2241 1 10:00 herdr",
            "47465 1 10:00 /etc/profiles/per-user/me/bin/herdr client",
            "8547 1 10:00 herdr session attach factorial",
            "9000 1 10:00 herdr remote-client-bridge",
            "9001 1 10:00 herdr --remote-keybindings server",
            "9002 1 10:00 herdr --remote",
            "9003 1 10:00 herdr --remote --remote-keybindings server",
            "9004 1 10:00 herdr-cast bridge-send --remote donut",
            "9005 1 10:00 ssh -C -T donut herdr --remote donut",
            "10916 1 10:00 node sbxctl.cjs herdr stellar-lee-adama",
            "not-a-pid 1 10:00 herdr --remote donut",
            "9006 1 bogus herdr --remote donut",
        ] {
            assert_eq!(parse_line(line), None, "matched {line:?}");
        }
    }

    #[test]
    fn prefers_the_newest_client_of_the_notifying_session() {
        let wanted = Wanted {
            target: "donut".into(),
            clients: vec![
                client(100, 90, 50, None),
                client(200, 190, 10, None),
                client(300, 290, 5, Some("work")),
            ],
        };
        assert_eq!(wanted.tab_pids(None), [200, 190, 100, 90, 300, 290]);
        assert_eq!(wanted.tab_pids(Some("work")), [300, 290, 200, 190, 100, 90]);
        assert_eq!(wanted.tab_pids(Some("gone")), [300, 290, 200, 190, 100, 90]);
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
