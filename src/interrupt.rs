//! Interrupt (Ctrl+C / SIGTERM / SIGHUP) handling for the parts of nawa that
//! change the system.
//!
//! The problem: with the default disposition, SIGINT kills nawa on the spot.
//! In the middle of an install that means files moved into place but no db
//! entry (or the other way around), leftover staging directories, and a
//! half-written transaction.
//!
//! The approach has two layers:
//!
//! 1. **Deferral** (this module). While a [`Guard`] is alive, SIGINT, SIGTERM
//!    and SIGHUP do not kill the process; they set a flag. Code that is in a
//!    state it can cleanly leave calls [`check`], which turns the flag into an
//!    [`Interrupted`] error so the normal error path rolls the transaction
//!    back. Code past the point of no return (db entry written) never checks
//!    and simply finishes. Pressing Ctrl+C three times forces an immediate
//!    exit; that is safe because of layer 2.
//! 2. **Crash recovery** (`install::recover`). Every file move is written to an
//!    on-disk journal first, so even SIGKILL or a power cut is repaired by the
//!    next nawa that takes the database lock.
//!
//! Outside a [`Guard`] the signals keep their default behaviour, so
//! `nawa search`, a prompt waiting for `y/N`, or a download still stop
//! immediately on Ctrl+C: none of them has anything half-done to protect.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;

static INTERRUPTED: AtomicBool = AtomicBool::new(false);
static SIGNALS: AtomicU32 = AtomicU32::new(0);

const SIGNUMS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

/// How many signals force an immediate exit (the first ones are deferred).
const FORCE_AFTER: u32 = 3;

const FIRST_MSG: &[u8] = b"\nnawa: interrupt received - finishing the current step safely \
(nothing is left half-installed; press Ctrl+C again to force quit)\n";
const FORCE_MSG: &[u8] = b"\nnawa: forced exit - the next nawa run will repair any \
interrupted transaction\n";

/// The error returned by [`check`]; `main` turns it into exit status 130.
#[derive(Debug)]
pub struct Interrupted;

impl fmt::Display for Interrupted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "interrupted")
    }
}

impl std::error::Error for Interrupted {}

/// True once a deferred signal has arrived in this process.
pub fn is_interrupted() -> bool {
    #[cfg(test)]
    if TEST_INTERRUPT.with(|c| c.get()) {
        return true;
    }
    INTERRUPTED.load(Ordering::Relaxed)
}

// Unit tests run on parallel threads of one process, so a test that
// simulates Ctrl+C must not trip every other test's `check()`: in test
// builds the simulated flag is per-thread.
#[cfg(test)]
thread_local! {
    static TEST_INTERRUPT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Test helper: behave as if Ctrl+C arrived on this thread.
#[cfg(test)]
pub(crate) fn simulate_interrupt() {
    TEST_INTERRUPT.with(|c| c.set(true));
}

/// Test helper: undo [`simulate_interrupt`].
#[cfg(test)]
pub(crate) fn clear_simulated_interrupt() {
    TEST_INTERRUPT.with(|c| c.set(false));
}

/// `Err(Interrupted)` when a deferred signal is pending. Call it only at
/// points where abandoning the work (and rolling it back) is safe.
pub fn check() -> Result<(), Interrupted> {
    if is_interrupted() {
        Err(Interrupted)
    } else {
        Ok(())
    }
}

struct State {
    depth: usize,
    previous: Vec<libc::sigaction>,
}

static STATE: Mutex<State> = Mutex::new(State { depth: 0, previous: Vec::new() });

extern "C" fn on_signal(_sig: libc::c_int) {
    // async-signal-safe only: atomics and write(2)/_exit(2)
    INTERRUPTED.store(true, Ordering::Relaxed);
    let n = SIGNALS.fetch_add(1, Ordering::Relaxed) + 1;
    let (msg, force) = if n >= FORCE_AFTER {
        (FORCE_MSG, true)
    } else if n == 1 {
        (FIRST_MSG, false)
    } else {
        (&b""[..], false)
    };
    if !msg.is_empty() {
        // SAFETY: write(2) on stderr with a valid static buffer.
        unsafe {
            libc::write(libc::STDERR_FILENO, msg.as_ptr() as *const libc::c_void, msg.len());
        }
    }
    if force {
        // SAFETY: _exit is async-signal-safe.
        unsafe { libc::_exit(130) };
    }
}

/// While alive, SIGINT/SIGTERM/SIGHUP are deferred instead of fatal. Guards
/// nest; the previous handlers come back when the outermost one is dropped.
pub struct Guard {
    _private: (),
}

impl Guard {
    pub fn new() -> Guard {
        let mut st = STATE.lock().unwrap_or_else(|p| p.into_inner());
        if st.depth == 0 {
            st.previous.clear();
            for &sig in &SIGNUMS {
                // SAFETY: sigaction with a zeroed struct, our extern "C"
                // handler and SA_RESTART so interrupted syscalls (reads,
                // renames, waits) continue instead of failing with EINTR.
                unsafe {
                    let mut new: libc::sigaction = std::mem::zeroed();
                    new.sa_sigaction = on_signal as *const () as libc::sighandler_t;
                    new.sa_flags = libc::SA_RESTART;
                    libc::sigemptyset(&mut new.sa_mask);
                    let mut old: libc::sigaction = std::mem::zeroed();
                    libc::sigaction(sig, &new, &mut old);
                    st.previous.push(old);
                }
            }
        }
        st.depth += 1;
        Guard { _private: () }
    }
}

impl Default for Guard {
    fn default() -> Self {
        Guard::new()
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        let mut st = STATE.lock().unwrap_or_else(|p| p.into_inner());
        st.depth = st.depth.saturating_sub(1);
        if st.depth == 0 {
            for (i, &sig) in SIGNUMS.iter().enumerate() {
                if let Some(old) = st.previous.get(i) {
                    // SAFETY: restoring exactly what sigaction returned.
                    unsafe {
                        libc::sigaction(sig, old, std::ptr::null_mut());
                    }
                }
            }
            st.previous.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handler_of(sig: libc::c_int) -> libc::sighandler_t {
        // SAFETY: query-only sigaction (null new action).
        unsafe {
            let mut cur: libc::sigaction = std::mem::zeroed();
            libc::sigaction(sig, std::ptr::null(), &mut cur);
            cur.sa_sigaction
        }
    }

    #[test]
    fn guards_install_the_handler_and_nest() {
        let outer = Guard::new();
        let inner = Guard::new();
        for sig in SIGNUMS {
            assert_eq!(handler_of(sig), on_signal as *const () as libc::sighandler_t);
        }
        drop(inner);
        for sig in SIGNUMS {
            assert_eq!(handler_of(sig), on_signal as *const () as libc::sighandler_t);
        }
        drop(outer);
    }

    #[test]
    fn simulated_interrupt_turns_into_an_error_on_this_thread_only() {
        assert!(check().is_ok());
        simulate_interrupt();
        assert!(check().is_err());
        let other = std::thread::spawn(|| check().is_ok()).join().unwrap();
        assert!(other);
        clear_simulated_interrupt();
        assert!(check().is_ok());
    }
}
