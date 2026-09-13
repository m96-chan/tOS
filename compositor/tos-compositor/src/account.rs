//! Who a session's shells run as.
//!
//! [`crate::lock`] answers "whose password does this screen ask for" and reads
//! `/etc/shadow` to do it. This answers the other half of the same question —
//! "and what do they get when they answer it" — and reads `/etc/passwd` and
//! `/etc/group`, which are the files every other program on the machine reads
//! for it.
//!
//! The two are deliberately separate. A login screen that checks a password
//! and then hands out a root shell is a boundary in the drawing only, which is
//! what tOS had: `TOS_USER` named the account, the lock asked for its
//! password, and `/etc/tos-session` ran everything as root anyway. The account
//! name was doing one job when it should have been doing two.
//!
//! ## What still runs as root
//!
//! The compositor. It holds the VT, DRM master and the evdev devices, and
//! nothing on the machine arbitrates those for a process that is not root yet
//! — that is `docs/design/init.md`'s open question about `logind` and `seatd`,
//! and it is #22. So the boundary this module draws is the one a display
//! server has always drawn: the thing that talks to the hardware is
//! privileged, and the programs the person actually types into are not.
//!
//! It is a real boundary rather than a cosmetic one. A shell started through
//! here cannot write `/etc`, cannot read `/etc/shadow`, and cannot signal the
//! compositor. It is not the whole of #22 and does not pretend to be.

use std::io;
use std::path::{Path, PathBuf};

use tos_pty::Credentials;

/// Where the accounts are, when nobody says otherwise.
pub const PASSWD_PATH: &str = "/etc/passwd";
/// Where the group memberships are, when nobody says otherwise.
pub const GROUP_PATH: &str = "/etc/group";

/// An account, as much of one as starting a shell needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    /// `HOME`, and the directory a pane starts in.
    pub home: PathBuf,
    /// The program a pane runs. Taken from the account rather than from the
    /// compositor's own `SHELL`, which is the session script's and says what
    /// root uses.
    pub shell: PathBuf,
    /// Every group but the primary one, which `setgid` handles.
    pub groups: Vec<u32>,
}

/// Why an account could not be read.
#[derive(Debug)]
pub enum Why {
    /// The file is not there, or cannot be opened.
    Unreadable(io::Error),
    /// The file is there and has no line for this account.
    NoSuchAccount,
    /// There is a line and it is not an account: too few fields, or a uid that
    /// is not a number. Refused rather than guessed at — a `/etc/passwd` this
    /// cannot parse is one where picking a uid out of it is picking somebody
    /// at random to be.
    Malformed,
}

impl std::fmt::Display for Why {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Why::Unreadable(e) => write!(f, "cannot read the account file: {e}"),
            Why::NoSuchAccount => write!(f, "no such account"),
            Why::Malformed => write!(f, "the account file has no account on that line"),
        }
    }
}

/// Read `user`'s account out of a passwd file and a group file.
pub fn read(passwd: &Path, group: &Path, user: &str) -> Result<Account, Why> {
    let text = std::fs::read_to_string(passwd).map_err(Why::Unreadable)?;
    let line = text
        .lines()
        .find(|line| line.split(':').next() == Some(user))
        .ok_or(Why::NoSuchAccount)?;

    // name:password:uid:gid:gecos:home:shell. Seven fields, and `splitn` on
    // the last one rather than `split`, because a shell is a path and a path
    // may hold anything a colon is not — which is to say the seventh field is
    // the rest of the line, not the next colon-free run.
    let fields: Vec<&str> = line.splitn(7, ':').collect();
    if fields.len() < 7 {
        return Err(Why::Malformed);
    }
    let uid: u32 = fields[2].parse().map_err(|_| Why::Malformed)?;
    let gid: u32 = fields[3].parse().map_err(|_| Why::Malformed)?;

    // A home or a shell that the file leaves empty is not malformed; it is an
    // account that never had one. The fallbacks are the ones login(1) uses,
    // and they are what makes an account with a blank seventh field usable
    // rather than a pane that exits immediately with 127.
    let home = match fields[5] {
        "" => PathBuf::from("/"),
        dir => PathBuf::from(dir),
    };
    let shell = match fields[6].trim_end() {
        "" => PathBuf::from("/bin/sh"),
        program => PathBuf::from(program),
    };

    Ok(Account {
        name: user.to_string(),
        uid,
        gid,
        home,
        shell,
        groups: supplementary(group, user, gid)?,
    })
}

/// Every group `user` is named in, less the one `setgid` already covers.
///
/// A group file that cannot be read is not an error that stops a login. The
/// account has its primary group either way, and refusing to start a session
/// because `/etc/group` is missing would turn a machine that is merely missing
/// its `video` group into one nobody can log into at all. It is the other way
/// round from `/etc/passwd`, where a missing file means there is no account to
/// become and guessing one would be the bug.
fn supplementary(group: &Path, user: &str, primary: u32) -> Result<Vec<u32>, Why> {
    let Ok(text) = std::fs::read_to_string(group) else {
        return Ok(Vec::new());
    };
    let mut groups = Vec::new();
    for line in text.lines() {
        // name:password:gid:member,member
        let fields: Vec<&str> = line.splitn(4, ':').collect();
        if fields.len() < 4 {
            continue;
        }
        let Ok(gid) = fields[2].parse::<u32>() else {
            continue;
        };
        if gid == primary || groups.contains(&gid) {
            continue;
        }
        if fields[3].split(',').any(|member| member == user) {
            groups.push(gid);
        }
    }
    Ok(groups)
}

/// What to hand a shell, given the account the session belongs to and the uid
/// this process is already running as.
///
/// `None` means "start it as whoever I am", and it is the right answer twice:
///
/// - **This process is not root.** It cannot drop to another uid and must not
///   pretend to. A nested tOS, a test, and anybody running the compositor from
///   their own shell are all here.
/// - **The account is the one already running.** The live image, whose only
///   account is Debian's root and whose `TOS_USER` says so. There is nothing
///   to drop and a `setuid(0)` from root is not a boundary.
///
/// Everything else is a session that was logged into, and gets the account.
pub fn credentials_for(account: &Account, running_as: u32) -> Option<Credentials> {
    if running_as != 0 || account.uid == running_as {
        return None;
    }
    Some(Credentials {
        uid: account.uid,
        gid: account.gid,
        groups: account.groups.clone(),
    })
}

/// The uid this process is running as.
pub fn running_as() -> u32 {
    // Safety: `getuid` cannot fail and touches nothing.
    unsafe { libc::getuid() as u32 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(passwd: &str, group: &str, name: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir();
        let p = dir.join(format!("tos-account-{}-{name}.passwd", std::process::id()));
        let g = dir.join(format!("tos-account-{}-{name}.group", std::process::id()));
        std::fs::write(&p, passwd).expect("passwd");
        std::fs::write(&g, group).expect("group");
        (p, g)
    }

    /// A passwd file laid out the way the installed machine's is: Debian's
    /// accounts first, the person appended to the end.
    const PASSWD: &str = "root:x:0:0:root:/root:/bin/bash\n\
                          daemon:x:1:1:daemon:/usr/sbin:/usr/sbin/nologin\n\
                          tos:x:1000:1000:tos:/home/tos:/bin/bash\n";
    const GROUP: &str = "root:x:0:\n\
                         tty:x:5:\n\
                         video:x:44:tos\n\
                         input:x:104:tos\n\
                         tos:x:1000:\n";

    #[test]
    fn reads_the_account_the_installer_appended() {
        let (p, g) = files(PASSWD, GROUP, "plain");
        let account = read(&p, &g, "tos").expect("account");
        assert_eq!(account.uid, 1000);
        assert_eq!(account.gid, 1000);
        assert_eq!(account.home, PathBuf::from("/home/tos"));
        assert_eq!(account.shell, PathBuf::from("/bin/bash"));
    }

    #[test]
    fn takes_the_supplementary_groups_and_not_the_primary_one() {
        let (p, g) = files(PASSWD, GROUP, "groups");
        let account = read(&p, &g, "tos").expect("account");
        // video and input, because `tos` is named in both. Not 1000, which is
        // the primary group and `setgid`'s job: handing it to `setgroups` too
        // is harmless but says this code cannot tell the two apart.
        assert_eq!(account.groups, vec![44, 104]);
    }

    #[test]
    fn a_line_for_somebody_else_is_not_this_account() {
        let (p, g) = files(PASSWD, GROUP, "other");
        // `root` is the first line in the file, and a reader that took the
        // first line rather than the matching one would pass every other test
        // here.
        let account = read(&p, &g, "root").expect("account");
        assert_eq!(account.uid, 0);
        assert_eq!(account.home, PathBuf::from("/root"));
    }

    #[test]
    fn an_account_that_is_not_there_is_not_invented() {
        let (p, g) = files(PASSWD, GROUP, "absent");
        assert!(matches!(
            read(&p, &g, "nobody-here"),
            Err(Why::NoSuchAccount)
        ));
    }

    #[test]
    fn a_uid_that_is_not_a_number_is_refused_rather_than_guessed() {
        let (p, g) = files("tos:x:notauid:1000:tos:/home/tos:/bin/bash\n", GROUP, "bad");
        assert!(matches!(read(&p, &g, "tos"), Err(Why::Malformed)));
    }

    #[test]
    fn an_empty_home_and_shell_fall_back_rather_than_failing() {
        let (p, g) = files("tos:x:1000:1000:tos::\n", GROUP, "empty");
        let account = read(&p, &g, "tos").expect("account");
        assert_eq!(account.home, PathBuf::from("/"));
        assert_eq!(account.shell, PathBuf::from("/bin/sh"));
    }

    #[test]
    fn a_missing_group_file_still_logs_somebody_in() {
        let (p, _) = files(PASSWD, GROUP, "nogroup");
        let account = read(&p, Path::new("/nonexistent/group"), "tos").expect("account");
        assert_eq!(account.uid, 1000);
        assert!(account.groups.is_empty());
    }

    #[test]
    fn a_process_that_is_not_root_drops_to_nobody() {
        let account = Account {
            name: "tos".into(),
            uid: 1000,
            gid: 1000,
            home: "/home/tos".into(),
            shell: "/bin/bash".into(),
            groups: vec![44],
        };
        // The compositor under `cargo test`, or a nested tOS. It cannot become
        // uid 1000 and must not act as though it had.
        assert_eq!(credentials_for(&account, 1000), None);
        assert_eq!(credentials_for(&account, 501), None);
    }

    #[test]
    fn the_live_images_root_session_has_nothing_to_drop() {
        let root = Account {
            name: "root".into(),
            uid: 0,
            gid: 0,
            home: "/root".into(),
            shell: "/bin/bash".into(),
            groups: Vec::new(),
        };
        assert_eq!(credentials_for(&root, 0), None);
    }

    #[test]
    fn a_root_compositor_hands_the_account_over() {
        let account = Account {
            name: "tos".into(),
            uid: 1000,
            gid: 1000,
            home: "/home/tos".into(),
            shell: "/bin/bash".into(),
            groups: vec![44, 104],
        };
        let credentials = credentials_for(&account, 0).expect("a root session drops");
        assert_eq!(credentials.uid, 1000);
        assert_eq!(credentials.gid, 1000);
        assert_eq!(credentials.groups, vec![44, 104]);
    }
}
