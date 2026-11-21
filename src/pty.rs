use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, SyncSender};

use std::os::unix::net::UnixStream;

#[derive(Default)]
pub struct LaunchOptions {
    pub directory: Option<PathBuf>,
    pub command: Vec<OsString>,
}

pub struct Session {
    pub output: Receiver<Vec<u8>>,
    pub wake: UnixStream,
    input: SyncSender<Vec<u8>>,
    master: File,
    child: Child,
    startup: Option<PathBuf>,
    pub integrated: bool,
    direct_program: bool,
}

fn size(cols: usize, rows: usize) -> libc::winsize {
    libc::winsize {
        ws_row: rows as u16,
        ws_col: cols as u16,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

fn open(cols: usize, rows: usize) -> io::Result<(File, File)> {
    let mut master = -1;
    let mut slave = -1;
    // openpty initializes both descriptors on success. File owns each descriptor thereafter.
    unsafe {
        if libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &size(cols, rows),
        ) == -1
        {
            return Err(io::Error::last_os_error());
        }
        let master = File::from_raw_fd(master);
        let slave = File::from_raw_fd(slave);
        for file in [&master, &slave] {
            if libc::fcntl(file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) == -1 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok((master, slave))
    }
}

impl Session {
    pub fn spawn(cols: usize, rows: usize, launch: &LaunchOptions) -> io::Result<Self> {
        let (wake, mut notify) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        notify.set_nonblocking(true)?;
        let shell = std::env::var_os("SHELL")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/bin/bash".into());
        let program = launch.command.split_first();
        let integrated = program.is_none()
            && std::path::Path::new(&shell)
                .file_name()
                .is_some_and(|name| name == "bash");
        let (master, slave) = open(cols, rows)?;
        let mut reader = master.try_clone()?;
        let mut writer = master.try_clone()?;
        let mut command = if let Some((executable, args)) = program {
            let mut command = Command::new(executable);
            command.args(args);
            command
        } else {
            Command::new(shell)
        };
        if let Some(directory) = &launch.directory {
            command.current_dir(directory);
        } else if program.is_none()
            && let Some(home) = std::env::home_dir().filter(|path| path.is_absolute())
        {
            command.current_dir(home);
        }
        let mut startup = None;
        if integrated {
            let mut template = std::env::temp_dir()
                .join("stride-bash-XXXXXX")
                .into_os_string()
                .into_encoded_bytes();
            template.push(0);
            // mkstemp creates the file atomically with mode 0600.
            let fd = unsafe { libc::mkstemp(template.as_mut_ptr().cast()) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            use std::os::unix::ffi::OsStringExt;
            template.pop();
            let path = PathBuf::from(std::ffi::OsString::from_vec(template));
            let mut file = unsafe { File::from_raw_fd(fd) };
            if let Err(error) = file.write_all(include_bytes!("bashrc")) {
                let _ = std::fs::remove_file(&path);
                return Err(error);
            }
            command.arg("--rcfile").arg(&path);
            startup = Some(path);
        }
        if program.is_none() {
            command.arg("-i");
        }
        command
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        // The child gets a session and a controlling tty for job control and Ctrl+C.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                if let Some(path) = startup {
                    let _ = std::fs::remove_file(path);
                }
                return Err(error);
            }
        };
        drop(command);
        let (sender, output) = mpsc::sync_channel(32);
        std::thread::spawn(move || {
            let mut buffer = [0; 16384];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        if sender.send(buffer[..n].to_vec()).is_err() {
                            return;
                        }
                        let _ = notify.write(&[1]);
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            drop(sender);
            let _ = notify.write(&[1]);
        });
        let (input, receiver) = mpsc::sync_channel::<Vec<u8>>(256);
        std::thread::spawn(move || {
            while let Ok(bytes) = receiver.recv() {
                if writer.write_all(&bytes).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            output,
            wake,
            input,
            master,
            child,
            startup,
            integrated,
            direct_program: program.is_some(),
        })
    }

    pub fn send(&self, bytes: Vec<u8>) -> bool {
        self.input.try_send(bytes).is_ok()
    }

    pub fn shell_foreground(&self) -> bool {
        // The kernel tracks the foreground process group for this PTY.
        unsafe { libc::tcgetpgrp(self.master.as_raw_fd()) == self.child.id() as libc::pid_t }
    }

    pub fn foreground_program(&self) -> Option<String> {
        let group = unsafe { libc::tcgetpgrp(self.master.as_raw_fd()) };
        if group <= 0 || (!self.direct_program && group == self.child.id() as libc::pid_t) {
            return None;
        }
        let name = std::fs::read_to_string(format!("/proc/{group}/comm")).ok()?;
        let name: String = name
            .trim()
            .chars()
            .filter(|ch| !ch.is_control())
            .take(48)
            .collect();
        (!name.is_empty()).then_some(name)
    }

    pub fn running_programs(&self) -> Vec<String> {
        let shell = self.child.id();
        let group = unsafe { libc::tcgetpgrp(self.master.as_raw_fd()) };
        let mut programs = Vec::new();
        if group > 0 && (self.direct_program || group as u32 != shell) {
            programs.push(
                self.foreground_program()
                    .unwrap_or_else(|| "Foreground process".into()),
            );
        }
        let children = std::fs::read_to_string(format!("/proc/{shell}/task/{shell}/children"))
            .unwrap_or_default();
        for child in children.split_whitespace() {
            let Ok(pid) = child.parse::<u32>() else {
                continue;
            };
            if Some(pid) == u32::try_from(group).ok() {
                continue;
            }
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                continue;
            };
            let Some((name, rest)) = stat.rsplit_once(") ") else {
                continue;
            };
            let mut fields = rest.split_whitespace();
            let state = fields.next();
            let parent = fields.next().and_then(|value| value.parse::<u32>().ok());
            let job = fields.next().and_then(|value| value.parse::<u32>().ok());
            // Ignore the prompt hook's short-lived children in the shell's own group.
            if matches!(state, None | Some("Z" | "X"))
                || parent != Some(shell)
                || job == Some(shell)
            {
                continue;
            }
            let label: String = name
                .split_once('(')
                .map_or("", |(_, name)| name)
                .chars()
                .filter(|ch| !ch.is_control())
                .take(48)
                .collect();
            if !label.is_empty() && !programs.contains(&label) {
                programs.push(label);
            }
        }
        programs
    }

    pub fn sudo_has_hidden_input(&self) -> bool {
        let fd = self.master.as_raw_fd();
        // Only the foreground job on this PTY may receive the password.
        let process = unsafe { libc::tcgetpgrp(fd) };
        if process <= 0 || process == self.child.id() as libc::pid_t {
            return false;
        }
        let mut settings = std::mem::MaybeUninit::<libc::termios>::uninit();
        if unsafe { libc::tcgetattr(fd, settings.as_mut_ptr()) } != 0 {
            return false;
        }
        if unsafe { settings.assume_init() }.c_lflag & libc::ECHO != 0 {
            return false;
        }
        let Ok(status) = std::fs::read_to_string(format!("/proc/{process}/status")) else {
            return false;
        };
        is_privileged_sudo(&status, unsafe { libc::getuid() })
            && unsafe { libc::tcgetpgrp(fd) } == process
    }

    pub fn resize(&self, cols: usize, rows: usize) {
        // The kernel signals the foreground process group when its window size changes.
        unsafe {
            libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &size(cols, rows));
        }
    }
}

fn is_privileged_sudo(status: &str, uid: libc::uid_t) -> bool {
    let mut sudo = false;
    let mut privileged = false;
    for line in status.lines() {
        if let Some(name) = line.strip_prefix("Name:") {
            sudo = name.trim() == "sudo";
        }
        if let Some(ids) = line.strip_prefix("Uid:") {
            let mut ids = ids
                .split_whitespace()
                .map(|id| id.parse::<libc::uid_t>().ok());
            let real = ids.next().flatten();
            let effective = ids.next().flatten();
            let saved = ids.next().flatten();
            privileged = real == Some(uid) && (effective == Some(0) || saved == Some(0));
        }
    }
    sudo && privileged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launches_a_program_with_its_arguments_and_working_directory() {
        let launch = LaunchOptions {
            directory: Some("/tmp".into()),
            command: [
                "/bin/sh",
                "-c",
                r#"printf '%s|%s' "$PWD" "$1""#,
                "sh",
                "two words; *",
            ]
            .into_iter()
            .map(OsString::from)
            .collect(),
        };
        let session = Session::spawn(80, 24, &launch).unwrap();
        assert!(!session.integrated);
        let mut output = Vec::new();
        while !output.ends_with(b"/tmp|two words; *") {
            let chunk = session
                .output
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
            output.extend(chunk);
        }
        assert_eq!(output, b"/tmp|two words; *");
    }

    #[test]
    fn requires_kernel_privilege_for_sudo_identity() {
        let status = |name, effective, saved| {
            format!("Name:\t{name}\nUid:\t1000 {effective} {saved} 1000\n")
        };
        assert!(is_privileged_sudo(&status("sudo", 0, 0), 1000));
        assert!(is_privileged_sudo(&status("sudo", 1000, 0), 1000));
        assert!(!is_privileged_sudo(&status("sudo", 1000, 1000), 1000));
        assert!(!is_privileged_sudo(&status("other", 0, 0), 1000));
        assert!(!is_privileged_sudo(&status("sudo", 0, 0), 1001));
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(path) = &self.startup {
            let _ = std::fs::remove_file(path);
        }
    }
}
