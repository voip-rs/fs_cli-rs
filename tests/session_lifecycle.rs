#![cfg(unix)]
//! Drives the `fs_cli` binary against a mock switch: connect, api round
//! trip, auth failure, and what a mid-session disconnect does with and without
//! reconnect.

use freeswitch_esl_tokio::mock::{MockBackgroundJob, MockConnection, MockEslServer};
use freeswitch_esl_tokio::LogLevel;
use std::io::Read;
use std::net::SocketAddr;
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

const PASSWORD: &str = "ClueCon";
const WAIT_LIMIT: Duration = Duration::from_secs(10);

async fn switch_with_password(password: &str) -> MockEslServer {
    MockEslServer::bind("127.0.0.1:0", password)
        .await
        .expect("bind mock switch")
}

async fn switch() -> MockEslServer {
    switch_with_password(PASSWORD).await
}

async fn accept(server: &MockEslServer) -> MockConnection {
    tokio::time::timeout(WAIT_LIMIT, server.accept())
        .await
        .expect("fs_cli never connected")
        .expect("auth handshake")
}

async fn assert_no_connection(server: &MockEslServer) {
    assert!(
        tokio::time::timeout(Duration::from_millis(200), server.accept())
            .await
            .is_err(),
        "fs_cli must not have connected"
    );
}

/// The next command, without its terminating blank line.
async fn next(conn: &mut MockConnection) -> std::io::Result<String> {
    Ok(conn
        .read_command()
        .await?
        .trim_end()
        .to_string())
}

async fn expect_next(conn: &mut MockConnection, prefix: &str) -> String {
    let command = next(conn)
        .await
        .expect("read command");
    assert!(
        command.starts_with(prefix),
        "expected {:?}, got {:?}",
        prefix,
        command
    );
    command
}

fn background_job(job_uuid: &str, command: &str) -> MockBackgroundJob {
    match command.split_once(' ') {
        Some((name, arg)) => MockBackgroundJob::new(job_uuid, name).with_arg(arg),
        None => MockBackgroundJob::new(job_uuid, command),
    }
}

/// Answer as a healthy switch would: a job completes at once, and `log_line`
/// is pushed after the log level is set.
async fn answer(
    conn: &mut MockConnection,
    command: &str,
    log_line: Option<&str>,
) -> std::io::Result<()> {
    if let Some(api) = command.strip_prefix("api ") {
        conn.reply_api(&format!("fake reply to {}\n", api))
            .await
    } else if let Some(job) = command.strip_prefix("bgapi ") {
        let job_uuid = format!("job-{}", job);
        conn.reply_bgapi(&job_uuid)
            .await?;
        conn.send_background_job(
            &background_job(&job_uuid, job),
            &format!("+OK job did {}\n", job),
        )
        .await
    } else {
        conn.reply_ok()
            .await?;
        match log_line {
            Some(line) if command.starts_with("log ") => {
                conn.send_log(LogLevel::Notice, "fake.c", &format!("{}\n", line))
                    .await
            }
            _ => Ok(()),
        }
    }
}

/// Answer every command until the client hangs up; the commands, in order.
async fn serve(mut conn: MockConnection, log_line: Option<&str>) -> Vec<String> {
    let mut seen = Vec::new();
    loop {
        match next(&mut conn).await {
            Ok(command) => {
                answer(&mut conn, &command, log_line)
                    .await
                    .expect("answer command");
                seen.push(command);
            }
            // A killed client resets the socket if it left replies unread.
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
                ) =>
            {
                return seen
            }
            Err(e) => panic!("mock switch read failed: {}", e),
        }
    }
}

fn scratch_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// A config file of our own: without one the binary would fall back to the
/// developer's ~/.config/fs_cli.yaml and write to it.
fn write_config(dir: &Path, addr: SocketAddr) -> PathBuf {
    let path = dir.join("fs_cli.yaml");
    std::fs::write(
        &path,
        format!(
            "fs_cli:\n  default:\n    host: {}\n    port: {}\n    password: {}\n    timeout: 1000\n    retry: false\n",
            addr.ip(),
            addr.port(),
            PASSWORD
        ),
    )
    .expect("write test config");
    path
}

fn cli(dir: &Path, addr: SocketAddr) -> Command {
    cli_with_color(dir, addr, "never")
}

fn cli_with_color(dir: &Path, addr: SocketAddr, color: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_fs_cli"));
    command
        .arg("--config")
        .arg(write_config(dir, addr))
        .arg("--history-file")
        .arg(dir.join("history"))
        .args(["--color", color]);
    command
}

/// Run the binary on a blocking thread while the test plays the switch.
fn spawn_cli(mut command: Command) -> JoinHandle<Output> {
    tokio::task::spawn_blocking(move || {
        command
            .output()
            .expect("run fs_cli")
    })
}

fn spawn_batch(dir: &Path, addr: SocketAddr, args: &[&str]) -> JoinHandle<Output> {
    let mut command = cli(dir, addr);
    command.args(args);
    spawn_cli(command)
}

async fn finish(run: JoinHandle<Output>) -> Output {
    run.await
        .expect("join fs_cli")
}

/// Both ends of a pty: the child needs a terminal on stdin and stdout or
/// rustyline refuses to build its external printer and the session quits at
/// once.
struct Pty {
    master: OwnedFd,
    slave: OwnedFd,
}

fn open_pty() -> Pty {
    let mut master = 0;
    let mut slave = 0;
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(rc, 0, "openpty: {}", std::io::Error::last_os_error());
    unsafe {
        Pty {
            master: OwnedFd::from_raw_fd(master),
            slave: OwnedFd::from_raw_fd(slave),
        }
    }
}

/// Drain the master end, so a chatty child never blocks on a full pty buffer.
fn drain_pty(pty: &Pty) {
    let mut master = std::fs::File::from(
        pty.master
            .try_clone()
            .expect("clone pty master"),
    );
    std::thread::spawn(move || {
        let mut sink = [0u8; 4096];
        loop {
            match master.read(&mut sink) {
                Ok(0) => return,
                Ok(_) => {}
                // EIO is how a pty reports that the last slave fd is gone.
                Err(e) if e.raw_os_error() == Some(libc::EIO) => return,
                Err(e) => {
                    eprintln!("pty master read failed: {}", e);
                    return;
                }
            }
        }
    });
}

/// Spawn the interactive binary on a pty, with stderr on a pipe to read back.
fn spawn_interactive(mut command: Command, pty: &Pty) -> Child {
    drain_pty(pty);
    command
        .stdin(Stdio::from(
            pty.slave
                .try_clone()
                .expect("clone pty slave"),
        ))
        .stdout(Stdio::from(
            pty.slave
                .try_clone()
                .expect("clone pty slave"),
        ))
        .stderr(Stdio::piped());
    command
        .spawn()
        .expect("spawn fs_cli")
}

/// Read the child's stderr to EOF, which happens when it exits.
fn wait_with_stderr(mut child: Child) -> (std::process::ExitStatus, String) {
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("stderr pipe")
        .read_to_string(&mut stderr)
        .expect("read stderr");
    let status = child
        .wait()
        .expect("wait for fs_cli");
    (status, stderr)
}

/// The interactive startup is two commands: the event subscription and the
/// log level.
async fn answer_startup(conn: &mut MockConnection) -> Vec<String> {
    let mut seen = Vec::new();
    for _ in 0..2 {
        let command = next(conn)
            .await
            .expect("read startup command");
        answer(conn, &command, None)
            .await
            .expect("answer startup command");
        seen.push(command);
    }
    seen
}

#[tokio::test]
async fn api_command_round_trips() {
    let server = switch().await;
    let dir = scratch_dir("api-round-trip");

    let run = spawn_batch(&dir, server.addr(), &["-x", "status"]);
    let seen = serve(accept(&server).await, None).await;
    let output = finish(run).await;

    assert!(
        output
            .status
            .success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("fake reply to status"),
        "stdout did not carry the api body: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(seen, vec!["api status".to_string()]);
}

#[tokio::test]
async fn auth_failure_is_reported_as_such() {
    let server = switch_with_password("not-the-one").await;
    let dir = scratch_dir("auth-failure");

    let run = spawn_batch(&dir, server.addr(), &["-x", "status"]);
    let refused = server
        .accept()
        .await
        .expect_err("the handshake must fail");
    assert_eq!(refused.kind(), std::io::ErrorKind::PermissionDenied);
    let output = finish(run).await;

    assert_eq!(
        output
            .status
            .code(),
        Some(255),
        "a refused password means nothing was sent"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Authentication failed"),
        "stderr must name the auth failure: {:?}",
        stderr
    );
}

#[tokio::test]
async fn a_server_close_ends_the_session_when_reconnect_is_off() {
    let server = switch().await;
    let dir = scratch_dir("no-reconnect");

    let pty = open_pty();
    let mut command = cli(&dir, server.addr());
    command.args(["--reconnect", "false"]);
    let child = spawn_interactive(command, &pty);

    let mut conn = accept(&server).await;
    answer_startup(&mut conn).await;
    conn.drop_connection()
        .await;

    let (status, stderr) = tokio::task::spawn_blocking(move || wait_with_stderr(child))
        .await
        .expect("join fs_cli");

    assert!(!status.success(), "a lost connection must exit non-zero");
    assert!(
        stderr.contains("Connection to FreeSWITCH lost: connection closed"),
        "the EOF must be classified as a closed connection: {:?}",
        stderr
    );
    assert_no_connection(&server).await;
}

#[tokio::test]
async fn reconnect_reruns_the_subscriptions() {
    let server = switch().await;
    let dir = scratch_dir("reconnect");

    let pty = open_pty();
    let mut command = cli(&dir, server.addr());
    command.args(["--reconnect", "true"]);
    let mut child = spawn_interactive(command, &pty);

    let mut conn = accept(&server).await;
    let first = answer_startup(&mut conn).await;
    conn.drop_connection()
        .await;
    let mut conn = accept(&server).await;
    let second = answer_startup(&mut conn).await;

    assert!(
        second
            .iter()
            .any(|c| c.starts_with("event plain")),
        "a reconnect that skips the event subscription leaves the user with no \
         events and no liveness timer: {:?}",
        second
    );
    assert!(
        second
            .iter()
            .any(|c| c.starts_with("log ")),
        "a reconnect that skips the log command leaves the user with no logs: {:?}",
        second
    );
    assert_eq!(
        first, second,
        "the reconnected session must run the same startup as the first"
    );

    child
        .kill()
        .expect("kill fs_cli");
    child
        .wait()
        .expect("reap fs_cli");
}

#[tokio::test]
async fn no_terminal_and_no_commands_fails_loudly() {
    let server = switch().await;
    let dir = scratch_dir("no-terminal");

    let mut command = cli(&dir, server.addr());
    command.stdin(Stdio::null());
    let output = finish(spawn_cli(command)).await;

    assert!(
        !output
            .status
            .success(),
        "a session that cannot read commands must not exit 0"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("-x") && stderr.contains("--log-file"),
        "the refusal must name the non-interactive options: {:?}",
        stderr
    );
    assert_no_connection(&server).await;
}

#[tokio::test]
async fn a_background_job_result_is_printed_when_it_arrives() {
    let server = switch().await;
    let dir = scratch_dir("bgapi-result");

    let run = spawn_batch(&dir, server.addr(), &["-X", "version"]);
    let seen = serve(accept(&server).await, None).await;
    let output = finish(run).await;

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output
            .status
            .success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("[version] job did version"),
        "the job result must be printed under the command that asked for it: {:?}",
        stdout
    );
    assert!(
        seen.iter()
            .any(|c| c.starts_with("event plain")),
        "without a BACKGROUND_JOB subscription no result can arrive: {:?}",
        seen
    );
}

#[tokio::test]
async fn another_clients_job_result_is_ignored() {
    let server = switch().await;
    let dir = scratch_dir("bgapi-foreign");

    let run = spawn_batch(
        &dir,
        server.addr(),
        &["--job-timeout", "1500", "-X", "version"],
    );
    let mut conn = accept(&server).await;
    expect_next(&mut conn, "event plain").await;
    conn.reply_ok()
        .await
        .expect("answer subscription");
    expect_next(&mut conn, "bgapi version").await;
    conn.reply_bgapi("job-mine")
        .await
        .expect("accept job");
    conn.send_background_job(
        &MockBackgroundJob::new("another-clients-job", "version"),
        "+OK not yours\n",
    )
    .await
    .expect("push foreign job");
    serve(conn, None).await;
    let output = finish(run).await;

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("not yours"),
        "a job result this client never asked for must not be reported: {:?}",
        stdout
    );
    assert_eq!(
        output
            .status
            .code(),
        Some(254),
        "the job it did ask for never completed, so its outcome is unknown"
    );
}

#[tokio::test]
async fn an_expired_job_timeout_names_the_outstanding_job() {
    let server = switch().await;
    let dir = scratch_dir("bgapi-timeout");

    let run = spawn_batch(
        &dir,
        server.addr(),
        &["--job-timeout", "300", "-X", "version"],
    );
    let mut conn = accept(&server).await;
    expect_next(&mut conn, "event plain").await;
    conn.reply_ok()
        .await
        .expect("answer subscription");
    expect_next(&mut conn, "bgapi version").await;
    conn.reply_bgapi("job-silent")
        .await
        .expect("accept job");
    serve(conn, None).await;
    let output = finish(run).await;

    assert_eq!(
        output
            .status
            .code(),
        Some(254),
        "a submitted job that never reported has an unknown outcome"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("job-silent") && stderr.contains("version"),
        "the failure must name the job that never reported: {:?}",
        stderr
    );
}

#[tokio::test]
async fn an_unreachable_switch_exits_255() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a port to free");
    let addr = listener
        .local_addr()
        .expect("freed port addr");
    drop(listener);
    let dir = scratch_dir("unreachable");

    let output = finish(spawn_batch(&dir, addr, &["-x", "status"])).await;

    assert_eq!(
        output
            .status
            .code(),
        Some(255),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn an_unanswered_command_exits_254() {
    let server = switch().await;
    let dir = scratch_dir("api-silent");

    let run = spawn_batch(&dir, server.addr(), &["-T", "300", "-x", "reloadxml"]);
    let mut conn = accept(&server).await;
    expect_next(&mut conn, "api reloadxml").await;
    let output = finish(run).await;
    drop(conn);

    assert_eq!(
        output
            .status
            .code(),
        Some(254),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn a_command_spanning_lines_is_refused_before_connecting() {
    let server = switch().await;
    let dir = scratch_dir("multiline");

    let output = finish(spawn_batch(&dir, server.addr(), &["-x", "status\nexit"])).await;

    assert_eq!(
        output
            .status
            .code(),
        Some(2)
    );
    assert_no_connection(&server).await;
}

#[tokio::test]
async fn a_log_file_captures_the_log_stream_without_escapes() {
    let server = switch().await;
    let dir = scratch_dir("log-file");
    let log_path = dir.join("captured.log");

    let mut command = cli_with_color(&dir, server.addr(), "line");
    command
        .arg("--log-file")
        .arg(&log_path)
        // The job result arrives after the log event, so waiting for it
        // pins the capture without a sleep.
        .args(["-x", "status", "-X", "version"]);
    let run = spawn_cli(command);
    serve(
        accept(&server).await,
        Some("2026-01-01 [NOTICE] fake.c:1 log line for the file"),
    )
    .await;
    let output = finish(run).await;

    assert!(
        output
            .status
            .success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let captured = std::fs::read_to_string(&log_path).expect("read the captured log");
    assert!(
        captured.contains("log line for the file"),
        "the pushed log event must reach the file: {:?}",
        captured
    );
    assert!(
        !captured.contains('\u{1b}'),
        "a file destination is never coloured, even with --color line: {:?}",
        captured
    );
}

#[tokio::test]
async fn an_interactive_session_tees_the_log_stream_to_the_file() {
    let server = switch().await;
    let dir = scratch_dir("log-file-tee");
    let log_path = dir.join("teed.log");

    let pty = open_pty();
    let mut command = cli(&dir, server.addr());
    command
        .arg("--log-file")
        .arg(&log_path);
    let mut child = spawn_interactive(command, &pty);
    let conn = accept(&server).await;
    let switch_side = tokio::spawn(serve(
        conn,
        Some("2026-01-01 [NOTICE] fake.c:1 teed to the file"),
    ));

    let deadline = tokio::time::Instant::now() + WAIT_LIMIT;
    let captured = loop {
        let captured = std::fs::read_to_string(&log_path).unwrap_or_default();
        if captured.contains("teed to the file") || tokio::time::Instant::now() >= deadline {
            break captured;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    child
        .kill()
        .expect("kill fs_cli");
    child
        .wait()
        .expect("reap fs_cli");
    switch_side
        .await
        .expect("join mock switch");

    assert!(
        captured.contains("teed to the file"),
        "an interactive session must write log lines to the file too: {:?}",
        captured
    );
}

#[tokio::test]
async fn batch_commands_reach_the_wire_in_the_typed_order() {
    let server = switch().await;
    let dir = scratch_dir("batch-order");

    let run = spawn_batch(
        &dir,
        server.addr(),
        &["-x", "one", "-X", "two", "-x", "three"],
    );
    let seen = serve(accept(&server).await, None).await;
    let output = finish(run).await;

    assert!(
        output
            .status
            .success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let issued: Vec<String> = seen
        .into_iter()
        .filter(|c| c.starts_with("api ") || c.starts_with("bgapi "))
        .collect();
    assert_eq!(
        issued,
        vec![
            "api one".to_string(),
            "bgapi two".to_string(),
            "api three".to_string()
        ]
    );
}

#[tokio::test]
async fn a_refused_command_exits_zero_by_default() {
    let server = switch().await;
    let dir = scratch_dir("refused-default");

    let run = spawn_batch(&dir, server.addr(), &["-x", "bogus", "-x", "status"]);
    let mut conn = accept(&server).await;
    expect_next(&mut conn, "api bogus").await;
    conn.reply_api("-ERR bogus Command not found!\n")
        .await
        .expect("refuse command");
    let rest = serve(conn, None).await;
    let output = finish(run).await;

    assert_eq!(
        output
            .status
            .code(),
        Some(0)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("API Error: bogus Command not found!"));
    assert_eq!(rest, vec!["api status".to_string()]);
}

#[tokio::test]
async fn fail_on_error_stops_at_a_refusal_but_awaits_submitted_jobs() {
    let server = switch().await;
    let dir = scratch_dir("refused-fail");

    let run = spawn_batch(
        &dir,
        server.addr(),
        &[
            "--fail-on-error",
            "-X",
            "version",
            "-x",
            "bogus",
            "-x",
            "status",
        ],
    );
    let mut conn = accept(&server).await;
    expect_next(&mut conn, "event plain").await;
    conn.reply_ok()
        .await
        .expect("answer subscription");
    expect_next(&mut conn, "bgapi version").await;
    conn.reply_bgapi("job-late")
        .await
        .expect("accept job");
    expect_next(&mut conn, "api bogus").await;
    conn.reply_api("-USAGE: bogus <arg>\n")
        .await
        .expect("refuse command");
    conn.send_background_job(&background_job("job-late", "version"), "+OK late\n")
        .await
        .expect("complete job");
    let rest = serve(conn, None).await;
    let output = finish(run).await;

    assert_eq!(
        output
            .status
            .code(),
        Some(3),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        rest.is_empty(),
        "nothing is sent after the refusal: {:?}",
        rest
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("Usage: bogus <arg>"));
    assert!(String::from_utf8_lossy(&output.stdout).contains("[version] late"));
}

#[tokio::test]
async fn the_profile_key_alone_enables_fail_on_error() {
    let server = switch().await;
    let dir = scratch_dir("refused-profile");

    let mut command = cli(&dir, server.addr());
    let mut config = std::fs::OpenOptions::new()
        .append(true)
        .open(dir.join("fs_cli.yaml"))
        .expect("open test config");
    std::io::Write::write_all(&mut config, b"    fail_on_error: true\n").expect("append key");
    command.args(["-x", "bogus"]);
    let run = spawn_cli(command);
    let mut conn = accept(&server).await;
    expect_next(&mut conn, "api bogus").await;
    conn.reply_api("-ERR no\n")
        .await
        .expect("refuse command");
    serve(conn, None).await;
    let output = finish(run).await;

    assert_eq!(
        output
            .status
            .code(),
        Some(3)
    );
}

#[tokio::test]
async fn a_refused_job_counts_and_names_the_job() {
    let server = switch().await;
    let dir = scratch_dir("refused-job");

    let run = spawn_batch(&dir, server.addr(), &["--fail-on-error", "-X", "version"]);
    let mut conn = accept(&server).await;
    expect_next(&mut conn, "event plain").await;
    conn.reply_ok()
        .await
        .expect("answer subscription");
    expect_next(&mut conn, "bgapi version").await;
    conn.reply_bgapi("job-refused")
        .await
        .expect("accept job");
    conn.send_background_job(&background_job("job-refused", "version"), "-ERR no way\n")
        .await
        .expect("refuse job");
    serve(conn, None).await;
    let output = finish(run).await;

    assert_eq!(
        output
            .status
            .code(),
        Some(3)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("API Error: [version] no way"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
