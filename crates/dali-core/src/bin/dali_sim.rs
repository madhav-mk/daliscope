//! Exposes the simulated DALI-2 USB interface on a pseudo-terminal so the app
//! (or any LUBA client) can connect to it as if it were a real serial port.
//!
//!     cargo run -p dali-core --bin dali-sim
//!     -> prints e.g. /dev/pts/5 ; pin that port in Daliscope's settings.

#[cfg(unix)]
fn main() {
    use dali_core::sim::{Sim, SimLink};
    use nix::pty::openpty;
    use nix::sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg};
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::time::Duration;

    let quiet = std::env::args().any(|a| a == "--quiet");
    let empty = std::env::args().any(|a| a == "--unaddressed");
    let pty = openpty(None, None).expect("openpty");
    for fd in [&pty.master, &pty.slave] {
        let mut t = tcgetattr(fd).expect("tcgetattr");
        cfmakeraw(&mut t);
        tcsetattr(fd, SetArg::TCSANOW, &t).expect("tcsetattr");
    }
    let path = nix::unistd::ttyname(&pty.slave).expect("ttyname");
    println!("{}", path.display());
    std::io::stdout().flush().ok();
    if !quiet {
        eprintln!("Simulated DALI-2 USB on {} (Ctrl-C to stop)", path.display());
    }
    // Non-blocking master.
    let fd = pty.master.as_raw_fd();
    let flags = nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_GETFL).unwrap();
    let flags = nix::fcntl::OFlag::from_bits_truncate(flags) | nix::fcntl::OFlag::O_NONBLOCK;
    nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_SETFL(flags)).unwrap();
    let mut master = std::fs::File::from(pty.master);
    let _keep_slave_open = pty.slave;
    let sim = if empty { Sim::unaddressed(4, 2) } else { Sim::demo() };
    let mut link = SimLink::new(sim);
    let mut buf = [0u8; 1024];
    loop {
        match master.read(&mut buf) {
            Ok(n) if n > 0 => link.input(&buf[..n]),
            _ => {}
        }
        let out = link.output();
        if !out.is_empty() {
            let _ = master.write_all(&out);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(not(unix))]
fn main() {
    eprintln!("dali-sim needs a Unix pseudo-terminal");
}
