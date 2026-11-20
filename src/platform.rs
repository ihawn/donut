//! Port of donut/platform.py. Small OS-specific locking and process-lifecycle operations.

use std::process::Command;

/// Start the child in its own process group (session on Unix), so stop_process can end its whole tree.
pub fn process_options(command: &mut Command) -> &mut Command {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    command
}
