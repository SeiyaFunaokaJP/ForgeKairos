//! Real holders, attaches and shells on a real kernel. The test binary stands
//! in for the remote server binary: [`subprocess`] runs it as `terminal-session
//! <arguments>` through [`subprocess_entry`].

use super::*;
use crate::terminal_session::truncated_replay_notice;
use std::cell::RefCell;
use std::process::{Child, ChildStdin, Stdio};
use std::sync::{Arc, Mutex};

const SUBPROCESS_ARGUMENTS_VAR: &str = "ZED_TERMINAL_SESSION_TEST_ARGUMENTS";
const SUBPROCESS_TEST: &str = "terminal_session::posix::tests::subprocess_entry";
const NONCE: &str = "TestNonce123";
const WAIT: Duration = Duration::from_secs(20);

pub(super) fn subprocess(arguments: &[OsString]) -> std::process::Command {
    let executable = std::env::current_exe().expect("the test binary's path");
    let mut command = std::process::Command::new(executable);
    command.args([
        "--exact",
        SUBPROCESS_TEST,
        "--nocapture",
        "--test-threads=1",
        "-q",
    ]);
    let arguments: Vec<&str> = arguments
        .iter()
        .map(|argument| argument.to_str().expect("test arguments are UTF-8"))
        .collect();
    command.env(SUBPROCESS_ARGUMENTS_VAR, arguments.join("\u{1f}"));
    command
}

#[test]
fn subprocess_entry() {
    let Ok(arguments) = std::env::var(SUBPROCESS_ARGUMENTS_VAR) else {
        return;
    };

    #[derive(clap::Parser)]
    struct Cli {
        #[command(subcommand)]
        command: TerminalSessionCommand,
    }

    let cli = <Cli as clap::Parser>::try_parse_from(
        std::iter::once("remote_server").chain(arguments.split('\u{1f}')),
    )
    .expect("valid arguments");
    std::process::exit(crate::terminal_session::run(cli.command));
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let started = Instant::now();
    while !condition() {
        assert!(started.elapsed() < WAIT, "timed out waiting until {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    find(haystack, needle).is_some()
}

fn process_is_alive(pid: libc::pid_t) -> bool {
    // A zombie still answers `kill`, so ask /proc about its state.
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => !stat
            .rsplit(')')
            .next()
            .is_some_and(|rest| rest.trim_start().starts_with('Z')),
        Err(_) => false,
    }
}

struct TestSessions {
    directory: tempfile::TempDir,
    ids: RefCell<Vec<String>>,
}

impl TestSessions {
    fn new() -> Self {
        Self {
            directory: tempfile::tempdir().expect("a temporary directory"),
            ids: RefCell::default(),
        }
    }

    fn spawn(&self, arguments: &[&str]) -> Child {
        self.spawn_with_env(arguments, &[])
    }

    /// `environment` also reaches a holder the attach starts.
    #[allow(clippy::disallowed_methods, reason = "a test drives real processes")]
    fn spawn_with_env(&self, arguments: &[&str], environment: &[(&str, &str)]) -> Child {
        let arguments: Vec<OsString> = arguments.iter().map(OsString::from).collect();
        subprocess(&arguments)
            .env(TEST_DIRECTORY_VAR, self.directory.path())
            .envs(environment.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start the test binary")
    }

    /// An attach whose output nobody reads until the test says so, as over
    /// a link far slower than the shell.
    fn attach_unread(&self, id: &str, program: &[&str], environment: &[(&str, &str)]) -> Child {
        self.ids.borrow_mut().push(id.to_string());
        let mut arguments = vec![
            "attach",
            "--id",
            id,
            "--nonce",
            NONCE,
            "--detached-timeout-secs",
            "60",
            "--",
        ];
        arguments.extend(program);
        self.spawn_with_env(&arguments, environment)
    }

    fn attach(&self, id: &str, options: &[&str], program: &[&str]) -> Attach {
        if !self.ids.borrow().iter().any(|known| known == id) {
            self.ids.borrow_mut().push(id.to_string());
        }
        let mut arguments = vec!["attach", "--id", id, "--nonce", NONCE];
        if !options.contains(&"--detached-timeout-secs") {
            // Leaked holders end on their own.
            arguments.extend(["--detached-timeout-secs", "60"]);
        }
        arguments.extend(options);
        arguments.push("--");
        arguments.extend(program);
        Attach::new(self.spawn(&arguments))
    }

    fn kill(&self, id: &str) -> i32 {
        let mut child = self.spawn(&["kill", "--id", id]);
        drop(child.stdin.take());
        exit_code(&mut child)
    }

    fn socket(&self, id: &str) -> PathBuf {
        SessionPaths::resolve(self.directory.path(), id, false)
            .expect("the session's paths")
            .socket
    }
}

impl Drop for TestSessions {
    fn drop(&mut self) {
        for id in self.ids.borrow().iter() {
            kill_session(self.directory.path(), id).log_err();
        }
    }
}

fn exit_code(child: &mut Child) -> i32 {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("the child's status") {
            return status.code().unwrap_or_else(|| {
                use std::os::unix::process::ExitStatusExt as _;
                128 + status.signal().unwrap_or(0)
            });
        }
        assert!(started.elapsed() < WAIT, "the process did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
}

struct Attach {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Arc<Mutex<Vec<u8>>>,
    stderr: Arc<Mutex<Vec<u8>>>,
}

impl Attach {
    fn new(mut child: Child) -> Self {
        let stdin = child.stdin.take();
        let collect = |stream: Option<Box<dyn io::Read + Send>>| {
            let collected = Arc::new(Mutex::new(Vec::new()));
            if let Some(mut stream) = stream {
                let collected = collected.clone();
                std::thread::spawn(move || {
                    let mut buffer = [0u8; 4096];
                    while let Ok(read) = stream.read(&mut buffer)
                        && read > 0
                    {
                        collected
                            .lock()
                            .expect("the output")
                            .extend_from_slice(&buffer[..read]);
                    }
                });
            }
            collected
        };
        let stdout = collect(
            child
                .stdout
                .take()
                .map(|stdout| Box::new(stdout) as Box<dyn io::Read + Send>),
        );
        let stderr = collect(
            child
                .stderr
                .take()
                .map(|stderr| Box::new(stderr) as Box<dyn io::Read + Send>),
        );
        Self {
            child,
            stdin,
            stdout,
            stderr,
        }
    }

    fn output(&self) -> Vec<u8> {
        self.stdout.lock().expect("the output").clone()
    }

    fn errors(&self) -> String {
        String::from_utf8_lossy(&self.stderr.lock().expect("the errors")).into_owned()
    }

    fn wait_for(&self, needle: &str) -> Vec<u8> {
        let started = Instant::now();
        loop {
            let output = self.output();
            if contains(&output, needle.as_bytes()) {
                return output;
            }
            assert!(
                started.elapsed() < WAIT,
                "timed out waiting for {needle:?} in {:?} (stderr: {})",
                String::from_utf8_lossy(&output),
                self.errors()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// What was replayed, and what came live after it.
    fn replay_and_live(&self) -> (Vec<u8>, Vec<u8>) {
        let end = replay_end_marker(NONCE);
        let output = self.wait_for(std::str::from_utf8(&end).expect("an ASCII marker"));
        let begin = replay_begin_marker(NONCE);
        let begin_at = find(&output, &begin).expect("the replay begins");
        let end_at = find(&output, &end).expect("the replay ends");
        assert!(begin_at < end_at);
        (
            output[begin_at + begin.len()..end_at].to_vec(),
            output[end_at + end.len()..].to_vec(),
        )
    }

    fn send(&mut self, input: &str) {
        self.stdin
            .as_mut()
            .expect("stdin is open")
            .write_all(input.as_bytes())
            .expect("write to the attach");
    }

    /// What ssh going away looks like to the attach.
    fn hang_up(&mut self) -> i32 {
        // SAFETY: `kill` only sends a signal.
        unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGHUP) };
        exit_code(&mut self.child)
    }

    fn close_stdin(&mut self) -> i32 {
        drop(self.stdin.take());
        exit_code(&mut self.child)
    }

    fn exit_code(&mut self) -> i32 {
        exit_code(&mut self.child)
    }
}

impl Drop for Attach {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

/// The pid a test program printed as `pid=<pid>;`.
fn printed_pid(attach: &Attach) -> libc::pid_t {
    let mut pid = None;
    wait_until("the program prints its pid", || {
        let output = attach.output();
        let Some(at) = find(&output, b"pid=") else {
            return false;
        };
        let rest = &output[at + 4..];
        let Some(end) = rest.iter().position(|&byte| byte == b';') else {
            return false;
        };
        pid = std::str::from_utf8(&rest[..end])
            .ok()
            .and_then(|digits| digits.parse().ok());
        true
    });
    pid.expect("a pid")
}

#[test]
fn output_made_while_detached_is_replayed() {
    let sessions = TestSessions::new();
    let program = [
        "/bin/sh",
        "-c",
        "echo first; sleep 1; echo during-detach; read line; echo \"got $line\"; read line; exit 7",
    ];
    let mut first = sessions.attach("replay", &["--resume"], &program);
    let (replay, _) = first.replay_and_live();
    assert!(replay.is_empty(), "a new shell has nothing to replay");
    let output = first.wait_for("first");
    assert!(
        contains(&output, resumed_without_shell_notice().as_bytes()),
        "a resumed tab is told that its shell was not there"
    );
    first.hang_up();

    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        sessions.socket("replay").exists(),
        "the holder outlives the attach"
    );
    let mut second = sessions.attach("replay", &["--resume"], &["/bin/false"]);
    let (replay, _) = second.replay_and_live();
    let replay_text = String::from_utf8_lossy(&replay);
    assert!(
        replay_text.contains("first") && replay_text.contains("during-detach"),
        "{replay_text:?}"
    );
    let first_at = find(&replay, b"first").expect("first");
    let later_at = find(&replay, b"during-detach").expect("during-detach");
    assert!(first_at < later_at);
    assert!(
        !contains(&second.output(), resumed_without_shell_notice().as_bytes()),
        "the shell was still there"
    );

    second.send("hello\n");
    second.wait_for("got hello");
    let (_, live) = second.replay_and_live();
    assert!(
        contains(&live, b"got hello"),
        "input reaches the kept shell"
    );
    second.send("bye\n");
    assert_eq!(second.exit_code(), 7);
    wait_until("the holder is gone", || !sessions.socket("replay").exists());
}

#[test]
fn the_exit_code_is_passed_on() {
    let sessions = TestSessions::new();
    for (id, script, expected) in [
        ("zero", "exit 0", 0),
        ("three", "exit 3", 3),
        ("two-five-five", "exit 255", 254),
        ("terminated", "kill -TERM $$", 143),
    ] {
        let mut attach = sessions.attach(id, &[], &["/bin/sh", "-c", script]);
        assert_eq!(attach.exit_code(), expected, "{script}");
        let output = attach.output();
        assert!(
            !contains(&output, b"while disconnected"),
            "the shell ended while attached"
        );
    }
}

#[test]
fn a_shell_that_ended_while_detached_is_reported_once() {
    let sessions = TestSessions::new();
    let mut first = sessions.attach(
        "ended",
        &[],
        &["/bin/sh", "-c", "echo started; sleep 0.5; exit 3"],
    );
    first.wait_for("started");
    first.close_stdin();

    std::thread::sleep(Duration::from_millis(1500));
    let mut second = sessions.attach("ended", &["--resume"], &["/bin/false"]);
    assert_eq!(second.exit_code(), 3);
    let (replay, live) = second.replay_and_live();
    assert!(contains(&replay, b"started"));
    assert_eq!(
        live,
        "\r\n[The shell exited while disconnected (exit code 3)]\r\n".as_bytes()
    );
    wait_until("the holder is gone", || !sessions.socket("ended").exists());

    let mut third = sessions.attach(
        "ended",
        &["--resume"],
        &["/bin/sh", "-c", "echo fresh; exit 0"],
    );
    assert_eq!(third.exit_code(), 0);
    let (replay, live) = third.replay_and_live();
    assert!(replay.is_empty());
    assert!(live.starts_with(resumed_without_shell_notice().as_bytes()));
    assert!(contains(&live, b"fresh"));
}

#[test]
fn kill_ends_the_shell_and_its_hangup_ignoring_jobs() {
    let sessions = TestSessions::new();
    let mut attach = sessions.attach(
        "doomed",
        &[],
        &["/bin/sh", "-c", "echo \"pid=$$;\"; exec sleep 1000"],
    );
    let pid = printed_pid(&attach);
    attach.close_stdin();
    assert!(process_is_alive(pid));
    assert_eq!(sessions.kill("doomed"), 0);
    wait_until("the shell is gone", || !process_is_alive(pid));
    wait_until("the holder is gone", || !sessions.socket("doomed").exists());

    // Ignoring SIGHUP only buys the grace period.
    let mut attach = sessions.attach(
        "stubborn",
        &[],
        &[
            "/bin/sh",
            "-c",
            "trap '' HUP; echo \"pid=$$;\"; while :; do sleep 0.2; done",
        ],
    );
    let pid = printed_pid(&attach);
    attach.close_stdin();
    let started = Instant::now();
    assert_eq!(sessions.kill("stubborn"), 0);
    std::thread::sleep(Duration::from_secs(1));
    assert!(process_is_alive(pid), "the hangup alone does not end it");
    wait_until("the shell is killed", || !process_is_alive(pid));
    assert!(started.elapsed() >= TERMINATION_GRACE);

    assert_eq!(sessions.kill("never-started"), 0);
    assert_eq!(sessions.kill("not/an/id"), 2);
    kill_session(sessions.directory.path(), "never-started").expect("nothing to kill");
    kill_session(Path::new("/nonexistent/terminal_sessions"), "never-started")
        .expect("nothing to kill without a directory");
}

#[test]
fn a_session_left_alone_ends_after_the_detached_timeout() {
    let sessions = TestSessions::new();
    let mut attach = sessions.attach(
        "lonely",
        &["--detached-timeout-secs", "1"],
        &["/bin/sh", "-c", "echo \"pid=$$;\"; exec sleep 1000"],
    );
    let pid = printed_pid(&attach);
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        process_is_alive(pid),
        "an attached session does not time out"
    );
    attach.close_stdin();
    wait_until("the shell is gone", || !process_is_alive(pid));
    wait_until("the holder is gone", || !sessions.socket("lonely").exists());
}

#[test]
fn a_holder_no_attach_reached_ends_even_when_kept_forever() {
    let sessions = TestSessions::new();
    let unattached_timeout = [(TEST_UNATTACHED_HOLDER_TIMEOUT_VAR, "1000")];
    sessions.ids.borrow_mut().push("unreached".to_string());
    let mut holder = sessions.spawn_with_env(
        &[
            "holder",
            "--id",
            "unreached",
            "--detached-timeout-secs",
            "0",
            "--replay-bytes",
            "1024",
            "--",
            "/bin/sh",
        ],
        &unattached_timeout,
    );
    wait_until("the holder listens", || {
        sessions.socket("unreached").exists()
    });
    assert_eq!(exit_code(&mut holder), 0, "the holder ends on its own");
    assert!(!sessions.socket("unreached").exists());

    // Once an attach got through and started the shell, the same settings
    // keep it for good.
    sessions.ids.borrow_mut().push("reached".to_string());
    let mut attach = Attach::new(sessions.spawn_with_env(
        &[
            "attach",
            "--id",
            "reached",
            "--nonce",
            NONCE,
            "--detached-timeout-secs",
            "0",
            "--",
            "/bin/sh",
            "-c",
            "echo \"pid=$$;\"; exec sleep 1000",
        ],
        &unattached_timeout,
    ));
    let pid = printed_pid(&attach);
    attach.close_stdin();
    std::thread::sleep(Duration::from_millis(2500));
    assert!(process_is_alive(pid), "a started shell outlives the wait");
    assert!(sessions.socket("reached").exists());
}

#[test]
fn a_new_attach_takes_over() {
    let sessions = TestSessions::new();
    let program = ["/bin/sh", "-c", "echo ready; exec cat"];
    let mut first = sessions.attach("shared", &[], &program);
    first.wait_for("ready");
    let mut second = sessions.attach("shared", &[], &program);
    assert_eq!(first.exit_code(), 253);
    assert!(first.output().ends_with(taken_over_notice().as_bytes()));
    let (replay, _) = second.replay_and_live();
    assert!(contains(&replay, b"ready"));
    second.send("ping\n");
    second.wait_for("ping");
    assert_eq!(sessions.kill("shared"), 0);
    assert_eq!(second.exit_code(), EXIT_FAILURE);
    wait_until("the attach says the session was ended", || {
        second.errors().contains(SESSION_ENDED_REASON)
    });
}

#[test]
fn the_replay_does_not_make_the_terminal_answer() {
    let sessions = TestSessions::new();
    let mut first = sessions.attach(
        "queries",
        &[],
        &[
            "/bin/sh",
            "-c",
            "printf 'before\\033[6nafter\\033]11;?\\007end\\n'; exec sleep 1000",
        ],
    );
    let live = first.wait_for("end");
    assert!(
        contains(&live, b"before\x1b[6nafter\x1b]11;?\x07end"),
        "live output is passed on untouched"
    );
    first.close_stdin();

    let second = sessions.attach("queries", &[], &["/bin/false"]);
    let (replay, _) = second.replay_and_live();
    assert!(contains(&replay, b"beforeafterend"), "{replay:?}");
    assert!(!contains(&replay, b"\x1b[6n"));
}

#[test]
fn a_truncated_replay_says_so() {
    let sessions = TestSessions::new();
    let mut first = sessions.attach(
        "long",
        &["--replay-bytes", "44"],
        &[
            "/bin/sh",
            "-c",
            "for i in 1 2 3 4 5 6 7 8 9; do echo line-$i; done; exec sleep 1000",
        ],
    );
    first.wait_for("line-9");
    first.close_stdin();
    let second = sessions.attach("long", &[], &["/bin/false"]);
    let (replay, _) = second.replay_and_live();
    assert!(
        replay.starts_with(truncated_replay_notice().as_bytes()),
        "{replay:?}"
    );
    let rest = &replay[truncated_replay_notice().len()..];
    // 44 bytes keep the end of `line-4` and the five lines after it.
    assert!(
        rest.starts_with(b"line-5\r\n"),
        "starts at a whole line: {rest:?}"
    );
    assert!(rest.ends_with(b"line-9\r\n"));
}

/// The client names its display language in the attach's environment; the
/// holder keeps the one of the attach that started it, and the shell does
/// not see it.
#[test]
fn the_notices_are_in_the_clients_language() {
    let sessions = TestSessions::new();
    sessions.ids.borrow_mut().push("localized".to_string());
    let mut first = Attach::new(sessions.spawn_with_env(
        &[
            "attach",
            "--id",
            "localized",
            "--nonce",
            NONCE,
            "--resume",
            "--replay-bytes",
            "44",
            "--detached-timeout-secs",
            "60",
            "--",
            "/bin/sh",
            "-c",
            "echo \"locale=[${ZED_UI_LOCALE-unset}] kept=[${ZED_TEST_KEPT-unset}]\"; \
             for i in 1 2 3 4 5 6 7 8 9; do echo line-$i; done; exec sleep 1000",
        ],
        &[
            (remote::DISPLAY_LANGUAGE_ENV_VAR, "ja"),
            ("ZED_TEST_KEPT", "a=b c"),
        ],
    ));
    let output = first.wait_for("line-9");
    assert!(
        contains(
            &output,
            "[以前のシェルは残っていませんでした。新しいシェルを起動します]".as_bytes()
        ),
        "{:?}",
        String::from_utf8_lossy(&output)
    );
    assert!(
        contains(&output, b"locale=[unset] kept=[a=b c]"),
        "the shell gets everything else of the environment: {:?}",
        String::from_utf8_lossy(&output)
    );
    first.close_stdin();

    // An attach from a client in English, to the holder started in Japanese.
    let second = sessions.attach("localized", &[], &["/bin/false"]);
    let (replay, _) = second.replay_and_live();
    assert!(
        replay.starts_with("\x1b[0m[… 古い出力は省略されました …]\r\n".as_bytes()),
        "{:?}",
        String::from_utf8_lossy(&replay)
    );
    drop(second);

    let mut third = Attach::new(sessions.spawn_with_env(
        &[
            "attach",
            "--id",
            "localized",
            "--nonce",
            NONCE,
            "--detached-timeout-secs",
            "60",
            "--",
            "/bin/false",
        ],
        &[(remote::DISPLAY_LANGUAGE_ENV_VAR, "ja")],
    ));
    third.replay_and_live();
    let mut fourth = sessions.attach("localized", &[], &["/bin/false"]);
    assert_eq!(third.exit_code(), 253);
    assert!(
        third
            .output()
            .ends_with("\r\n[別の接続がこのシェルを引き継ぎました]\r\n".as_bytes()),
        "the attach speaks its own client's language"
    );
    assert_eq!(sessions.kill("localized"), 0);
    assert_eq!(fourth.exit_code(), EXIT_FAILURE);
}

/// `execv` handed the shell the holder's environment as it was; `execve`
/// has to do the same, byte for byte and in order, but for the one variable.
#[test]
fn the_shell_gets_the_holders_environment_but_the_display_language() {
    let variables = [
        ("PATH", b"/usr/bin:/bin".as_slice()),
        (remote::DISPLAY_LANGUAGE_ENV_VAR, b"ja"),
        ("ZED_UI_LOCALE_OTHER", b"kept"),
        ("NOT_UTF8", b"\xff\xfe=x"),
        ("EMPTY", b""),
    ]
    .map(|(name, value)| (OsString::from(name), OsString::from_vec(value.to_vec())));
    let environment = shell_environment(variables).expect("no variable has NUL");
    let environment: Vec<&[u8]> = environment
        .iter()
        .map(|variable| variable.as_bytes())
        .collect();
    assert_eq!(
        environment,
        [
            b"PATH=/usr/bin:/bin".as_slice(),
            b"ZED_UI_LOCALE_OTHER=kept",
            b"NOT_UTF8=\xff\xfe=x",
            b"EMPTY=",
        ]
    );
}

#[test]
fn a_full_screen_program_is_asked_to_redraw() {
    let sessions = TestSessions::new();
    let program = [
        "/bin/sh",
        "-c",
        "trap 'echo winch' WINCH; printf '\\033[?1049h'; echo ready; while :; do sleep 0.1; done",
    ];
    let mut first = sessions.attach("fullscreen", &[], &program);
    first.wait_for("ready");
    first.close_stdin();
    let second = sessions.attach("fullscreen", &[], &program);
    let (replay, _) = second.replay_and_live();
    assert!(contains(&replay, b"\x1b[?1049h"));
    wait_until("the program is told to redraw", || {
        let (_, live) = second.replay_and_live();
        contains(&live, b"winch")
    });
}

#[test]
fn an_attach_of_another_version_leaves_the_shell_alone() {
    let sessions = TestSessions::new();
    let mut first = sessions.attach("versions", &[], &["/bin/sh", "-c", "echo ready; exec cat"]);
    first.wait_for("ready");

    // A newer attach reaches this holder: it is turned away, and the attach
    // that is there stays.
    let mut stream = UnixStream::connect(sessions.socket("versions")).expect("connect");
    let mut preamble = encode_preamble(Role::Attach);
    preamble[4..8].copy_from_slice(&999u32.to_be_bytes());
    stream.write_all(&preamble).expect("introduce");
    let mut answer = Vec::new();
    stream.read_to_end(&mut answer).expect("the answer");
    assert_eq!(answer, encode_preamble(Role::Holder).to_vec());
    first.send("still-here\n");
    first.wait_for("still-here");
    assert!(first.child.try_wait().expect("status").is_none());

    // And this attach reaching a holder of another version says so.
    let other = TestSessions::new();
    let socket = SessionPaths::resolve(other.directory.path(), "old", true)
        .expect("paths")
        .socket;
    let listener = UnixListener::bind(&socket).expect("bind");
    let old_holder = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut introduction = [0; PREAMBLE_LEN];
        stream.read_exact(&mut introduction).expect("the preamble");
        let mut answer = encode_preamble(Role::Holder);
        answer[4..8].copy_from_slice(&999u32.to_be_bytes());
        stream.write_all(&answer).expect("answer");
    });
    let mut attach = other.attach("old", &[], &["/bin/sh"]);
    assert_eq!(attach.exit_code(), EXIT_FAILURE);
    assert!(
        attach.errors().contains("protocol 999"),
        "{}",
        attach.errors()
    );
    assert!(!contains(&attach.output(), &replay_begin_marker(NONCE)));
    old_holder.join().expect("the stand-in holder");
}

/// Reads everything the attach prints until it exits, and its exit code.
fn read_to_exit(child: &mut Child) -> (Vec<u8>, i32) {
    let mut output = Vec::new();
    child
        .stdout
        .take()
        .expect("stdout is piped")
        .read_to_end(&mut output)
        .expect("the attach's output");
    (output, exit_code(child))
}

const LOTS_OF_OUTPUT_BYTES: usize = 8_000_000;

fn lots_of_output_then(after: &str) -> String {
    format!("head -c {LOTS_OF_OUTPUT_BYTES} /dev/zero | tr '\\0' x; echo; {after}")
}

#[test]
fn a_slow_terminal_holds_the_shell_back_instead_of_losing_it() {
    let sessions = TestSessions::new();
    let finished = sessions.directory.path().join("finished");
    let finished = finished.to_str().expect("a UTF-8 path");
    let script = lots_of_output_then("echo finished > \"$0\"; exit 0");
    let mut attach = sessions.attach_unread("slow", &["/bin/sh", "-c", &script, finished], &[]);

    std::thread::sleep(Duration::from_secs(2));
    assert!(
        !Path::new(finished).exists(),
        "the shell waits for the terminal to take its output, as on a plain slow terminal"
    );

    let (output, code) = read_to_exit(&mut attach);
    assert_eq!(code, 0, "the terminal kept its shell to the end");
    assert_eq!(
        output.iter().filter(|&&byte| byte == b'x').count(),
        LOTS_OF_OUTPUT_BYTES,
        "every byte arrived"
    );
    assert!(Path::new(finished).exists());
}

#[test]
fn an_attach_that_takes_nothing_is_let_go_and_the_shell_goes_on() {
    let sessions = TestSessions::new();
    let finished = sessions.directory.path().join("finished");
    let finished = finished.to_str().expect("a UTF-8 path");
    let script =
        lots_of_output_then("echo after-the-flood; echo finished > \"$0\"; exec sleep 1000");
    let mut attach = sessions.attach_unread(
        "stalled",
        &["/bin/sh", "-c", &script, finished],
        &[(TEST_STALLED_ATTACH_TIMEOUT_VAR, "1000")],
    );

    wait_until("the shell goes on without the stalled attach", || {
        Path::new(finished).exists()
    });
    let (output, code) = read_to_exit(&mut attach);
    assert_eq!(
        code, EXIT_DETACHED,
        "the terminal resumes the session instead of closing the tab"
    );
    assert!(contains(&output, &replay_begin_marker(NONCE)));

    let second = sessions.attach("stalled", &["--resume"], &["/bin/false"]);
    let output = second.wait_for("after-the-flood");
    assert!(
        contains(&output, truncated_replay_notice().as_bytes()),
        "the shell went on printing more than the replay keeps"
    );
    assert!(
        !contains(&output, resumed_without_shell_notice().as_bytes()),
        "the shell was kept"
    );
}

#[test]
fn losing_the_holder_ends_the_attach_as_a_dropped_connection() {
    let sessions = TestSessions::new();
    let socket = SessionPaths::resolve(sessions.directory.path(), "vanishing", true)
        .expect("paths")
        .socket;
    let listener = UnixListener::bind(&socket).expect("bind");
    // A holder that goes away (a crash, say) right after the introduction.
    let holder = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut introduction = [0; PREAMBLE_LEN];
        stream.read_exact(&mut introduction).expect("the preamble");
        // The whole hello, so that closing is an orderly end of the stream
        // rather than a reset over unread data.
        let mut header = [0; 5];
        stream.read_exact(&mut header).expect("the hello's header");
        let mut hello =
            vec![0; u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize];
        stream.read_exact(&mut hello).expect("the hello");
        stream
            .write_all(&encode_preamble(Role::Holder))
            .expect("answer");
    });
    let mut attach = sessions.attach("vanishing", &[], &["/bin/sh"]);
    assert_eq!(attach.exit_code(), EXIT_DETACHED);
    assert!(contains(&attach.output(), &replay_begin_marker(NONCE)));
    holder.join().expect("the stand-in holder");
}
