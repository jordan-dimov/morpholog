//! `morpholog serve` - the resident runtime behind a Unix socket, for
//! callers that must not hold the database credential or choose the
//! programme.
//!
//! The service holds one database connection per presenter, each logged
//! in as that presenter's own role, and picks it by the uid the kernel
//! reports for the socket's peer. So the actor policy and the audit row
//! see that role, exactly as if the presenter had connected itself: but
//! here PostgreSQL authenticated the service, and the service mapped the
//! peer to the role. A peer whose uid is not mapped is closed before
//! anything is read or written.
//!
//! Each connection speaks the session protocol: a ready line, then one
//! request in and one response out, in order. A presenter may propose,
//! transact and ping; anything else needs a grant. A failure that cannot
//! be a receipt closes that connection only. The caller of a request in
//! flight then cannot know whether it committed, as when a session
//! aborts.

use anyhow::{Context, anyhow, bail};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::ServeArgs;
use crate::commands::resident::{Gated, Permits, Programme, Resident, ResidentFailure};
use morpholog_cli::envelopes::ErrorReceipt;

/// The longest request line. A longer one closes the connection: a
/// half-read line cannot be resynchronised.
const MAX_REQUEST_LINE: usize = 1024 * 1024;

/// Open connections per presenter. Past it, a new connection is closed at
/// once, so one presenter can exhaust only its own share.
const MAX_CONNECTIONS_PER_PRESENTER: usize = 4;

struct Presenter {
    resident: Resident,
    permits: Permits,
    open: AtomicUsize,
}

pub(crate) async fn run(args: ServeArgs) -> anyhow::Result<()> {
    let logins = parse_presenters(&args.presenters)?;
    let mut permits = parse_grants(&args.grants, &logins)?;
    let programme = Arc::new(Programme::load(&args.file, Some(&args.expect_hash))?);

    let mut presenters = HashMap::new();
    for (uid, login) in logins {
        let url = morpholog_postgres::with_user(&args.db.database_url, &login);
        let pool = crate::commands::connect_single(&url)
            .await
            .with_context(|| format!("connecting as `{login}` for uid {uid}"))?;
        let actual = morpholog_postgres::session_login(&pool).await?;
        if actual != login {
            bail!(
                "uid {uid} is mapped to `{login}`, but the connection logged in as `{actual}`; \
                 remove the user from the database URL, since serve sets each presenter's own"
            );
        }
        let presenter = Presenter {
            resident: Resident::new(Arc::clone(&programme), pool),
            permits: permits.remove(&uid).unwrap_or_default(),
            open: AtomicUsize::new(0),
        };
        presenters.insert(uid, presenter);
    }
    let presenters = Arc::new(presenters);

    let listener = UnixListener::bind(&args.socket).with_context(|| {
        format!(
            "binding `{}`; if a stopped service left it behind, remove it",
            args.socket.display()
        )
    })?;
    eprintln!(
        "serving {} on {}, presenters: {}",
        programme
            .ready()
            .get("model_hash")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default(),
        args.socket.display(),
        presenters.len()
    );

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => admit(stream, &presenters),
                Err(e) => eprintln!("accepting a connection: {e}"),
            },
            signal = &mut shutdown => {
                signal?;
                break;
            }
        }
    }
    remove_socket(&args.socket);
    Ok(())
}

/// Look the peer up and hand the connection to its presenter, or close it.
fn admit(stream: UnixStream, presenters: &Arc<HashMap<u32, Presenter>>) {
    let uid = match stream.peer_cred() {
        Ok(credentials) => credentials.uid(),
        Err(e) => {
            eprintln!("refused a connection: its peer could not be read: {e}");
            return;
        }
    };
    let Some((&key, presenter)) = presenters.get_key_value(&uid) else {
        eprintln!("refused uid {uid}: not a presenter");
        return;
    };
    if presenter.open.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS_PER_PRESENTER {
        presenter.open.fetch_sub(1, Ordering::SeqCst);
        eprintln!("refused uid {uid}: {MAX_CONNECTIONS_PER_PRESENTER} connections already open");
        return;
    }
    let presenters = Arc::clone(presenters);
    tokio::spawn(async move {
        let presenter = &presenters[&key];
        let outcome = converse(stream, presenter).await;
        presenter.open.fetch_sub(1, Ordering::SeqCst);
        if let Err(e) = outcome {
            eprintln!("uid {uid}: connection closed: {e:#}");
        }
    });
}

/// The session protocol over one connection.
async fn converse(stream: UnixStream, presenter: &Presenter) -> anyhow::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    write_line(&mut write, presenter.resident.ready()).await?;
    let mut line = String::new();
    let mut row: u64 = 0;
    loop {
        line.clear();
        if read_line_capped(&mut read, &mut line).await? == 0 {
            return Ok(());
        }
        row += 1;
        if line.trim().is_empty() {
            continue;
        }
        match presenter
            .resident
            .handle(line.trim(), row, &presenter.permits)
            .await
        {
            Ok(response) => write_line(&mut write, &response).await?,
            Err(ResidentFailure::Request { code, reason }) => {
                let receipt = ErrorReceipt::new(code, format!("{reason:#}"), row);
                write_line(&mut write, &serde_json::to_value(&receipt)?).await?;
            }
            Err(ResidentFailure::Operational(reason)) => {
                return Err(reason.context(format!("operational failure at request {row}")));
            }
        }
    }
}

/// `--presenter UID=LOGIN`, each uid and each login at most once: two
/// presenters on one login could not be told apart by the rules.
fn parse_presenters(specs: &[String]) -> anyhow::Result<Vec<(u32, String)>> {
    let mut presenters: Vec<(u32, String)> = Vec::new();
    for spec in specs {
        let (uid, login) = split_uid(spec, "--presenter", "UID=LOGIN")?;
        if login.is_empty() {
            bail!("`--presenter {spec}` names no login");
        }
        if presenters.iter().any(|(u, _)| *u == uid) {
            bail!("uid {uid} is mapped twice; a uid is one presenter");
        }
        if presenters.iter().any(|(_, l)| l == login) {
            bail!("`{login}` is mapped to two uids; the rules could not tell them apart");
        }
        presenters.push((uid, login.to_string()));
    }
    Ok(presenters)
}

/// `--grant UID=OP[,OP...]`, for a uid that is a presenter.
fn parse_grants(
    specs: &[String],
    presenters: &[(u32, String)],
) -> anyhow::Result<HashMap<u32, Permits>> {
    let mut grants: HashMap<u32, Permits> = HashMap::new();
    for spec in specs {
        let (uid, ops) = split_uid(spec, "--grant", "UID=OP[,OP...]")?;
        if !presenters.iter().any(|(u, _)| *u == uid) {
            bail!("`--grant {spec}` names uid {uid}, which is not a presenter");
        }
        let permits = grants.entry(uid).or_default();
        for op in ops.split(',') {
            let gated = Gated::parse(op).ok_or_else(|| {
                anyhow!(
                    "`{op}` cannot be granted; the grantable operations are explain, claims, \
                     derived, audit, rejections"
                )
            })?;
            permits.grant(gated);
        }
    }
    Ok(grants)
}

fn split_uid<'a>(spec: &'a str, flag: &str, shape: &str) -> anyhow::Result<(u32, &'a str)> {
    let (uid, rest) = spec
        .split_once('=')
        .ok_or_else(|| anyhow!("`{flag} {spec}` is not {shape}"))?;
    let uid = uid
        .parse()
        .with_context(|| format!("`{flag} {spec}`: `{uid}` is not a numeric uid"))?;
    Ok((uid, rest))
}

async fn shutdown_signal() -> anyhow::Result<()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("listening for SIGTERM")?;
    tokio::select! {
        interrupted = tokio::signal::ctrl_c() => interrupted.context("listening for SIGINT")?,
        _ = terminate.recv() => {}
    }
    Ok(())
}

fn remove_socket(path: &Path) {
    if let Err(e) = std::fs::remove_file(path) {
        eprintln!("removing `{}`: {e}", path.display());
    }
}

async fn write_line(
    out: &mut (impl AsyncWrite + Unpin),
    value: &serde_json::Value,
) -> anyhow::Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    out.write_all(&bytes)
        .await
        .context("writing a response line")?;
    out.flush().await.context("flushing a response line")?;
    Ok(())
}

/// One line, at most [`MAX_REQUEST_LINE`] bytes, decoded once whole.
/// Returns the bytes read; 0 is end of stream.
async fn read_line_capped(
    input: &mut (impl AsyncBufRead + Unpin),
    line: &mut String,
) -> anyhow::Result<usize> {
    let mut bytes = Vec::new();
    loop {
        let chunk = input.fill_buf().await.context("reading a request line")?;
        if chunk.is_empty() {
            break;
        }
        let (take, done) = match chunk.iter().position(|&b| b == b'\n') {
            Some(pos) => (pos + 1, true),
            None => (chunk.len(), false),
        };
        if bytes.len() + take > MAX_REQUEST_LINE {
            bail!("a request line exceeded {MAX_REQUEST_LINE} bytes");
        }
        bytes.extend_from_slice(&chunk[..take]);
        input.consume(take);
        if done {
            break;
        }
    }
    line.push_str(std::str::from_utf8(&bytes).context("a request line is not valid UTF-8")?);
    Ok(bytes.len())
}
