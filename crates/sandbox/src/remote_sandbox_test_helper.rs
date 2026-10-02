//! Standalone driver that exercises the ADR-F18 remote sandbox helper
//! (`linux_remote_helper`) end-to-end on a real remote Linux host during
//! acceptance testing. It is the exact code path `remote_server` runs, minus the
//! rest of the server binary, so it can be cross-compiled to a small musl binary
//! and `scp`'d to a test host.
//!
//! Built only under the `remote-sandbox-test` feature; never shipped. Invoke it
//! the same way the client would:
//!
//! ```text
//! remote_sandbox_test_helper --remote-sandbox-helper <policy...> -- <shell> -c <cmd>
//! ```
//!
//! ADR-F49 runs an external agent's process under the helper's one-shot mode for
//! hours, with JSON-RPC on its piped standard streams. Two more modes check
//! exactly that:
//!
//! ```text
//! remote_sandbox_test_helper --json-rpc-echo [--ignore-eof] [--ignore-hangup] [--linger]
//! remote_sandbox_test_helper --ide-probe <port> [--linger]
//! remote_sandbox_test_helper --stdio-e2e <scratch directory> [--seconds <n>] [--network]
//! ```
//!
//! The first stands in for the agent inside the sandbox, and the second for one
//! that speaks to Zed over the ADR-F49 IDE channel: it connects to the port on
//! the namespace's own loopback, exchanges a line and reports what it saw. The
//! third starts this binary as the helper around them, with the argv the client
//! builds, and checks what the client sees: the answers and nothing else on
//! stdout, EOF reaching the agent, nothing growing while it runs and nothing
//! left behind, the client's hangup stopping it, what may be read among the
//! hidden entries and what refuses the command, the IDE channel of a one-shot
//! command carrying a connection out to the bridge and back, and with
//! `--network` (`slirp4netns` beside this binary) the egress proxy keeping
//! local addresses out of reach. It creates files only under the scratch
//! directory, and — for the IDE channel, which has to be under this account's
//! home — one directory of its own in `~/.cache/forgekairos/ide`.

#![allow(
    clippy::disallowed_methods,
    reason = "a standalone test driver, not the GPUI app: it spawns and waits on processes directly"
)]

fn main() {
    // Both the stage-1 (`--remote-sandbox-helper`) and the internal stage-2
    // re-exec are handled here; neither returns when it matches.
    sandbox::run_remote_sandbox_helper_if_invoked();

    #[cfg(target_os = "linux")]
    {
        let args: Vec<String> = std::env::args().skip(1).collect();
        if let Some((mode, options)) = args.split_first() {
            match mode.as_str() {
                "--json-rpc-echo" => json_rpc_echo::run(options),
                "--ide-probe" => ide_probe::run(options),
                "--stdio-e2e" => stdio_e2e::run(options),
                _ => {}
            }
        }
    }

    eprintln!(
        "remote_sandbox_test_helper: not invoked as a sandbox helper\n\
         usage: remote_sandbox_test_helper --remote-sandbox-helper <policy> -- <program> <args...>\n\
         \x20      remote_sandbox_test_helper --json-rpc-echo [--ignore-eof] [--ignore-hangup] [--linger]\n\
         \x20      remote_sandbox_test_helper --ide-probe <port> [--linger]\n\
         \x20      remote_sandbox_test_helper --stdio-e2e <scratch directory> [--seconds <n>] [--network]"
    );
    std::process::exit(2);
}

/// Stands in for an ACP agent inside the sandbox: one JSON-RPC request per line
/// on stdin, one answer per line on stdout, until stdin ends.
#[cfg(target_os = "linux")]
mod json_rpc_echo {
    use std::io::{BufRead as _, Read as _, Write as _};
    use std::time::Duration;

    pub fn run(options: &[String]) -> ! {
        let option = |name: &str| options.iter().any(|option| option == name);
        // SAFETY: installs `SIG_IGN`, which runs no code of ours.
        if option("--ignore-hangup")
            && unsafe { libc::signal(libc::SIGHUP, libc::SIG_IGN) } == libc::SIG_ERR
        {
            fail("cannot ignore SIGHUP");
        }
        if option("--hold-stdout-reader")
            && let Err(error) = hold_stdout_reader()
        {
            fail(&format!("cannot hold a reader of stdout: {error}"));
        }
        if option("--linger")
            && let Err(error) = linger()
        {
            fail(&format!("cannot leave a process behind: {error}"));
        }
        let mut stdout = std::io::stdout().lock();
        for line in std::io::stdin().lock().lines() {
            let line = line.unwrap_or_else(|error| fail(&format!("reading stdin: {error}")));
            let written = stdout
                .write_all(respond(&line).as_bytes())
                .and_then(|()| stdout.write_all(b"\n"))
                .and_then(|()| stdout.flush());
            if let Err(error) = written {
                fail(&format!("writing stdout: {error}"));
            }
        }
        if option("--ignore-eof") {
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
        std::process::exit(0);
    }

    fn fail(message: &str) -> ! {
        eprintln!("json-rpc-echo: {message}");
        std::process::exit(1);
    }

    /// Open this process's own standard output for *reading*, and never close
    /// it.
    ///
    /// A command handed the client's own pipe can do exactly this: the pipe
    /// then has a reader whatever the client does, so the helper watching for
    /// its readers to go never sees them go, and a command that ignores the end
    /// of its input outlives the session that started it.
    fn hold_stdout_reader() -> std::io::Result<()> {
        let reader = std::fs::File::open("/proc/self/fd/1")?;
        // Held for the life of the process, which is the whole point.
        std::mem::forget(reader);
        Ok(())
    }

    /// The answer to one request: its `params` echoed back as the `result`, for
    /// `connect` the first line of what the egress proxy says to a CONNECT to
    /// `params.target`, and for `environment` this process's whole environment.
    fn respond(line: &str) -> String {
        let id: String = after(line, "\"id\":")
            .map(|rest| rest.chars().take_while(char::is_ascii_digit).collect())
            .unwrap_or_default();
        let params = after(line, "\"params\":")
            .and_then(|rest| rest.strip_suffix('}'))
            .unwrap_or("null");
        let result = if line.contains("\"method\":\"environment\"") {
            let mut variables: Vec<String> = std::env::vars()
                .map(|(name, value)| format!("{name}={value}"))
                .collect();
            variables.sort();
            format!(
                "{{\"environment\":\"{}\"}}",
                variables.join("\t").replace(['"', '\\'], "'")
            )
        } else if line.contains("\"method\":\"connect\"") {
            let target = after(params, "\"target\":\"")
                .and_then(|rest| rest.split('"').next())
                .unwrap_or_default();
            format!(
                "{{\"status\":\"{}\"}}",
                connect_through_proxy(target).replace(['"', '\\'], "'")
            )
        } else {
            params.to_string()
        };
        format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{result}}}")
    }

    fn after<'a>(text: &'a str, marker: &str) -> Option<&'a str> {
        let start = text.find(marker)? + marker.len();
        text.get(start..)
    }

    fn connect_through_proxy(target: &str) -> String {
        let Ok(proxy) = std::env::var("HTTPS_PROXY") else {
            return "no proxy".to_string();
        };
        let address = proxy.trim_start_matches("http://").trim_end_matches('/');
        let status = || -> std::io::Result<String> {
            let address: std::net::SocketAddr = address.parse().map_err(|error| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{error}"))
            })?;
            let mut stream =
                std::net::TcpStream::connect_timeout(&address, Duration::from_secs(10))?;
            stream.set_read_timeout(Some(Duration::from_secs(30)))?;
            write!(
                stream,
                "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n"
            )?;
            let mut status = Vec::new();
            let mut byte = [0u8; 1];
            while status.len() < 512 && stream.read(&mut byte)? == 1 && byte != *b"\n" {
                status.extend_from_slice(&byte);
            }
            Ok(String::from_utf8_lossy(&status).trim_end().to_string())
        };
        status().unwrap_or_else(|error| format!("error: {error}"))
    }

    /// Leaves a process running past the end of the session, as a daemon that
    /// `setsid`s away would, with the session's stdout still open in it.
    fn linger() -> std::io::Result<()> {
        use std::os::unix::process::CommandExt as _;
        let mut command = std::process::Command::new("sleep");
        command.arg("3600").stdin(std::process::Stdio::null());
        // SAFETY: `setsid` is async-signal-safe, and it is all the closure does.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command.spawn().map(drop)
    }
}

/// ADR-F49 decision 24: stands in for an external ACP agent's use of the IDE
/// channel, which is not a terminal session's. It connects to
/// `127.0.0.1:<port>` on the namespace's own loopback, sends one line, reads
/// one back, and prints what it found on its standard output — the client's
/// own pipe — as a single line, so the driver reads it the way it reads an
/// agent's answers.
#[cfg(target_os = "linux")]
mod ide_probe {
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::net::TcpStream;
    use std::time::Duration;

    /// What the probe sends, and what the driver on the other side of the
    /// bridge expects to see arrive.
    pub const REQUEST: &str = "zed-ide-probe hello";

    pub fn run(options: &[String]) -> ! {
        let Some(port) = options.first().and_then(|value| value.parse::<u16>().ok()) else {
            eprintln!("ide-probe: usage: --ide-probe <port> [--linger]");
            std::process::exit(2);
        };
        let linger = options.iter().any(|option| option == "--linger");
        let outcome = match exchange(port) {
            Ok(answer) => format!("reply={answer}"),
            Err(error) => format!("error={error}"),
        };
        // The proxy variables the helper wrote: with an IDE channel loopback
        // has to be exempt, or a client that honours them would send this very
        // connection to the egress proxy, which refuses loopback.
        let variable = |name: &str| std::env::var(name).unwrap_or_else(|_| "<unset>".to_string());
        println!(
            "zed-ide-probe {outcome} no_proxy={} https_proxy={}",
            variable("NO_PROXY"),
            variable("HTTPS_PROXY")
        );
        if let Err(error) = std::io::stdout().flush() {
            eprintln!("ide-probe: writing stdout: {error}");
            std::process::exit(1);
        }
        if linger {
            // Until the client goes away and the helper stops this process.
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
        std::process::exit(0);
    }

    fn exchange(port: u16) -> std::io::Result<String> {
        let mut stream = TcpStream::connect(("127.0.0.1", port))?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        stream.write_all(REQUEST.as_bytes())?;
        stream.write_all(b"\n")?;
        stream.flush()?;
        let mut answer = String::new();
        BufReader::new(stream).read_line(&mut answer)?;
        Ok(answer.trim_end().replace(char::is_whitespace, "_"))
    }
}

/// Starts this binary as the helper around [`json_rpc_echo`] and checks what the
/// client sees (see the crate documentation).
#[cfg(target_os = "linux")]
mod stdio_e2e {
    use anyhow::{Context as _, Result, bail};
    use sandbox::RemoteSandboxNetwork;
    use std::io::{Read as _, Write as _};
    use std::os::fd::AsRawFd as _;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
    use std::time::{Duration, Instant};

    const TAG: &str = "[stdio_e2e]";

    pub fn run(options: &[String]) -> ! {
        match run_checks(options) {
            Ok(0) => {
                println!("{TAG} ALL PASSED");
                std::process::exit(0);
            }
            Ok(failures) => {
                println!("{TAG} {failures} FAILED");
                std::process::exit(1);
            }
            Err(error) => {
                println!("{TAG} ERROR: {error:#}");
                std::process::exit(1);
            }
        }
    }

    fn run_checks(options: &[String]) -> Result<usize> {
        let Some((scratch, options)) = options.split_first() else {
            bail!("usage: --stdio-e2e <scratch directory> [--seconds <n>] [--network]");
        };
        let scratch = std::fs::canonicalize(scratch)
            .with_context(|| format!("the scratch directory {scratch}"))?;
        let mut seconds = 60;
        let mut network = false;
        let mut options = options.iter();
        while let Some(option) = options.next() {
            match option.as_str() {
                "--seconds" => {
                    seconds = options
                        .next()
                        .and_then(|value| value.parse().ok())
                        .context("--seconds needs a number")?;
                }
                "--network" => network = true,
                other => bail!("unknown option {other}"),
            }
        }
        let helper = std::env::current_exe().context("locating this binary")?;
        let world = World::prepare(&scratch, helper)?;
        let mut checks = Checks::default();
        world.long_round_trip(&mut checks, seconds)?;
        world.lingering_child(&mut checks)?;
        world.client_hangup(&mut checks, false, false)?;
        world.client_hangup(&mut checks, true, false)?;
        world.client_hangup(&mut checks, false, true)?;
        world.command_environment(&mut checks)?;
        world.readable(&mut checks)?;
        world.credentials_in_the_worktree(&mut checks)?;
        world.refused_readable(&mut checks)?;
        world.ide_forward(&mut checks, network)?;
        if network {
            world.network(&mut checks)?;
        } else {
            checks.info("--network not given: the egress proxy checks are skipped");
        }
        Ok(checks.failures)
    }

    #[derive(Default)]
    struct Checks {
        failures: usize,
    }

    impl Checks {
        fn expect(&mut self, description: &str, passed: bool, detail: impl std::fmt::Display) {
            if passed {
                println!("{TAG} PASS: {description}");
            } else {
                self.failures += 1;
                println!("{TAG} FAIL: {description}: {detail}");
            }
        }

        fn info(&self, text: impl std::fmt::Display) {
            println!("{TAG} INFO: {text}");
        }
    }

    fn preview(text: &str) -> String {
        text.chars()
            .take(300)
            .collect::<String>()
            .replace('\n', " | ")
    }

    /// What the helper is started with, beside the command.
    struct Policy<'a> {
        writable: Vec<PathBuf>,
        readable: Vec<PathBuf>,
        /// Directories the command may neither read nor write. The IDE
        /// rendezvous has to be in one (ADR-F49), or the helper refuses.
        protected: Vec<PathBuf>,
        network: RemoteSandboxNetwork,
        /// The helper's *own* environment, as the session that starts it has
        /// one.
        environment: &'a [(&'a str, &'a str)],
        /// What the client asks the helper to set in the command's environment
        /// (`--setenv`).
        set_environment: &'a [(&'a str, &'a str)],
        /// ADR-F49 decision 24: the IDE channel (`--ide-listen` /
        /// `--ide-rendezvous`), on a one-shot command rather than an
        /// interactive session.
        ide_forward: Option<(u16, PathBuf)>,
    }

    impl Policy<'static> {
        fn confined(writable: Vec<PathBuf>) -> Self {
            Self {
                writable,
                readable: Vec::new(),
                protected: Vec::new(),
                network: RemoteSandboxNetwork::Blocked,
                environment: &[],
                set_environment: &[],
                ide_forward: None,
            }
        }
    }

    /// The scratch directory's layout, and this binary.
    struct World {
        helper: PathBuf,
        scratch: PathBuf,
        work: PathBuf,
        /// A home for the helper's read hiding to apply to instead of the
        /// account's (`ZED_SANDBOX_READ_HIDDEN_HOME_FOR_TESTS`), so nothing of a
        /// real one is used.
        home: PathBuf,
    }

    impl World {
        fn prepare(scratch: &Path, helper: PathBuf) -> Result<Self> {
            let work = scratch.join("work");
            let home = scratch.join("home");
            let data = home
                .join(".local/share")
                .join(branding::CURRENT.app_directory_name_lowercase);
            let projects = home.join(sandbox::SANDBOXED_AGENT_PROJECTS_HOME_RELATIVE);
            for (directory, file, contents) in [
                (work.clone(), "main.rs", "work"),
                (
                    data.join("external_agents/registry/npx/agent"),
                    "agent.js",
                    "agent-code",
                ),
                (
                    data.join("external_agents/registry/other"),
                    "secret",
                    "other-agent",
                ),
                (data.join("external_agents_old"), "old", "old"),
                (data.join("node/v22/bin"), "node", "node-binary"),
                (data.join("db"), "threads.db", "database"),
                (home.join(".ssh"), "id_test", "key"),
                (projects.join("own/config"), "settings.json", "own"),
                (projects.join("other/config"), "token", "other-project"),
                // ADR-F49 decision 8: credential stores that used to be hidden
                // from reads and left writable, with a project beside them, so
                // the home itself can stand in for a worktree.
                (home.clone(), ".npmrc", "//registry:_authToken=secret"),
                (home.join(".config/gcloud"), "credentials.db", "token"),
                (home.join(".cargo"), "credentials.toml", "registry token"),
                (home.join("project"), "main.rs", "project"),
            ] {
                std::fs::create_dir_all(&directory)
                    .with_context(|| format!("creating {}", directory.display()))?;
                std::fs::write(directory.join(file), contents)
                    .with_context(|| format!("writing in {}", directory.display()))?;
            }
            let link = data.join("external_agents/registry/npx/link");
            if std::fs::symlink_metadata(&link).is_err() {
                std::os::unix::fs::symlink(home.join(".ssh"), &link)
                    .with_context(|| format!("creating {}", link.display()))?;
            }
            Ok(Self {
                helper,
                scratch: scratch.to_path_buf(),
                work,
                home,
            })
        }

        fn data(&self) -> PathBuf {
            self.home
                .join(".local/share")
                .join(branding::CURRENT.app_directory_name_lowercase)
        }

        fn projects(&self) -> PathBuf {
            self.home
                .join(sandbox::SANDBOXED_AGENT_PROJECTS_HOME_RELATIVE)
        }

        fn echo(&self, options: &[&str]) -> Vec<String> {
            let mut command = vec![
                self.helper.display().to_string(),
                "--json-rpc-echo".to_string(),
            ];
            command.extend(options.iter().map(|option| option.to_string()));
            command
        }

        /// Starts the helper around `command` as the client does: the policy
        /// argv `build_remote_sandbox_helper_argv` builds, packed in hex.
        fn start(&self, label: &str, policy: &Policy, command: &[String]) -> Result<Session> {
            let strings = |paths: &[PathBuf]| {
                paths
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
            };
            let chdir = self.work.display().to_string();
            let mut argv = sandbox::build_remote_sandbox_helper_argv(
                &strings(&policy.writable),
                &strings(&policy.protected),
                &strings(&policy.readable),
                &policy.network,
                // No editor-wide ceiling: these tests are about what the
                // helper itself enforces around a command's own allowlist.
                None,
                false,
                Some(&chdir),
            );
            // The policy ends with `--`, and the environment belongs before it,
            // exactly as the client builds it.
            let separator = argv.pop().filter(|argument| argument == "--");
            for (name, value) in policy.set_environment {
                argv.extend([
                    "--setenv".to_string(),
                    (*name).to_string(),
                    (*value).to_string(),
                ]);
            }
            // No `--interactive`: ADR-F49 decision 24 gives the channel to a
            // one-shot command.
            if let Some((port, rendezvous)) = &policy.ide_forward {
                argv.extend([
                    "--ide-listen".to_string(),
                    port.to_string(),
                    "--ide-rendezvous".to_string(),
                    rendezvous.display().to_string(),
                ]);
            }
            argv.extend(separator);
            argv.extend(command.iter().cloned());
            let [flag, payload] = sandbox::encode_remote_sandbox_helper_argv(&argv);
            let stderr_path = self.scratch.join(format!("{label}.stderr"));
            let stderr = std::fs::File::create(&stderr_path)
                .with_context(|| format!("creating {}", stderr_path.display()))?;
            let mut process = Command::new(&self.helper);
            process
                .arg(flag)
                .arg(payload)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::from(stderr))
                .env("ZED_SANDBOX_READ_HIDDEN_HOME_FOR_TESTS", &self.home);
            for (name, value) in policy.environment {
                process.env(name, value);
            }
            let mut child = process
                .spawn()
                .with_context(|| format!("starting the helper for {label}"))?;
            let stdin = child.stdin.take().context("the helper's stdin")?;
            let stdout = child.stdout.take().context("the helper's stdout")?;
            Ok(Session {
                child,
                stdin: Some(stdin),
                stdout: Some(LineReader::new(stdout)),
                stderr_path,
                next_id: 0,
            })
        }

        /// An agent answering for `seconds`, then told to stop by stdin's end.
        fn long_round_trip(&self, checks: &mut Checks, seconds: u64) -> Result<()> {
            let mut session = self.start(
                "round-trip",
                &Policy::confined(vec![self.work.clone()]),
                &self.echo(&[]),
            )?;
            let sizes = [16usize, 1000, 65_536, 16, 300_000];
            let started = Instant::now();
            let mut answered = 0usize;
            let mut failure = None;
            let mut baseline = None;
            let mut slowest = Duration::ZERO;
            while failure.is_none()
                && (answered < 20 || started.elapsed() < Duration::from_secs(seconds))
            {
                let size = sizes.get(answered % sizes.len()).copied().unwrap_or(16);
                let params = format!("{{\"n\":{answered},\"pad\":\"{}\"}}", "x".repeat(size));
                let sent = Instant::now();
                match session.call("echo", &params) {
                    Ok(result) if result == params => {}
                    Ok(result) => {
                        failure = Some(format!(
                            "request {answered} came back as {}",
                            preview(&result)
                        ))
                    }
                    Err(error) => failure = Some(format!("{error:#}")),
                }
                slowest = slowest.max(sent.elapsed());
                answered += 1;
                if answered == 10 {
                    baseline = Some(footprint(session.pid()));
                }
                if size < 1000 {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
            let last = footprint(session.pid());
            checks.expect(
                &format!(
                    "{answered} requests over {} s were each answered in order, with nothing else \
                     on stdout (slowest {} ms)",
                    started.elapsed().as_secs(),
                    slowest.as_millis()
                ),
                failure.is_none(),
                failure.as_deref().unwrap_or_default(),
            );
            checks.expect(
                "the helper's descriptors, threads and processes stay as they were while it runs",
                baseline.as_ref() == Some(&last),
                format!("{baseline:?} then {last:?}"),
            );
            let holder = descendants(session.pid()).first().copied();
            session.close_stdin();
            let status = session.wait(Duration::from_secs(10))?;
            let trailing = session.next_line(Duration::from_secs(5))?;
            checks.expect(
                "closing stdin reaches the agent as EOF: it and the helper exit 0, and stdout \
                 ends with nothing more on it",
                status.is_some_and(|status| status.success()) && matches!(trailing, Line::End),
                format!("status {status:?}, then {trailing:?}"),
            );
            if let Some(holder) = holder {
                let left: Vec<String> = [
                    format!("/tmp/zed-remote-sandbox-{holder}"),
                    format!("/tmp/.zed-remote-jail-{holder}"),
                ]
                .into_iter()
                .filter(|path| Path::new(path).exists())
                .collect();
                checks.expect(
                    "the command's scratch directories are gone",
                    left.is_empty(),
                    left.join(", "),
                );
            }
            checks.info(format!("stderr: {}", preview(&session.stderr())));
            Ok(())
        }

        fn lingering_child(&self, checks: &mut Checks) -> Result<()> {
            let mut session = self.start(
                "linger",
                &Policy::confined(vec![self.work.clone()]),
                &self.echo(&["--linger"]),
            )?;
            let answered = session.call("echo", "{\"n\":1}");
            session.close_stdin();
            let ended = session.next_line(Duration::from_secs(15))?;
            let status = session.wait(Duration::from_secs(5))?;
            checks.expect(
                "what the agent left running, holding stdout, is stopped once the agent exits: \
                 stdout ends and the helper exits 0",
                answered.is_ok()
                    && matches!(ended, Line::End)
                    && status.is_some_and(|status| status.success()),
                format!("{answered:?}, {ended:?}, {status:?}"),
            );
            Ok(())
        }

        /// The client going away while the agent ignores its stdin's end.
        fn client_hangup(
            &self,
            checks: &mut Checks,
            ignore_hangup: bool,
            hold_stdout_reader: bool,
        ) -> Result<()> {
            let mut options = vec!["--ignore-eof"];
            if ignore_hangup {
                options.push("--ignore-hangup");
            }
            if hold_stdout_reader {
                options.push("--hold-stdout-reader");
            }
            let label = if hold_stdout_reader {
                "hangup-reader-held"
            } else if ignore_hangup {
                "hangup-ignored"
            } else {
                "hangup"
            };
            let mut session = self.start(
                label,
                &Policy::confined(vec![self.work.clone()]),
                &self.echo(&options),
            )?;
            let answered = session.call("echo", "{\"n\":1}");
            let tree = descendants(session.pid());
            let started = Instant::now();
            session.hang_up();
            let status = session.wait(Duration::from_secs(30))?;
            let took = started.elapsed();
            std::thread::sleep(Duration::from_millis(500));
            let left: Vec<u32> = tree.into_iter().filter(|pid| alive(*pid)).collect();
            let description = if hold_stdout_reader {
                "an agent holding a reader of its own output (through /proc/self/fd/1, which \
                 would keep the client's pipe from ever reporting its readers gone) is stopped \
                 all the same"
            } else if ignore_hangup {
                "an agent that ignores SIGHUP too is killed after the grace period, with \
                 everything it started"
            } else {
                "the client going away (its end of stdin and stdout closed) stops an agent that \
                 ignores EOF: SIGHUP reaches it, and nothing it started is left"
            };
            checks.expect(
                &format!("{description} ({} ms)", took.as_millis()),
                answered.is_ok() && status.is_some() && left.is_empty(),
                format!("{answered:?}, status {status:?}, still running {left:?}"),
            );
            Ok(())
        }

        /// ADR-F46 C-1: the command's environment is built from nothing, so
        /// nothing of the session that started the helper — an agent socket, a
        /// connection's details — reaches an agent that runs for hours.
        fn command_environment(&self, checks: &mut Checks) -> Result<()> {
            let policy = Policy {
                writable: vec![self.work.clone()],
                readable: Vec::new(),
                protected: Vec::new(),
                network: RemoteSandboxNetwork::Blocked,
                environment: &[
                    ("SSH_AUTH_SOCK", "/tmp/ssh-agent.sock"),
                    ("SSH_CONNECTION", "10.0.0.1 1 10.0.0.2 22"),
                    ("LANG", "en_US.UTF-8"),
                    ("PATH", "/session/only:/usr/bin"),
                ],
                set_environment: &[("AGENT_SETTING", "from the client")],
                ide_forward: None,
            };
            let mut session = self.start("environment", &policy, &self.echo(&[]))?;
            let answered = session.call("environment", "{}")?;
            session.close_stdin();
            session.wait(Duration::from_secs(10))?;
            let variables: Vec<&str> = answered
                .strip_prefix("{\"environment\":\"")
                .and_then(|rest| rest.strip_suffix("\"}"))
                .unwrap_or_default()
                .split('\t')
                .collect();
            let has = |variable: &str| variables.contains(&variable);
            checks.expect(
                "nothing of the session that started the helper is in the command's environment",
                !variables.iter().any(|variable| {
                    variable.starts_with("SSH_AUTH_SOCK=")
                        || variable.starts_with("SSH_CONNECTION=")
                }),
                format!("{variables:?}"),
            );
            checks.expect(
                "the command gets what the client asked for, the default PATH, the locale and \
                 the sandbox's own settings",
                has("AGENT_SETTING=from the client")
                    && has("PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
                    && has("LANG=en_US.UTF-8")
                    && variables
                        .iter()
                        .any(|variable| variable.starts_with("TMPDIR=")),
                format!("{variables:?}"),
            );
            // `HOME` is not in what the client sent, so it can only have come
            // from the account database — without it `git` / `npm` / `cargo`
            // break.
            let home = variables
                .iter()
                .find_map(|variable| variable.strip_prefix("HOME="));
            checks.expect(
                "HOME is filled in from the account database (an absolute path the client never \
                 sent)",
                home.is_some_and(|home| home.starts_with('/') && !home.is_empty()),
                format!("{variables:?}"),
            );

            // What the client sends explicitly wins over the account database.
            let overriding = Policy {
                writable: vec![self.work.clone()],
                readable: Vec::new(),
                protected: Vec::new(),
                network: RemoteSandboxNetwork::Blocked,
                environment: &[],
                set_environment: &[("HOME", "/agent/home")],
                ide_forward: None,
            };
            let mut session = self.start("environment-home", &overriding, &self.echo(&[]))?;
            let answered = session.call("environment", "{}")?;
            session.close_stdin();
            session.wait(Duration::from_secs(10))?;
            let overridden: Vec<&str> = answered
                .strip_prefix("{\"environment\":\"")
                .and_then(|rest| rest.strip_suffix("\"}"))
                .unwrap_or_default()
                .split('\t')
                .collect();
            checks.expect(
                "a HOME the client sends explicitly wins over the account database, and appears \
                 only once",
                overridden
                    .iter()
                    .filter(|variable| variable.starts_with("HOME="))
                    .eq(std::iter::once(&"HOME=/agent/home")),
                format!("{overridden:?}"),
            );
            Ok(())
        }

        /// ADR-F49 decisions 14 and 15, through the helper.
        fn readable(&self, checks: &mut Checks) -> Result<()> {
            let data = self.data();
            let agent = data.join("external_agents/registry/npx/agent");
            let node = data.join("node");
            let projects = self.projects();
            let own = projects.join("own/config");
            let quoted = |path: &Path| format!("'{}'", path.display());
            // Right at the top of the session's own configuration directory a
            // new entry can be made, since no agent on the host reads it; right
            // at the top of the worktree it cannot, while the agent
            // configurations it might hold are protected (ADR-F49 decision 8).
            let data_directory = self.data();
            let script = format!(
                "cat {agent_file} && echo; \
                 cat {node_file} && echo; \
                 touch {agent_new} 2>/dev/null && echo agent-writable; \
                 touch {node_new} 2>/dev/null && echo node-writable; \
                 cat {database} 2>/dev/null && echo && echo database-visible; \
                 cat {other_agent} 2>/dev/null && echo && echo other-agent-visible; \
                 cat {key} 2>/dev/null && echo && echo key-visible; \
                 echo data=$(ls -A {data}); \
                 echo projects=$(ls -A {projects}); \
                 cat {other_project} 2>/dev/null && echo && echo other-project-visible; \
                 touch {projects_new} 2>/dev/null && echo projects-writable; \
                 echo updated >> {own_file} && echo own-writable; \
                 touch {own_new} 2>/dev/null && echo own-root-takes-new-entries; \
                 touch {work_new} 2>/dev/null && echo work-root-takes-new-entries; \
                 true",
                agent_file = quoted(&agent.join("agent.js")),
                node_file = quoted(&node.join("v22/bin/node")),
                data = quoted(&data_directory),
                agent_new = quoted(&agent.join("new")),
                node_new = quoted(&node.join("new")),
                database = quoted(&data.join("db/threads.db")),
                other_agent = quoted(&data.join("external_agents/registry/other/secret")),
                key = quoted(&self.home.join(".ssh/id_test")),
                projects = quoted(&projects),
                other_project = quoted(&projects.join("other/config/token")),
                projects_new = quoted(&projects.join("new")),
                own_file = quoted(&own.join("settings.json")),
                own_new = quoted(&own.join(".credentials.json")),
                work_new = quoted(&self.work.join("new-at-the-top")),
            );
            let policy = Policy {
                writable: vec![self.work.clone(), own],
                readable: vec![agent, node],
                protected: Vec::new(),
                network: RemoteSandboxNetwork::Blocked,
                environment: &[],
                set_environment: &[],
                ide_forward: None,
            };
            let command = ["/bin/sh".to_string(), "-c".to_string(), script];
            let mut session = self.start("readable", &policy, &command)?;
            session.close_stdin();
            let output = session.read_to_end(Duration::from_secs(30))?;
            let status = session.wait(Duration::from_secs(10))?;
            let stderr = session.stderr();
            let has = |line: &str| output.iter().any(|output| output == line);
            let detail = format!(
                "status {status:?}, stdout {output:?}, stderr {}",
                preview(&stderr)
            );
            checks.expect(
                "the command runs with an agent installation and Node.js named as readable, and \
                 can read them",
                status.is_some_and(|status| status.success())
                    && has("agent-code")
                    && has("node-binary"),
                &detail,
            );
            checks.expect(
                "what it may read it cannot write",
                !has("agent-writable") && !has("node-writable"),
                &detail,
            );
            checks.expect(
                "a new entry can be made right at the top of the session's own configuration \
                 directory (its credentials, at the first login), and still not at the top of \
                 the worktree",
                has("own-root-takes-new-entries") && !has("work-root-takes-new-entries"),
                &detail,
            );
            if stderr.contains("read-hiding-unavailable") {
                checks.info(
                    "this host refuses the read-hiding mounts: nothing is hidden, so what may be \
                     read changes nothing, as designed",
                );
                checks.expect(
                    "the refused hiding is reported on stderr, never on stdout",
                    !output.iter().any(|line| line.contains("read-hiding")),
                    &detail,
                );
                checks.expect(
                    "the session's own configuration stays writable",
                    has("own-writable"),
                    &detail,
                );
            } else if stderr.contains("read-hiding-by-landlock") {
                checks.info(
                    "this host refuses the read-hiding mounts and Landlock denies the reads \
                     instead (ADR-F49 decision 11)",
                );
                checks.expect(
                    "the fallback is reported on stderr, never on stdout",
                    !output.iter().any(|line| line.contains("read-hiding")),
                    &detail,
                );
                checks.expect(
                    "the rest of Zed's data, the other agents, the other projects and the \
                     credentials are as unreadable as the covers would have made them",
                    !has("database-visible")
                        && !has("other-agent-visible")
                        && !has("key-visible")
                        && !has("other-project-visible"),
                    &detail,
                );
                checks.expect(
                    "what may be read is readable, the session writes its own configuration, \
                     and the directory holding the projects takes nothing new",
                    has("own-writable") && !has("projects-writable"),
                    &detail,
                );
                checks.expect(
                    "the cost of hiding them this way: a directory holding a hidden entry \
                     cannot be listed at all, where a cover shows what it binds back",
                    has("data=") && has("projects="),
                    &detail,
                );
            } else {
                checks.expect(
                    "the rest of Zed's data, the other agents and the credentials stay hidden, \
                     and only the way down to what may be read is in the data directory — so \
                     what is added to it while the command runs is not there either",
                    !has("database-visible")
                        && !has("other-agent-visible")
                        && !has("key-visible")
                        && has("data=external_agents node"),
                    &detail,
                );
                checks.expect(
                    "the projects directory shows only the session's own configuration, which \
                     stays writable, and takes nothing new",
                    has("projects=own")
                        && !has("other-project-visible")
                        && !has("projects-writable")
                        && has("own-writable"),
                    &detail,
                );
            }
            Ok(())
        }

        /// ADR-F49 decision 8 (2026-09-15): a worktree that *is* the account's
        /// home leaves every credential store inside a write grant. Hiding
        /// them from reads used to be all that happened to them, which the
        /// covers turned into a write refusal by accident and the Landlock
        /// hiding did not — so they are protected in their own right now, on
        /// whichever way this host hides reads.
        fn credentials_in_the_worktree(&self, checks: &mut Checks) -> Result<()> {
            let quoted = |path: &Path| format!("'{}'", path.display());
            // What is on disk afterwards is the check, not what the shell was
            // told: where the hiding is a cover, a credential file is bound to
            // `/dev/null`, so the command's own write "succeeds" and reaches
            // nothing. What must never happen is the store changing.
            let credentials = [
                (".npmrc", "//registry:_authToken=secret"),
                (".cargo/credentials.toml", "registry token"),
                (".config/gcloud/credentials.db", "token"),
                (".ssh/id_test", "key"),
            ];
            let script = format!(
                "grep -q secret {npmrc} && echo npmrc-secret-visible; \
                 echo tampered >> {npmrc} 2>/dev/null; \
                 echo tampered >> {gcloud} 2>/dev/null; \
                 touch {gcloud_new} 2>/dev/null; \
                 echo tampered >> {cargo} 2>/dev/null; \
                 echo tampered >> {key} 2>/dev/null; \
                 echo worked >> {project} && echo project-writable; \
                 true",
                npmrc = quoted(&self.home.join(".npmrc")),
                gcloud = quoted(&self.home.join(".config/gcloud/credentials.db")),
                gcloud_new = quoted(&self.home.join(".config/gcloud/new")),
                cargo = quoted(&self.home.join(".cargo/credentials.toml")),
                key = quoted(&self.home.join(".ssh/id_test")),
                project = quoted(&self.home.join("project/main.rs")),
            );
            let policy = Policy::confined(vec![self.home.clone()]);
            let command = ["/bin/sh".to_string(), "-c".to_string(), script];
            let mut session = self.start("credentials", &policy, &command)?;
            session.close_stdin();
            let output = session.read_to_end(Duration::from_secs(30))?;
            let status = session.wait(Duration::from_secs(10))?;
            let stderr = session.stderr();
            let has = |line: &str| output.iter().any(|output| output == line);
            // The stores whose contents on disk are no longer what was put
            // there: the check is that this stays empty.
            let changed: Vec<&str> = credentials
                .iter()
                .filter(|(relative, contents)| {
                    std::fs::read_to_string(self.home.join(relative))
                        .ok()
                        .as_deref()
                        != Some(*contents)
                })
                .map(|(relative, _)| *relative)
                .collect();
            let added = self.home.join(".config/gcloud/new").exists();
            let detail = format!(
                "status {status:?}, stdout {output:?}, changed {changed:?}, added {added}, \
                 stderr {}",
                preview(&stderr)
            );
            checks.expect(
                "a worktree that is the home still writes its own files",
                status.is_some_and(|status| status.success()) && has("project-writable"),
                &detail,
            );
            checks.expect(
                "and no credential store in it is changed or added to, whichever way this host \
                 hides reads",
                changed.is_empty() && !added,
                &detail,
            );
            if stderr.contains("read-hiding-unavailable") {
                checks.info(
                    "this host hides no reads, so the credentials stay readable; keeping them \
                     unchanged is what decision 8 adds there",
                );
            } else {
                checks.expect(
                    "and none of them can be read either",
                    !has("npmrc-secret-visible"),
                    &detail,
                );
            }
            Ok(())
        }

        fn refused_readable(&self, checks: &mut Checks) -> Result<()> {
            let data = self.data();
            for (what, path) in [
                ("a credential store", self.home.join(".ssh")),
                ("Zed's data directory itself", data.clone()),
                ("Zed's database", data.join("db")),
                (
                    "a directory beside external_agents",
                    data.join("external_agents_old"),
                ),
                (
                    "a symlink out of an agent installation into ~/.ssh",
                    data.join("external_agents/registry/npx/link"),
                ),
                ("the worktree", self.work.clone()),
                ("a path that is not there", data.join("node/missing")),
                ("the root", PathBuf::from("/")),
            ] {
                let policy = Policy {
                    writable: vec![self.work.clone()],
                    readable: vec![path],
                    protected: Vec::new(),
                    network: RemoteSandboxNetwork::Blocked,
                    environment: &[],
                    set_environment: &[],
                    ide_forward: None,
                };
                let command = ["/bin/echo".to_string(), "ran".to_string()];
                let mut session = self.start("refused", &policy, &command)?;
                session.close_stdin();
                let output = session.read_to_end(Duration::from_secs(15))?;
                let status = session.wait(Duration::from_secs(10))?;
                let stderr = session.stderr();
                let marked = stderr
                    .lines()
                    .next()
                    .is_some_and(|line| line.starts_with("zed-remote-sandbox: readable-refused: "));
                checks.expect(
                    &format!(
                        "--readable naming {what} refuses the command: exit 126, \
                         `readable-refused` on stderr, nothing run"
                    ),
                    status.and_then(|status| status.code()) == Some(126)
                        && marked
                        && output.is_empty(),
                    format!(
                        "status {status:?}, stdout {output:?}, stderr {}",
                        preview(&stderr)
                    ),
                );
            }
            Ok(())
        }

        /// ADR-F49 decision 24: the IDE channel on a **one-shot** command,
        /// which is what an external ACP agent is. The bridge listens outside
        /// the sandbox on a socket in a protected directory; the command
        /// connects to the port on its own loopback, and what it sends comes
        /// out of the bridge's standard output as a frame and goes back the
        /// same way. With `--network` the egress proxy runs beside it, so the
        /// two listeners in the namespace are checked together.
        fn ide_forward(&self, checks: &mut Checks, network: bool) -> Result<()> {
            let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
                checks.info("no HOME: the IDE forward checks are skipped");
                return Ok(());
            };
            // Where Zed really puts it, which is also where the bridge insists
            // it be: under this account's home, in a private directory.
            let directory = home
                .join(branding::CURRENT.remote_cache_home_relative)
                .join("ide")
                .join(format!("e2e-{}", std::process::id()));
            std::fs::create_dir_all(&directory)
                .with_context(|| format!("creating {}", directory.display()))?;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("securing {}", directory.display()))?;
            let rendezvous = directory.join("bridge.sock");
            let mut bridge = Bridge::start(&self.helper, &rendezvous, &self.scratch)?;

            let port: u16 = 45123;
            let policy = Policy {
                writable: vec![self.work.clone()],
                readable: Vec::new(),
                protected: vec![directory.clone()],
                network: if network {
                    RemoteSandboxNetwork::AnyHost
                } else {
                    RemoteSandboxNetwork::Blocked
                },
                environment: &[],
                set_environment: &[],
                ide_forward: Some((port, rendezvous.clone())),
            };
            let probe = [
                self.helper.display().to_string(),
                "--ide-probe".to_string(),
                port.to_string(),
                "--linger".to_string(),
            ];
            let mut session = self.start("ide-forward", &policy, &probe)?;

            // The command's connection arrives as an Open frame, then what it
            // wrote as Data.
            let opened = bridge.next_frame(Duration::from_secs(30))?;
            let connection = opened.as_ref().map(|frame| frame.connection_id);
            checks.expect(
                "a one-shot command with an IDE channel reaches the bridge \
                 through the namespace's own loopback",
                opened
                    .as_ref()
                    .is_some_and(|frame| matches!(frame.kind, sandbox::BridgeFrameKind::Open)),
                format!("{opened:?}"),
            );
            let sent = bridge.next_data(Duration::from_secs(30))?;
            checks.expect(
                "what the command wrote comes out of the bridge unchanged",
                sent.trim_end() == crate::ide_probe::REQUEST,
                preview(&sent),
            );
            if let Some(connection) = connection {
                bridge.send_data(
                    connection,
                    b"zed-ide-reply
",
                )?;
            }
            let answered = match session.next_line(Duration::from_secs(30))? {
                Line::Text(line) => line,
                other => format!("{other:?}"),
            };
            checks.expect(
                "and Zed's answer reaches the command on that same connection",
                answered.contains("reply=zed-ide-reply"),
                preview(&answered),
            );
            // ADR-F49 decision 24: without this a client that honours the
            // proxy variables would send its own loopback connection to the
            // egress proxy, which refuses loopback.
            checks.expect(
                "an IDE channel takes loopback out of the command's proxy variables",
                answered.contains("no_proxy=127.0.0.1,localhost") || !network,
                preview(&answered),
            );
            if !network {
                checks.expect(
                    "with no network grant there is no proxy to go around either",
                    answered.contains("no_proxy=<unset>"),
                    preview(&answered),
                );
            }

            // The client going away stops it all, one-shot or not.
            let holder = descendants(session.pid()).first().copied();
            session.hang_up();
            let status = session.wait(Duration::from_secs(15))?;
            checks.expect(
                "the client's hangup stops a one-shot command that was still \
                 holding its IDE channel",
                status.is_some(),
                format!("status {status:?}"),
            );
            if let Some(holder) = holder {
                let left: Vec<String> = [
                    format!("/tmp/zed-remote-sandbox-{holder}"),
                    format!("/tmp/.zed-remote-jail-{holder}"),
                ]
                .into_iter()
                .filter(|path| Path::new(path).exists())
                .collect();
                checks.expect(
                    "and leaves none of the command's scratch directories behind",
                    left.is_empty(),
                    left.join(", "),
                );
            }
            checks.info(format!("stderr: {}", preview(&session.stderr())));

            // Zed's own end: closing the bridge's input removes the socket and
            // the directory it created.
            let bridge_status = bridge.shut_down(Duration::from_secs(15))?;
            checks.expect(
                "closing the bridge's input ends it and removes its socket",
                bridge_status.is_some_and(|status| status.success()) && !rendezvous.exists(),
                format!("status {bridge_status:?}, socket {}", rendezvous.exists()),
            );
            checks.info(format!("bridge stderr: {}", preview(&bridge.stderr())));
            std::fs::remove_dir_all(&directory).ok();
            Ok(())
        }

        /// The egress proxy: local addresses out of reach, and nothing growing
        /// with the connections it relays.
        fn network(&self, checks: &mut Checks) -> Result<()> {
            if !self.helper.with_file_name("slirp4netns").is_file() {
                checks.info("no slirp4netns beside this binary: the network checks are skipped");
                return Ok(());
            }
            // Any host with no editor-wide allowlist — an embedder's case
            // since ADR-F17 started sending the ceiling to a remote host too,
            // and still what keeps a command off local addresses when no
            // ceiling decides about them. With the helper's diagnostics on,
            // slirp4netns's own reports included.
            let policy = Policy {
                writable: vec![self.work.clone()],
                readable: Vec::new(),
                protected: Vec::new(),
                network: RemoteSandboxNetwork::AnyHost,
                environment: &[("ZED_REMOTE_SANDBOX_LOG", "1")],
                set_environment: &[],
                ide_forward: None,
            };
            let mut session = self.start("any-host", &policy, &self.echo(&[]))?;
            let mut local: Vec<String> = [
                "127.0.0.1:22",
                "127.0.0.2:22",
                "10.0.0.1:443",
                "172.16.0.1:443",
                "192.168.0.1:22",
                "[::1]:22",
                "169.254.169.254:80",
                "100.64.0.1:443",
                "0.0.0.0:22",
                "localhost:22",
            ]
            .into_iter()
            .map(String::from)
            .collect();
            for address in own_addresses() {
                local.push(format!("{address}:22"));
            }
            let mut reached = Vec::new();
            for target in &local {
                let status = connect(&mut session, target)?;
                if !status.starts_with("HTTP/1.1 511") {
                    reached.push(format!("{target} -> {status}"));
                }
            }
            checks.expect(
                &format!(
                    "an any-host grant with no editor-wide allowlist reaches no local address \
                     (loopback, private, link-local, shared, unspecified, localhost, and every \
                     address this host itself holds, public ones included): {} targets refused \
                     with 511",
                    local.len()
                ),
                reached.is_empty(),
                reached.join("; "),
            );
            let public = connect(&mut session, "example.com:443")?;
            checks.info(format!("example.com:443 -> {public}"));
            checks.expect(
                "a public host still passes the proxy's policy",
                !public.starts_with("HTTP/1.1 511"),
                &public,
            );

            let baseline = steady_footprint(session.pid());
            let mut unexpected = None;
            for _ in 0..300 {
                let status = connect(&mut session, "10.0.0.1:443")?;
                if !status.starts_with("HTTP/1.1 511") {
                    unexpected = Some(status);
                }
            }
            let settled = settle(session.pid(), &baseline, Duration::from_secs(10));
            checks.expect(
                "300 relayed connections leave the helper with no more descriptors, threads or \
                 processes than before them",
                settled.within(&baseline) && unexpected.is_none(),
                format!("{baseline:?} then {settled:?}, {unexpected:?}"),
            );
            session.close_stdin();
            let status = session.wait(Duration::from_secs(10))?;
            checks.expect(
                "with the helper's diagnostics on, stdout carried only the answers and the \
                 session ends cleanly",
                status.is_some_and(|status| status.success()),
                format!("status {status:?}, stderr {}", preview(&session.stderr())),
            );

            let policy = Policy {
                writable: vec![self.work.clone()],
                readable: Vec::new(),
                protected: Vec::new(),
                network: RemoteSandboxNetwork::Hosts(vec!["example.com".to_string()]),
                environment: &[],
                set_environment: &[],
                ide_forward: None,
            };
            let mut session = self.start("hosts", &policy, &self.echo(&[]))?;
            let mut reached = Vec::new();
            for target in [
                "localhost:22",
                "127.0.0.1:22",
                "10.0.0.1:443",
                "example.org:443",
            ] {
                let status = connect(&mut session, target)?;
                if !status.starts_with("HTTP/1.1 511") {
                    reached.push(format!("{target} -> {status}"));
                }
            }
            checks.expect(
                "a host list refuses local addresses and hosts not on it",
                reached.is_empty(),
                reached.join("; "),
            );
            let listed = connect(&mut session, "example.com:443")?;
            checks.expect(
                "a host on the list passes the proxy's policy",
                !listed.starts_with("HTTP/1.1 511"),
                &listed,
            );
            session.close_stdin();
            session.wait(Duration::from_secs(10))?;
            Ok(())
        }
    }

    fn connect(session: &mut Session, target: &str) -> Result<String> {
        let result = session.call("connect", &format!("{{\"target\":\"{target}\"}}"))?;
        Ok(result
            .strip_prefix("{\"status\":\"")
            .and_then(|rest| rest.strip_suffix("\"}"))
            .unwrap_or(&result)
            .to_string())
    }

    /// Every address this host holds that the proxy must refuse whatever range
    /// it is in: something bound to `0.0.0.0` or `::` answers on each of them,
    /// and the proxy runs in the host's own network namespace.
    fn own_addresses() -> Vec<String> {
        let mut addresses: Vec<String> = own_lan_address().into_iter().collect();
        // `/proc/net/if_inet6`: the address as 32 hex digits, then the
        // interface, the prefix length and the scope (`00` is global).
        let Ok(text) = std::fs::read_to_string("/proc/net/if_inet6") else {
            return addresses;
        };
        for line in text.lines() {
            let mut fields = line.split_whitespace();
            let (Some(digits), _, _, Some(scope)) =
                (fields.next(), fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            if scope != "00" || digits.len() != 32 {
                continue;
            }
            let groups: Vec<&str> = (0..8)
                .filter_map(|group| digits.get(group * 4..group * 4 + 4))
                .collect();
            let Ok(address) = groups.join(":").parse::<std::net::Ipv6Addr>() else {
                continue;
            };
            if !address.is_loopback() {
                addresses.push(format!("[{address}]"));
            }
        }
        addresses
    }

    /// This host's own address on its default route, found without sending
    /// anything (connecting a UDP socket only picks the source address).
    fn own_lan_address() -> Option<String> {
        let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
        socket.connect("192.0.2.1:9").ok()?;
        let address = socket.local_addr().ok()?.ip();
        (!address.is_loopback() && !address.is_unspecified()).then(|| address.to_string())
    }

    /// One helper run with its standard streams piped, as Zed spawns an agent.
    struct Session {
        child: Child,
        stdin: Option<ChildStdin>,
        stdout: Option<LineReader>,
        stderr_path: PathBuf,
        next_id: u64,
    }

    impl Session {
        fn pid(&self) -> u32 {
            self.child.id()
        }

        /// Sends one request and returns the `result` of the next line, which
        /// has to be its answer.
        fn call(&mut self, method: &str, params: &str) -> Result<String> {
            let id = self.next_id;
            self.next_id += 1;
            let stdin = self.stdin.as_mut().context("stdin is closed")?;
            writeln!(
                stdin,
                "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"{method}\",\"params\":{params}}}"
            )?;
            stdin.flush()?;
            match self.next_line(Duration::from_secs(60))? {
                Line::Text(line) => {
                    let prefix = format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":");
                    let result = line
                        .strip_prefix(&prefix)
                        .and_then(|rest| rest.strip_suffix('}'))
                        .with_context(|| {
                            format!("not the answer to request {id}: {}", preview(&line))
                        })?;
                    Ok(result.to_string())
                }
                Line::End => bail!("stdout ended before the answer to request {id}"),
                Line::TimedOut => bail!("no answer to request {id} within 60 s"),
            }
        }

        fn next_line(&mut self, timeout: Duration) -> Result<Line> {
            match self.stdout.as_mut() {
                Some(stdout) => Ok(stdout.next_line(timeout)?),
                None => Ok(Line::End),
            }
        }

        fn read_to_end(&mut self, timeout: Duration) -> Result<Vec<String>> {
            let deadline = Instant::now() + timeout;
            let mut lines = Vec::new();
            loop {
                match self.next_line(deadline.saturating_duration_since(Instant::now()))? {
                    Line::Text(line) => lines.push(line),
                    Line::End => return Ok(lines),
                    Line::TimedOut => bail!("stdout did not end within {timeout:?}: {lines:?}"),
                }
            }
        }

        fn close_stdin(&mut self) {
            self.stdin = None;
        }

        /// The client going away: neither its end of stdin nor of stdout is
        /// open any more.
        fn hang_up(&mut self) {
            self.stdin = None;
            self.stdout = None;
        }

        fn wait(&mut self, timeout: Duration) -> Result<Option<ExitStatus>> {
            let deadline = Instant::now() + timeout;
            loop {
                if let Some(status) = self.child.try_wait()? {
                    return Ok(Some(status));
                }
                if Instant::now() >= deadline {
                    return Ok(None);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }

        fn stderr(&self) -> String {
            std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
        }
    }

    impl Drop for Session {
        fn drop(&mut self) {
            // A check that failed half-way must not leave a helper running.
            if matches!(self.child.try_wait(), Ok(None)) {
                if let Err(error) = self.child.kill() {
                    eprintln!("{TAG} could not stop helper {}: {error}", self.child.id());
                }
                if let Err(error) = self.child.wait() {
                    eprintln!("{TAG} could not reap helper {}: {error}", self.child.id());
                }
            }
        }
    }

    #[derive(Debug)]
    enum Line {
        Text(String),
        End,
        TimedOut,
    }

    /// ADR-F49: Zed's end of the IDE channel — the `--ide-bridge` process,
    /// which listens on the rendezvous socket outside the sandbox and carries
    /// every connection over its own standard I/O as frames.
    struct Bridge {
        child: Child,
        stdin: Option<ChildStdin>,
        stdout: ChildStdout,
        decoder: sandbox::BridgeFrameDecoder,
        pending: std::collections::VecDeque<sandbox::BridgeFrame>,
        buffer: Vec<u8>,
        stderr_path: PathBuf,
    }

    impl Bridge {
        fn start(helper: &Path, rendezvous: &Path, scratch: &Path) -> Result<Self> {
            let stderr_path = scratch.join("ide-bridge.stderr");
            let stderr = std::fs::File::create(&stderr_path)
                .with_context(|| format!("creating {}", stderr_path.display()))?;
            let mut child = Command::new(helper)
                .arg("--ide-bridge")
                .arg("--rendezvous")
                .arg(rendezvous)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::from(stderr))
                .spawn()
                .context("starting the IDE bridge")?;
            let stdin = child.stdin.take().context("the bridge's stdin")?;
            let stdout = child.stdout.take().context("the bridge's stdout")?;
            let bridge = Self {
                child,
                stdin: Some(stdin),
                stdout,
                decoder: sandbox::BridgeFrameDecoder::new(),
                pending: std::collections::VecDeque::new(),
                buffer: vec![0; 64 * 1024],
                stderr_path,
            };
            // It says so on its stderr once it listens; nothing may connect
            // before that.
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                let said = bridge.stderr();
                if said.contains("zed-ide-bridge: ready") {
                    return Ok(bridge);
                }
                if Instant::now() >= deadline {
                    bail!("the IDE bridge did not start: {}", preview(&said));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }

        fn next_frame(&mut self, timeout: Duration) -> Result<Option<sandbox::BridgeFrame>> {
            let deadline = Instant::now() + timeout;
            loop {
                if let Some(frame) = self.pending.pop_front() {
                    return Ok(Some(frame));
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Ok(None);
                }
                let mut poll_fd = libc::pollfd {
                    fd: self.stdout.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                let milliseconds = i32::try_from(remaining.as_millis()).unwrap_or(i32::MAX);
                // SAFETY: one valid `pollfd` for the duration of the call.
                let ready = unsafe { libc::poll(&mut poll_fd, 1, milliseconds) };
                if ready < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error).context("waiting for the IDE bridge");
                }
                if ready == 0 {
                    return Ok(None);
                }
                let read = self
                    .stdout
                    .read(&mut self.buffer)
                    .context("reading from the IDE bridge")?;
                if read == 0 {
                    self.decoder.finish()?;
                    return Ok(None);
                }
                let bytes = self
                    .buffer
                    .get(..read)
                    .context("read past the buffer")?
                    .to_vec();
                self.pending.extend(self.decoder.push(&bytes)?);
            }
        }

        /// The payload of the next Data frame, as text.
        fn next_data(&mut self, timeout: Duration) -> Result<String> {
            let deadline = Instant::now() + timeout;
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                match self.next_frame(remaining)? {
                    Some(frame) if matches!(frame.kind, sandbox::BridgeFrameKind::Data) => {
                        return Ok(String::from_utf8_lossy(&frame.payload).into_owned());
                    }
                    Some(_) => continue,
                    None => bail!("no data came out of the IDE bridge"),
                }
            }
        }

        fn send_data(&mut self, connection_id: u32, payload: &[u8]) -> Result<()> {
            let stdin = self
                .stdin
                .as_mut()
                .context("the bridge's stdin is closed")?;
            let mut encoded = Vec::new();
            sandbox::encode_frame(
                &sandbox::BridgeFrame::data(connection_id, payload.to_vec()),
                &mut encoded,
            );
            stdin.write_all(&encoded)?;
            stdin.flush()?;
            Ok(())
        }

        fn shut_down(&mut self, timeout: Duration) -> Result<Option<ExitStatus>> {
            self.stdin = None;
            let deadline = Instant::now() + timeout;
            loop {
                if let Some(status) = self.child.try_wait()? {
                    return Ok(Some(status));
                }
                if Instant::now() >= deadline {
                    return Ok(None);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }

        fn stderr(&self) -> String {
            std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
        }
    }

    impl Drop for Bridge {
        fn drop(&mut self) {
            self.stdin = None;
            if matches!(self.child.try_wait(), Ok(None)) {
                self.child.kill().ok();
                self.child.wait().ok();
            }
        }
    }

    /// Lines of the helper's stdout, read with a deadline and closable at any
    /// time, which a reader thread blocked in `read` would not allow.
    struct LineReader {
        stream: ChildStdout,
        buffer: Vec<u8>,
        ended: bool,
    }

    impl LineReader {
        fn new(stream: ChildStdout) -> Self {
            Self {
                stream,
                buffer: Vec::new(),
                ended: false,
            }
        }

        fn next_line(&mut self, timeout: Duration) -> std::io::Result<Line> {
            let deadline = Instant::now() + timeout;
            loop {
                if let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') {
                    let line: Vec<u8> = self.buffer.drain(..=newline).collect();
                    let text = line.strip_suffix(b"\n").unwrap_or(&line);
                    return Ok(Line::Text(String::from_utf8_lossy(text).into_owned()));
                }
                if self.ended {
                    if self.buffer.is_empty() {
                        return Ok(Line::End);
                    }
                    let rest = std::mem::take(&mut self.buffer);
                    return Ok(Line::Text(String::from_utf8_lossy(&rest).into_owned()));
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Ok(Line::TimedOut);
                }
                let mut poll_fd = libc::pollfd {
                    fd: self.stream.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                let milliseconds = i32::try_from(remaining.as_millis()).unwrap_or(i32::MAX);
                // SAFETY: one valid `pollfd` for the duration of the call.
                let ready = unsafe { libc::poll(&mut poll_fd, 1, milliseconds) };
                if ready < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error);
                }
                if ready == 0 {
                    continue;
                }
                let mut chunk = vec![0u8; 1 << 16];
                let read = self.stream.read(&mut chunk)?;
                if read == 0 {
                    self.ended = true;
                }
                self.buffer
                    .extend_from_slice(chunk.get(..read).unwrap_or_default());
            }
        }
    }

    /// What the helper holds while it runs.
    #[derive(Debug, PartialEq)]
    struct Footprint {
        descriptors: usize,
        threads: usize,
        processes: usize,
    }

    fn footprint(pid: u32) -> Footprint {
        let count = |path: String| {
            std::fs::read_dir(path)
                .map(|entries| entries.count())
                .unwrap_or(0)
        };
        Footprint {
            descriptors: count(format!("/proc/{pid}/fd")),
            threads: count(format!("/proc/{pid}/task")),
            processes: descendants(pid).len(),
        }
    }

    impl Footprint {
        fn within(&self, limit: &Footprint) -> bool {
            self.descriptors <= limit.descriptors
                && self.threads <= limit.threads
                && self.processes <= limit.processes
        }
    }

    /// The footprint once it has stopped changing: the threads and
    /// descriptors of connections just closed take a moment to go.
    fn steady_footprint(pid: u32) -> Footprint {
        let mut last = footprint(pid);
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(200));
            let now = footprint(pid);
            if now == last {
                return now;
            }
            last = now;
        }
        last
    }

    /// The footprint once it is back within `baseline`, or as it is after
    /// `timeout`.
    fn settle(pid: u32, baseline: &Footprint, timeout: Duration) -> Footprint {
        let deadline = Instant::now() + timeout;
        loop {
            let now = footprint(pid);
            if now.within(baseline) || Instant::now() >= deadline {
                return now;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Every process below `pid`, by their host pids, nearest first.
    fn descendants(pid: u32) -> Vec<u32> {
        let mut found = Vec::new();
        let mut pending = vec![pid];
        while let Some(parent) = pending.pop() {
            let Ok(tasks) = std::fs::read_dir(format!("/proc/{parent}/task")) else {
                continue;
            };
            for task in tasks.flatten() {
                let Ok(children) = std::fs::read_to_string(task.path().join("children")) else {
                    continue;
                };
                for child in children
                    .split_whitespace()
                    .filter_map(|child| child.parse::<u32>().ok())
                {
                    found.push(child);
                    pending.insert(0, child);
                }
            }
        }
        found
    }

    /// Whether `pid` is a process that has not exited.
    fn alive(pid: u32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| {
                let state = stat.get(stat.rfind(')')? + 2..)?.chars().next()?;
                Some(!matches!(state, 'Z' | 'X'))
            })
            .unwrap_or(false)
    }
}
