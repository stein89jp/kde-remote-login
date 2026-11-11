//! Process memory: glibc heap hygiene and the opt-in memory diagnostics line.
//!
//! By default glibc raises its mmap threshold to the size of any larger
//! mmapped chunk that is freed (up to 32 MiB), and its automatic trim
//! threshold to twice that. The NVENC preflight frees a zeroed 1080p frame,
//! and closing a 1080p encoder frees about 20 MiB of mmapped chunks (its
//! frames and CUDA/NVENC state), so
//! afterwards frame-sized buffers come from the heap arenas, where freed
//! memory stays resident. Pinning the threshold at start keeps buffers above
//! 128 KiB mmapped, so freeing them returns them to the kernel. Closing an
//! encoder still leaves freed heap behind, which `malloc_trim(0)` returns: it
//! runs on this module's own thread after the preflight and after a client
//! disconnect (at most once per 30 s), never on the RDP runtime thread and
//! never per frame.
//!
//! With `KDE_RDP_DIAGNOSTICS=1` the same thread logs one `RDP performance`
//! line with `"component":"memory"` at start, every minute and with each
//! disconnect trim: the process RSS split from /proc/self/status, glibc's heap
//! state from `mallinfo2`, and the service cgroup's charge and limits. Each
//! is a few small file reads; nothing here runs on the runtime thread.
use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
    sync::{OnceLock, mpsc},
    time::{Duration, Instant},
};

/// glibc's own default. Any explicit M_MMAP_THRESHOLD also disables the
/// dynamic adjustment, which is the point: the threshold cannot grow.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
const MMAP_THRESHOLD: i32 = 128 * 1024;
/// Diagnostics lines between disconnects.
const REPORT_INTERVAL: Duration = Duration::from_secs(60);
/// Disconnect teardown finishes within this: the capture worker's final
/// compositor sync (500 ms at most), the last encoder task and the stream.
const SETTLE: Duration = Duration::from_secs(2);
/// Least time between two trims. Every served connection ends in a
/// disconnect, a failed handshake or a port scan included, and a trim takes
/// each arena lock in turn: it must not recur at a stranger's pace while
/// another client's session streams.
const TRIM_INTERVAL: Duration = Duration::from_secs(30);

/// Pin glibc's mmap threshold. Call first in `main`, before anything frees
/// a large allocation.
pub fn configure_allocator() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY: mallopt only changes allocator parameters; it returns 0
        // when it rejects the value.
        if unsafe { libc::mallopt(libc::M_MMAP_THRESHOLD, MMAP_THRESHOLD) } == 0 {
            eprintln!("glibc rejected M_MMAP_THRESHOLD; large buffers may stay in the heap");
        }
    }
}

enum Request {
    Disconnected,
}

static MAINTENANCE: OnceLock<mpsc::Sender<Request>> = OnceLock::new();

/// Start the memory thread: it trims the heap now (the NVENC preflight has
/// just closed its encoder) and after each disconnect, and reports under
/// `KDE_RDP_DIAGNOSTICS`. Without it, [`client_disconnected`] does nothing.
pub fn start() {
    if MAINTENANCE.get().is_some() {
        return;
    }
    let (tx, rx) = mpsc::channel();
    let diagnostics = crate::performance::diagnostics_enabled();
    match std::thread::Builder::new()
        .name("kde-rdp-memory".into())
        .spawn(move || run(&rx, diagnostics))
    {
        Ok(_) => {
            let _ = MAINTENANCE.set(tx);
        }
        Err(error) => eprintln!("memory maintenance thread unavailable: {error}"),
    }
}

/// A client connection ended: trim (and report) once its teardown settled.
/// Never blocks: it only queues a message for the memory thread.
pub fn client_disconnected() {
    if let Some(tx) = MAINTENANCE.get() {
        let _ = tx.send(Request::Disconnected);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    Start,
    Periodic,
    Disconnect,
}
impl Event {
    fn name(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Periodic => "periodic",
            Self::Disconnect => "disconnect",
        }
    }
    fn trims(self) -> bool {
        self != Self::Periodic
    }
}

/// When the memory thread next acts.
#[derive(Debug)]
struct Schedule {
    report_at: Option<Instant>,
    trim_at: Option<Instant>,
    last_trim: Option<Instant>,
}
impl Schedule {
    fn new(now: Instant, diagnostics: bool) -> Self {
        Self {
            report_at: diagnostics.then(|| now + REPORT_INTERVAL),
            trim_at: None,
            last_trim: None,
        }
    }
    /// A burst of disconnects (a client retrying, a port scan) shares the
    /// pending trim, which is never postponed; the one after it waits out
    /// [`TRIM_INTERVAL`].
    fn disconnected(&mut self, now: Instant) {
        let earliest = self.last_trim.map_or(now, |at| at + TRIM_INTERVAL);
        self.trim_at.get_or_insert((now + SETTLE).max(earliest));
    }
    /// How long to wait for a request; None waits until one arrives.
    fn timeout(&self, now: Instant) -> Option<Duration> {
        [self.trim_at, self.report_at]
            .into_iter()
            .flatten()
            .min()
            .map(|at| at.saturating_duration_since(now))
    }
    fn due(&mut self, now: Instant) -> Option<Event> {
        let event = if self.trim_at.is_some_and(|at| at <= now) {
            self.trim_at = None;
            self.last_trim = Some(now);
            Event::Disconnect
        } else if self.report_at.is_some_and(|at| at <= now) {
            Event::Periodic
        } else {
            return None;
        };
        // A disconnect line also starts a new periodic interval.
        if let Some(at) = &mut self.report_at {
            *at = now + REPORT_INTERVAL;
        }
        Some(event)
    }
}

fn run(rx: &mpsc::Receiver<Request>, diagnostics: bool) {
    let cgroup = std::fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|text| cgroup_dir(&text));
    let mut last_line = None;
    handle(Event::Start, diagnostics, cgroup.as_deref(), &mut last_line);
    let mut schedule = Schedule::new(Instant::now(), diagnostics);
    loop {
        let now = Instant::now();
        if let Some(event) = schedule.due(now) {
            handle(event, diagnostics, cgroup.as_deref(), &mut last_line);
            continue;
        }
        let request = match schedule.timeout(now) {
            Some(timeout) => rx.recv_timeout(timeout),
            None => rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
        };
        match request {
            Ok(Request::Disconnected) => schedule.disconnected(Instant::now()),
            Err(mpsc::RecvTimeoutError::Timeout) => (),
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn handle(event: Event, diagnostics: bool, cgroup: Option<&Path>, last_line: &mut Option<Instant>) {
    if !diagnostics {
        if event.trims() {
            trim_heap();
        }
        return;
    }
    let snapshot = Snapshot::read(cgroup);
    let trim = event.trims().then(|| {
        let duration = trim_heap();
        let after = read_status().get("rss_anon_bytes");
        Trim {
            released: match (snapshot.process.get("rss_anon_bytes"), after) {
                (Value::Number(before), Value::Number(after)) => {
                    Value::Number(before.saturating_sub(after))
                }
                _ => Value::Missing,
            },
            duration,
        }
    });
    let now = Instant::now();
    let interval = last_line.map_or(Duration::ZERO, |at| now.saturating_duration_since(at));
    *last_line = Some(now);
    eprintln!("{}", line(event, interval, &snapshot, trim.as_ref()));
}

/// Return freed heap pages of every arena to the kernel.
fn trim_heap() -> Duration {
    let started = Instant::now();
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: malloc_trim takes each arena's lock itself; any thread may call it.
    unsafe {
        libc::malloc_trim(0);
    }
    started.elapsed()
}

/// One reported quantity: a number, an unlimited cgroup limit ("max"), or
/// unreadable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Value {
    Number(u64),
    Unlimited,
    Missing,
}
impl Value {
    fn write(self, out: &mut String) {
        match self {
            Self::Number(value) => {
                let _ = write!(out, "{value}");
            }
            Self::Unlimited => out.push_str("\"max\""),
            Self::Missing => out.push_str("null"),
        }
    }
}

/// Named values in reporting order.
#[derive(Debug, Default, PartialEq, Eq)]
struct Fields(Vec<(&'static str, Value)>);
impl Fields {
    fn get(&self, name: &str) -> Value {
        self.0
            .iter()
            .find(|(key, _)| *key == name)
            .map_or(Value::Missing, |(_, value)| *value)
    }
    fn write(&self, out: &mut String) {
        out.push('{');
        for (index, (name, value)) in self.0.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            let _ = write!(out, "\"{name}\":");
            value.write(out);
        }
        out.push('}');
    }
}

/// /proc/self/status fields, in bytes. RssShmem counts mapped shared memory
/// such as KWin's screencast buffers, which KWin's cgroup is charged for.
const STATUS_FIELDS: [(&str, &str); 7] = [
    ("VmRSS", "rss_bytes"),
    ("RssAnon", "rss_anon_bytes"),
    ("RssFile", "rss_file_bytes"),
    ("RssShmem", "rss_shmem_bytes"),
    ("VmHWM", "rss_peak_bytes"),
    ("VmSwap", "swap_bytes"),
    ("Threads", "threads"),
];
/// memory.stat fields: what the service is charged for, by kind. `kernel`
/// includes driver allocations accounted to the cgroup; `file` includes
/// mapped libraries, which reclaim near the limit would evict.
const STAT_FIELDS: [(&str, &str); 7] = [
    ("anon", "anon_bytes"),
    ("anon_thp", "anon_thp_bytes"),
    ("file", "file_bytes"),
    ("file_mapped", "file_mapped_bytes"),
    ("shmem", "shmem_bytes"),
    ("kernel", "kernel_bytes"),
    ("sock", "sock_bytes"),
];
/// memory.events counters: how often usage reached memory.high or
/// memory.max, and OOM kills.
const EVENT_FIELDS: [(&str, &str); 3] = [
    ("high", "events_high"),
    ("max", "events_max"),
    ("oom_kill", "events_oom_kill"),
];

/// "Key:  123 kB" lines; kB values in bytes, bare counts as they are.
fn parse_status(text: &str) -> Fields {
    let values: Vec<_> = text
        .lines()
        .filter_map(|line| {
            let (key, rest) = line.split_once(':')?;
            let mut words = rest.split_whitespace();
            let value: u64 = words.next()?.parse().ok()?;
            let value = match words.next() {
                Some("kB") => value.checked_mul(1024)?,
                Some(_) => return None,
                None => value,
            };
            Some((key, value))
        })
        .collect();
    Fields(
        STATUS_FIELDS
            .iter()
            .map(|(key, name)| (*name, lookup(&values, key)))
            .collect(),
    )
}

/// memory.stat and memory.events: "key value" lines.
fn parse_keyed(text: &str, fields: &[(&str, &'static str)]) -> Fields {
    let values: Vec<_> = text
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(' ')?;
            Some((key, value.trim().parse::<u64>().ok()?))
        })
        .collect();
    Fields(
        fields
            .iter()
            .map(|(key, name)| (*name, lookup(&values, key)))
            .collect(),
    )
}

fn lookup(values: &[(&str, u64)], key: &str) -> Value {
    values
        .iter()
        .find(|(found, _)| *found == key)
        .map_or(Value::Missing, |(_, value)| Value::Number(*value))
}

/// A single-value cgroup file: a number, or "max" for no limit.
fn parse_single(text: &str) -> Value {
    match text.trim() {
        "max" => Value::Unlimited,
        value => value.parse().map_or(Value::Missing, Value::Number),
    }
}

/// The unified (v2) hierarchy directory of this process, from
/// /proc/self/cgroup. ProtectControlGroups=yes keeps /sys/fs/cgroup
/// readable; inside a cgroup namespace the path is "/", its root.
fn cgroup_dir(text: &str) -> Option<PathBuf> {
    let path = text.lines().find_map(|line| line.strip_prefix("0::"))?;
    let relative = path.strip_prefix('/')?;
    if relative.split('/').any(|part| part == "..") {
        return None;
    }
    Some(Path::new("/sys/fs/cgroup").join(relative))
}

fn read_status() -> Fields {
    std::fs::read_to_string("/proc/self/status")
        .map(|text| parse_status(&text))
        .unwrap_or_else(|_| {
            Fields(
                STATUS_FIELDS
                    .iter()
                    .map(|(_, name)| (*name, Value::Missing))
                    .collect(),
            )
        })
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn read_heap() -> Option<Fields> {
    // SAFETY: mallinfo2 only reads allocator statistics under arena locks.
    let info = unsafe { libc::mallinfo2() };
    let number = |value: usize| Value::Number(value as u64);
    Some(Fields(vec![
        // Heap obtained with brk/sbrk and arena mmaps, in use or free.
        ("arena_bytes", number(info.arena)),
        ("in_use_bytes", number(info.uordblks)),
        // Free heap chunks, resident or not: malloc_trim returns their pages
        // to the kernel but keeps the address space, so this stays high.
        ("free_bytes", number(info.fordblks)),
        ("top_releasable_bytes", number(info.keepcost)),
        // Chunks above the mmap threshold, returned to the kernel on free.
        ("mmapped_bytes", number(info.hblkhd)),
        ("mmapped_chunks", number(info.hblks)),
    ]))
}
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn read_heap() -> Option<Fields> {
    None
}

fn read_cgroup(dir: &Path) -> Option<Fields> {
    let read = |name: &str| std::fs::read_to_string(dir.join(name)).ok();
    // memory.current is the charge itself; without it there is no memory
    // controller to report.
    let current = parse_single(&read("memory.current")?);
    let mut fields = vec![
        ("current_bytes", current),
        (
            "peak_bytes",
            read("memory.peak").map_or(Value::Missing, |text| parse_single(&text)),
        ),
        (
            "max_bytes",
            read("memory.max").map_or(Value::Missing, |text| parse_single(&text)),
        ),
        (
            "high_bytes",
            read("memory.high").map_or(Value::Missing, |text| parse_single(&text)),
        ),
        (
            "swap_bytes",
            read("memory.swap.current").map_or(Value::Missing, |text| parse_single(&text)),
        ),
    ];
    for (file, keys) in [
        ("memory.stat", &STAT_FIELDS[..]),
        ("memory.events", &EVENT_FIELDS[..]),
    ] {
        let text = read(file).unwrap_or_default();
        fields.extend(parse_keyed(&text, keys).0);
    }
    Some(Fields(fields))
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Snapshot {
    process: Fields,
    heap: Option<Fields>,
    cgroup: Option<Fields>,
}
impl Snapshot {
    fn read(cgroup: Option<&Path>) -> Self {
        Self {
            process: read_status(),
            heap: read_heap(),
            cgroup: cgroup.and_then(read_cgroup),
        }
    }
}

struct Trim {
    /// RssAnon before minus after; other threads allocate meanwhile.
    released: Value,
    duration: Duration,
}

fn line(event: Event, interval: Duration, snapshot: &Snapshot, trim: Option<&Trim>) -> String {
    let mut out = format!(
        "RDP performance {{\"component\":\"memory\",\"event\":\"{}\",\"interval_ms\":{},\"process\":",
        event.name(),
        interval.as_millis()
    );
    snapshot.process.write(&mut out);
    for (name, fields) in [("heap", &snapshot.heap), ("cgroup", &snapshot.cgroup)] {
        let _ = write!(out, ",\"{name}\":");
        match fields {
            Some(fields) => fields.write(&mut out),
            None => out.push_str("null"),
        }
    }
    if let Some(trim) = trim {
        out.push_str(",\"trim\":{\"released_bytes\":");
        trim.released.write(&mut out);
        let _ = write!(out, ",\"duration_us\":{}}}", trim.duration.as_micros());
    }
    out.push_str(",\"physical_presentation_measured\":false}");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Captured from the running frontend (kernel 7.2, glibc 2.44).
    const STATUS: &str = "Name:\tkde-rdp-bridge\nUmask:\t0077\nState:\tS (sleeping)\n\
VmPeak:\t 7456440 kB\nVmSize:\t 7390904 kB\nVmHWM:\t  636676 kB\nVmRSS:\t  509792 kB\n\
RssAnon:\t  215060 kB\nRssFile:\t  181616 kB\nRssShmem:\t  113116 kB\nVmSwap:\t       0 kB\n\
Threads:\t14\nSigQ:\t0/127573\nCpus_allowed_list:\t0-15\n";
    const STAT: &str = "anon 220221440\nfile 63897600\nkernel 70959104\nkernel_stack 212992\n\
pagetables 1683456\nsock 28672\nshmem 8409088\nfile_mapped 116776960\nfile_dirty 0\n\
anon_thp 188743680\nfile_thp 0\nworkingset_refault_anon 0\n";
    const EVENTS: &str = "low 0\nhigh 0\nmax 3\noom 0\noom_kill 0\noom_group_kill 0\n";

    #[test]
    fn status_fields_are_bytes_and_counts() {
        let fields = parse_status(STATUS);
        assert_eq!(fields.get("rss_bytes"), Value::Number(509_792 * 1024));
        assert_eq!(fields.get("rss_anon_bytes"), Value::Number(215_060 * 1024));
        assert_eq!(fields.get("rss_file_bytes"), Value::Number(181_616 * 1024));
        assert_eq!(fields.get("rss_shmem_bytes"), Value::Number(113_116 * 1024));
        assert_eq!(fields.get("rss_peak_bytes"), Value::Number(636_676 * 1024));
        assert_eq!(fields.get("swap_bytes"), Value::Number(0));
        assert_eq!(fields.get("threads"), Value::Number(14));
        // Fields keep their order; absent or malformed ones are null.
        let names: Vec<_> = fields.0.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, STATUS_FIELDS.map(|(_, name)| name));
        let partial = parse_status("VmRSS:\t12 kB\nRssAnon:\tlots kB\nRssFile:\t1 MB\n");
        assert_eq!(partial.get("rss_bytes"), Value::Number(12 * 1024));
        assert_eq!(partial.get("rss_anon_bytes"), Value::Missing);
        assert_eq!(partial.get("rss_file_bytes"), Value::Missing);
        assert_eq!(partial.get("threads"), Value::Missing);
    }

    #[test]
    fn cgroup_stat_and_event_fields_are_selected_by_exact_key() {
        let stat = parse_keyed(STAT, &STAT_FIELDS);
        assert_eq!(stat.get("anon_bytes"), Value::Number(220_221_440));
        // "file" is not confused with file_mapped, file_dirty or file_thp.
        assert_eq!(stat.get("file_bytes"), Value::Number(63_897_600));
        assert_eq!(stat.get("file_mapped_bytes"), Value::Number(116_776_960));
        assert_eq!(stat.get("anon_thp_bytes"), Value::Number(188_743_680));
        assert_eq!(stat.get("kernel_bytes"), Value::Number(70_959_104));
        assert_eq!(stat.get("shmem_bytes"), Value::Number(8_409_088));
        assert_eq!(stat.get("sock_bytes"), Value::Number(28_672));
        let events = parse_keyed(EVENTS, &EVENT_FIELDS);
        assert_eq!(events.get("events_high"), Value::Number(0));
        assert_eq!(events.get("events_max"), Value::Number(3));
        assert_eq!(events.get("events_oom_kill"), Value::Number(0));
        assert_eq!(
            parse_keyed("", &EVENT_FIELDS).get("events_max"),
            Value::Missing
        );
    }

    #[test]
    fn single_value_files_distinguish_no_limit_from_unreadable() {
        assert_eq!(parse_single("1610612736\n"), Value::Number(1_610_612_736));
        assert_eq!(parse_single("max\n"), Value::Unlimited);
        assert_eq!(parse_single(""), Value::Missing);
        assert_eq!(parse_single("-1\n"), Value::Missing);
    }

    #[test]
    fn cgroup_directory_comes_from_the_unified_hierarchy_line() {
        assert_eq!(
            cgroup_dir("0::/system.slice/kde-rdp-frontend.service\n"),
            Some(PathBuf::from(
                "/sys/fs/cgroup/system.slice/kde-rdp-frontend.service"
            ))
        );
        // Hybrid hosts list v1 controllers first.
        assert_eq!(
            cgroup_dir("12:memory:/system.slice/x.service\n0::/system.slice/x.service\n"),
            Some(PathBuf::from("/sys/fs/cgroup/system.slice/x.service"))
        );
        // A cgroup namespace shows its root.
        assert_eq!(cgroup_dir("0::/\n"), Some(PathBuf::from("/sys/fs/cgroup/")));
        assert_eq!(cgroup_dir("12:memory:/x\n"), None);
        assert_eq!(cgroup_dir("0::relative\n"), None);
        assert_eq!(cgroup_dir("0::/a/../../etc\n"), None);
    }

    #[test]
    fn report_line_matches_the_performance_line_conventions() {
        let snapshot = Snapshot {
            process: parse_status("VmRSS:\t2 kB\nThreads:\t3\n"),
            heap: Some(Fields(vec![
                ("arena_bytes", Value::Number(10)),
                ("free_bytes", Value::Number(4)),
            ])),
            cgroup: Some(Fields(vec![
                ("current_bytes", Value::Number(100)),
                ("max_bytes", Value::Unlimited),
                ("peak_bytes", Value::Missing),
            ])),
        };
        assert_eq!(
            line(Event::Periodic, Duration::from_secs(60), &snapshot, None),
            "RDP performance {\"component\":\"memory\",\"event\":\"periodic\",\"interval_ms\":60000,\
\"process\":{\"rss_bytes\":2048,\"rss_anon_bytes\":null,\"rss_file_bytes\":null,\
\"rss_shmem_bytes\":null,\"rss_peak_bytes\":null,\"swap_bytes\":null,\"threads\":3},\
\"heap\":{\"arena_bytes\":10,\"free_bytes\":4},\
\"cgroup\":{\"current_bytes\":100,\"max_bytes\":\"max\",\"peak_bytes\":null},\
\"physical_presentation_measured\":false}"
        );
        let trim = Trim {
            released: Value::Number(8192),
            duration: Duration::from_micros(250),
        };
        let unreadable = Snapshot::default();
        let text = line(Event::Disconnect, Duration::ZERO, &unreadable, Some(&trim));
        assert!(text.starts_with(
            "RDP performance {\"component\":\"memory\",\"event\":\"disconnect\",\"interval_ms\":0,"
        ));
        assert!(text.contains(",\"heap\":null,\"cgroup\":null,"));
        assert!(text.ends_with(
            ",\"trim\":{\"released_bytes\":8192,\"duration_us\":250},\"physical_presentation_measured\":false}"
        ));
    }

    #[test]
    fn disconnects_trim_once_after_teardown_settles() {
        let start = Instant::now();
        let mut schedule = Schedule::new(start, false);
        assert_eq!(schedule.timeout(start), None, "idle without diagnostics");
        assert_eq!(schedule.due(start), None);
        schedule.disconnected(start);
        assert_eq!(
            schedule.due(start + SETTLE / 2),
            None,
            "teardown settles first"
        );
        // A reconnect attempt that fails at once shares the pending trim,
        // so constant reconnects cannot postpone it.
        let retry = start + Duration::from_secs(1);
        schedule.disconnected(retry);
        assert_eq!(
            schedule.timeout(retry),
            Some(SETTLE - Duration::from_secs(1))
        );
        let settled = start + SETTLE;
        assert_eq!(schedule.due(settled), Some(Event::Disconnect));
        assert_eq!(schedule.due(settled), None);
        assert_eq!(schedule.timeout(settled), None);
        // Disconnects that keep coming (a port scan during another client's
        // session) trim no more often than the trim interval.
        schedule.disconnected(settled);
        assert_eq!(schedule.due(settled + SETTLE), None);
        schedule.disconnected(settled + SETTLE);
        assert_eq!(schedule.timeout(settled), Some(TRIM_INTERVAL));
        let next = settled + TRIM_INTERVAL;
        assert_eq!(schedule.due(next), Some(Event::Disconnect));
        assert_eq!(schedule.timeout(next), None);
        // After a quiet interval a disconnect only waits for its teardown.
        let later = next + TRIM_INTERVAL;
        schedule.disconnected(later);
        assert_eq!(schedule.due(later + SETTLE), Some(Event::Disconnect));
    }

    #[test]
    fn diagnostics_report_every_interval_and_a_disconnect_restarts_it() {
        let start = Instant::now();
        let mut schedule = Schedule::new(start, true);
        assert_eq!(schedule.timeout(start), Some(REPORT_INTERVAL));
        assert_eq!(schedule.due(start), None);
        let first = start + REPORT_INTERVAL;
        assert_eq!(schedule.due(first), Some(Event::Periodic));
        assert_eq!(schedule.due(first), None);
        schedule.disconnected(first + Duration::from_secs(10));
        let disconnect = first + Duration::from_secs(10) + SETTLE;
        assert_eq!(
            schedule.timeout(first),
            Some(Duration::from_secs(10) + SETTLE)
        );
        assert_eq!(schedule.due(disconnect), Some(Event::Disconnect));
        assert_eq!(schedule.timeout(disconnect), Some(REPORT_INTERVAL));
        assert!(Event::Start.trims() && Event::Disconnect.trims());
        assert!(!Event::Periodic.trims(), "periodic lines never trim");
    }

    #[test]
    fn live_snapshot_reads_this_process() {
        let snapshot = Snapshot::read(None);
        assert!(matches!(snapshot.process.get("rss_bytes"), Value::Number(rss) if rss > 0));
        assert!(matches!(snapshot.process.get("threads"), Value::Number(threads) if threads >= 1));
        #[cfg(all(target_os = "linux", target_env = "gnu"))]
        assert!(snapshot.heap.is_some());
        assert_eq!(snapshot.cgroup, None);
        // A cgroup directory without memory files reports nothing.
        assert_eq!(read_cgroup(Path::new("/nonexistent-kde-rdp-cgroup")), None);
    }

    /// glibc marks an mmapped chunk with bit 1 (IS_MMAPPED) of the size word
    /// just before the returned pointer.
    #[cfg(all(target_os = "linux", target_env = "gnu", target_pointer_width = "64"))]
    fn mmapped(buffer: &[u8]) -> bool {
        // SAFETY: every glibc chunk has its size word in the eight bytes
        // before the user pointer, and the buffer is live.
        unsafe { *(buffer.as_ptr() as *const usize).sub(1) & 2 != 0 }
    }

    #[test]
    #[cfg(all(target_os = "linux", target_env = "gnu", target_pointer_width = "64"))]
    fn pinned_threshold_keeps_frame_buffers_mmapped_after_a_large_free() {
        // The threshold and the heap are process-wide. In this test binary
        // other tests free frame-sized buffers before the pin, which raises
        // the dynamic threshold and leaves free heap chunks large enough to
        // serve the allocations below. The frontend pins first thing in
        // `main`, so check that order in a fresh process: this test again,
        // alone, with the marker set.
        const CHILD: &str = "KDE_RDP_TEST_PINNED_THRESHOLD_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "memory::tests::pinned_threshold_keeps_frame_buffers_mmapped_after_a_large_free",
                    "--test-threads=1",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            // The child ran the checks below rather than filtering to nothing.
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            return;
        }
        configure_allocator();
        const FRAME: usize = 1920 * 1080 * 4;
        // The NVENC preflight: one zeroed 1080p frame, allocated and freed.
        // With glibc's dynamic threshold this free raises the threshold and
        // the next frame-sized buffer would come from the heap.
        let preflight = std::hint::black_box(vec![0u8; FRAME]);
        assert!(mmapped(&preflight));
        drop(preflight);
        let frame = std::hint::black_box(vec![1u8; FRAME]);
        assert!(mmapped(&frame), "freeing this buffer must unmap it");
        // Packet-sized buffers below the threshold stay in the heap.
        let small = std::hint::black_box(vec![1u8; 4096]);
        assert!(!mmapped(&small));
    }
}
