//! wlpause -- pause a video wallpaper while it is covered.

use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::Duration;

use wlpause::compositor::{self, hyprland, Compositor};
use wlpause::decide::{combine, Decision};
use wlpause::target::{freeze::Freeze, mpv::MpvIpc, Target};

static STOP: AtomicBool = AtomicBool::new(false);
/// Write end of the self-pipe, so a signal can interrupt `poll` immediately.
static SIGPIPE_W: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
    let fd = SIGPIPE_W.load(Ordering::SeqCst);
    if fd >= 0 {
        // write(2) is async-signal-safe; this is the whole point of the
        // self-pipe trick. Without it the daemon sits in a blocking read
        // until the heartbeat expires before noticing SIGTERM, which makes
        // logout wait on us for no reason.
        unsafe {
            libc::write(fd, c"x".as_ptr().cast(), 1);
        }
    }
}

/// Create the shutdown self-pipe, returning its read end.
fn install_signal_pipe() -> io::Result<RawFd> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: pipe2 fills a two-element array we own.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        return Err(io::Error::last_os_error());
    }
    SIGPIPE_W.store(fds[1], Ordering::SeqCst);
    Ok(fds[0])
}

/// Wait for compositor news, the heartbeat, or a shutdown signal.
///
/// `Ok(true)` means the compositor had something to say.
fn wait(comp: &mut dyn Compositor, sig_r: RawFd, timeout: Duration) -> io::Result<bool> {
    let ev = comp.event_fd();
    let mut fds: Vec<libc::pollfd> = Vec::with_capacity(2);
    if let Some(fd) = ev {
        fds.push(libc::pollfd { fd, events: libc::POLLIN, revents: 0 });
    }
    fds.push(libc::pollfd { fd: sig_r, events: libc::POLLIN, revents: 0 });

    let ms = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
    // SAFETY: fds is a valid, correctly-sized array of pollfd for its length.
    let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
    if n < 0 {
        let e = io::Error::last_os_error();
        if e.kind() == io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(e);
    }
    if n == 0 {
        return Ok(false); // heartbeat
    }
    if ev.is_some() && fds[0].revents != 0 {
        comp.drain_events()?;
        return Ok(true);
    }
    Ok(false)
}

const HELP: &str = "\
wlpause -- pause a video wallpaper while it is covered

USAGE:
    wlpause [OPTIONS]

Measures how much of the wallpaper surface is actually hidden by windows and
by opaque layer-shell surfaces such as a bar, then pauses the player once the
covered fraction crosses a threshold. Unlike counting windows, a small
floating window will not freeze a wallpaper you can still see.

OPTIONS:
    -s, --mpv-socket <PATH>   mpv IPC socket. Default: discovered from the
                              wallpaper process's own command line, so
                              `mpvpaper -o '... input-ipc-server=/tmp/mpvsocket'`
                              needs no flag here.
    -t, --threshold <0..1>    Covered fraction at which to pause [default: 0.90]
                              1.0 means \"only when nothing at all is visible\";
                              gaps between tiled windows keep it below 1.0.
    -n, --namespace <NAME>    Wallpaper layer-shell namespace [default: mpvpaper]
        --alpha-min <0..1>    Ignore layer surfaces more transparent than this
                              when deciding what counts as cover [default: 1.0]
        --heartbeat <SECS>    Re-check even without events [default: 10]
        --debounce <MS>       Settle time after an event burst [default: 250]
        --wait <SECS>         How long to wait for the wallpaper at startup
                              [default: 30]
        --freeze              Also SIGSTOP the wallpaper process while it is
                              hidden. Pausing mpv stops decoding, but its
                              video output keeps redrawing the same frame
                              because the compositor keeps sending frame
                              callbacks; freezing removes that too. Measured
                              fully covered: 40% of a core with no pauser,
                              10% with --no-freeze, 0% with --freeze.
        --no-resume-on-exit   Leave the wallpaper paused when wlpause exits.
                              By default it is resumed, so killing wlpause
                              never leaves a frozen wallpaper behind.
        --once                Report one decision and exit. Implies --verbose.
        --dry-run             Decide and log, but never touch the player.
    -v, --verbose             Log every decision.
    -h, --help                Show this help.
    -V, --version             Show version.
";

struct Opts {
    threshold: f64,
    namespace: String,
    alpha_min: f64,
    heartbeat: Duration,
    debounce: Duration,
    wait: Duration,
    mpv_socket: Option<PathBuf>,
    dry_run: bool,
    verbose: bool,
    once: bool,
    resume_on_exit: bool,
    freeze: bool,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            threshold: 0.90,
            namespace: "mpvpaper".into(),
            alpha_min: 1.0,
            heartbeat: Duration::from_secs(10),
            debounce: Duration::from_millis(250),
            wait: Duration::from_secs(30),
            mpv_socket: None,
            dry_run: false,
            verbose: false,
            once: false,
            resume_on_exit: true,
            freeze: false,
        }
    }
}

fn parse_args() -> Result<Option<Opts>, String> {
    let mut o = Opts::default();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = |name: &str| -> Result<String, String> {
            args.next()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match a.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("wlpause {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "-s" | "--mpv-socket" => o.mpv_socket = Some(PathBuf::from(val("--mpv-socket")?)),
            "-t" | "--threshold" => {
                o.threshold = val("--threshold")?
                    .parse()
                    .map_err(|_| "--threshold must be a number".to_string())?
            }
            "-n" | "--namespace" => o.namespace = val("--namespace")?,
            "--alpha-min" => {
                o.alpha_min = val("--alpha-min")?
                    .parse()
                    .map_err(|_| "--alpha-min must be a number".to_string())?
            }
            "--heartbeat" => {
                o.heartbeat = Duration::from_secs_f64(
                    val("--heartbeat")?
                        .parse()
                        .map_err(|_| "--heartbeat must be a number".to_string())?,
                )
            }
            "--debounce" => {
                o.debounce = Duration::from_millis(
                    val("--debounce")?
                        .parse()
                        .map_err(|_| "--debounce must be a whole number of ms".to_string())?,
                )
            }
            "--wait" => {
                o.wait = Duration::from_secs_f64(
                    val("--wait")?
                        .parse()
                        .map_err(|_| "--wait must be a number".to_string())?,
                )
            }
            "--freeze" => o.freeze = true,
            "--no-resume-on-exit" => o.resume_on_exit = false,
            "--once" => {
                o.once = true;
                o.verbose = true;
            }
            "--dry-run" => o.dry_run = true,
            "-v" | "--verbose" => o.verbose = true,
            other => return Err(format!("unknown option: {other}")),
        }
    }
    if !(0.0..=1.0).contains(&o.threshold) {
        return Err("--threshold must be between 0 and 1".into());
    }
    if !(0.0..=1.0).contains(&o.alpha_min) {
        return Err("--alpha-min must be between 0 and 1".into());
    }
    if o.heartbeat.is_zero() {
        return Err("--heartbeat must be greater than zero".into());
    }
    Ok(Some(o))
}

/// Find the mpv IPC socket, waiting for it if the wallpaper is still starting.
///
/// wlpause is normally launched from the same autostart block as the
/// wallpaper, a fraction of a second behind it, so "not there yet" is the
/// expected case rather than an error.
fn resolve_socket(
    comp: &mut dyn Compositor,
    opts: &Opts,
) -> Result<PathBuf, String> {
    if let Some(p) = &opts.mpv_socket {
        let deadline = std::time::Instant::now() + opts.wait;
        while !p.exists() {
            if std::time::Instant::now() >= deadline || STOP.load(Ordering::SeqCst) {
                return Err(format!("{} never appeared", p.display()));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        return Ok(p.clone());
    }

    let deadline = std::time::Instant::now() + opts.wait;
    loop {
        if let Ok(pids) = comp.wallpaper_pids() {
            for pid in pids {
                if let Some(p) = hyprland::ipc_socket_of(pid) {
                    return Ok(p);
                }
            }
        }
        if std::time::Instant::now() >= deadline || STOP.load(Ordering::SeqCst) {
            return Err(format!(
                "could not find an mpv IPC socket for a '{}' layer.\n\
                 Start the wallpaper with an IPC socket, e.g.\n  \
                 mpvpaper -o 'input-ipc-server=/tmp/mpvsocket' '*' video.mp4\n\
                 or pass --mpv-socket <path>.",
                opts.namespace
            ));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// State carried between reconciles.
#[derive(Default)]
struct Seen {
    outputs: Option<Vec<String>>,
}

fn reconcile(
    comp: &mut dyn Compositor,
    target: &mut dyn Target,
    opts: &Opts,
    seen: &mut Seen,
) -> std::io::Result<()> {
    let states = comp.snapshot()?;

    let names: Vec<String> = states.iter().map(|s| s.name.clone()).collect();
    let outputs_changed = seen.outputs.as_ref().is_some_and(|prev| *prev != names);
    seen.outputs = Some(names);

    let mut want = combine(&states, opts.threshold);

    // Hot-plug deadlock guard. A frozen wallpaper process cannot map a
    // surface onto a newly attached output, so that output reports "no
    // wallpaper", which reads as "nothing to show" and keeps us frozen --
    // forever, and the new monitor never gets a wallpaper at all. Whenever
    // the set of outputs changes, resume for one cycle so the wallpaper can
    // catch up, then decide again normally. Keyed on the output set rather
    // than on the missing surface so it cannot oscillate when a wallpaper is
    // legitimately bound to only one output.
    if outputs_changed && want == Decision::Pause {
        want = Decision::Play;
        if opts.verbose {
            eprintln!("  outputs changed -> resuming so the wallpaper can map");
        }
    }

    if opts.verbose {
        for s in &states {
            let cov = match s.covered() {
                Some(c) => format!("{:5.1}% covered", c * 100.0),
                None => "no wallpaper  ".to_string(),
            };
            eprintln!(
                "  {:<12} {cov}  {:>2} occluders  {}",
                s.name,
                s.occluders.len(),
                if s.powered { "on" } else { "OFF" },
            );
        }
        eprintln!("  -> {want:?}");
    }

    let actual = target.is_paused()?;
    if actual == want.is_paused() {
        return Ok(());
    }
    if opts.dry_run {
        eprintln!("[dry-run] would {} the wallpaper", infinitive(want));
        return Ok(());
    }
    target.set_paused(want.is_paused())?;
    if opts.verbose {
        eprintln!("  {} the wallpaper", verb(want));
    }
    Ok(())
}

fn verb(d: Decision) -> &'static str {
    match d {
        Decision::Pause => "paused",
        Decision::Play => "resumed",
    }
}

fn infinitive(d: Decision) -> &'static str {
    match d {
        Decision::Pause => "pause",
        Decision::Play => "resume",
    }
}

fn run() -> Result<(), String> {
    let Some(opts) = parse_args()? else {
        return Ok(());
    };

    let sig_r = install_signal_pipe().map_err(|e| e.to_string())?;
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP, on_signal as *const () as libc::sighandler_t);
        // Do not die if the wallpaper closes the socket under us.
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    let mut comp = compositor::detect(&opts.namespace, opts.alpha_min).map_err(|e| e.to_string())?;
    let sock = resolve_socket(comp.as_mut(), &opts)?;
    let mpv = MpvIpc::new(&sock);
    let mut target: Box<dyn Target> = if opts.freeze {
        let pids = comp.wallpaper_pids().unwrap_or_default();
        if pids.is_empty() {
            return Err(format!(
                "--freeze needs the pid of the '{}' layer, which the compositor did not report",
                opts.namespace
            ));
        }
        Box::new(Freeze::new(mpv, pids))
    } else {
        Box::new(mpv)
    };

    if opts.verbose {
        eprintln!(
            "wlpause: {} ({}), {}, threshold {:.0}%",
            comp.name(),
            if comp.event_driven() { "event-driven" } else { "polling" },
            target.describe(),
            opts.threshold * 100.0
        );
    }

    if opts.once {
        return reconcile(comp.as_mut(), target.as_mut(), &opts, &mut Seen::default())
            .map_err(|e| e.to_string());
    }

    let mut failures = 0u32;
    let mut seen = Seen::default();
    while !STOP.load(Ordering::SeqCst) {
        match reconcile(comp.as_mut(), target.as_mut(), &opts, &mut seen) {
            Ok(()) => failures = 0,
            Err(e) => {
                failures += 1;
                // A daemon that exits on a transient IPC hiccup is worse than
                // useless, so log and keep going -- but do not spin silently
                // forever either.
                if failures <= 3 || failures % 60 == 0 {
                    eprintln!("wlpause: {e} (failure {failures})");
                }
            }
        }
        if STOP.load(Ordering::SeqCst) {
            break;
        }
        let wait_for = if failures > 0 {
            opts.heartbeat.min(Duration::from_secs(2))
        } else {
            opts.heartbeat
        };
        match wait(comp.as_mut(), sig_r, wait_for) {
            Ok(true) => {
                // Let a burst settle, then swallow whatever arrived during
                // the pause so one flurry of window events costs one
                // reconcile rather than a dozen.
                std::thread::sleep(opts.debounce);
                let _ = comp.drain_events();
            }
            Ok(false) => {}
            Err(e) => eprintln!("wlpause: event wait failed: {e}"),
        }
    }

    // Never leave a frozen wallpaper behind on the way out.
    if opts.resume_on_exit && !opts.dry_run {
        if let Err(e) = target.set_paused(false) {
            eprintln!("wlpause: could not resume on exit: {e}");
        } else if opts.verbose {
            eprintln!("wlpause: resumed wallpaper on exit");
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("wlpause: {e}");
            ExitCode::FAILURE
        }
    }
}
