//! The complete-prefix pack as NDJSON, written and verified one line at a
//! time so that neither side holds the log in memory.
//!
//! Line 1 is a small manifest; the next `checkpoint_count` lines are the
//! checkpoint chain; then exactly `tree_size` audit rows in log order. Every
//! line ends in a newline and nothing follows the last row. The manifest
//! has a line of its own so a reader can tell this format apart by reading
//! a few bytes: checkpoints carry witness tokens, so a header holding them
//! would have no useful size limit.

use std::io::BufRead;

use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Transaction};

use super::{
    DOCUMENT_FORMATS, EvidencePack, FormatLadder, PackError, PackManifest, covering_checkpoint,
    manifest_agrees, row_count_disagrees, validate_prefix_chain,
};
use crate::audit::{AuditRow, RowRung};
use crate::audit_pages::AuditPages;
use crate::checkpoints::{Checkpoint, TreeVerification, load_checkpoint_chain};
use crate::error::{PgError, classify_checked_query};
use crate::merkle::Digest;
use crate::prefix_verify::PrefixVerifier;
use crate::role_rebindings::{RebindingScope, RoleRebindings};
use crate::txn::{TxIsolation, begin_isolated_tx};

pub(crate) const PACK_FORMAT_V4: u32 = 4;
const PACK_KIND_PREFIX: &str = "prefix";
/// The streamed prefix's versions: 4, 5 once a covered row names its
/// programme, 9 once one names its semantics. See [`FormatLadder`].
pub(crate) const PREFIX_FORMATS: FormatLadder = FormatLadder {
    legacy: PACK_FORMAT_V4,
    model: 5,
    semantics: 9,
};

/// Line 1 of a complete-prefix pack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrefixPackManifest {
    pub pack_format_version: u32,
    pub pack_kind: String,
    pub tree_size: i64,
    pub root_hash: Digest,
    pub checkpoint_hash: Digest,
    pub checkpoint_count: u64,
}

/// An export in progress: the manifest and chain, then the covered rows a
/// page at a time, all from one read-only snapshot.
pub struct PrefixExport<'p> {
    pub manifest: PrefixPackManifest,
    pub checkpoints: Vec<Checkpoint>,
    tx: Transaction<'p, Postgres>,
    pages: AuditPages,
    remaining: i64,
}

/// Open an export of the complete prefix covered by a checkpoint (the
/// latest, or the one at `tree_size`). Refuses an incomplete prefix before
/// anything is written, so a short export never leaves a partial pack.
pub async fn begin_prefix_export(
    pool: &PgPool,
    tree_size: Option<i64>,
) -> Result<PrefixExport<'_>, PgError> {
    let mut tx = begin_isolated_tx(pool, TxIsolation::SerializableReadOnlyDeferrable).await?;
    let mut checkpoints = load_checkpoint_chain(&mut tx).await?;
    let covering = covering_checkpoint(&checkpoints, tree_size)?;
    checkpoints.retain(|c| c.tree_size <= covering.tree_size);

    let rows_present = sqlx::query_scalar!(r#"SELECT count(*) AS "n!" FROM morpholog.audit"#)
        .fetch_one(&mut *tx)
        .await
        .map_err(classify_checked_query)?;
    if rows_present < covering.tree_size {
        return Err(PgError::AuditPrefixIncomplete {
            tree_size: covering.tree_size,
            rows_present,
        });
    }
    // The manifest goes out before the rows, so the highest rung among the
    // covered rows is asked first, in the same snapshot and order.
    let rung = sqlx::query_scalar!(
        r#"SELECT coalesce(max(CASE
                   WHEN prefix.semantics_version IS NOT NULL THEN 2
                   WHEN prefix.model_hash IS NOT NULL THEN 1
                   ELSE 0
               END), 0) AS "rung!"
           FROM (
               SELECT model_hash, semantics_version FROM morpholog.audit
               ORDER BY committed_at, transition_id
               LIMIT $1
           ) AS prefix"#,
        covering.tree_size
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(classify_checked_query)?;
    let highest = match rung {
        0 => RowRung::Legacy,
        1 => RowRung::Model,
        _ => RowRung::Semantics,
    };

    Ok(PrefixExport {
        manifest: PrefixPackManifest {
            pack_format_version: PREFIX_FORMATS.version(highest),
            pack_kind: PACK_KIND_PREFIX.to_string(),
            tree_size: covering.tree_size,
            root_hash: covering.root_hash,
            checkpoint_hash: covering.checkpoint_hash,
            checkpoint_count: checkpoints.len() as u64,
        },
        checkpoints,
        tx,
        pages: AuditPages::new(None),
        remaining: covering.tree_size,
    })
}

impl PrefixExport<'_> {
    /// The next page of covered rows in log order; empty once all are out.
    pub async fn next_page(&mut self) -> Result<Vec<AuditRow>, PgError> {
        if self.remaining == 0 {
            return Ok(Vec::new());
        }
        let mut page = self.pages.next(&mut self.tx).await?;
        if page.is_empty() {
            return Err(PgError::AuditPrefixIncomplete {
                tree_size: self.manifest.tree_size,
                rows_present: self.manifest.tree_size - self.remaining,
            });
        }
        page.truncate(self.remaining as usize);
        self.remaining -= page.len() as i64;
        Ok(page)
    }
}

/// The format version line 1 of a line-oriented pack announces, or `None`
/// when the line is not such a manifest (a single-document pack, or not a
/// pack at all).
pub fn streamed_pack_version(first_line: &[u8]) -> Option<u32> {
    #[derive(Deserialize)]
    struct Probe {
        pack_format_version: u32,
    }
    serde_json::from_slice::<Probe>(first_line)
        .ok()
        .map(|probe| probe.pack_format_version)
}

/// What verifying a streamed pack found.
#[derive(Debug)]
pub struct PrefixStreamReport {
    pub verdict: TreeVerification,
    pub checkpoints: Vec<Checkpoint>,
    pub role_rebindings: RoleRebindings,
}

/// Verify a complete-prefix pack read line by line, optionally against an
/// external anchor. A pack that breaks the format is [`PackError`] however
/// far in the break comes, so it outranks any verdict about the rows before
/// it. Memory holds one row at a time.
pub fn verify_prefix_stream(
    input: impl BufRead,
    anchor: Option<&Checkpoint>,
) -> Result<PrefixStreamReport, PackError> {
    let (mut rows, checkpoints) = RowStream::open(input)?;
    let mut verifier = PrefixVerifier::new(&checkpoints, anchor);
    while let Some(row) = rows.next_row()? {
        verifier.push(&row)?;
    }
    let (verdict, rebindings) = verifier.finish();
    let established = matches!(verdict, TreeVerification::Intact { .. });
    Ok(PrefixStreamReport {
        verdict,
        role_rebindings: rebindings.finish(RebindingScope::CompletePrefix, established),
        checkpoints,
    })
}

/// A complete-prefix pack read whole into the single-document shape, for a
/// caller that needs every row in memory, such as scoring. The format is
/// held to the same rules as [`verify_prefix_stream`]; the tree is not
/// verified here.
pub fn read_prefix_stream(input: impl BufRead) -> Result<EvidencePack, PackError> {
    let (mut stream, checkpoints) = RowStream::open(input)?;
    let mut rows = Vec::new();
    while let Some(row) = stream.next_row()? {
        rows.push(row);
    }
    let manifest = stream.manifest;
    Ok(EvidencePack {
        manifest: PackManifest {
            pack_format_version: DOCUMENT_FORMATS.for_rows(&rows),
            tree_size: manifest.tree_size,
            root_hash: manifest.root_hash,
            checkpoint_hash: manifest.checkpoint_hash,
        },
        checkpoints,
        rows,
    })
}

/// The rows of a pack whose manifest and chain have been read and checked:
/// exactly the covered number, in strictly increasing log order, and
/// nothing after them.
struct RowStream<R> {
    lines: Lines<R>,
    manifest: PrefixPackManifest,
    fed: usize,
    previous: Option<(jiff::Timestamp, uuid::Uuid)>,
    /// The highest rung among the rows read so far, held to the
    /// manifest's version once the last row is in.
    highest_seen: RowRung,
}

impl<R: BufRead> RowStream<R> {
    fn open(input: R) -> Result<(Self, Vec<Checkpoint>), PackError> {
        let mut lines = Lines::new(input);
        let manifest: PrefixPackManifest = lines.parse("the manifest")?;
        if !PREFIX_FORMATS.admits(manifest.pack_format_version)
            || manifest.pack_kind != PACK_KIND_PREFIX
        {
            return Err(PackError::Malformed {
                detail: format!(
                    "not a complete-prefix pack: pack_format_version {}, pack_kind {:?}",
                    manifest.pack_format_version, manifest.pack_kind
                ),
            });
        }
        let mut checkpoints = Vec::new();
        for _ in 0..manifest.checkpoint_count {
            checkpoints.push(lines.parse::<Checkpoint>("a checkpoint")?);
        }
        let covering = validate_prefix_chain(&checkpoints)?;
        manifest_agrees(
            covering,
            manifest.tree_size,
            &manifest.root_hash,
            &manifest.checkpoint_hash,
        )?;
        let stream = RowStream {
            lines,
            manifest,
            fed: 0,
            previous: None,
            highest_seen: RowRung::Legacy,
        };
        Ok((stream, checkpoints))
    }

    fn next_row(&mut self) -> Result<Option<AuditRow>, PackError> {
        let expected = self.manifest.tree_size as usize;
        if self.fed == expected {
            if !self.lines.at_end()? {
                return Err(PackError::Malformed {
                    detail: format!(
                        "line {}: data after the {expected} rows the covering checkpoint commits to",
                        self.lines.number + 1
                    ),
                });
            }
            PREFIX_FORMATS.check(self.manifest.pack_format_version, self.highest_seen)?;
            return Ok(None);
        }
        if self.lines.at_end()? {
            return Err(row_count_disagrees(self.fed, self.manifest.tree_size));
        }
        let row: AuditRow = self.lines.parse("an audit row")?;
        let here = (row.committed_at, row.transition_id);
        if let Some(before) = self.previous
            && here <= before
        {
            return Err(PackError::Malformed {
                detail: format!(
                    "line {}: rows are not in strictly increasing log order ({}, {}) after ({}, {})",
                    self.lines.number, here.0, here.1, before.0, before.1
                ),
            });
        }
        self.previous = Some(here);
        self.fed += 1;
        let rung = row.rung();
        if rung > self.highest_seen {
            self.highest_seen = rung;
            // Refused at the row, not after the stream: a reader of the
            // announced version must never have been handed it.
            if PREFIX_FORMATS
                .rung(self.manifest.pack_format_version)
                .is_some_and(|announced| rung > announced)
            {
                PREFIX_FORMATS.check(self.manifest.pack_format_version, rung)?;
            }
        }
        Ok(Some(row))
    }
}

/// Newline-terminated lines, each parsed on its own, numbered for the
/// message when one is wrong.
struct Lines<R> {
    input: R,
    buf: Vec<u8>,
    number: usize,
}

impl<R: BufRead> Lines<R> {
    fn new(input: R) -> Self {
        Lines {
            input,
            buf: Vec::new(),
            number: 0,
        }
    }

    fn at_end(&mut self) -> Result<bool, PackError> {
        Ok(self.input.fill_buf().map_err(read_error)?.is_empty())
    }

    fn parse<T: serde::de::DeserializeOwned>(&mut self, what: &str) -> Result<T, PackError> {
        self.number += 1;
        let malformed = |detail: String| PackError::Malformed { detail };
        self.buf.clear();
        self.input
            .read_until(b'\n', &mut self.buf)
            .map_err(read_error)?;
        let Some(line) = self.buf.strip_suffix(b"\n") else {
            return Err(malformed(if self.buf.is_empty() {
                format!(
                    "line {}: the pack ends where {what} was expected",
                    self.number
                )
            } else {
                format!(
                    "line {}: the last line does not end in a newline",
                    self.number
                )
            }));
        };
        serde_json::from_slice(line).map_err(|e| {
            malformed(format!(
                "line {}: {what} that does not parse: {e}",
                self.number
            ))
        })
    }
}

/// A stream that yields bytes it cannot decode, or ends inside a
/// compressed block, is a malformed pack; any other read failure is the
/// reader's own and is reported as such.
fn read_error(e: std::io::Error) -> PackError {
    use std::io::ErrorKind;
    match e.kind() {
        ErrorKind::InvalidData | ErrorKind::InvalidInput | ErrorKind::UnexpectedEof => {
            PackError::Malformed {
                detail: format!("the pack could not be read: {e}"),
            }
        }
        _ => PackError::Read(e),
    }
}
