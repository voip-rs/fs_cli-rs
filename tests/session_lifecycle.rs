#![cfg(unix)]
//! Drives the `fs_cli` binary against a fake ESL server: connect, api round
//! trip, auth failure, and what a mid-session disconnect does with and without
//! reconnect.

use std::io::Read;
use std::net::SocketAddr;
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

const PASSWORD: &str = "ClueCon";
const WAIT_LIMIT: Duration = Duration::from_secs(10);

/// What the fake server does with the BACKGROUND_JOB event a `bgapi` owes.
#[derive(Clone, Copy, PartialEq)]
enum JobReply {
    /// Push the result under the Job-UUID it just handed out.
    Matching,
    /// Push a result for another client's job, as the global bus does.
    Foreign,
    /// Hand out a Job-UUID and never report.
    Silent,
}

/// What the fake server answers an `api` command with.
#[derive(Clone)]
enum ApiReply {
    /// `fake reply to <command>`.
    Echo,
    /// Read the command and never answer.
    Silent,
}

impl ApiReply {
    fn body(&self, command: &str) -> Option<String> {
        match self {
            Self::Echo => Some(format!("fake reply to {}\n", command)),
            Self::Silent => None,
        }
    }
}

/// How the fake server treats each connection it accepts.
#[derive(Clone)]
struct Script {
    password: String,
    /// Close the socket once this many post-auth commands have arrived.
    close_after_commands: Option<usize>,
    api_reply: ApiReply,
    job_reply: JobReply,
    /// Pushed as a log/data event once the client sends `log <level>`.
    log_line: Option<String>,
}

impl Default for Script {
    fn default() -> Self {
        Self {
            password: PASSWORD.to_string(),
            close_after_commands: None,
            api_reply: ApiReply::Echo,
            job_reply: JobReply::Matching,
            log_line: None,
        }
    }
}

/// Commands seen on each accepted connection, in accept order.
type Seen = Arc<Mutex<Vec<Vec<String>>>>;

struct FakeEsl {
    addr: SocketAddr,
    seen: Seen,
    accept_task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeEsl {
    fn drop(&mut self) {
        self.accept_task
            .abort();
    }
}

impl FakeEsl {
    async fn start(script: Script) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake ESL listener");
        let addr = listener
            .local_addr()
            .expect("fake ESL local addr");
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let accept_task = tokio::spawn(accept_loop(listener, script, seen.clone()));
        Self {
            addr,
            seen,
            accept_task,
        }
    }

    fn connection_count(&self) -> usize {
        self.seen
            .lock()
            .expect("seen lock")
            .len()
    }

    fn commands(&self, connection: usize) -> Vec<String> {
        self.seen
            .lock()
            .expect("seen lock")
            .get(connection)
            .cloned()
            .unwrap_or_default()
    }

    /// Poll until `pred` holds, so a test never depends on a fixed sleep.
    async fn wait_until(&self, what: &str, pred: impl Fn(&FakeEsl) -> bool) {
        let deadline = tokio::time::Instant::now() + WAIT_LIMIT;
        while tokio::time::Instant::now() < deadline {
            if pred(self) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("timed out waiting for {}", what);
    }
}

async fn accept_loop(listener: TcpListener, script: Script, seen: Seen) {
    loop {
        match listener
            .accept()
            .await
        {
            Ok((socket, _)) => {
                let index = {
                    let mut seen = seen
                        .lock()
                        .expect("seen lock");
                    seen.push(Vec::new());
                    seen.len() - 1
                };
                let script = script.clone();
                let seen = seen.clone();
                tokio::spawn(async move {
                    if let Err(e) = serve(socket, script, seen, index).await {
                        eprintln!("fake ESL connection {} ended: {}", index, e);
                    }
                });
            }
            Err(e) => {
                eprintln!("fake ESL accept failed: {}", e);
                return;
            }
        }
    }
}

async fn serve(socket: TcpStream, script: Script, seen: Seen, index: usize) -> std::io::Result<()> {
    let (reader, mut writer) = socket.into_split();
    let mut lines = BufReader::new(reader).lines();

    writer
        .write_all(b"Content-Type: auth/request\n\n")
        .await?;

    let auth = next_command(&mut lines).await?;
    let expected = format!("auth {}", script.password);
    if auth != expected {
        writer
            .write_all(b"Content-Type: command/reply\nReply-Text: -ERR invalid\n\n")
            .await?;
        return Ok(());
    }
    writer
        .write_all(b"Content-Type: command/reply\nReply-Text: +OK accepted\n\n")
        .await?;

    let mut count = 0usize;
    loop {
        let command = next_command(&mut lines).await?;
        count += 1;
        seen.lock()
            .expect("seen lock")[index]
            .push(command.clone());

        if let Some(word) = command.strip_prefix("api ") {
            if let Some(body) = script
                .api_reply
                .body(word)
            {
                writer
                    .write_all(
                        format!(
                            "Content-Type: api/response\nContent-Length: {}\n\n{}",
                            body.len(),
                            body
                        )
                        .as_bytes(),
                    )
                    .await?;
            }
        } else if let Some(word) = command.strip_prefix("bgapi ") {
            let job_uuid = format!("job-{}-{}", index, count);
            writer
                .write_all(
                    format!(
                        "Content-Type: command/reply\nReply-Text: +OK Job-UUID: {}\nJob-UUID: {}\n\n",
                        job_uuid, job_uuid
                    )
                    .as_bytes(),
                )
                .await?;
            match script.job_reply {
                JobReply::Matching => {
                    write_job_event(&mut writer, &job_uuid, &format!("+OK job did {}\n", word))
                        .await?
                }
                JobReply::Foreign => {
                    write_job_event(&mut writer, "another-clients-job", "+OK not yours\n").await?
                }
                JobReply::Silent => {}
            }
        } else if command.starts_with("log ") {
            writer
                .write_all(b"Content-Type: command/reply\nReply-Text: +OK log level\n\n")
                .await?;
            if let Some(line) = &script.log_line {
                write_log_event(&mut writer, line).await?;
            }
        } else {
            writer
                .write_all(b"Content-Type: command/reply\nReply-Text: +OK\n\n")
                .await?;
        }

        if command == "exit" || script.close_after_commands == Some(count) {
            return Ok(());
        }
    }
}

/// A BACKGROUND_JOB event: envelope, then event headers, then the result body.
async fn write_job_event<W>(writer: &mut W, job_uuid: &str, result: &str) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let body = format!(
        "Event-Name: BACKGROUND_JOB\nJob-UUID: {}\nContent-Length: {}\n\n{}",
        job_uuid,
        result.len(),
        result
    );
    writer
        .write_all(
            format!(
                "Content-Length: {}\nContent-Type: text/event-plain\n\n{}",
                body.len(),
                body
            )
            .as_bytes(),
        )
        .await
}

/// log/data is single-level framing: metadata in the envelope, text as body.
async fn write_log_event<W>(writer: &mut W, line: &str) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let text = format!("{}\n", line);
    writer
        .write_all(
            format!(
                "Content-Type: log/data\nContent-Length: {}\nLog-Level: 6\nLog-File: fake.c\n\n{}",
                text.len(),
                text
            )
            .as_bytes(),
        )
        .await
}

/// ESL commands are newline-terminated and separated by a blank line.
async fn next_command<R>(lines: &mut tokio::io::Lines<BufReader<R>>) -> std::io::Result<String>
where
    R: tokio::io::AsyncRead + Unpin,
{
    loop {
        match lines
            .next_line()
            .await?
        {
            Some(line)
                if line
                    .trim()
                    .is_empty() =>
            {
                continue
            }
            Some(line) => return Ok(line),
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "client closed the connection",
                ))
            }
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

#[tokio::test]
async fn api_command_round_trips() {
    let server = FakeEsl::start(Script::default()).await;
    let dir = scratch_dir("api-round-trip");

    let output = tokio::task::spawn_blocking({
        let mut command = cli(&dir, server.addr);
        command.args(["-x", "status"]);
        move || {
            command
                .output()
                .expect("run fs_cli -x")
        }
    })
    .await
    .expect("join fs_cli");

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
    assert_eq!(server.connection_count(), 1);
    assert_eq!(server.commands(0), vec!["api status".to_string()]);
}

#[tokio::test]
async fn auth_failure_is_reported_as_such() {
    let server = FakeEsl::start(Script {
        password: "not-the-one".to_string(),
        ..Script::default()
    })
    .await;
    let dir = scratch_dir("auth-failure");

    let output = tokio::task::spawn_blocking({
        let mut command = cli(&dir, server.addr);
        command.args(["-x", "status"]);
        move || {
            command
                .output()
                .expect("run fs_cli -x")
        }
    })
    .await
    .expect("join fs_cli");

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
    // Two commands is the whole startup: the event subscription and the log
    // level, after which the server hangs up.
    let server = FakeEsl::start(Script {
        close_after_commands: Some(2),
        ..Script::default()
    })
    .await;
    let dir = scratch_dir("no-reconnect");

    let pty = open_pty();
    let mut command = cli(&dir, server.addr);
    command.args(["--reconnect", "false"]);
    let child = spawn_interactive(command, &pty);

    let (status, stderr) = tokio::task::spawn_blocking(move || wait_with_stderr(child))
        .await
        .expect("join fs_cli");

    assert!(!status.success(), "a lost connection must exit non-zero");
    assert!(
        stderr.contains("Connection to FreeSWITCH lost: connection closed"),
        "the EOF must be classified as a closed connection: {:?}",
        stderr
    );
    assert_eq!(server.connection_count(), 1, "no reconnect was asked for");
}

#[tokio::test]
async fn reconnect_reruns_the_subscriptions() {
    let server = FakeEsl::start(Script {
        close_after_commands: Some(2),
        ..Script::default()
    })
    .await;
    let dir = scratch_dir("reconnect");

    let pty = open_pty();
    let mut command = cli(&dir, server.addr);
    command.args(["--reconnect", "true"]);
    let mut child = spawn_interactive(command, &pty);

    server
        .wait_until("the client to come back", |s| s.connection_count() >= 2)
        .await;
    server
        .wait_until("the second connection to subscribe", |s| {
            s.commands(1)
                .len()
                >= 2
        })
        .await;

    let second = server.commands(1);
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
        server.commands(0),
        second,
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
    let server = FakeEsl::start(Script::default()).await;
    let dir = scratch_dir("no-terminal");

    let output = tokio::task::spawn_blocking({
        let mut command = cli(&dir, server.addr);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        move || {
            command
                .spawn()
                .expect("spawn fs_cli")
                .wait_with_output()
                .expect("wait for fs_cli")
        }
    })
    .await
    .expect("join fs_cli");

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
    assert_eq!(
        server.connection_count(),
        0,
        "the mode check must run before connecting"
    );
}

/// Run the binary to completion with the given extra arguments.
async fn run_cli(dir: PathBuf, addr: SocketAddr, args: Vec<String>) -> std::process::Output {
    tokio::task::spawn_blocking(move || {
        let mut command = cli(&dir, addr);
        command.args(args);
        command
            .output()
            .expect("run fs_cli")
    })
    .await
    .expect("join fs_cli")
}

#[tokio::test]
async fn a_background_job_result_is_printed_when_it_arrives() {
    let server = FakeEsl::start(Script::default()).await;
    let dir = scratch_dir("bgapi-result");

    let output = run_cli(
        dir,
        server.addr,
        ["-X", "version"]
            .map(String::from)
            .to_vec(),
    )
    .await;

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
        server
            .commands(0)
            .iter()
            .any(|c| c.starts_with("event plain")),
        "without a BACKGROUND_JOB subscription no result can arrive: {:?}",
        server.commands(0)
    );
}

#[tokio::test]
async fn another_clients_job_result_is_ignored() {
    let server = FakeEsl::start(Script {
        job_reply: JobReply::Foreign,
        ..Script::default()
    })
    .await;
    let dir = scratch_dir("bgapi-foreign");

    let output = run_cli(
        dir,
        server.addr,
        ["--job-timeout", "1500", "-X", "version"]
            .map(String::from)
            .to_vec(),
    )
    .await;

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("not yours"),
        "a job result this client never asked for must not be reported: {:?}",
        stdout
    );
    assert!(
        !output
            .status
            .success(),
        "the job it did ask for never completed, so the run must fail"
    );
}

#[tokio::test]
async fn an_expired_job_timeout_names_the_outstanding_job() {
    let server = FakeEsl::start(Script {
        job_reply: JobReply::Silent,
        ..Script::default()
    })
    .await;
    let dir = scratch_dir("bgapi-timeout");

    let output = run_cli(
        dir,
        server.addr,
        ["--job-timeout", "300", "-X", "version"]
            .map(String::from)
            .to_vec(),
    )
    .await;

    assert_eq!(
        output
            .status
            .code(),
        Some(254),
        "a submitted job that never reported has an unknown outcome"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("job-0-") && stderr.contains("version"),
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

    let output = run_cli(
        dir,
        addr,
        ["-x", "status"]
            .map(String::from)
            .to_vec(),
    )
    .await;

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
    let server = FakeEsl::start(Script {
        api_reply: ApiReply::Silent,
        ..Script::default()
    })
    .await;
    let dir = scratch_dir("api-silent");

    let output = run_cli(
        dir,
        server.addr,
        ["-T", "300", "-x", "reloadxml"]
            .map(String::from)
            .to_vec(),
    )
    .await;

    assert_eq!(
        output
            .status
            .code(),
        Some(254),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(server.commands(0), vec!["api reloadxml".to_string()]);
}

#[tokio::test]
async fn a_command_spanning_lines_is_refused_before_connecting() {
    let server = FakeEsl::start(Script::default()).await;
    let dir = scratch_dir("multiline");

    let output = run_cli(
        dir,
        server.addr,
        ["-x", "status\nexit"]
            .map(String::from)
            .to_vec(),
    )
    .await;

    assert_eq!(
        output
            .status
            .code(),
        Some(2)
    );
    assert_eq!(server.connection_count(), 0);
}

#[tokio::test]
async fn a_log_file_captures_the_log_stream_without_escapes() {
    let server = FakeEsl::start(Script {
        log_line: Some("2026-01-01 [NOTICE] fake.c:1 log line for the file".to_string()),
        ..Script::default()
    })
    .await;
    let dir = scratch_dir("log-file");
    let log_path = dir.join("captured.log");

    let output = tokio::task::spawn_blocking({
        let mut command = cli_with_color(&dir, server.addr, "line");
        command
            .arg("--log-file")
            .arg(&log_path)
            // The job result arrives after the log event, so waiting for it
            // pins the capture without a sleep.
            .args(["-x", "status", "-X", "version"]);
        move || {
            command
                .output()
                .expect("run fs_cli")
        }
    })
    .await
    .expect("join fs_cli");

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
    let server = FakeEsl::start(Script {
        log_line: Some("2026-01-01 [NOTICE] fake.c:1 teed to the file".to_string()),
        ..Script::default()
    })
    .await;
    let dir = scratch_dir("log-file-tee");
    let log_path = dir.join("teed.log");

    let pty = open_pty();
    let mut command = cli(&dir, server.addr);
    command
        .arg("--log-file")
        .arg(&log_path);
    let mut child = spawn_interactive(command, &pty);

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

    assert!(
        captured.contains("teed to the file"),
        "an interactive session must write log lines to the file too: {:?}",
        captured
    );
}

#[tokio::test]
async fn batch_commands_reach_the_wire_in_the_typed_order() {
    let server = FakeEsl::start(Script::default()).await;
    let dir = scratch_dir("batch-order");

    let output = tokio::task::spawn_blocking({
        let mut command = cli(&dir, server.addr);
        command.args(["-x", "one", "-X", "two", "-x", "three"]);
        move || {
            command
                .output()
                .expect("run fs_cli batch")
        }
    })
    .await
    .expect("join fs_cli");

    assert!(
        output
            .status
            .success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let issued: Vec<String> = server
        .commands(0)
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
