//! Bounded external helper execution with concurrency, launch-rate, output, and
//! spawn-plus-wait deadline enforcement.

use std::ffi::OsString;
use std::fmt;
use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use crate::budget;

/// Default maximum bytes captured from each helper stream (`4 MiB`).
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
/// Default per-call timeout covering process spawn, execution, and output drain.
pub const DEFAULT_PER_CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Limits applied to every helper process launched through a [`HelperRunner`].
#[derive(Debug, Clone)]
pub struct HelperPolicy {
    /// Maximum helper subprocesses in flight at once.
    pub max_concurrent: usize,
    /// Per-call timeout covering spawn, execution, and output drain.
    pub per_call_timeout: Duration,
    /// Maximum bytes captured from standard output or standard error.
    pub max_output_bytes: usize,
    /// Minimum spacing between successive helper launches.
    pub min_launch_interval: Duration,
    /// Optional cap on total helper launches for this runner.
    pub max_launches: Option<u64>,
    /// Shared cancellation flag that stops new launches and terminates active children.
    pub cancel: Arc<AtomicBool>,
    /// Optional overall monotonic deadline for the enclosing operation.
    pub deadline: Option<Instant>,
    /// Apply OS background priority to helper worker threads.
    pub background_priority: bool,
}

impl Default for HelperPolicy {
    fn default() -> Self {
        HelperPolicy {
            max_concurrent: 1,
            per_call_timeout: DEFAULT_PER_CALL_TIMEOUT,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            min_launch_interval: Duration::ZERO,
            max_launches: None,
            cancel: Arc::new(AtomicBool::new(false)),
            deadline: None,
            background_priority: false,
        }
    }
}

impl HelperPolicy {
    /// Bounded helper policy for unattended maintenance runs.
    pub fn for_maintenance(cancel: Arc<AtomicBool>, deadline: Option<Instant>) -> Self {
        HelperPolicy {
            max_concurrent: 1,
            per_call_timeout: DEFAULT_PER_CALL_TIMEOUT,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            min_launch_interval: Duration::from_millis(25),
            max_launches: Some(256),
            cancel,
            deadline,
            background_priority: true,
        }
    }
}

/// Specification of one external helper invocation.
#[derive(Debug, Clone)]
pub struct HelperSpec {
    pub operation: String,
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub current_dir: Option<PathBuf>,
    pub env_set: Vec<(OsString, OsString)>,
    pub env_remove: Vec<OsString>,
    pub stdin: Option<Vec<u8>>,
}

/// Captured result of a completed helper process.
#[derive(Debug, Clone)]
pub struct HelperOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Reason a helper invocation did not finish normally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelperError {
    Cancelled {
        operation: String,
    },
    BudgetExhausted {
        operation: String,
        reason: String,
    },
    TimedOut {
        operation: String,
        program: PathBuf,
        draining: usize,
    },
    OutputExceeded {
        operation: String,
        limit: usize,
    },
    SpawnFailed {
        operation: String,
        program: PathBuf,
        reason: String,
    },
}

impl fmt::Display for HelperError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HelperError::Cancelled { operation } => {
                write!(f, "helper cancelled during {operation}")
            }
            HelperError::BudgetExhausted { operation, reason } => {
                write!(f, "helper budget exhausted during {operation}: {reason}")
            }
            HelperError::TimedOut {
                operation,
                program,
                draining,
            } => write!(
                f,
                "helper {} timed out during {operation} (draining: {draining})",
                program.display()
            ),
            HelperError::OutputExceeded { operation, limit } => {
                write!(f, "helper output exceeded {limit} bytes during {operation}")
            }
            HelperError::SpawnFailed {
                operation,
                program,
                reason,
            } => write!(
                f,
                "could not run {} during {operation}: {reason}",
                program.display()
            ),
        }
    }
}

impl std::error::Error for HelperError {}

/// Coordinates bounded helper launches across one operation.
#[derive(Debug)]
pub struct HelperRunner {
    policy: HelperPolicy,
    in_flight: Arc<AtomicUsize>,
    draining: Arc<AtomicUsize>,
    launches: AtomicU64,
    last_launch: Mutex<Option<Instant>>,
}

struct SlotGuard {
    in_flight: Arc<AtomicUsize>,
    draining: Arc<AtomicUsize>,
    marked_draining: Arc<AtomicBool>,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        if self.marked_draining.load(Ordering::Acquire) {
            self.draining.fetch_sub(1, Ordering::AcqRel);
        }
        self.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

impl HelperRunner {
    pub fn new(policy: HelperPolicy) -> Arc<Self> {
        Arc::new(HelperRunner {
            policy,
            in_flight: Arc::new(AtomicUsize::new(0)),
            draining: Arc::new(AtomicUsize::new(0)),
            launches: AtomicU64::new(0),
            last_launch: Mutex::new(None),
        })
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Acquire)
    }

    pub fn draining(&self) -> usize {
        self.draining.load(Ordering::Acquire)
    }

    pub fn launches(&self) -> u64 {
        self.launches.load(Ordering::Acquire)
    }

    fn effective_deadline(&self) -> Instant {
        let per_call = Instant::now() + self.policy.per_call_timeout;
        match self.policy.deadline {
            Some(overall) => per_call.min(overall),
            None => per_call,
        }
    }

    fn acquire_slot(
        &self,
        operation: &str,
        program: &Path,
        deadline: Instant,
    ) -> Result<SlotGuard, HelperError> {
        let cap = self.policy.max_concurrent.max(1);
        loop {
            if self.policy.cancel.load(Ordering::Acquire) {
                return Err(HelperError::Cancelled {
                    operation: operation.to_owned(),
                });
            }
            if Instant::now() >= deadline {
                return Err(HelperError::TimedOut {
                    operation: operation.to_owned(),
                    program: program.to_path_buf(),
                    draining: self.draining(),
                });
            }
            let current = self.in_flight.load(Ordering::Acquire);
            if current < cap
                && self
                    .in_flight
                    .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                return Ok(SlotGuard {
                    in_flight: Arc::clone(&self.in_flight),
                    draining: Arc::clone(&self.draining),
                    marked_draining: Arc::new(AtomicBool::new(false)),
                });
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn pace_launch(
        &self,
        operation: &str,
        program: &Path,
        deadline: Instant,
    ) -> Result<(), HelperError> {
        if self.policy.min_launch_interval.is_zero() {
            return Ok(());
        }
        loop {
            if self.policy.cancel.load(Ordering::Acquire) {
                return Err(HelperError::Cancelled {
                    operation: operation.to_owned(),
                });
            }
            let wait = {
                let guard = self
                    .last_launch
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                guard.and_then(|last| {
                    let target = last + self.policy.min_launch_interval;
                    let now = Instant::now();
                    (target > now).then_some(target - now)
                })
            };
            let Some(remaining) = wait else {
                break;
            };
            if Instant::now() >= deadline {
                return Err(HelperError::TimedOut {
                    operation: operation.to_owned(),
                    program: program.to_path_buf(),
                    draining: self.draining(),
                });
            }
            std::thread::sleep(remaining.min(Duration::from_millis(10)));
        }
        let mut guard = self
            .last_launch
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        *guard = Some(Instant::now());
        Ok(())
    }

    /// Executes `spec` within this runner's concurrency, rate, output, and
    /// deadline bounds. The deadline covers both `spawn` and process wait.
    pub fn run(&self, spec: HelperSpec) -> Result<HelperOutput, HelperError> {
        if self.policy.cancel.load(Ordering::Acquire) {
            return Err(HelperError::Cancelled {
                operation: spec.operation,
            });
        }
        if let Some(overall) = self.policy.deadline
            && Instant::now() >= overall
        {
            return Err(HelperError::BudgetExhausted {
                operation: spec.operation,
                reason: "operation deadline elapsed".to_owned(),
            });
        }
        let deadline = self.effective_deadline();
        let guard = self.acquire_slot(&spec.operation, &spec.program, deadline)?;
        if let Some(limit) = self.policy.max_launches {
            let next = self.launches.fetch_add(1, Ordering::AcqRel) + 1;
            if next > limit {
                self.launches.fetch_sub(1, Ordering::AcqRel);
                return Err(HelperError::BudgetExhausted {
                    operation: spec.operation,
                    reason: format!("launch limit of {limit} reached"),
                });
            }
        } else {
            self.launches.fetch_add(1, Ordering::AcqRel);
        }
        self.pace_launch(&spec.operation, &spec.program, deadline)?;

        let slot_cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&slot_cancel);
        let shared_cancel = Arc::clone(&self.policy.cancel);
        let marked_draining = Arc::clone(&guard.marked_draining);
        let max_output = self.policy.max_output_bytes;
        let background = self.policy.background_priority;
        let operation = spec.operation.clone();
        let program = spec.program.clone();
        let (tx, rx) = mpsc::channel();

        let spawn_result = std::thread::Builder::new()
            .name("suds-helper".to_owned())
            .spawn(move || {
                let _guard = guard;
                if background {
                    budget::apply_background_thread_policy();
                }
                let result = run_worker(spec, max_output, deadline, &worker_cancel, &shared_cancel);
                let _ = tx.send(result);
            });
        if let Err(err) = spawn_result {
            return Err(HelperError::SpawnFailed {
                operation,
                program,
                reason: err.to_string(),
            });
        }

        loop {
            let now = Instant::now();
            if self.policy.cancel.load(Ordering::Acquire) {
                slot_cancel.store(true, Ordering::Release);
                if !marked_draining.swap(true, Ordering::AcqRel) {
                    self.draining.fetch_add(1, Ordering::AcqRel);
                }
                return Err(HelperError::Cancelled { operation });
            }
            if now >= deadline {
                slot_cancel.store(true, Ordering::Release);
                if !marked_draining.swap(true, Ordering::AcqRel) {
                    self.draining.fetch_add(1, Ordering::AcqRel);
                }
                return Err(HelperError::TimedOut {
                    operation,
                    program,
                    draining: self.draining(),
                });
            }
            let wait = (deadline - now).min(Duration::from_millis(20));
            match rx.recv_timeout(wait) {
                Ok(result) => return result,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(HelperError::SpawnFailed {
                        operation,
                        program,
                        reason: "helper worker exited without a result".to_owned(),
                    });
                }
            }
        }
    }
}

fn read_bounded(mut stream: impl Read, limit: usize) -> Result<Vec<u8>, io::Error> {
    let mut out = Vec::new();
    let take_limit = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
    stream.by_ref().take(take_limit).read_to_end(&mut out)?;
    Ok(out)
}

fn run_worker(
    spec: HelperSpec,
    max_output: usize,
    deadline: Instant,
    slot_cancel: &AtomicBool,
    shared_cancel: &AtomicBool,
) -> Result<HelperOutput, HelperError> {
    let mut command = Command::new(&spec.program);
    command.args(&spec.args);
    if let Some(dir) = &spec.current_dir {
        command.current_dir(dir);
    }
    for key in &spec.env_remove {
        command.env_remove(key);
    }
    for (key, value) in &spec.env_set {
        command.env(key, value);
    }
    command
        .stdin(if spec.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);

    let mut child = command.spawn().map_err(|err| HelperError::SpawnFailed {
        operation: spec.operation.clone(),
        program: spec.program.clone(),
        reason: err.to_string(),
    })?;

    if slot_cancel.load(Ordering::Acquire) || shared_cancel.load(Ordering::Acquire) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(HelperError::Cancelled {
            operation: spec.operation,
        });
    }

    if let Some(input) = spec.stdin
        && let Some(mut stdin) = child.stdin.take()
    {
        let _ = std::thread::Builder::new()
            .name("suds-helper-in".to_owned())
            .spawn(move || {
                let _ = stdin.write_all(&input);
            });
    }

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let (out_tx, out_rx) = mpsc::channel();
    let (err_tx, err_rx) = mpsc::channel();
    if let Some(pipe) = stdout_pipe {
        let _ = std::thread::Builder::new()
            .name("suds-helper-out".to_owned())
            .spawn(move || {
                let _ = out_tx.send(read_bounded(pipe, max_output));
            });
    } else {
        let _ = out_tx.send(Ok(Vec::new()));
    }
    if let Some(pipe) = stderr_pipe {
        let _ = std::thread::Builder::new()
            .name("suds-helper-err".to_owned())
            .spawn(move || {
                let _ = err_tx.send(read_bounded(pipe, max_output));
            });
    } else {
        let _ = err_tx.send(Ok(Vec::new()));
    }

    let status = loop {
        if slot_cancel.load(Ordering::Acquire) || shared_cancel.load(Ordering::Acquire) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(HelperError::Cancelled {
                operation: spec.operation,
            });
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(HelperError::TimedOut {
                operation: spec.operation,
                program: spec.program,
                draining: 0,
            });
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(HelperError::SpawnFailed {
                    operation: spec.operation,
                    program: spec.program,
                    reason: err.to_string(),
                });
            }
        }
    };

    let drain_budget = deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_millis(50));
    let stdout = out_rx
        .recv_timeout(drain_budget)
        .map_err(|_| HelperError::TimedOut {
            operation: spec.operation.clone(),
            program: spec.program.clone(),
            draining: 0,
        })?
        .map_err(|err| HelperError::SpawnFailed {
            operation: spec.operation.clone(),
            program: spec.program.clone(),
            reason: err.to_string(),
        })?;
    let stderr = err_rx
        .recv_timeout(drain_budget)
        .map_err(|_| HelperError::TimedOut {
            operation: spec.operation.clone(),
            program: spec.program.clone(),
            draining: 0,
        })?
        .map_err(|err| HelperError::SpawnFailed {
            operation: spec.operation.clone(),
            program: spec.program.clone(),
            reason: err.to_string(),
        })?;

    if stdout.len() > max_output || stderr.len() > max_output {
        return Err(HelperError::OutputExceeded {
            operation: spec.operation,
            limit: max_output,
        });
    }

    Ok(HelperOutput {
        status,
        stdout,
        stderr,
    })
}
