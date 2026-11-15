//! The daemon of the static musl build allocates through mimalloc; every other
//! mode of the binary keeps musl's allocator.
//!
//! musl's allocator keeps what a burst of allocations used: indexing the C and
//! C++ files of a large worktree (ADR-F68) left about 400 MB resident after the
//! index was dropped, and each further full build added about 300 MB, for as
//! long as the daemon lived. With mimalloc, tree-sitter routed to it and freed
//! memory purged at once, the same builds leave about 15 MB. The proxy, which
//! only relays messages, is the other way round: musl gives back what it
//! buffered, mimalloc kept about 150 MB of it.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    ffi::c_void,
    sync::atomic::{AtomicU8, Ordering},
};

#[global_allocator]
static GLOBAL: ModeAllocator = ModeAllocator;

const UNDECIDED: u8 = 0;
const SYSTEM: u8 = 1;
const MIMALLOC: u8 = 2;

/// Which allocator this process uses. It is decided when the first allocation
/// is served and never changes after, so that every block is freed by the
/// allocator that handed it out.
static CHOICE: AtomicU8 = AtomicU8::new(UNDECIDED);

/// `mi_option_purge_delay` in the `mi_option_t` enum of the mimalloc that
/// `libmimalloc-sys` builds; `libmimalloc-sys` does not export it.
const MI_OPTION_PURGE_DELAY: libmimalloc_sys::mi_option_t = 15;

/// The subcommand `proxy` starts the daemon with (see `Commands::Daemon`).
const DAEMON_SUBCOMMAND: &[u8] = b"daemon";

struct ModeAllocator;

fn choice() -> u8 {
    match CHOICE.load(Ordering::Acquire) {
        UNDECIDED => {
            let decided = if this_process_is_the_daemon() {
                // An idle thread never purges what it freed while the delay
                // ran, and the index keeps its worker threads; without this
                // they hold the memory of the last build. mimalloc reads the
                // option at every free, so this only needs to come before the
                // daemon's work; here it comes before any block is served.
                // SAFETY: setting an option has no preconditions and does not
                // allocate.
                unsafe { libmimalloc_sys::mi_option_set(MI_OPTION_PURGE_DELAY, 0) };
                MIMALLOC
            } else {
                SYSTEM
            };
            // The first allocation comes before any other thread exists, so
            // nothing can have been allocated under another choice; and every
            // caller computes the same one anyway.
            match CHOICE.compare_exchange(UNDECIDED, decided, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => decided,
                Err(earlier) => earlier,
            }
        }
        decided => decided,
    }
}

// SAFETY: every method forwards to the allocator that `choice` picked, which
// is fixed for the life of the process, so a block is always resized and freed
// by the allocator that allocated it.
unsafe impl GlobalAlloc for ModeAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if choice() == MIMALLOC {
            // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
            unsafe { mimalloc::MiMalloc.alloc(layout) }
        } else {
            // SAFETY: as above.
            unsafe { System.alloc(layout) }
        }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if choice() == MIMALLOC {
            // SAFETY: the caller upholds `GlobalAlloc::alloc_zeroed`'s contract.
            unsafe { mimalloc::MiMalloc.alloc_zeroed(layout) }
        } else {
            // SAFETY: as above.
            unsafe { System.alloc_zeroed(layout) }
        }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        if choice() == MIMALLOC {
            // SAFETY: `pointer` came from `MiMalloc` (see the impl's comment).
            unsafe { mimalloc::MiMalloc.dealloc(pointer, layout) }
        } else {
            // SAFETY: `pointer` came from `System` (see the impl's comment).
            unsafe { System.dealloc(pointer, layout) }
        }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if choice() == MIMALLOC {
            // SAFETY: `pointer` came from `MiMalloc` (see the impl's comment).
            unsafe { mimalloc::MiMalloc.realloc(pointer, layout, new_size) }
        } else {
            // SAFETY: `pointer` came from `System` (see the impl's comment).
            unsafe { System.realloc(pointer, layout, new_size) }
        }
    }
}

/// Reads `/proc/self/cmdline` without allocating: this runs inside the first
/// allocation. Anything unreadable counts as "not the daemon".
fn this_process_is_the_daemon() -> bool {
    let mut buffer = [0u8; 4096];
    // SAFETY: a NUL-terminated path literal and plain flags.
    let descriptor = unsafe {
        libc::open(
            c"/proc/self/cmdline".as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        return false;
    }
    let mut filled = 0;
    while filled < buffer.len() {
        let Some(rest) = buffer.get_mut(filled..) else {
            break;
        };
        // SAFETY: `rest` is writable for `rest.len()` bytes.
        let read = unsafe { libc::read(descriptor, rest.as_mut_ptr().cast(), rest.len()) };
        match usize::try_from(read) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(_) => {
                // SAFETY: closes the descriptor opened above.
                unsafe { libc::close(descriptor) };
                return false;
            }
        }
    }
    // SAFETY: closes the descriptor opened above.
    unsafe { libc::close(descriptor) };
    buffer.get(..filled).is_some_and(is_daemon_command_line)
}

/// Whether a `/proc/<pid>/cmdline` (arguments each followed by a NUL) starts
/// this binary as the daemon: its first argument is the `daemon` subcommand.
/// A command line cut short before that argument ends counts as not.
fn is_daemon_command_line(command_line: &[u8]) -> bool {
    let mut arguments = command_line.split(|byte| *byte == 0);
    let (Some(_program), Some(subcommand)) = (arguments.next(), arguments.next()) else {
        return false;
    };
    // `split` yields what follows the last NUL as a final piece; a subcommand
    // with no NUL after it may have been cut off by the buffer.
    subcommand == DAEMON_SUBCOMMAND && arguments.next().is_some()
}

/// Must run first thing in `main`, before anything creates a tree-sitter
/// parser or tree: memory that tree-sitter took from musl's allocator must not
/// be freed through mimalloc.
pub fn configure() {
    if choice() != MIMALLOC {
        return;
    }
    // SAFETY: called first thing in `main`, before any tree-sitter allocation.
    unsafe {
        tree_sitter::set_allocator(Some(tree_sitter::Allocator {
            malloc: tree_sitter_malloc,
            calloc: tree_sitter_calloc,
            realloc: tree_sitter_realloc,
            free: tree_sitter_free,
        }))
    };
}

unsafe extern "C" fn tree_sitter_malloc(size: usize) -> *mut c_void {
    // SAFETY: forwards the C allocation contract unchanged.
    unsafe { libmimalloc_sys::mi_malloc(size) }
}

unsafe extern "C" fn tree_sitter_calloc(count: usize, size: usize) -> *mut c_void {
    // SAFETY: forwards the C allocation contract unchanged.
    unsafe { libmimalloc_sys::mi_calloc(count, size) }
}

unsafe extern "C" fn tree_sitter_realloc(pointer: *mut c_void, size: usize) -> *mut c_void {
    // SAFETY: `pointer` came from one of these functions (see `configure`).
    unsafe { libmimalloc_sys::mi_realloc(pointer, size) }
}

unsafe extern "C" fn tree_sitter_free(pointer: *mut c_void) {
    // SAFETY: `pointer` came from one of these functions (see `configure`).
    unsafe { libmimalloc_sys::mi_free(pointer) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_daemon_subcommand_picks_mimalloc() {
        assert!(is_daemon_command_line(
            b"/home/u/.zed_server/server\0daemon\0--log-file\0/l\0--state-dir\0/s\0"
        ));
        assert!(is_daemon_command_line(b"server\0daemon\0"));
        assert!(!is_daemon_command_line(
            b".zed_server/server\0proxy\0--identifier\0abc\0"
        ));
        assert!(!is_daemon_command_line(
            b"server\0terminal-session\0attach\0"
        ));
        assert!(!is_daemon_command_line(b"server\0--askpass\0/tmp/s\0"));
        assert!(!is_daemon_command_line(
            b"server\0--remote-sandbox-helper\0daemon\0"
        ));
        assert!(!is_daemon_command_line(b"server\0daemons\0"));
        assert!(!is_daemon_command_line(b"server\0"));
        assert!(!is_daemon_command_line(b""));
    }

    #[test]
    fn a_command_line_cut_off_inside_the_subcommand_is_not_the_daemon() {
        assert!(!is_daemon_command_line(b"server\0daemon"));
        assert!(!is_daemon_command_line(b"server\0dae"));
    }

    #[test]
    fn a_test_binary_keeps_the_system_allocator() {
        assert_eq!(choice(), SYSTEM);
    }

    #[test]
    fn the_purge_delay_option_is_the_one_configured() {
        // mimalloc's documented default for `purge_delay`; another value means
        // the enum moved and `configure` would change some other option.
        // SAFETY: reading an option has no preconditions.
        let purge_delay = unsafe { libmimalloc_sys::mi_option_get(MI_OPTION_PURGE_DELAY) };
        assert_eq!(purge_delay, 10);
    }
}
