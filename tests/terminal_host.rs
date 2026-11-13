//! An interactive harness's terminal host, run as the real executable.
mod common;

use model_router::{
    adapters::{Invocation, OutputKind},
    process::{self, InteractiveSession},
};
use std::{
    fs,
    os::unix::net::UnixStream,
    sync::atomic::AtomicBool,
    thread,
    time::{Duration, Instant},
};

fn running(pid: i32) -> bool {
    let output = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&output.stdout);
    let stat = stat.trim();
    !stat.is_empty() && !stat.starts_with('Z')
}

/// Ends the group however the test ends.
struct EndGroup(Option<u32>);
impl Drop for EndGroup {
    fn drop(&mut self) {
        if let Some(pgid) = self.0 {
            process::end_group(pgid, Duration::from_millis(100), || {});
        }
    }
}

#[test]
fn a_host_that_dies_before_the_first_attach_reports_a_surviving_group() {
    let dir = common::tempdir();
    let attempt = dir.path().join("attempt");
    fs::create_dir(&attempt).unwrap();
    let member_file = dir.path().join("member.pid");
    // The harness starts a member that ignores SIGTERM and SIGHUP and carries
    // no marker, then kills its host, as a crash would.
    let inv = Invocation {
        program: "sh".into(),
        args: vec![
            "-c".into(),
            r#"sh -c 'trap "" TERM HUP; echo $$ > "$1"; exec env -u MODEL_ROUTER_GROUP sleep 30' member "$0" &
sleep 1; kill -KILL $PPID; exec sleep 30"#
                .into(),
            member_file.display().to_string(),
        ],
        stdin: None,
        kind: OutputKind::Text,
        final_file: attempt.join("unused"),
        env: vec![],
        files: vec![],
        assign: None,
    };
    // This test binary cannot act as the terminal host; the router can.
    process::host::use_program(env!("CARGO_BIN_EXE_model-router").into());
    let session = InteractiveSession::new(80, 24);
    let mut end = EndGroup(None);
    let error = process::interactive(
        &inv,
        dir.path(),
        &attempt,
        &AtomicBool::new(false),
        &session,
        None,
        &[],
        |group, socket| {
            end.0 = Some(group.pgid);
            // The router attaches only once the host is gone.
            let deadline = Instant::now() + Duration::from_secs(10);
            while UnixStream::connect(socket).is_ok() {
                assert!(Instant::now() < deadline, "the host never died");
                thread::sleep(Duration::from_millis(20));
            }
        },
    )
    .unwrap_err();
    let pgid = end.0.unwrap();
    let survivor = error
        .downcast_ref::<process::Survivor>()
        .unwrap_or_else(|| panic!("no survivor reported: {error:#}"));
    assert_eq!(survivor.group.pgid, pgid);
    assert!(
        format!("{error:#}").contains(&format!("Process group {pgid} may still be running")),
        "{error:#}"
    );
    let member: i32 = fs::read_to_string(&member_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(running(member), "the member was killed");
}
