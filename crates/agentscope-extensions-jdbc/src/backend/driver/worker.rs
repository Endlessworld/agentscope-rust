//! One dedicated thread per driver connection (request/reply plumbing).
//!
//! The components are synchronous while drivers are asynchronous, and the components are
//! called from async code, so the async driver cannot be driven with `Runtime::block_on` on
//! the caller's thread (Tokio panics: "cannot start a runtime from within a runtime"). This
//! worker owns a `current_thread` runtime plus the connection on a **separate** thread and
//! serves blocking jobs over a channel; the caller only waits for the reply.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Mutex;

use tokio::runtime::Runtime;

use crate::backend::{JdbcError, JdbcResult};

type Job<C> = Box<dyn FnOnce(&mut C, &Runtime) + Send>;

/// A driver connection living on its own thread.
///
/// Dropping the worker drops the job channel, which ends the thread (and therefore the
/// connection) — the same effect as closing the Java `Connection`.
pub(crate) struct DriverWorker<C> {
    jobs: Mutex<Sender<Job<C>>>,
}

impl<C> std::fmt::Debug for DriverWorker<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DriverWorker")
    }
}

impl<C: Send + 'static> DriverWorker<C> {
    /// Spawns the thread and connects **before** returning, so a connection or configuration
    /// failure surfaces from the constructor instead of the first statement.
    pub(crate) fn spawn<F>(connect: F) -> JdbcResult<Self>
    where
        F: FnOnce(&Runtime) -> JdbcResult<C> + Send + 'static,
    {
        let (jobs, incoming) = channel::<Job<C>>();
        let (started, start_report) = channel::<JdbcResult<()>>();
        std::thread::Builder::new()
            .name("agentscope-jdbc-driver".to_string())
            .spawn(move || Self::run(connect, started, incoming))
            .map_err(|e| {
                JdbcError::Database(format!("failed to spawn the driver thread: {e}"))
            })?;
        match start_report.recv() {
            Ok(Ok(())) => Ok(Self {
                jobs: Mutex::new(jobs),
            }),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(JdbcError::Database(
                "the driver thread exited during start-up".to_string(),
            )),
        }
    }

    fn run<F>(connect: F, started: Sender<JdbcResult<()>>, incoming: Receiver<Job<C>>)
    where
        F: FnOnce(&Runtime) -> JdbcResult<C>,
    {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                let _ = started.send(Err(JdbcError::Database(format!(
                    "failed to build the driver runtime: {error}"
                ))));
                return;
            }
        };
        let mut client = match connect(&runtime) {
            Ok(client) => client,
            Err(error) => {
                let _ = started.send(Err(error));
                return;
            }
        };
        if started.send(Ok(())).is_err() {
            // The constructor already gave up; drop the connection.
            return;
        }
        // Ends when the last `DriverWorker` (and therefore the last `Sender`) is dropped.
        while let Ok(job) = incoming.recv() {
            job(&mut client, &runtime);
        }
    }

    /// Runs `work` on the driver thread and waits for its result.
    pub(crate) fn call<T, F>(&self, work: F) -> JdbcResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut C, &Runtime) -> JdbcResult<T> + Send + 'static,
    {
        let (reply, response) = channel();
        let job: Job<C> = Box::new(move |client, runtime| {
            let _ = reply.send(work(client, runtime));
        });
        {
            let jobs = self
                .jobs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            jobs.send(job)
                .map_err(|_| JdbcError::Database("the driver thread is gone".to_string()))?;
        }
        response.recv().map_err(|_| {
            JdbcError::Database("the driver thread dropped the reply".to_string())
        })?
    }
}
