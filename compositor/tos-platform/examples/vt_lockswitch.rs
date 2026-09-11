//! Does the kernel clear `vt_dont_switch` when the process that set it dies?
//!
//! `VT_LOCKSWITCH` sets one global kernel flag. Nothing owns it. If the flag
//! outlives the process that set it, a compositor that takes it and is then
//! killed leaves a machine whose virtual terminals cannot be reached until it
//! reboots. This answers that question on whatever kernel it is run on, and
//! then answers the one that follows it: what the flag costs, which is the
//! only way a crashed compositor's console is ever recovered.
//!
//! Run it as root on a machine with virtual terminals:
//!
//! ```text
//! cargo +1.98.1 build --release --example vt_lockswitch -p tos-platform
//! sudo ./target/release/examples/vt_lockswitch
//! ```
//!
//! It switches the console to a free VT and back several times, so run it
//! from a VT or over ssh, not from a session that minds losing the screen for
//! a second. It puts everything back before it exits unless `--leave-locked`
//! says otherwise; `--unlock` clears the flag and does nothing else, and
//! `--active` prints the VT the console is on, which is how a human checks
//! whether a Ctrl+Alt+Fn did anything.

use std::ffi::CString;
use std::io;
use std::os::unix::io::RawFd;
use std::process::ExitCode;

// From <linux/kd.h> and <linux/vt.h>.
const KDSETMODE: u64 = 0x4b3a;
const KDSKBMODE: u64 = 0x4b45;
const VT_OPENQRY: u64 = 0x5600;
const VT_SETMODE: u64 = 0x5602;
const VT_GETSTATE: u64 = 0x5603;
const VT_ACTIVATE: u64 = 0x5606;
const VT_LOCKSWITCH: u64 = 0x560b;
const VT_UNLOCKSWITCH: u64 = 0x560c;

const KD_TEXT: libc::c_long = 0x00;
const KD_GRAPHICS: libc::c_long = 0x01;
const K_XLATE: libc::c_long = 0x01;
const K_OFF: libc::c_long = 0x04;
const VT_AUTO: u8 = 0x00;
const VT_PROCESS: u8 = 0x01;

/// How long a switch is given to land before it is called blocked. The kernel
/// does the switch from a work queue, so `VT_ACTIVATE` returning is not the
/// switch having happened.
const SWITCH_TIMEOUT_MS: u32 = 2000;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct VtMode {
    mode: u8,
    waitv: u8,
    relsig: i16,
    acqsig: i16,
    frsig: i16,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct VtState {
    v_active: u16,
    v_signal: u16,
    v_state: u16,
}

fn ioctl_ptr<T>(fd: RawFd, request: u64, arg: &mut T) -> io::Result<()> {
    if unsafe { libc::ioctl(fd, request as _, arg as *mut T) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn ioctl_value(fd: RawFd, request: u64, arg: libc::c_long) -> io::Result<()> {
    if unsafe { libc::ioctl(fd, request as _, arg) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn open_console() -> io::Result<RawFd> {
    let path = CString::new("/dev/tty0").expect("no interior nul");
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

fn active_vt(fd: RawFd) -> io::Result<u16> {
    let mut state = VtState::default();
    ioctl_ptr(fd, VT_GETSTATE, &mut state)?;
    Ok(state.v_active)
}

/// The lowest VT the kernel has no console on, which is where this switches
/// to so that it disturbs nothing that is running.
fn free_vt(fd: RawFd) -> io::Result<u16> {
    let mut number: libc::c_int = 0;
    ioctl_ptr(fd, VT_OPENQRY, &mut number)?;
    if number < 1 {
        return Err(io::Error::other("no free virtual terminal"));
    }
    Ok(number as u16)
}

fn sleep_ms(ms: u32) {
    let request = libc::timespec {
        // `as _` because musl and glibc disagree about the width of both of
        // these, and the ISO builds against musl.
        tv_sec: (ms / 1000) as _,
        tv_nsec: ((ms % 1000) * 1_000_000) as _,
    };
    unsafe { libc::nanosleep(&request, std::ptr::null_mut()) };
}

/// What asking for a switch actually did.
struct Switch {
    /// What `VT_ACTIVATE` itself answered. The kernel throws away
    /// `set_console`'s return value, so this is expected to be `Ok` whether
    /// the switch happens or not — which is the point of measuring it.
    activate: io::Result<()>,
    /// Whether the console really ended up on the requested terminal.
    landed: bool,
}

impl Switch {
    fn describe(&self) -> String {
        let activate = match &self.activate {
            Ok(()) => "VT_ACTIVATE returned 0".to_string(),
            Err(e) => format!("VT_ACTIVATE failed: {e}"),
        };
        let landed = if self.landed {
            "the console switched"
        } else {
            "the console did not move"
        };
        format!("{activate}, {landed}")
    }
}

/// Ask for a switch to `target` and wait to see whether it lands.
fn try_switch(fd: RawFd, target: u16) -> Switch {
    let activate = ioctl_value(fd, VT_ACTIVATE, target as libc::c_long);
    let mut waited = 0;
    while waited < SWITCH_TIMEOUT_MS {
        if active_vt(fd).map(|vt| vt == target).unwrap_or(false) {
            return Switch {
                activate,
                landed: true,
            };
        }
        sleep_ms(20);
        waited += 20;
    }
    Switch {
        activate,
        landed: false,
    }
}

/// What the process that is about to be killed does to the terminal first.
#[derive(Clone, Copy)]
struct Victim {
    /// Take the terminal the way tOS does: graphics mode, keyboard off,
    /// switches under `VT_PROCESS` control.
    take_over: bool,
    /// Take `VT_LOCKSWITCH`.
    lock: bool,
}

/// Fork a process that does what `Victim` says and then waits to be killed.
///
/// It reports through a pipe rather than a file, because the whole point is
/// that it is about to die without running anything of its own.
fn spawn_victim(victim: Victim) -> io::Result<(libc::pid_t, io::Result<()>)> {
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let (read_fd, write_fd) = (fds[0], fds[1]);
    // Opened before the fork so that the child, which is about to be killed
    // without running anything of its own, allocates nothing after it.
    let console = open_console()?;
    let mut mode = VtMode {
        mode: VT_PROCESS,
        waitv: 0,
        relsig: libc::SIGUSR1 as i16,
        acqsig: libc::SIGUSR2 as i16,
        frsig: 0,
    };

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        let e = io::Error::last_os_error();
        unsafe {
            libc::close(read_fd);
            libc::close(write_fd);
            libc::close(console);
        }
        return Err(e);
    }

    if pid == 0 {
        // Child. Only async-signal-safe calls from here: no allocation, no
        // destructors, and no orderly exit — it exists to be SIGKILLed.
        unsafe {
            libc::close(read_fd);
            libc::signal(libc::SIGUSR1, libc::SIG_IGN);
            libc::signal(libc::SIGUSR2, libc::SIG_IGN);
            let mut ok = true;
            if victim.take_over {
                ok &= libc::ioctl(console, KDSETMODE as _, KD_GRAPHICS) == 0;
                ok &= libc::ioctl(console, KDSKBMODE as _, K_OFF) == 0;
                ok &= libc::ioctl(console, VT_SETMODE as _, &mut mode as *mut VtMode) == 0;
            }
            if victim.lock {
                ok &= libc::ioctl(console, VT_LOCKSWITCH as _, 0) == 0;
            }
            let answer = [if ok { b'y' } else { b'n' }];
            libc::write(write_fd, answer.as_ptr() as *const libc::c_void, 1);
            loop {
                libc::pause();
            }
        }
    }

    unsafe {
        libc::close(write_fd);
        libc::close(console);
    }
    let mut answer = [0u8; 1];
    let read = unsafe { libc::read(read_fd, answer.as_mut_ptr() as *mut libc::c_void, 1) };
    unsafe { libc::close(read_fd) };
    let ready = if read == 1 && answer[0] == b'y' {
        Ok(())
    } else if read == 1 {
        Err(io::Error::other(
            "the child could not set the terminal up (VT_LOCKSWITCH needs CAP_SYS_TTY_CONFIG)",
        ))
    } else {
        Err(io::Error::other("the child said nothing"))
    };
    Ok((pid, ready))
}

fn kill_and_reap(pid: libc::pid_t) -> io::Result<()> {
    if unsafe { libc::kill(pid, libc::SIGKILL) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut status: libc::c_int = 0;
    if unsafe { libc::waitpid(pid, &mut status, 0) } != pid {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn kernel_release() -> String {
    let mut info: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut info) } < 0 {
        return "unknown".to_string();
    }
    let bytes: Vec<u8> = info
        .release
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Clear the flag, from a process that is not the one that set it.
fn unlock(fd: RawFd) -> io::Result<()> {
    ioctl_value(fd, VT_UNLOCKSWITCH, 0)
}

/// Put the terminal back into the state a console expects, for the runs where
/// the kernel did not do it itself.
fn restore_console(fd: RawFd) {
    let mut mode = VtMode {
        mode: VT_AUTO,
        ..VtMode::default()
    };
    let _ = ioctl_ptr(fd, VT_SETMODE, &mut mode);
    let _ = ioctl_value(fd, KDSKBMODE, K_XLATE);
    let _ = ioctl_value(fd, KDSETMODE, KD_TEXT);
}

/// Part one: does the flag outlive the process that set it?
fn part_one(fd: RawFd, home: u16, scratch: u16, leave_locked: bool) -> Result<bool, String> {
    println!("Part one: does vt_dont_switch outlive the process that set it?");
    println!();

    let baseline = try_switch(fd, scratch);
    println!("1. baseline            {}", baseline.describe());
    if !baseline.landed {
        return Err(format!(
            "switching is already blocked before the experiment starts. Something \
             else holds vt_dont_switch, or VT {scratch} cannot be reached. Try \
             `vt_lockswitch --unlock`."
        ));
    }
    let back = try_switch(fd, home);
    if !back.landed {
        return Err(format!(
            "could not get back to VT {home}: {}",
            back.describe()
        ));
    }

    let victim = Victim {
        take_over: false,
        lock: true,
    };
    let (pid, ready) = spawn_victim(victim).map_err(|e| format!("cannot fork: {e}"))?;
    if let Err(e) = ready {
        let _ = kill_and_reap(pid);
        return Err(format!("{e}"));
    }
    println!("2. child locked        VT_LOCKSWITCH taken by pid {pid}");

    let while_locked = try_switch(fd, scratch);
    println!("3. while locked        {}", while_locked.describe());
    if while_locked.landed {
        let _ = kill_and_reap(pid);
        let _ = try_switch(fd, home);
        let _ = unlock(fd);
        return Err("VT_LOCKSWITCH did not stop a switch at all".to_string());
    }

    kill_and_reap(pid).map_err(|e| format!("cannot kill the child: {e}"))?;
    println!("4. child SIGKILLed     pid {pid} reaped");

    let after_death = try_switch(fd, scratch);
    println!("5. after the kill      {}", after_death.describe());
    let survives = !after_death.landed;

    if leave_locked {
        println!();
        println!("--leave-locked: the flag is still set. Press Ctrl+Alt+F2 to see what");
        println!("a user would see, then clear it with:  vt_lockswitch --unlock");
        return Ok(survives);
    }

    unlock(fd).map_err(|e| format!("VT_UNLOCKSWITCH failed: {e}"))?;
    let recovered = try_switch(fd, scratch);
    println!("6. unlocked elsewhere  {}", recovered.describe());
    if !recovered.landed {
        return Err(
            "VT_UNLOCKSWITCH did not give switching back. This machine needs a reboot.".to_string(),
        );
    }
    let home_again = try_switch(fd, home);
    println!("7. back home           {}", home_again.describe());

    Ok(survives)
}

/// One run of part two: kill a process that had taken the terminal the way
/// tOS does, and see whether another process can still rescue the console.
fn rescue_run(fd: RawFd, home: u16, scratch: u16, lock: bool) -> Result<bool, String> {
    let victim = Victim {
        take_over: true,
        lock,
    };
    let (pid, ready) = spawn_victim(victim).map_err(|e| format!("cannot fork: {e}"))?;
    if let Err(e) = ready {
        let _ = kill_and_reap(pid);
        let _ = unlock(fd);
        restore_console(fd);
        return Err(format!("{e}"));
    }
    kill_and_reap(pid).map_err(|e| format!("cannot kill the child: {e}"))?;

    let rescue = try_switch(fd, scratch);
    let rescued = rescue.landed;
    println!(
        "   {:<22} {}",
        if lock {
            "with VT_LOCKSWITCH"
        } else {
            "without it"
        },
        rescue.describe()
    );

    // The kernel only resets the terminal on a switch it actually performs,
    // so the locked run leaves it graphics-mode and keyboard-off until this.
    let _ = unlock(fd);
    if !rescued {
        restore_console(fd);
    }
    let back = try_switch(fd, home);
    if !back.landed {
        return Err(format!(
            "could not get back to VT {home}: {}",
            back.describe()
        ));
    }
    if rescued {
        restore_console(fd);
    }
    Ok(rescued)
}

/// Part two: what the flag costs, which is the rescue path.
fn part_two(fd: RawFd, home: u16, scratch: u16) -> Result<(bool, bool), String> {
    println!();
    println!("Part two: after a compositor dies holding the terminal, can another");
    println!("process still reach a console?");
    println!();
    let without = rescue_run(fd, home, scratch, false)?;
    let with = rescue_run(fd, home, scratch, true)?;
    Ok((without, with))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let leave_locked = args.iter().any(|a| a == "--leave-locked");
    let unlock_only = args.iter().any(|a| a == "--unlock");
    let active_only = args.iter().any(|a| a == "--active");

    let fd = match open_console() {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("vt_lockswitch: cannot open /dev/tty0: {e}");
            eprintln!("This needs root and a machine with virtual terminals.");
            return ExitCode::from(2);
        }
    };

    if active_only {
        return match active_vt(fd) {
            Ok(vt) => {
                println!("{vt}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("vt_lockswitch: VT_GETSTATE failed: {e}");
                ExitCode::from(2)
            }
        };
    }

    if unlock_only {
        return match unlock(fd) {
            Ok(()) => {
                println!("vt_dont_switch cleared");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("vt_lockswitch: VT_UNLOCKSWITCH failed: {e}");
                ExitCode::from(2)
            }
        };
    }

    println!("VT_LOCKSWITCH after the setting process dies");
    println!("kernel   {}", kernel_release());
    let home = match active_vt(fd) {
        Ok(vt) => vt,
        Err(e) => {
            eprintln!("vt_lockswitch: cannot read the active VT: {e}");
            return ExitCode::from(2);
        }
    };
    let scratch = match free_vt(fd) {
        Ok(vt) => vt,
        Err(e) => {
            eprintln!("vt_lockswitch: cannot find a free VT: {e}");
            return ExitCode::from(2);
        }
    };
    println!("console  /dev/tty0, active VT {home}, scratch VT {scratch}");
    println!();

    let survives = match part_one(fd, home, scratch, leave_locked) {
        Ok(survives) => survives,
        Err(e) => {
            eprintln!();
            eprintln!("vt_lockswitch: {e}");
            return ExitCode::from(2);
        }
    };
    if leave_locked {
        return ExitCode::SUCCESS;
    }
    let rescue = match part_two(fd, home, scratch) {
        Ok(rescue) => rescue,
        Err(e) => {
            eprintln!();
            eprintln!("vt_lockswitch: {e}");
            return ExitCode::from(2);
        }
    };

    println!();
    if survives {
        println!("VERDICT: the kernel does NOT clear vt_dont_switch when the process that");
        println!("set it is killed. It is cleared by VT_UNLOCKSWITCH or by a reboot, and");
        println!("by nothing else.");
    } else {
        println!("VERDICT: the kernel DOES clear vt_dont_switch when the process that set");
        println!("it is killed. Taking VT_LOCKSWITCH is safe against a crash on this");
        println!("kernel.");
    }
    match rescue {
        (true, false) => {
            println!("A killed compositor's console is recoverable by VT_ACTIVATE from");
            println!("another process, and taking VT_LOCKSWITCH is what removes that.");
        }
        (true, true) => println!("VT_LOCKSWITCH did not cost the rescue path on this kernel."),
        (false, _) => println!("The console was not recoverable either way on this kernel."),
    }
    ExitCode::SUCCESS
}
