use std::time::{Duration, Instant};

const BURST: Duration = Duration::from_millis(10);
const MAX_PAUSE: Duration = Duration::from_millis(10);

/// Holds one thread to a share of a CPU by pausing it whenever its CPU time
/// gets ahead of `share` × wall time. Idle time, such as waiting on
/// `system1.decide()`, earns at most `BURST` of credit.
pub(super) struct Throttle {
    share: f64,
    credit: f64,
    wall: Instant,
    cpu: Duration,
}

impl Throttle {
    pub(super) fn new(share: f64) -> Self {
        Self {
            share,
            credit: BURST.as_secs_f64(),
            wall: Instant::now(),
            cpu: thread_cpu_time(),
        }
    }

    /// Pauses for at most `MAX_PAUSE` so the caller can still observe
    /// cancellation and the deadline; any remaining debt carries over.
    pub(super) fn pace(&mut self, deadline: Instant) {
        let wall = Instant::now();
        let cpu = thread_cpu_time();
        let earned = self.share * wall.duration_since(self.wall).as_secs_f64();
        let consumed = cpu.saturating_sub(self.cpu).as_secs_f64();
        self.credit = (self.credit + earned - consumed).min(BURST.as_secs_f64());
        self.wall = wall;
        self.cpu = cpu;
        if self.credit < 0.0 {
            let pause =
                Duration::from_secs_f64((-self.credit / self.share).min(MAX_PAUSE.as_secs_f64()))
                    .min(deadline.saturating_duration_since(wall));
            std::thread::sleep(pause);
        }
    }
}

fn thread_cpu_time() -> Duration {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `time` is a valid, writable timespec for the duration of the call.
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &raw mut time) } != 0 {
        return Duration::ZERO;
    }
    Duration::new(
        u64::try_from(time.tv_sec).unwrap_or_default(),
        u32::try_from(time.tv_nsec).unwrap_or_default(),
    )
}
