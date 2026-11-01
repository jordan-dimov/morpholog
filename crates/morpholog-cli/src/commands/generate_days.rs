//! `morpholog generate local-days` and `generate business-days`: calendar
//! claims as `propose --batch` rows, one per day, for a transformation the
//! programme declares. A calendar is data that changes over time, so a
//! day enters the record as a claim with its authority and version on it,
//! never as a runtime lookup; this is what writes a year of them.
//!
//! The generator is an adapter to the proposal interface, not a producer
//! of its own format: it reads the transformation's parameters, fills the
//! fields it generates by name, takes every other parameter as a fixed
//! value, and decodes every row through the same codec `propose` uses.
//! The whole batch is validated before the first row is written, so a
//! refusal leaves stdout empty.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use jiff::civil::{Date, Weekday};
use jiff::tz::{AmbiguousOffset, TimeZone};
use jiff::{SignedDuration, Timestamp};
use morpholog_core::{
    ParamKind, PredicateArgKind, PreparedProgram, Transformation, transformation_param_kinds,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::commands::args::{CliArgs, decode_args};
use crate::commands::{lookup_transformation, parse_or_report, prepare_or_report};
use crate::{GenerateBusinessDaysArgs, GenerateLocalDaysArgs, GeneratedRowsArgs};

const LOCAL_DAY_FIELDS: [&str; 4] = ["local_date", "starts_at", "ends_at", "tzdb_version"];
const BUSINESS_DAY_FIELDS: [&str; 3] = ["date", "authority", "version"];

pub(crate) fn local_days(args: &GenerateLocalDaysArgs) -> anyhow::Result<()> {
    let target = Target::resolve(&args.rows, &LOCAL_DAY_FIELDS)?;
    let zone = check_zone_name(&args.zone)?;
    let dir = zoneinfo_dir(args.zoneinfo.as_deref());
    let version = tzdb_version(&dir)?;
    let tz = {
        let path = dir.join(zone);
        let bytes = std::fs::read(&path)
            .with_context(|| format!("read the zone `{zone}` at {}", path.display()))?;
        TimeZone::tzif(zone, &bytes)
            .with_context(|| format!("{} is not a TZif zone file", path.display()))?
    };
    let fields = target.dates().into_iter().map(|date| {
        let starts_at = day_start(&tz, date)?;
        let ends_at = day_start(&tz, date.tomorrow()?)?;
        if ends_at <= starts_at {
            bail!(
                "{date} does not exist in {zone}: its clock skips from {starts_at} straight \
                 to the next day, so the day has no instants; narrow the range around it"
            );
        }
        Ok([
            ("local_date", date.to_string()),
            ("starts_at", starts_at.to_string()),
            ("ends_at", ends_at.to_string()),
            ("tzdb_version", version.clone()),
        ])
    });
    let rows = target.rows(fields)?;
    emit(
        &rows,
        &format!(
            "{} local days of {zone} for `{}`, {} to {} inclusive, tzdb {version} from {}",
            rows.len(),
            target.transformation.name,
            args.rows.from,
            args.rows.until,
            dir.display()
        ),
    )
}

pub(crate) fn business_days(args: &GenerateBusinessDaysArgs) -> anyhow::Result<()> {
    let target = Target::resolve(&args.rows, &BUSINESS_DAY_FIELDS)?;
    let holidays = HolidayList::read(&args.holidays)?;
    if args.rows.from < holidays.from || holidays.until < args.rows.until {
        bail!(
            "{} covers {} to {}; the range {} to {} is not inside it, and a weekday the list \
             does not cover is not an established business day",
            args.holidays.display(),
            holidays.from,
            holidays.until,
            args.rows.from,
            args.rows.until
        );
    }
    let fields = target
        .dates()
        .into_iter()
        .filter(|date| {
            !matches!(date.weekday(), Weekday::Saturday | Weekday::Sunday)
                && !holidays.holidays.contains(date)
        })
        .map(|date| {
            Ok([
                ("date", date.to_string()),
                ("authority", holidays.authority.clone()),
                ("version", holidays.version.clone()),
            ])
        });
    let rows = target.rows(fields)?;
    emit(
        &rows,
        &format!(
            "{} business days for `{}`, {} to {} inclusive, {} holidays of {} version {}",
            rows.len(),
            target.transformation.name,
            args.rows.from,
            args.rows.until,
            holidays.holidays.len(),
            holidays.authority,
            holidays.version
        ),
    )
}

/// The transformation the rows propose, with the generated fields
/// matched against its parameters and the fixed values for the rest.
struct Target<'a> {
    args: &'a GeneratedRowsArgs,
    prepared: PreparedProgram,
    transformation: Transformation,
    fixed: BTreeMap<String, String>,
}

impl<'a> Target<'a> {
    fn resolve(args: &'a GeneratedRowsArgs, generated: &[&str]) -> anyhow::Result<Self> {
        let parsed = parse_or_report(&args.file)?;
        let prepared = prepare_or_report(&parsed)?;
        let transformation =
            lookup_transformation(&prepared, &args.transformation, &args.file)?.clone();

        let mut fixed = BTreeMap::new();
        for pair in &args.arg {
            let Some((name, value)) = pair.split_once('=') else {
                bail!("--arg `{pair}` is not `name=value`");
            };
            if generated.contains(&name) {
                bail!(
                    "--arg `{name}` names a field the generator fills ({}); a generated \
                     value is never overridden",
                    generated.join(", ")
                );
            }
            if fixed.insert(name.to_string(), value.to_string()).is_some() {
                bail!("--arg `{name}` is given twice");
            }
        }

        let declared: Vec<&str> = transformation
            .parameters
            .iter()
            .map(morpholog_core::Var::as_str)
            .collect();
        let missing: Vec<&str> = declared
            .iter()
            .copied()
            .filter(|p| !generated.contains(p) && !fixed.contains_key(*p))
            .collect();
        let unexpected: Vec<&str> = generated
            .iter()
            .copied()
            .chain(fixed.keys().map(String::as_str))
            .filter(|p| !declared.contains(p))
            .collect();
        if !missing.is_empty() || !unexpected.is_empty() {
            bail!(
                "`{}` takes ({}); the generator fills {} and --arg supplies the rest. \
                 Not supplied: [{}]. Not a parameter: [{}].",
                transformation.name,
                declared.join(", "),
                generated.join(", "),
                missing.join(", "),
                unexpected.join(", "),
            );
        }
        if args.from > args.until {
            bail!(
                "--from {} is after --until {}; the range is inclusive at both ends",
                args.from,
                args.until
            );
        }
        Ok(Self {
            args,
            prepared,
            transformation,
            fixed,
        })
    }

    /// Every date from `--from` to `--until`, both included.
    fn dates(&self) -> Vec<Date> {
        let until = self.args.until;
        self.args
            .from
            .series(jiff::Span::new().days(1))
            .take_while(|d| *d <= until)
            .collect()
    }

    /// Every row decoded as `propose` would decode it, before any is
    /// written: a failure on the last day leaves no usable prefix.
    fn rows<const N: usize>(
        &self,
        generated: impl Iterator<Item = anyhow::Result<[(&'static str, String); N]>>,
    ) -> anyhow::Result<Vec<Value>> {
        let validated = self.prepared.validated();
        let kinds = transformation_param_kinds(&validated, &self.transformation.name)
            .map_err(|e| anyhow!("internal: param-kind analysis failed: {e}"))?;
        let mut rows = Vec::new();
        for fields in generated {
            let fields = fields?;
            let mut named = Map::new();
            for (param, kind) in &kinds {
                let value = match fields.iter().find(|(name, _)| *name == param.as_str()) {
                    Some((_, generated)) => Value::String(generated.clone()),
                    None => {
                        let fixed = self.fixed.get(param.as_str()).ok_or_else(|| {
                            anyhow!("internal: `{param}` is neither generated nor fixed")
                        })?;
                        fixed_value(kind, fixed)
                    }
                };
                named.insert(param.to_string(), value);
            }
            let named_json = Value::Object(named).to_string();
            decode_args(
                &validated,
                &self.transformation,
                &self.args.file,
                CliArgs::Named(&named_json),
            )
            .with_context(|| format!("row for {}", fields[0].1))?;
            rows.push(json!({
                "transformation": self.transformation.name.as_str(),
                "actor": self.args.actor,
                "args_named": serde_json::from_str::<Value>(&named_json)?,
            }));
        }
        Ok(rows)
    }
}

/// A `--arg` value in the JSON shape its parameter's kind takes: a
/// boolean or a collection is written as JSON, everything else travels
/// as the string the named codec reads. A value that does not parse is
/// passed on as text, so the codec's own message names the parameter.
fn fixed_value(kind: &ParamKind, text: &str) -> Value {
    let typed = match kind {
        ParamKind::Concrete(PredicateArgKind::Bool) => text.parse::<bool>().ok().map(Value::Bool),
        ParamKind::Collection(_) => serde_json::from_str::<Value>(text)
            .ok()
            .filter(Value::is_array),
        _ => None,
    };
    typed.unwrap_or_else(|| Value::String(text.to_string()))
}

/// The first instant of the local date. Where midnight is skipped by a
/// clock change, the day begins at the change itself, which may fall at
/// any minute of the gap; where midnight occurs twice, at the first.
fn day_start(tz: &TimeZone, date: Date) -> anyhow::Result<Timestamp> {
    let midnight = tz.to_ambiguous_timestamp(date.at(0, 0, 0, 0));
    let compatible = midnight.compatible()?;
    if !matches!(midnight.offset(), AmbiguousOffset::Gap { .. }) {
        return Ok(compatible);
    }
    let just_after = compatible.checked_add(SignedDuration::from_nanos(1))?;
    tz.preceding(just_after)
        .next()
        .map(|transition| transition.timestamp())
        .ok_or_else(|| anyhow!("{date}: midnight falls in a clock change with no transition"))
}

fn emit(rows: &[Value], summary: &str) -> anyhow::Result<()> {
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    for row in rows {
        serde_json::to_writer(&mut out, row)?;
        out.write_all(b"\n")?;
    }
    out.flush()?;
    eprintln!("{summary}");
    Ok(())
}

/// A zone name is a relative path inside the zoneinfo directory and
/// nothing else: no absolute path, no `..`, no other characters than the
/// database's own names use.
fn check_zone_name(zone: &str) -> anyhow::Result<&str> {
    let plain = !zone.is_empty()
        && zone
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '+'));
    let inside = Path::new(zone)
        .components()
        .all(|c| matches!(c, Component::Normal(_)));
    if !plain || !inside {
        bail!("`{zone}` is not a zone name such as Europe/London");
    }
    Ok(zone)
}

fn zoneinfo_dir(flag: Option<&Path>) -> PathBuf {
    flag.map(Path::to_path_buf)
        .or_else(|| std::env::var_os("TZDIR").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/usr/share/zoneinfo"))
}

/// The release the directory declares in its `tzdata.zi` header. The
/// version is stamped on every claim, so a directory that does not state
/// it stops the run. It names the directory's declared release, which
/// is where the zone file was read from; it is not a proof that the
/// file belongs to that release.
fn tzdb_version(dir: &Path) -> anyhow::Result<String> {
    let path = dir.join("tzdata.zi");
    let text = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "cannot determine the tzdb version: {} is not readable; the version is \
             stamped on every claim, so nothing was generated",
            path.display()
        )
    })?;
    let version = text
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("# version "))
        .map(str::trim)
        .filter(|v| !v.is_empty() && !v.contains(char::is_whitespace));
    version.map(str::to_string).ok_or_else(|| {
        anyhow!(
            "cannot determine the tzdb version: {} does not start with `# version <release>`; \
             nothing was generated",
            path.display()
        )
    })
}

/// A holiday list as one authority publishes it: the version that names
/// the publication, the dates it covers, and the holidays inside them.
/// A weekday outside the covered dates is not known to be a working day,
/// so the coverage is the authority's statement, required and checked.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HolidayList {
    authority: String,
    version: String,
    from: Date,
    until: Date,
    holidays: Vec<Date>,
}

impl HolidayList {
    fn read(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read the holiday list {}", path.display()))?;
        let list: Self = serde_json::from_str(&text).with_context(|| {
            format!(
                "{} is not a holiday list: an object with `authority`, `version`, the \
                 covered `from` and `until`, and `holidays` (dates as YYYY-MM-DD)",
                path.display()
            )
        })?;
        if list.authority.is_empty() || list.version.is_empty() {
            bail!(
                "{}: `authority` and `version` must both be given",
                path.display()
            );
        }
        if list.from > list.until {
            bail!(
                "{}: covers {} to {}, which is empty",
                path.display(),
                list.from,
                list.until
            );
        }
        let mut seen = std::collections::BTreeSet::new();
        for day in &list.holidays {
            if !seen.insert(*day) {
                bail!("{}: {day} is listed twice", path.display());
            }
            if *day < list.from || list.until < *day {
                bail!(
                    "{}: {day} is outside the dates the list covers ({} to {})",
                    path.display(),
                    list.from,
                    list.until
                );
            }
        }
        Ok(list)
    }
}
