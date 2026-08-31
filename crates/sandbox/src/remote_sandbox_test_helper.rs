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

fn main() {
    // Both the stage-1 (`--remote-sandbox-helper`) and the internal stage-2
    // re-exec are handled here; neither returns when it matches.
    sandbox::run_remote_sandbox_helper_if_invoked();

    eprintln!(
        "remote_sandbox_test_helper: not invoked as a sandbox helper\n\
         usage: remote_sandbox_test_helper --remote-sandbox-helper <policy> -- <program> <args...>"
    );
    std::process::exit(2);
}
