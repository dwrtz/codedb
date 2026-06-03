use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value as JsonValue, json};

use crate::artifact::ArtifactKind;
use crate::store::{CacheEntry, CodeDb, canonical_json};

const ARTIFACT_STATUS_SCHEMA: &str = "codedb/artifact-status/v1";
const ARTIFACT_JOB_SCHEMA: &str = "codedb/artifact-job/v1";
const ARTIFACT_JOB_ERROR_SCHEMA: &str = "codedb/artifact-job-error/v1";
const JOB_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
const JOB_WAIT_POLL: Duration = Duration::from_millis(10);

static NEXT_WORKER_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ArtifactJobClaim {
    Claimed,
    Succeeded,
    Busy {
        status: String,
        worker_id: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct ArtifactJobRecord {
    pub(crate) cache_key: String,
    pub(crate) artifact_kind: String,
    pub(crate) status: String,
    pub(crate) worker_id: Option<String>,
    pub(crate) started_at: Option<String>,
    pub(crate) finished_at: Option<String>,
    pub(crate) error_json: Option<JsonValue>,
}

impl ArtifactJobRecord {
    pub(crate) fn to_json(&self) -> JsonValue {
        json!({
            "schema": ARTIFACT_JOB_SCHEMA,
            "cache_key": self.cache_key,
            "artifact_kind": self.artifact_kind,
            "status": self.status,
            "worker_id": self.worker_id,
            "started_at": self.started_at,
            "finished_at": self.finished_at,
            "error": self.error_json,
        })
    }
}

pub(crate) fn new_worker_id(label: &str) -> String {
    let sequence = NEXT_WORKER_ID.fetch_add(1, Ordering::Relaxed);
    format!("worker:{label}:{}:{sequence}", std::process::id())
}

impl CodeDb {
    #[allow(dead_code)]
    pub(crate) fn ensure_artifact_job(
        &mut self,
        cache_key: &str,
        artifact_kind: ArtifactKind,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO artifact_jobs
             (cache_key, artifact_kind, status)
             VALUES (?1, ?2, 'queued')",
            params![cache_key, artifact_kind.as_str()],
        )?;
        Ok(())
    }

    pub(crate) fn ensure_artifact_job_for_cache_state(
        &mut self,
        cache_key: &str,
        artifact_kind: ArtifactKind,
        cache_exists: bool,
    ) -> Result<()> {
        if cache_exists {
            self.conn.execute(
                "INSERT OR IGNORE INTO artifact_jobs
                 (cache_key, artifact_kind, status, worker_id, started_at, finished_at)
                 VALUES (?1, ?2, 'succeeded', 'worker:cache-observed', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)",
                params![cache_key, artifact_kind.as_str()],
            )?;
            self.conn.execute(
                "UPDATE artifact_jobs
                 SET status = 'succeeded',
                     worker_id = COALESCE(worker_id, 'worker:cache-observed'),
                     started_at = COALESCE(started_at, CURRENT_TIMESTAMP),
                     finished_at = COALESCE(finished_at, CURRENT_TIMESTAMP),
                     error_json = NULL
                 WHERE cache_key = ?1
                   AND artifact_kind = ?2
                   AND status NOT IN ('succeeded', 'running')",
                params![cache_key, artifact_kind.as_str()],
            )?;
        } else {
            self.ensure_artifact_job(cache_key, artifact_kind)?;
        }
        let record = self
            .artifact_job_record(cache_key)?
            .ok_or_else(|| anyhow!("artifact job {cache_key} was not recorded"))?;
        if record.artifact_kind != artifact_kind.as_str() {
            bail!(
                "artifact job {cache_key} was registered as {}, not {}",
                record.artifact_kind,
                artifact_kind.as_str()
            );
        }
        Ok(())
    }

    pub(crate) fn claim_artifact_job(
        &mut self,
        cache_key: &str,
        artifact_kind: ArtifactKind,
        worker_id: &str,
    ) -> Result<ArtifactJobClaim> {
        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = (|| {
            self.conn.execute(
                "INSERT OR IGNORE INTO artifact_jobs
                 (cache_key, artifact_kind, status)
                 VALUES (?1, ?2, 'queued')",
                params![cache_key, artifact_kind.as_str()],
            )?;
            self.conn.execute(
                "UPDATE artifact_jobs
                 SET status = 'running',
                     worker_id = ?2,
                     started_at = CURRENT_TIMESTAMP,
                     finished_at = NULL,
                     error_json = NULL
                 WHERE cache_key = ?1
                   AND artifact_kind = ?3
                   AND (
                        status IN ('queued', 'failed', 'abandoned')
                        OR (
                            status = 'succeeded'
                            AND NOT EXISTS (
                                SELECT 1 FROM compile_cache WHERE cache_key = ?1
                            )
                        )
                   )",
                params![cache_key, worker_id, artifact_kind.as_str()],
            )?;
            let changed = self.conn.changes();
            let record = self
                .artifact_job_record(cache_key)?
                .ok_or_else(|| anyhow!("artifact job disappeared after claim attempt"))?;
            if changed == 1 {
                return Ok(ArtifactJobClaim::Claimed);
            }
            if record.artifact_kind != artifact_kind.as_str() {
                bail!(
                    "artifact job {cache_key} was registered as {}, not {}",
                    record.artifact_kind,
                    artifact_kind.as_str()
                );
            }
            if record.status == "succeeded" {
                return Ok(ArtifactJobClaim::Succeeded);
            }
            Ok(ArtifactJobClaim::Busy {
                status: record.status,
                worker_id: record.worker_id,
            })
        })();
        finish_job_transaction(&mut self.conn, result)
    }

    pub(crate) fn complete_artifact_job(&mut self, cache_key: &str, worker_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE artifact_jobs
             SET status = 'succeeded',
                 finished_at = CURRENT_TIMESTAMP,
                 error_json = NULL
             WHERE cache_key = ?1
               AND status = 'running'
               AND worker_id = ?2",
            params![cache_key, worker_id],
        )?;
        if self.conn.changes() != 1 {
            bail!("artifact job {cache_key} is not owned by worker {worker_id}");
        }
        Ok(())
    }

    pub(crate) fn fail_artifact_job(
        &mut self,
        cache_key: &str,
        worker_id: &str,
        error: &JsonValue,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE artifact_jobs
             SET status = 'failed',
                 finished_at = CURRENT_TIMESTAMP,
                 error_json = ?3
             WHERE cache_key = ?1
               AND status = 'running'
               AND worker_id = ?2",
            params![cache_key, worker_id, canonical_json(error)],
        )?;
        if self.conn.changes() != 1 {
            bail!("artifact job {cache_key} is not owned by worker {worker_id}");
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn abandon_artifact_job(
        &mut self,
        cache_key: &str,
        worker_id: &str,
        error: &JsonValue,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE artifact_jobs
             SET status = 'abandoned',
                 finished_at = CURRENT_TIMESTAMP,
                 error_json = ?3
             WHERE cache_key = ?1
               AND status = 'running'
               AND worker_id = ?2",
            params![cache_key, worker_id, canonical_json(error)],
        )?;
        if self.conn.changes() != 1 {
            bail!("artifact job {cache_key} is not owned by worker {worker_id}");
        }
        Ok(())
    }

    pub(crate) fn wait_for_artifact_cache(
        &mut self,
        key_input: &crate::artifact::CacheKeyInput,
        cache_key: &str,
    ) -> Result<CacheEntry> {
        let deadline = Instant::now() + JOB_WAIT_TIMEOUT;
        loop {
            if let Some(cache_entry) = self.lookup_cache(key_input)? {
                return Ok(cache_entry);
            }
            if Instant::now() >= deadline {
                bail!("timed out waiting for artifact job {cache_key}");
            }
            if let Some(record) = self.artifact_job_record(cache_key)? {
                match record.status.as_str() {
                    "failed" | "abandoned" => {
                        bail!(
                            "artifact job {cache_key} ended as {}: {}",
                            record.status,
                            record
                                .error_json
                                .as_ref()
                                .map(canonical_json)
                                .unwrap_or_else(|| "null".to_string())
                        );
                    }
                    "queued" | "running" | "succeeded" => {}
                    other => bail!("artifact job {cache_key} has unknown status {other:?}"),
                }
                if record.status == "succeeded" {
                    self.conn.execute(
                        "UPDATE artifact_jobs
                         SET status = 'queued',
                             worker_id = NULL,
                             started_at = NULL,
                             finished_at = NULL,
                             error_json = NULL
                         WHERE cache_key = ?1
                           AND status = 'succeeded'
                           AND NOT EXISTS (
                               SELECT 1 FROM compile_cache WHERE cache_key = ?1
                           )",
                        params![cache_key],
                    )?;
                    if self.conn.changes() == 1 {
                        bail!(
                            "artifact job {cache_key} succeeded but its disposable cache entry is missing; retry the build"
                        );
                    }
                }
            }
            thread::sleep(JOB_WAIT_POLL);
        }
    }

    pub(crate) fn artifact_job_record(&self, cache_key: &str) -> Result<Option<ArtifactJobRecord>> {
        self.conn
            .query_row(
                "SELECT cache_key, artifact_kind, status, worker_id,
                        started_at, finished_at, error_json
                 FROM artifact_jobs
                 WHERE cache_key = ?1",
                params![cache_key],
                artifact_job_record_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub(crate) fn artifact_job_json_for_cache_keys(
        &self,
        cache_keys: &[String],
    ) -> Result<Vec<JsonValue>> {
        let mut jobs = Vec::new();
        for cache_key in cache_keys {
            if let Some(record) = self.artifact_job_record(cache_key)? {
                jobs.push(record.to_json());
            }
        }
        Ok(jobs)
    }

    pub fn artifact_status_json(&self) -> Result<String> {
        let mut job_stmt = self.conn.prepare(
            "SELECT cache_key, artifact_kind, status, worker_id,
                    started_at, finished_at, error_json
             FROM artifact_jobs
             ORDER BY cache_key",
        )?;
        let jobs = job_stmt
            .query_map([], artifact_job_record_from_row)?
            .map(|row| row.map(|record| record.to_json()))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(job_stmt);

        let mut cache_stmt = self.conn.prepare(
            "SELECT cache_key, artifact_kind, input_hash, backend, target,
                    artifact_hash, artifact_bytes IS NOT NULL
             FROM compile_cache
             ORDER BY cache_key",
        )?;
        let cache_entries = cache_stmt
            .query_map([], |row| {
                Ok(json!({
                    "cache_key": row.get::<_, String>(0)?,
                    "artifact_kind": row.get::<_, String>(1)?,
                    "input_hash": row.get::<_, String>(2)?,
                    "backend": row.get::<_, String>(3)?,
                    "target": row.get::<_, String>(4)?,
                    "artifact_hash": row.get::<_, String>(5)?,
                    "has_artifact_bytes": row.get::<_, bool>(6)?,
                }))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(format!(
            "{}\n",
            canonical_json(&json!({
                "schema": ARTIFACT_STATUS_SCHEMA,
                "jobs": jobs,
                "cache_entries": cache_entries,
            }))
        ))
    }
}

pub(crate) fn artifact_job_error(kind: &str, message: impl Into<String>) -> JsonValue {
    json!({
        "schema": ARTIFACT_JOB_ERROR_SCHEMA,
        "kind": kind,
        "message": message.into(),
    })
}

fn artifact_job_record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArtifactJobRecord> {
    let error_json_text = row.get::<_, Option<String>>(6)?;
    Ok(ArtifactJobRecord {
        cache_key: row.get(0)?,
        artifact_kind: row.get(1)?,
        status: row.get(2)?,
        worker_id: row.get(3)?,
        started_at: row.get(4)?,
        finished_at: row.get(5)?,
        error_json: error_json_text
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|err| {
                rusqlite::Error::FromSqlConversionFailure(
                    6,
                    rusqlite::types::Type::Text,
                    Box::new(err),
                )
            })?,
    })
}

fn finish_job_transaction<T>(conn: &mut rusqlite::Connection, result: Result<T>) -> Result<T> {
    match result {
        Ok(value) => {
            conn.execute_batch("COMMIT")
                .context("failed to commit artifact job transaction")?;
            Ok(value)
        }
        Err(err) => {
            if let Err(rollback_err) = conn.execute_batch("ROLLBACK") {
                return Err(err).context(format!("rollback failed: {rollback_err}"));
            }
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::params;
    use serde_json::json;
    use tempfile::tempdir;

    use crate::backend::ArtifactKind;
    use crate::jobs::ArtifactJobClaim;
    use crate::store::CodeDb;

    #[test]
    fn claim_allows_one_owner_and_retries_failed_or_abandoned_jobs() {
        let temp = tempdir().unwrap();
        let mut db = CodeDb::open(temp.path().join("jobs.sqlite")).unwrap();
        let cache_key = "sha256:job";

        assert_eq!(
            db.claim_artifact_job(cache_key, ArtifactKind::ObjectFile, "worker-a")
                .unwrap(),
            ArtifactJobClaim::Claimed
        );
        assert_eq!(
            db.claim_artifact_job(cache_key, ArtifactKind::ObjectFile, "worker-b")
                .unwrap(),
            ArtifactJobClaim::Busy {
                status: "running".to_string(),
                worker_id: Some("worker-a".to_string()),
            }
        );
        db.complete_artifact_job(cache_key, "worker-a").unwrap();
        assert_eq!(
            db.claim_artifact_job(cache_key, ArtifactKind::ObjectFile, "worker-b")
                .unwrap(),
            ArtifactJobClaim::Claimed
        );

        let succeeded_key = "sha256:succeeded-job";
        let input_hash = db.put_object("TestInput", &json!({})).unwrap();
        db.conn
            .execute(
                "INSERT INTO compile_cache
                 (cache_key, cache_key_json, input_hash, backend, target, compiler_version,
                  artifact_kind, artifact_hash)
                 VALUES (?1, '{}', ?2, 'test', 'test', 'test', ?3, 'sha256:artifact')",
                params![succeeded_key, input_hash, ArtifactKind::ObjectFile.as_str()],
            )
            .unwrap();
        assert_eq!(
            db.claim_artifact_job(succeeded_key, ArtifactKind::ObjectFile, "worker-a")
                .unwrap(),
            ArtifactJobClaim::Claimed
        );
        db.complete_artifact_job(succeeded_key, "worker-a").unwrap();
        assert_eq!(
            db.claim_artifact_job(succeeded_key, ArtifactKind::ObjectFile, "worker-b")
                .unwrap(),
            ArtifactJobClaim::Succeeded
        );

        let failed_key = "sha256:failed-job";
        assert_eq!(
            db.claim_artifact_job(failed_key, ArtifactKind::ObjectFile, "worker-a")
                .unwrap(),
            ArtifactJobClaim::Claimed
        );
        db.fail_artifact_job(
            failed_key,
            "worker-a",
            &json!({"kind": "compile_failed", "message": "boom"}),
        )
        .unwrap();
        assert_eq!(
            db.claim_artifact_job(failed_key, ArtifactKind::ObjectFile, "worker-b")
                .unwrap(),
            ArtifactJobClaim::Claimed
        );

        let abandoned_key = "sha256:abandoned-job";
        assert_eq!(
            db.claim_artifact_job(abandoned_key, ArtifactKind::LinkPlan, "worker-a")
                .unwrap(),
            ArtifactJobClaim::Claimed
        );
        db.abandon_artifact_job(
            abandoned_key,
            "worker-a",
            &json!({"kind": "worker_lost", "message": "retry"}),
        )
        .unwrap();
        assert_eq!(
            db.claim_artifact_job(abandoned_key, ArtifactKind::LinkPlan, "worker-b")
                .unwrap(),
            ArtifactJobClaim::Claimed
        );
    }
}
