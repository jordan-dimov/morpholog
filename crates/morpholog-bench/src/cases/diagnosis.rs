//! Where a point's conflicts sat: the statement each failed act was
//! running, from the server's log, and the serializable read locks held
//! while the workers ran. Both are optional, and neither is timed.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use morpholog_postgres::PgPool;
use serde::Serialize;

/// Reads only what the server log gained since the last read.
pub(super) struct LogCursor {
    path: PathBuf,
    offset: u64,
}

impl LogCursor {
    /// Starts at the log's end, so nothing written before counts.
    pub(super) fn at_end(path: &Path) -> Result<Self> {
        let offset = std::fs::metadata(path)
            .with_context(|| format!("read the server log {}", path.display()))?
            .len();
        Ok(LogCursor {
            path: path.to_path_buf(),
            offset,
        })
    }

    /// Failed statements written since the last call, by kind and stage.
    pub(super) fn failures_since(&mut self) -> Result<BTreeMap<String, u64>> {
        let mut file = std::fs::File::open(&self.path)
            .with_context(|| format!("open {}", self.path.display()))?;
        file.seek(SeekFrom::Start(self.offset))?;
        let mut text = String::new();
        file.read_to_string(&mut text)
            .with_context(|| format!("read {}", self.path.display()))?;
        self.offset += text.len() as u64;
        Ok(failures(&text))
    }
}

/// Lines carry `log_line_prefix = '%m [%p] %d '`. An error line names
/// what went wrong; the same process's next `STATEMENT` line names what
/// it was running.
fn failures(log: &str) -> BTreeMap<String, u64> {
    let mut pending: BTreeMap<&str, &str> = BTreeMap::new();
    let mut counts = BTreeMap::new();
    for line in log.lines() {
        let Some((_, rest)) = line.split_once(" [") else {
            continue;
        };
        let Some((pid, rest)) = rest.split_once("] ") else {
            continue;
        };
        if let Some((_, message)) = rest.split_once("ERROR:  ") {
            if let Some(kind) = kind(message) {
                pending.insert(pid, kind);
            }
        } else if let Some((_, statement)) = rest.split_once("STATEMENT:  ")
            && let Some(kind) = pending.remove(pid)
        {
            *counts
                .entry(format!("{kind} at {}", stage(statement)))
                .or_default() += 1;
        }
    }
    counts
}

fn kind(message: &str) -> Option<&'static str> {
    if message.starts_with("could not serialize access") {
        Some("serialization failure")
    } else if message.starts_with("deadlock detected") {
        Some("deadlock")
    } else if message.starts_with("out of shared memory") || message.contains("RWConflictPool") {
        Some("out of lock memory")
    } else {
        None
    }
}

fn stage(statement: &str) -> String {
    let statement = statement.trim();
    if let Some(rest) = statement.strip_prefix("/* morpholog compiled invariant ") {
        let name = rest.split_whitespace().next().unwrap_or("?");
        format!("the check {name}")
    } else if statement.starts_with("COMMIT") {
        "commit".to_string()
    } else if statement.starts_with("INSERT INTO morpholog.audit") {
        "the audit insert".to_string()
    } else if statement.starts_with("INSERT INTO morpholog.claims") {
        "the claim insert".to_string()
    } else {
        statement.chars().take(48).collect()
    }
}

/// The serializable read locks of the point's database, sampled while
/// its workers run.
#[derive(Serialize, Default)]
pub(super) struct Locks {
    samples: u64,
    /// The most any one transaction was seen holding.
    peak_per_transaction: u64,
    /// The most held at once, across every transaction.
    peak_total: u64,
    /// The relations held most, with the lock's grain and the mean count
    /// a sample found.
    by_relation: Vec<(String, String, f64)>,
}

impl Locks {
    pub(super) fn peak_per_transaction(&self) -> u64 {
        self.peak_per_transaction
    }

    pub(super) fn peak_total(&self) -> u64 {
        self.peak_total
    }

    pub(super) fn summary(&self) -> String {
        self.by_relation
            .iter()
            .take(4)
            .map(|(relation, grain, mean)| format!("{relation} ({grain}) {mean:.1}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

pub(super) struct Sampler {
    stop: Arc<AtomicBool>,
    handle: tokio::task::JoinHandle<Result<Locks>>,
}

impl Sampler {
    /// Samples every 50 ms on a connection of its own.
    pub(super) fn start(pool: PgPool) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let handle = tokio::spawn(async move {
            let mut locks = Locks::default();
            let mut held: BTreeMap<(String, String), u64> = BTreeMap::new();
            while !stopping.load(Ordering::Relaxed) {
                let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
                    "SELECT l.virtualtransaction, l.locktype, l.relation::regclass::text
                     FROM pg_locks l
                     WHERE l.mode = 'SIReadLock'
                       AND l.database = (SELECT oid FROM pg_database WHERE datname = current_database())",
                )
                .fetch_all(&pool)
                .await
                .context("sample pg_locks")?;
                let mut per_transaction: BTreeMap<&str, u64> = BTreeMap::new();
                for (transaction, locktype, relation) in &rows {
                    *per_transaction.entry(transaction).or_default() += 1;
                    *held
                        .entry((relation.clone().unwrap_or_default(), locktype.clone()))
                        .or_default() += 1;
                }
                let peak = per_transaction.values().copied().max().unwrap_or(0);
                locks.peak_per_transaction = locks.peak_per_transaction.max(peak);
                locks.peak_total = locks.peak_total.max(rows.len() as u64);
                locks.samples += 1;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let samples = locks.samples.max(1) as f64;
            let mut by_relation: Vec<(String, String, f64)> = held
                .into_iter()
                .map(|((relation, grain), n)| (relation, grain, n as f64 / samples))
                .collect();
            by_relation.sort_by(|a, b| b.2.total_cmp(&a.2));
            by_relation.truncate(8);
            locks.by_relation = by_relation;
            pool.close().await;
            Ok(locks)
        });
        Sampler { stop, handle }
    }

    pub(super) async fn finish(self) -> Result<Locks> {
        self.stop.store(true, Ordering::Relaxed);
        self.handle.await.context("join the lock sampler")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_is_named_by_the_statement_its_process_was_running() {
        let log = "\
2026-11-24 13:52:00.101 GMT [41] morpholog_bench ERROR:  could not serialize access due to read/write dependencies among transactions
2026-11-24 13:52:00.101 GMT [41] morpholog_bench DETAIL:  Reason code: Canceled on identification as a pivot, during conflict out checking.
2026-11-24 13:52:00.101 GMT [42] morpholog_bench ERROR:  out of shared memory
2026-11-24 13:52:00.101 GMT [41] morpholog_bench STATEMENT:  /* morpholog compiled invariant never_crossed v1 stage2 */
\tSELECT 1
2026-11-24 13:52:00.102 GMT [42] morpholog_bench STATEMENT:  INSERT INTO morpholog.audit (
2026-11-24 13:52:00.103 GMT [43] morpholog_bench ERROR:  could not serialize access due to read/write dependencies among transactions
2026-11-24 13:52:00.103 GMT [43] morpholog_bench STATEMENT:  COMMIT
2026-11-24 13:52:00.104 GMT [44] morpholog_bench ERROR:  relation \"x\" does not exist
2026-11-24 13:52:00.104 GMT [44] morpholog_bench STATEMENT:  SELECT * FROM x
";
        let counts = failures(log);
        assert_eq!(
            counts.into_iter().collect::<Vec<_>>(),
            [
                ("out of lock memory at the audit insert".to_string(), 1),
                ("serialization failure at commit".to_string(), 1),
                (
                    "serialization failure at the check never_crossed".to_string(),
                    1
                ),
            ]
        );
    }
}
