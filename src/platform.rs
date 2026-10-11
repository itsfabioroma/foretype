//! Which job is in the foreground of the terminal foretype provides.

use std::os::fd::{AsRawFd, BorrowedFd};

/// The foreground process group of the terminal behind `master`. Each job a shell
/// runs gets its own group, so a change means a different program is now typing.
pub fn foreground_group(master: BorrowedFd<'_>) -> i32 {
    // SAFETY: tcgetpgrp only reads the terminal's foreground process group.
    unsafe { libc::tcgetpgrp(master.as_raw_fd()) }.max(0)
}
