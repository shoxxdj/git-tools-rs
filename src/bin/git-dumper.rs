//! git-dumper — Rust reimplementation of GitTools' gitdumper.sh.
//!
//! Downloads as much as possible of an exposed `.git` directory over HTTP(S)
//! when directory listing is disabled. It only *retrieves* the repository;
//! run `git-extractor-rs` (or `git checkout .`) on the result to get files.
//!
//! Discovery works like the original script: start from well-known paths,
//! then parse every downloaded file for object hashes, refs and pack names,
//! and queue whatever they point to. Improvements over the shell version:
//! - objects are parsed natively (zlib + tree/commit/tag/index parsers),
//!   no `git` binary required;
//! - every loose object is verified against its SHA-1, so soft-404 pages
//!   returned with a 200 status are rejected instead of polluting the dump;
//! - pack files are checked (magic + trailing checksum);
//! - refs found in packed-refs, HEAD, config and logs are followed, along
//!   with their reflogs;
//! - downloads run in parallel, and an existing dump can be resumed.

use anyhow::{bail, Context, Result};
use clap::Parser;
use colored::Colorize;
use flate2::read::ZlibDecoder;
use rayon::prelude::*;
use regex::bytes::Regex;
use reqwest::{
    blocking::Client,
    header::{HeaderMap, HeaderName, HeaderValue},
    redirect::Policy,
    StatusCode,
};
use sha1::{Digest, Sha1};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        LazyLock,
    },
    time::Duration,
};

const DEFAULT_UA: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0";

/// Paths queued before anything is known about the repository.
/// Same seed list as gitdumper.sh, plus a few files that frequently leak
/// extra hashes (ORIG_HEAD, FETCH_HEAD, reflog of the stash...).
const SEED_PATHS: &[&str] = &[
    "HEAD",
    "ORIG_HEAD",
    "FETCH_HEAD",
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "objects/info/packs",
    "description",
    "config",
    "COMMIT_EDITMSG",
    "index",
    "packed-refs",
    "shallow",
    "refs/heads/master",
    "refs/heads/main",
    "refs/remotes/origin/HEAD",
    "refs/remotes/origin/master",
    "refs/remotes/origin/main",
    "refs/stash",
    "logs/HEAD",
    "logs/refs/heads/master",
    "logs/refs/heads/main",
    "logs/refs/remotes/origin/HEAD",
    "logs/refs/stash",
    "info/refs",
    "info/exclude",
    "refs/wip/index/refs/heads/master",
    "refs/wip/index/refs/heads/main",
    "refs/wip/wtree/refs/heads/master",
    "refs/wip/wtree/refs/heads/main",
];

#[derive(Parser, Debug)]
#[command(
    name = "git-dumper",
    version,
    about = "Download an exposed .git directory over HTTP(S) (no extraction)",
    after_help = "Example:\n  git-dumper https://target.tld/.git/ ./dump\n  git-extractor-rs ./dump ./xtracted"
)]
struct Args {
    /// URL of the exposed git directory, e.g. https://target.tld/.git/
    url: String,

    /// Destination directory; the repository is written to <OUTPUT>/<GIT_DIR>
    output: PathBuf,

    /// Name of the git directory created inside OUTPUT (and expected at the
    /// end of the URL)
    #[arg(long, default_value = ".git")]
    git_dir: String,

    /// Number of parallel downloads
    #[arg(short = 'j', long, default_value_t = 8)]
    threads: usize,

    /// Extra request header, repeatable ("Name: value")
    #[arg(short = 'H', long = "header", value_name = "HEADER")]
    headers: Vec<String>,

    /// User-Agent (defaults to a browser one, like gitdumper.sh)
    #[arg(short = 'A', long)]
    user_agent: Option<String>,

    /// Skip TLS certificate verification (gitdumper.sh always does this)
    #[arg(short = 'k', long)]
    insecure: bool,

    /// HTTP(S)/SOCKS proxy, e.g. http://127.0.0.1:8080 for Burp
    #[arg(short = 'x', long)]
    proxy: Option<String>,

    /// Follow HTTP redirects (off by default: redirects are usually error pages)
    #[arg(long)]
    follow_redirects: bool,

    /// Per-request timeout in seconds
    #[arg(long, default_value_t = 20)]
    timeout: u64,

    /// Do not accept a URL that does not end with /<GIT_DIR>/
    #[arg(long)]
    strict_url: bool,

    /// Also print cached, missing and rejected (soft-404) files
    #[arg(short, long)]
    verbose: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Status {
    Downloaded,
    Cached,
    Missing,
    Rejected,
    Error,
}

struct Outcome {
    path: String,
    status: Status,
    detail: Option<String>,
    discovered: Vec<String>,
}

struct Ctx {
    client: Client,
    base_url: String,
    git_root: PathBuf,
    verbose: bool,
    done: AtomicUsize,
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

fn object_path(hash: &str) -> String {
    format!("objects/{}/{}", &hash[..2], &hash[2..])
}

/// Hash of a loose object path ("objects/ab/cdef..."), if it is one.
fn loose_object_hash(path: &str) -> Option<String> {
    let rest = path.strip_prefix("objects/")?;
    let (dir, file) = rest.split_once('/')?;
    let hash = format!("{dir}{file}");
    (dir.len() == 2 && hash.len() == 40 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
        .then_some(hash)
}

/// A ref name taken from server-controlled content must stay inside the git
/// directory and follow git's own rules closely enough to be harmless.
fn valid_ref(r: &str) -> bool {
    r.starts_with("refs/")
        && r.len() < 512
        && !r.ends_with('/')
        && !r.ends_with(".lock")
        && !r.contains("//")
        && !r.contains("..")
        && r
            .split('/')
            .all(|c| !c.is_empty() && !c.starts_with('.'))
}

/// Defense in depth: every path written to disk must be relative and free of
/// `..`, whatever produced it.
fn safe_join(root: &Path, rel: &str) -> Option<PathBuf> {
    let p = Path::new(rel);
    p.components()
        .all(|c| matches!(c, Component::Normal(_)))
        .then(|| root.join(p))
}

// ---------------------------------------------------------------------------
// Discovery: what does a downloaded file point to?
// ---------------------------------------------------------------------------

static HASH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?-u)\b[0-9a-f]{40}\b").unwrap());
static REF_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?-u)refs/[A-Za-z0-9._/+@-]+").unwrap());
static PACK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?-u)pack-[0-9a-f]{40}").unwrap());
static BRANCH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?-u)\[branch "([^"\r\n]+)"\]"#).unwrap());
static REMOTE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?-u)\[remote "([^"\r\n]+)"\]"#).unwrap());

fn push_ref(out: &mut Vec<String>, r: &str) {
    if valid_ref(r) {
        out.push(r.to_owned());
        out.push(format!("logs/{r}"));
    }
}

/// Text files (HEAD, packed-refs, logs, config, info/packs, FETCH_HEAD...):
/// scan for hashes, ref names and pack names, as gitdumper.sh does with
/// `strings | grep`.
fn discover_in_text(data: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    for m in HASH_RE.find_iter(data) {
        out.push(object_path(std::str::from_utf8(m.as_bytes()).unwrap()));
    }
    for m in REF_RE.find_iter(data) {
        let r = String::from_utf8_lossy(m.as_bytes());
        // Strip trailing punctuation picked up from reflog messages.
        let r = r.trim_end_matches(['.', ',', ':', ')']);
        push_ref(&mut out, r);
    }
    for m in PACK_RE.find_iter(data) {
        let name = std::str::from_utf8(m.as_bytes()).unwrap();
        out.push(format!("objects/pack/{name}.pack"));
        out.push(format!("objects/pack/{name}.idx"));
    }
    for c in BRANCH_RE.captures_iter(data) {
        let b = String::from_utf8_lossy(&c[1]);
        push_ref(&mut out, &format!("refs/heads/{b}"));
    }
    for c in REMOTE_RE.captures_iter(data) {
        let r = String::from_utf8_lossy(&c[1]);
        push_ref(&mut out, &format!("refs/remotes/{r}/HEAD"));
        push_ref(&mut out, &format!("refs/remotes/{r}/master"));
        push_ref(&mut out, &format!("refs/remotes/{r}/main"));
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Inflate a loose object and check it hashes to the expected id.
/// Returns (type, body).
fn decode_loose_object(raw: &[u8], expected: &str) -> Result<(String, Vec<u8>)> {
    let mut data = Vec::new();
    ZlibDecoder::new(raw)
        .read_to_end(&mut data)
        .context("not zlib data")?;
    let actual = hex(&Sha1::digest(&data));
    if actual != expected {
        bail!("SHA-1 mismatch (got {actual})");
    }
    let nul = data
        .iter()
        .position(|&b| b == 0)
        .context("missing object header")?;
    let header = std::str::from_utf8(&data[..nul]).context("invalid object header")?;
    let (kind, size) = header.split_once(' ').context("invalid object header")?;
    let size: usize = size.parse().context("invalid object size")?;
    let body = data[nul + 1..].to_vec();
    if body.len() != size {
        bail!("size mismatch");
    }
    Ok((kind.to_owned(), body))
}

/// Hashes referenced by a commit/tag/tree object.
fn discover_in_object(kind: &str, body: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    match kind {
        "commit" | "tag" => {
            // Only headers carry object links; the message is free text.
            for line in body.split(|&b| b == b'\n') {
                if line.is_empty() {
                    break;
                }
                for key in [&b"tree "[..], b"parent ", b"object "] {
                    if let Some(h) = line.strip_prefix(key) {
                        if h.len() == 40 && h.iter().all(u8::is_ascii_hexdigit) {
                            out.push(object_path(std::str::from_utf8(h).unwrap()));
                        }
                    }
                }
            }
        }
        "tree" => {
            // Entries: "<octal mode> <name>\0<20-byte id>"
            let mut i = 0;
            while i < body.len() {
                let Some(sp) = body[i..].iter().position(|&b| b == b' ') else { break };
                let mode = &body[i..i + sp];
                let Some(nul) = body[i + sp..].iter().position(|&b| b == 0) else { break };
                let id_start = i + sp + nul + 1;
                if id_start + 20 > body.len() {
                    break;
                }
                // 160000 = gitlink (submodule commit): lives in another repo.
                if mode != b"160000" {
                    out.push(object_path(&hex(&body[id_start..id_start + 20])));
                }
                i = id_start + 20;
            }
        }
        _ => {} // blobs reference nothing
    }
    out
}

/// Blob ids listed in the staging area (`index`, formats v2 to v4).
/// The index often references blobs that were never committed.
fn discover_in_index(data: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    if data.len() < 12 || &data[..4] != b"DIRC" {
        return out;
    }
    let be32 = |p: usize| u32::from_be_bytes(data[p..p + 4].try_into().unwrap());
    let version = be32(4);
    let count = be32(8) as usize;
    let mut pos = 12;
    for _ in 0..count {
        if pos + 62 > data.len() {
            break;
        }
        let mode = be32(pos + 24);
        let id = &data[pos + 40..pos + 60];
        let flags = u16::from_be_bytes([data[pos + 60], data[pos + 61]]);
        let mut name = pos + 62;
        if version >= 3 && flags & 0x4000 != 0 {
            name += 2; // extended flags
        }
        if version >= 4 {
            // Prefix-compressed path: varint, then NUL-terminated suffix, no padding.
            while name < data.len() && data[name] & 0x80 != 0 {
                name += 1;
            }
            name += 1;
            let Some(nul) = data.get(name..).and_then(|s| s.iter().position(|&b| b == 0)) else { break };
            pos = name + nul + 1;
        } else {
            // NUL-terminated path padded with 1..8 NULs to a multiple of 8.
            let Some(nul) = data.get(name..).and_then(|s| s.iter().position(|&b| b == 0)) else { break };
            let len = name + nul - pos;
            pos += (len + 8) & !7;
        }
        if mode >> 12 != 0o16 {
            out.push(object_path(&hex(id)));
        }
    }
    out
}

/// Soft-404 detector for text-ish files, like gitdumper's "HTML document" test.
fn looks_like_html(data: &[u8]) -> bool {
    let head = &data[..data.len().min(512)];
    let lower = String::from_utf8_lossy(head).to_ascii_lowercase();
    let trimmed = lower.trim_start();
    trimmed.starts_with("<!doctype") || trimmed.starts_with("<html") || lower.contains("<body")
}

/// Validate a downloaded file and return what it references.
/// Err means the content is not what the path should contain.
fn validate_and_discover(path: &str, data: &[u8]) -> Result<Vec<String>> {
    if let Some(hash) = loose_object_hash(path) {
        let (kind, body) = decode_loose_object(data, &hash)?;
        return Ok(discover_in_object(&kind, &body));
    }
    if path.ends_with(".pack") {
        if data.len() < 32 || &data[..4] != b"PACK" {
            bail!("not a pack file");
        }
        let (content, trailer) = data.split_at(data.len() - 20);
        if Sha1::digest(content).as_slice() != trailer {
            bail!("pack checksum mismatch (truncated download?)");
        }
        return Ok(Vec::new());
    }
    if path.ends_with(".idx") {
        if data.len() < 8 || (data[..4] != [0xff, b't', b'O', b'c'] && data.len() < 1024 + 40) {
            bail!("not a pack index");
        }
        return Ok(Vec::new());
    }
    if path == "index" {
        if data.len() < 12 || &data[..4] != b"DIRC" {
            bail!("not a git index");
        }
        return Ok(discover_in_index(data));
    }
    if looks_like_html(data) {
        bail!("HTML document (soft 404?)");
    }
    Ok(discover_in_text(data))
}

// ---------------------------------------------------------------------------
// Download
// ---------------------------------------------------------------------------

fn process(ctx: &Ctx, path: &str) -> Outcome {
    let mk = |status, detail: Option<String>, discovered| Outcome {
        path: path.to_owned(),
        status,
        detail,
        discovered,
    };
    let Some(target) = safe_join(&ctx.git_root, path) else {
        return mk(Status::Rejected, Some("unsafe path".into()), Vec::new());
    };

    // Resume: reuse a previously downloaded file if it still validates.
    if let Ok(data) = fs::read(&target) {
        if let Ok(found) = validate_and_discover(path, &data) {
            return mk(Status::Cached, None, found);
        }
    }

    let url = format!("{}{}", ctx.base_url, path);
    let response = match ctx.client.get(&url).send() {
        Ok(r) => r,
        Err(e) => return mk(Status::Error, Some(e.to_string()), Vec::new()),
    };
    let status = response.status();
    if status != StatusCode::OK {
        let s = if status == StatusCode::NOT_FOUND || status.is_redirection() || status == StatusCode::FORBIDDEN {
            Status::Missing
        } else {
            Status::Error
        };
        return mk(s, Some(format!("HTTP {}", status.as_u16())), Vec::new());
    }
    let data = match response.bytes() {
        Ok(b) => b,
        Err(e) => return mk(Status::Error, Some(e.to_string()), Vec::new()),
    };
    let found = match validate_and_discover(path, &data) {
        Ok(f) => f,
        Err(e) => return mk(Status::Rejected, Some(e.to_string()), Vec::new()),
    };
    let write = || -> Result<()> {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&target, &data)?;
        Ok(())
    };
    match write() {
        Ok(()) => mk(Status::Downloaded, None, found),
        Err(e) => mk(Status::Error, Some(format!("write failed: {e}")), Vec::new()),
    }
}

fn report(ctx: &Ctx, o: &Outcome) {
    ctx.done.fetch_add(1, Ordering::Relaxed);
    let detail = o.detail.as_deref().unwrap_or("");
    match o.status {
        Status::Downloaded => println!("{} {}", "[+]".green().bold(), o.path),
        Status::Cached if ctx.verbose => println!("{} {} (cached)", "[=]".cyan(), o.path),
        Status::Missing if ctx.verbose => println!("{} {} {}", "[-]".dimmed(), o.path, detail.dimmed()),
        Status::Rejected if ctx.verbose => println!("{} {} {}", "[!]".yellow(), o.path, detail.yellow()),
        Status::Error => eprintln!("{} {} {}", "[x]".red().bold(), o.path, detail.red()),
        _ => {}
    }
}

fn build_client(args: &Args) -> Result<Client> {
    let mut headers = HeaderMap::new();
    for h in &args.headers {
        let (name, value) = h
            .split_once(':')
            .with_context(|| format!("invalid header {h:?}, expected \"Name: value\""))?;
        headers.insert(
            HeaderName::from_bytes(name.trim().as_bytes())?,
            HeaderValue::from_str(value.trim())?,
        );
    }
    let mut builder = Client::builder()
        .timeout(Duration::from_secs(args.timeout))
        .user_agent(args.user_agent.as_deref().unwrap_or(DEFAULT_UA))
        .default_headers(headers)
        .danger_accept_invalid_certs(args.insecure)
        .redirect(if args.follow_redirects {
            Policy::limited(5)
        } else {
            Policy::none()
        })
        .pool_max_idle_per_host(args.threads);
    if let Some(p) = &args.proxy {
        builder = builder.proxy(reqwest::Proxy::all(p).context("invalid proxy URL")?);
    }
    Ok(builder.build()?)
}

fn main() -> Result<()> {
    let args = Args::parse();
    if args.threads == 0 {
        bail!("--threads must be greater than zero");
    }
    let git_dir = args.git_dir.trim_matches('/');
    if git_dir.is_empty() || safe_join(Path::new(""), git_dir).is_none() {
        bail!("invalid --git-dir {:?}", args.git_dir);
    }

    let mut base_url = args.url.clone();
    if !base_url.ends_with('/') {
        base_url.push('/');
    }
    if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
        bail!("URL must start with http:// or https://");
    }
    if !base_url.ends_with(&format!("/{git_dir}/")) {
        let msg = format!("URL does not end with /{git_dir}/ — is it really the git directory?");
        if args.strict_url {
            bail!(msg);
        }
        eprintln!("{} {}", "[!]".yellow(), msg);
    }

    let git_root = args.output.join(git_dir);
    fs::create_dir_all(&git_root)
        .with_context(|| format!("cannot create {}", git_root.display()))?;

    let ctx = Ctx {
        client: build_client(&args)?,
        base_url,
        git_root: git_root.clone(),
        verbose: args.verbose,
        done: AtomicUsize::new(0),
    };
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(args.threads)
        .build()?;

    println!("{} dumping {} into {}", "[*]".cyan().bold(), ctx.base_url, git_root.display());

    let mut seen: HashSet<String> = HashSet::new();
    let mut frontier: Vec<String> = SEED_PATHS.iter().map(|s| s.to_string()).collect();
    seen.extend(frontier.iter().cloned());
    let mut stats: BTreeMap<Status, usize> = BTreeMap::new();
    let mut objects_ok = 0usize;
    let mut packs_ok = 0usize;
    let mut head_ok = false;

    // Breadth-first: each round downloads everything discovered in the
    // previous one, in parallel.
    while !frontier.is_empty() {
        let outcomes: Vec<Outcome> = pool.install(|| {
            frontier
                .par_iter()
                .map(|p| {
                    let o = process(&ctx, p);
                    report(&ctx, &o);
                    o
                })
                .collect()
        });
        let mut next = Vec::new();
        for o in outcomes {
            *stats.entry(o.status).or_default() += 1;
            if matches!(o.status, Status::Downloaded | Status::Cached) {
                if loose_object_hash(&o.path).is_some() {
                    objects_ok += 1;
                } else if o.path.ends_with(".pack") {
                    packs_ok += 1;
                } else if o.path == "HEAD" {
                    head_ok = true;
                }
            }
            for d in o.discovered {
                if seen.insert(d.clone()) {
                    next.push(d);
                }
            }
        }
        next.sort();
        frontier = next;
    }

    let get = |s| stats.get(&s).copied().unwrap_or(0);
    println!("{}", "[+] Dump finished".green().bold());
    println!("    downloaded {}", get(Status::Downloaded));
    println!("    cached     {}", get(Status::Cached));
    println!("    missing    {}", get(Status::Missing));
    println!("    rejected   {}", get(Status::Rejected));
    println!("    errors     {}", get(Status::Error));
    println!("    loose objects {objects_ok}, packs {packs_ok}");
    println!("    output     {}", git_root.display());

    if !head_ok {
        eprintln!(
            "{} HEAD was not retrieved: the URL is probably wrong or not exposed",
            "[!]".yellow().bold()
        );
    }
    if get(Status::Error) > 0 {
        eprintln!(
            "{} some requests failed; run the same command again to resume",
            "[!]".yellow()
        );
    }
    println!(
        "{} next step: git-extractor-rs {} <output>",
        "[i]".cyan(),
        args.output.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs_are_sanitized() {
        assert!(valid_ref("refs/heads/feature/x"));
        assert!(!valid_ref("refs/heads/../../../etc/passwd"));
        assert!(!valid_ref("refs/heads/.hidden"));
        assert!(!valid_ref("refs/heads/x.lock"));
        assert!(!valid_ref("refs//heads"));
        assert!(safe_join(Path::new("/x"), "../y").is_none());
        assert!(safe_join(Path::new("/x"), "/etc/passwd").is_none());
    }

    #[test]
    fn text_discovery() {
        let d = discover_in_text(
            b"0123456789abcdef0123456789abcdef01234567 refs/heads/dev\nP pack-89abcdef0123456789abcdef0123456789abcdef.pack\n[branch \"feat\"]\n",
        );
        assert!(d.contains(&"objects/01/23456789abcdef0123456789abcdef01234567".to_string()));
        assert!(d.contains(&"refs/heads/dev".to_string()));
        assert!(d.contains(&"logs/refs/heads/dev".to_string()));
        assert!(d.contains(&"refs/heads/feat".to_string()));
        assert!(d.contains(&"objects/pack/pack-89abcdef0123456789abcdef0123456789abcdef.idx".to_string()));
    }

    #[test]
    fn html_is_rejected() {
        assert!(validate_and_discover("HEAD", b"<!DOCTYPE html><html>").is_err());
        assert!(validate_and_discover(
            "objects/01/23456789abcdef0123456789abcdef01234567",
            b"<html>not found</html>"
        )
        .is_err());
    }
}
