//! Telling a recorded process group from a later one that reuses its ID.
//!
//! A run records each harness's process group ID so that a later `mr` can
//! stop the group if the router died. The ID alone is not enough: after a
//! reboot, or long enough after the crash, it can name an unrelated group of
//! the user's. So each record also keeps the leader's start time and a random
//! marker that every process in the group inherits in its environment, and
//! recovery signals a group only when one of them still matches.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// The environment variable that carries a group's marker to every process
/// the harness starts.
pub const MARKER_ENV: &str = "MODEL_ROUTER_GROUP";

/// What identifies a process group the router started, besides its ID.
/// Empty in runs saved before groups were identified.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    /// When the leader started (see `started`); None when it could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<String>,
    /// The `MARKER_ENV` value the leader was launched with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker: Option<String>,
}

impl Identity {
    pub fn is_empty(&self) -> bool {
        self.started.is_none() && self.marker.is_none()
    }
}

/// A fresh random marker for `MARKER_ENV`.
pub fn new_marker() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// A process group ID and what identifies its group. Saved as a bare number
/// when nothing does, as runs saved before groups were identified are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "Stored", into = "Stored")]
pub struct Group {
    pub pgid: u32,
    pub identity: Identity,
}

impl Group {
    /// Group `pgid`, whose leader is running now and was launched with
    /// `marker`. Call it before the leader can have been reaped.
    pub fn launched(pgid: u32, marker: Option<String>) -> Self {
        Self {
            pgid,
            identity: Identity {
                started: started(pgid),
                marker,
            },
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum Stored {
    Bare(u32),
    Identified {
        pgid: u32,
        #[serde(flatten)]
        identity: Identity,
    },
}
impl From<Stored> for Group {
    fn from(stored: Stored) -> Self {
        match stored {
            Stored::Bare(pgid) => Self {
                pgid,
                identity: Identity::default(),
            },
            Stored::Identified { pgid, identity } => Self { pgid, identity },
        }
    }
}
impl From<Group> for Stored {
    fn from(group: Group) -> Self {
        if group.identity.is_empty() {
            Self::Bare(group.pgid)
        } else {
            Self::Identified {
                pgid: group.pgid,
                identity: group.identity,
            }
        }
    }
}

/// Whether a recorded group still is the one the router started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// No process is left in a group with this ID.
    Gone,
    /// The recorded group: its leader is the recorded process, or a process
    /// in it carries the recorded marker.
    Ours,
    /// A different process now leads a group with this ID.
    Reused,
    /// A group with this ID is alive, but nothing shows it is the recorded
    /// one; the reason says why.
    Unverified(&'static str),
}

/// Checks a recorded group against what runs under its ID now. The ID of a
/// live group is never reused, so a leader with the recorded start time, or
/// any process carrying the recorded marker, proves the group is the same.
pub fn verify(group: &Group) -> Verdict {
    if !super::group_alive(group.pgid) {
        return Verdict::Gone;
    }
    let identity = &group.identity;
    let leader = started(group.pgid);
    if let (Some(now), Some(recorded)) = (&leader, &identity.started) {
        return if now == recorded {
            Verdict::Ours
        } else {
            Verdict::Reused
        };
    }
    if let Some(marker) = &identity.marker {
        let needle = format!("{MARKER_ENV}={marker}");
        if members(group.pgid)
            .into_iter()
            .any(|pid| carries(pid, needle.as_bytes()))
        {
            return Verdict::Ours;
        }
    }
    Verdict::Unverified(if identity.is_empty() {
        "the run recorded only its ID, as runs saved by earlier versions do"
    } else if leader.is_none() {
        "its leader has exited and no process left in it carries the run's marker"
    } else {
        "its leader's start time could not be compared and it carries no run marker"
    })
}

/// Ends `group` as `end_group` does, but only if `verify` finds it is the
/// recorded one. Each signal, SIGTERM and SIGKILL, is sent only if `verify`
/// still finds it so just before: the group may end during the grace period
/// and its ID pass to an unrelated group. Checking against the recorded
/// identity, not what ran at the first check, means a leaderless group needs
/// a member carrying the marker every time.
///
/// Returns the verdict once it is done, not the first one: `Gone` when
/// nothing is left; `Unverified` when a member survived that no longer
/// proves the group is the recorded one (its leader exited on SIGTERM, say,
/// and the member carries no marker), so SIGKILL was skipped; `Ours` when the
/// group outlived SIGKILL; `Reused` when another group has the ID. Only a
/// group first found `Ours` was signalled.
///
/// A narrow race remains between each check and its `kill`: the group would
/// have to end and its ID be taken by a new group within microseconds.
/// Closing it needs a handle on the group itself; Linux's pidfd names one
/// process, signals a group through it only from 6.9, and not at all once
/// the leader has exited, and macOS has nothing like it.
pub fn end_recorded(group: &Group, grace: Duration) -> Verdict {
    let verdict = verify(group);
    if verdict != Verdict::Ours {
        return verdict;
    }
    super::end_group_if(group.pgid, grace, || {}, || verify(group) == Verdict::Ours);
    // Killed members linger until their parent, often init, reaps them.
    let deadline = std::time::Instant::now() + SETTLE;
    while super::group_alive(group.pgid) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    verify(group)
}

/// How long `end_recorded` waits for signalled members to be reaped.
const SETTLE: Duration = Duration::from_secs(1);

/// When process `pid` started, in a form that differs between any two
/// processes that ever had this ID: the boot ID and start time in clock
/// ticks since boot on Linux, the start time in microseconds on macOS. None
/// when there is no such process or it cannot be read. A zombie still has
/// its start time.
#[cfg(target_os = "linux")]
pub fn started(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Field 22; the name in parentheses (field 2) may contain spaces.
    let ticks = stat.rsplit_once(')')?.1.split_whitespace().nth(19)?;
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let boot = boot.trim();
    (!boot.is_empty()).then(|| format!("{boot}:{ticks}"))
}

#[cfg(target_os = "macos")]
pub fn started(pid: u32) -> Option<String> {
    let id = libc::c_int::try_from(pid).ok()?;
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    // SAFETY: proc_bsdinfo holds only integers, for which zero is valid.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    // SAFETY: the buffer is one proc_bsdinfo, and its size is passed.
    let written = unsafe {
        libc::proc_pidinfo(
            id,
            libc::PROC_PIDTBSDINFO,
            0,
            (&raw mut info).cast(),
            libc::c_int::try_from(size).ok()?,
        )
    };
    (usize::try_from(written).ok() == Some(size) && info.pbi_pid == pid)
        .then(|| format!("{}.{:06}", info.pbi_start_tvsec, info.pbi_start_tvusec))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn started(_pid: u32) -> Option<String> {
    None
}

/// The processes in group `pgid`.
#[cfg(target_os = "linux")]
fn members(pgid: u32) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return vec![];
    };
    entries
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| {
                    // Field 5.
                    let group = stat.rsplit_once(')')?.1.split_whitespace().nth(2)?;
                    group.parse::<u32>().ok()
                })
                == Some(pgid)
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn members(pgid: u32) -> Vec<u32> {
    let Ok(group) = libc::pid_t::try_from(pgid) else {
        return vec![];
    };
    let mut capacity = 256;
    while capacity <= 1 << 20 {
        let mut pids: Vec<libc::pid_t> = vec![0; capacity];
        let Ok(bytes) = libc::c_int::try_from(capacity * std::mem::size_of::<libc::pid_t>()) else {
            return vec![];
        };
        // SAFETY: the buffer holds `capacity` pids, and its size is passed.
        let count = unsafe { libc::proc_listpgrppids(group, pids.as_mut_ptr().cast(), bytes) };
        let Ok(count) = usize::try_from(count) else {
            return vec![];
        };
        if count < capacity {
            pids.truncate(count);
            return pids
                .into_iter()
                .filter_map(|pid| u32::try_from(pid).ok())
                .filter(|&pid| pid != 0)
                .collect();
        }
        capacity *= 4;
    }
    vec![]
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn members(_pgid: u32) -> Vec<u32> {
    vec![]
}

/// Whether process `pid`'s environment (or, on macOS, its arguments or
/// environment) contains `needle`. False when it cannot be read, as for a
/// zombie or another user's process. macOS also hides the environment of
/// its own system binaries (`/bin/sleep`, for one).
#[cfg(target_os = "linux")]
fn carries(pid: u32, needle: &[u8]) -> bool {
    std::fs::read(format!("/proc/{pid}/environ"))
        .is_ok_and(|environ| environ.windows(needle.len()).any(|w| w == needle))
}

#[cfg(target_os = "macos")]
fn carries(pid: u32, needle: &[u8]) -> bool {
    let Ok(id) = libc::c_int::try_from(pid) else {
        return false;
    };
    let mut max: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
    // SAFETY: the output is one c_int, and its size is passed.
    let read = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            2,
            (&raw mut max).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    let Ok(max) = usize::try_from(max) else {
        return false;
    };
    if read != 0 || max == 0 {
        return false;
    }
    let mut buffer = vec![0u8; max];
    let mut size = max;
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, id];
    // SAFETY: the output buffer is `size` bytes long.
    let read = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buffer.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    read == 0
        && buffer[..size.min(max)]
            .windows(needle.len())
            .any(|w| w == needle)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn carries(_pid: u32, _needle: &[u8]) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command};

    fn spawn(marker: Option<&str>, script: &str) -> Child {
        let mut command = Command::new("sh");
        command.args(["-c", script]).process_group(0);
        if let Some(marker) = marker {
            command.env(MARKER_ENV, marker);
        }
        command.spawn().unwrap()
    }

    #[test]
    fn a_live_leader_is_identified_by_its_start_time() {
        let mut child = spawn(None, "sleep 30");
        let group = Group::launched(child.id(), None);
        assert!(group.identity.started.is_some(), "start time unreadable");
        assert_eq!(started(child.id()), group.identity.started);
        assert_eq!(verify(&group), Verdict::Ours);
        // Saved and loaded again, it still names the same process.
        let saved = serde_json::to_string(&group).unwrap();
        let loaded: Group = serde_json::from_str(&saved).unwrap();
        assert_eq!(loaded, group);
        assert_eq!(verify(&loaded), Verdict::Ours);
        // Its zombie is still in the group until this test reaps it.
        let ended = std::thread::spawn(move || child.wait());
        assert_eq!(end_recorded(&loaded, Duration::from_secs(1)), Verdict::Gone);
        assert!(ended.join().unwrap().is_ok());
        assert_eq!(verify(&loaded), Verdict::Gone);
    }

    #[test]
    fn a_different_leader_under_the_recorded_id_is_never_signalled() {
        let mut child = spawn(None, "sleep 30");
        let mut group = Group::launched(child.id(), None);
        group.identity.started = Some("another process".into());
        assert_eq!(
            end_recorded(&group, Duration::from_secs(1)),
            Verdict::Reused
        );
        assert!(
            child.try_wait().unwrap().is_none(),
            "the group was signalled"
        );
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn an_id_saved_without_identity_is_never_signalled() {
        let mut child = spawn(None, "sleep 30");
        let group: Group = serde_json::from_str(&child.id().to_string()).unwrap();
        assert!(group.identity.is_empty());
        assert_eq!(
            serde_json::to_string(&group).unwrap(),
            child.id().to_string()
        );
        assert!(matches!(
            end_recorded(&group, Duration::from_secs(1)),
            Verdict::Unverified(_)
        ));
        assert!(
            child.try_wait().unwrap().is_none(),
            "the group was signalled"
        );
        child.kill().unwrap();
        child.wait().unwrap();
    }

    /// Not a test: what `leave_member` leaves running. This test binary,
    /// unlike system binaries, shows its environment to `carries` on macOS.
    #[test]
    #[ignore]
    fn linger() {
        if std::env::var_os("MODEL_ROUTER_TEST_LINGER").is_some() {
            std::thread::sleep(Duration::from_secs(30));
        }
    }

    /// The leader exits, leaving a background child in its group.
    fn leave_member(marker: Option<&str>) -> (u32, libc::pid_t) {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pid");
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(r#""$0" --exact process::group::tests::linger --ignored >/dev/null 2>&1 & echo $! > "$1""#)
            .arg(std::env::current_exe().unwrap())
            .arg(&file)
            .env("MODEL_ROUTER_TEST_LINGER", "1")
            .process_group(0);
        if let Some(marker) = marker {
            command.env(MARKER_ENV, marker);
        }
        let mut leader = command.spawn().unwrap();
        let pgid = leader.id();
        leader.wait().unwrap();
        let pid = std::fs::read_to_string(&file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(super::super::group_alive(pgid));
        (pgid, pid)
    }

    fn running(pid: libc::pid_t) -> bool {
        // SAFETY: signal 0 only checks existence and permission.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[test]
    fn a_member_carrying_the_marker_proves_a_leaderless_group() {
        let marker = new_marker();
        let (pgid, _) = leave_member(Some(&marker));
        let group = Group {
            pgid,
            identity: Identity {
                started: Some("the exited leader".into()),
                marker: Some(marker),
            },
        };
        assert_eq!(end_recorded(&group, Duration::from_secs(1)), Verdict::Gone);
        // Reaped by init, so it may linger briefly.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while super::super::group_alive(pgid) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!super::super::group_alive(pgid), "member survived");
    }

    #[test]
    fn a_group_whose_leader_exits_during_the_grace_needs_the_marker_for_sigkill() {
        // The leader exits on SIGTERM; a member ignores it and, like a group
        // that took the ID after the original ended, carries no marker.
        let marker = new_marker();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pid");
        let mut leader = Command::new("sh")
            .arg("-c")
            .arg(
                r#"trap 'exit 0' TERM
sh -c 'trap "" TERM; echo $$ > "$1"; exec env -u MODEL_ROUTER_GROUP sleep 30' member "$0" &
wait"#,
            )
            .arg(&file)
            .env(MARKER_ENV, &marker)
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = leader.id();
        let group = Group::launched(pgid, Some(marker));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let pid: libc::pid_t = loop {
            if let Some(pid) = std::fs::read_to_string(&file)
                .ok()
                .and_then(|text| text.trim().parse().ok())
            {
                break pid;
            }
            assert!(std::time::Instant::now() < deadline, "member never started");
            std::thread::sleep(Duration::from_millis(10));
        };
        // Reaped as soon as it exits, as init reaps an orphaned leader.
        let reaper = std::thread::spawn(move || leader.wait());
        // Left running, and said so: the caller must not count it stopped.
        assert!(matches!(
            end_recorded(&group, Duration::from_millis(500)),
            Verdict::Unverified(_)
        ));
        reaper.join().unwrap().unwrap();
        // A SIGKILL takes effect, and init reaps the member, shortly after.
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while std::time::Instant::now() < deadline {
            assert!(running(pid), "SIGKILL reached a group without the marker");
            std::thread::sleep(Duration::from_millis(20));
        }
        // SAFETY: kill takes no pointers; this is the test's own descendant.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }

    #[test]
    fn a_leaderless_group_without_the_marker_is_left_alone() {
        let (pgid, pid) = leave_member(Some("someone else's"));
        let group = Group {
            pgid,
            identity: Identity {
                started: Some("the exited leader".into()),
                marker: Some(new_marker()),
            },
        };
        assert!(matches!(
            end_recorded(&group, Duration::from_secs(1)),
            Verdict::Unverified(_)
        ));
        assert!(running(pid), "the group was signalled");
        // SAFETY: kill takes no pointers; this is the test's own child.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
}
