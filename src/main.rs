use anyhow::{bail, Context, Result};
use clap::{Parser, ValueEnum};
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use serde::Serialize;
use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
};

#[derive(Parser, Debug)]
#[command(name = "git-extractor-rs", version, about = "Extract all objects from a Git repository")]
struct Args {
    /// Repository working tree, .git directory, or bare repository
    repository: PathBuf,

    /// Destination directory (positional form, as in GitTools' extractor.sh)
    #[arg(value_name = "OUTPUT", conflicts_with = "output")]
    output_pos: Option<PathBuf>,

    /// Destination directory [default: ./xtracted]
    #[arg(short, long, value_name = "DIR")]
    output: Option<PathBuf>,

    /// Which commit date prefixes snapshot directory names
    #[arg(long, value_enum, default_value_t = SnapshotDate::Committer)]
    snapshot_date: SnapshotDate,

    /// Number of parallel Git batch workers
    #[arg(short = 'j', long, default_value_t = 4)]
    threads: usize,

    /// Also write a human-readable summary
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    report: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum SnapshotDate {
    /// Date the commit was (re)written: changes on rebase/amend/cherry-pick
    Committer,
    /// Date the change was originally authored
    Author,
}

#[derive(Serialize)]
struct SnapshotMeta {
    commit: String,
    directory: String,
    /// YYYYMMDD-HHMM in the commit's own timezone, or null if unparsable
    date: Option<String>,
    timestamp: Option<i64>,
    tz_offset: Option<String>,
    files: usize,
}

#[derive(Clone, Serialize)]
struct ObjectMeta {
    hash: String,
    object_type: String,
    size: u64,
    path: String,
}

fn git(args: &Args, git_args: &[&str]) -> Result<std::process::Output> {
    Command::new("git")
        .arg("-C")
        .arg(&args.repository)
        .args(git_args)
        .output()
        .with_context(|| format!("failed to execute git {:?}", git_args))
}

fn list_objects(args: &Args) -> Result<Vec<String>> {
    // --batch-all-objects includes objects in packfiles and unreachable objects.
    let output = git(args, &["cat-file", "--batch-all-objects", "--batch-check=%(objectname)"])?;
    if !output.status.success() {
        bail!(
            "git cat-file failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let mut hashes: Vec<String> = String::from_utf8(output.stdout)?
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    hashes.sort();
    hashes.dedup();
    Ok(hashes)
}

fn extract_chunk(repo: &Path, output: &Path, hashes: &[String]) -> Result<Vec<ObjectMeta>> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("cannot start git cat-file --batch")?;

    let mut stdin = child.stdin.take().context("missing child stdin")?;
    let stdout = child.stdout.take().context("missing child stdout")?;
    let mut reader = BufReader::new(stdout);
    let mut result = Vec::with_capacity(hashes.len());

    for hash in hashes {
        writeln!(stdin, "{hash}")?;
        stdin.flush()?;

        let mut header = String::new();
        let count = reader.read_line(&mut header)?;
        if count == 0 {
            bail!("git cat-file ended unexpectedly while reading object {hash}");
        }
        let fields: Vec<&str> = header.trim_end().split_whitespace().collect();
        if fields.len() != 3 {
            bail!("unexpected cat-file response for {hash}: {}", header.trim());
        }
        let returned_hash = fields[0].to_owned();
        let object_type = fields[1].to_owned();
        let size: u64 = fields[2].parse().context("invalid object size")?;

        let subdir = match object_type.as_str() {
            "commit" => "commits",
            "tree" => "trees",
            "blob" => "blobs",
            "tag" => "tags",
            other => {
                bail!("unknown Git object type: {other}");
            }
        };
        let dir = output.join(subdir);
        fs::create_dir_all(&dir)?;
        let path = dir.join(&returned_hash);

        let mut file = File::create(&path)
            .with_context(|| format!("cannot create {}", path.display()))?;
        let mut limited = (&mut reader).take(size);
        let copied = std::io::copy(&mut limited, &mut file)?;
        if copied != size {
            bail!("truncated object {returned_hash}: expected {size}, got {copied}");
        }
        file.flush()?;

        let mut newline = [0u8; 1];
        reader.read_exact(&mut newline)?;
        if newline[0] != b'\n' {
            bail!("invalid batch separator after object {returned_hash}");
        }

        result.push(ObjectMeta {
            hash: returned_hash,
            object_type,
            size,
            path: path.to_string_lossy().into_owned(),
        });
    }

    drop(stdin);
    let status = child.wait()?;
    if !status.success() {
        bail!("git cat-file --batch worker exited with {status}");
    }
    Ok(result)
}

/// Convert days since 1970-01-01 to a (year, month, day) civil date.
/// Howard Hinnant's algorithm, valid for the whole proleptic Gregorian range.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Parse "+HHMM" / "-HHMM" into seconds.
fn parse_tz_offset(tz: &str) -> Option<i64> {
    let (sign, digits) = match tz.as_bytes().first()? {
        b'+' => (1, &tz[1..]),
        b'-' => (-1, &tz[1..]),
        _ => return None,
    };
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let hours: i64 = digits[..2].parse().ok()?;
    let minutes: i64 = digits[2..].parse().ok()?;
    Some(sign * (hours * 3600 + minutes * 60))
}

/// Format a Unix timestamp as YYYYMMDD-HHMM, shifted to the given offset.
fn format_stamp(timestamp: i64, offset_secs: i64) -> String {
    let local = timestamp + offset_secs;
    let (y, m, d) = civil_from_days(local.div_euclid(86_400));
    let secs = local.rem_euclid(86_400);
    format!("{:04}{:02}{:02}-{:02}{:02}", y, m, d, secs / 3600, (secs % 3600) / 60)
}

/// Extract (timestamp, tz) from the `author` or `committer` header of a raw
/// commit object. The ident line is "<kind> Name <email> <timestamp> <tz>";
/// the name may contain spaces, so the two last fields are read from the end.
fn commit_date(raw: &[u8], which: SnapshotDate) -> Option<(i64, String)> {
    let key: &[u8] = match which {
        SnapshotDate::Committer => b"committer ",
        SnapshotDate::Author => b"author ",
    };
    for line in raw.split(|&b| b == b'\n') {
        if line.is_empty() {
            break; // end of headers, message follows
        }
        if let Some(rest) = line.strip_prefix(key) {
            let rest = String::from_utf8_lossy(rest);
            let after_email = &rest[rest.rfind('>')? + 1..];
            let mut fields = after_email.split_whitespace();
            let ts: i64 = fields.next()?.parse().ok()?;
            let tz = fields.next().unwrap_or("+0000").to_owned();
            return Some((ts, tz));
        }
    }
    None
}

fn export_commit_snapshots(
    args: &Args,
    output: &Path,
    objects: &[ObjectMeta],
) -> Result<Vec<SnapshotMeta>> {
    let commits: Vec<&ObjectMeta> = objects.iter()
        .filter(|o| o.object_type == "commit")
        .collect();

    if commits.is_empty() {
        return Ok(Vec::new());
    }

    let root = output.join("snapshots");
    fs::create_dir_all(&root)?;
    let pb = ProgressBar::new(commits.len() as u64);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} [snapshots] [{bar:40.cyan/blue}] {pos}/{len} commits ({eta})",
        )?.progress_chars("##-"),
    );

    let repo = Arc::new(args.repository.clone());
    let out = Arc::new(root);
    let which = args.snapshot_date;
    let exported: Result<Vec<SnapshotMeta>> = commits.par_iter().map(|commit| {
        // The raw commit object was already extracted under commits/<hash>.
        let raw = fs::read(&commit.path)
            .with_context(|| format!("cannot read commit object {}", commit.path))?;
        let parsed = commit_date(&raw, which)
            .and_then(|(ts, tz)| parse_tz_offset(&tz).map(|off| (ts, tz, off)));
        let date = parsed.as_ref().map(|(ts, _, off)| format_stamp(*ts, *off));

        // Unparsable dates get a sentinel so directories still sort first
        // and the name pattern stays uniform.
        let dir_name = format!(
            "{}_{}",
            date.as_deref().unwrap_or("00000000-0000"),
            commit.hash
        );
        let destination = out.join(&dir_name);
        fs::create_dir_all(&destination)?;

        // git archive resolves the commit's tree and emits its tracked snapshot.
        // This works for commits that are unreachable from current refs as long
        // as the commit object is present in the object database.
        let output = Command::new("git")
            .arg("-C")
            .arg(&*repo)
            .args(["archive", "--format=tar", &commit.hash])
            .output()
            .with_context(|| format!("failed to archive commit {}", commit.hash))?;

        if !output.status.success() {
            bail!(
                "git archive failed for {}: {}",
                commit.hash,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        let mut archive = tar::Archive::new(output.stdout.as_slice());
        let mut count = 0usize;

        for entry in archive.entries()? {
            let mut entry = entry?;
            let entry_type = entry.header().entry_type();

            // Only regular files are materialized. Symlinks, devices and other
            // special entries are deliberately skipped for safer extraction.
            if !entry_type.is_file() {
                continue;
            }

            let relative = entry.path()?.into_owned();
            if relative.is_absolute()
                || relative.components().any(|c| matches!(
                    c,
                    std::path::Component::ParentDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                ))
            {
                continue;
            }

            let target = destination.join(&relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }

            let mut file = File::create(&target)
                .with_context(|| format!("cannot create snapshot file {}", target.display()))?;
            std::io::copy(&mut entry, &mut file)?;
            count += 1;
        }

        pb.inc(1);
        Ok(SnapshotMeta {
            commit: commit.hash.clone(),
            directory: dir_name,
            date,
            timestamp: parsed.as_ref().map(|(ts, _, _)| *ts),
            tz_offset: parsed.map(|(_, tz, _)| tz),
            files: count,
        })
    }).collect();

    pb.finish_with_message("snapshots exported");
    let mut snapshots = exported?;
    snapshots.sort_by(|a, b| a.directory.cmp(&b.directory));
    Ok(snapshots)
}

fn main() -> Result<()> {
    let args = Args::parse();
    if args.threads == 0 {
        bail!("--threads must be greater than zero");
    }

    let check = git(&args, &["rev-parse", "--git-dir"])
        .context("is Git installed and is the repository path valid?")?;
    if !check.status.success() {
        bail!(
            "not a Git repository: {}",
            String::from_utf8_lossy(&check.stderr).trim()
        );
    }

    let output: PathBuf = args
        .output_pos
        .clone()
        .or_else(|| args.output.clone())
        .unwrap_or_else(|| PathBuf::from("./xtracted"));
    fs::create_dir_all(&output)
        .with_context(|| format!("cannot create output directory {}", output.display()))?;
    let hashes = list_objects(&args)?;
    if hashes.is_empty() {
        println!("{}", "No Git objects found.".yellow());
        return Ok(());
    }

    println!(
        "{} {} objects from {}",
        "[+]".green().bold(),
        hashes.len(),
        args.repository.display()
    );
    println!(
        "{} packed, loose, reachable and unreachable objects are included",
        "[i]".cyan()
    );

    let pb = ProgressBar::new(hashes.len() as u64);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} objects ({eta})",
        )?
        .progress_chars("##-"),
    );

    let workers = args.threads.min(hashes.len());
    let chunk_size = (hashes.len() + workers - 1) / workers;
    let chunks: Vec<Vec<String>> = hashes.chunks(chunk_size).map(|c| c.to_vec()).collect();
    let repo = Arc::new(args.repository.clone());
    let out = Arc::new(output.clone());

    let extracted: Result<Vec<Vec<ObjectMeta>>> = chunks
        .par_iter()
        .map(|chunk| {
            let metas = extract_chunk(&repo, &out, chunk)?;
            pb.inc(metas.len() as u64);
            Ok(metas)
        })
        .collect();

    pb.finish_with_message("extraction complete");
    let mut objects: Vec<ObjectMeta> = extracted?.into_iter().flatten().collect();
    objects.sort_by(|a, b| a.hash.cmp(&b.hash));

    let metadata_dir = output.join("metadata");
    fs::create_dir_all(&metadata_dir)?;
    let metadata_path = metadata_dir.join("objects.json");
    let metadata_file = File::create(&metadata_path)?;
    serde_json::to_writer_pretty(metadata_file, &objects)?;

    let snapshots = export_commit_snapshots(&args, &output, &objects)?;
    let snapshot_files: usize = snapshots.iter().map(|s| s.files).sum();
    let snapshots_index = metadata_dir.join("snapshots.json");
    serde_json::to_writer_pretty(File::create(&snapshots_index)?, &snapshots)?;
    println!(
        "{} {} tracked files exported across {} commit snapshots",
        "[+]".green().bold(),
        snapshot_files,
        snapshots.len()
    );

    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    for object in &objects {
        *counts.entry(object.object_type.clone()).or_default() += 1;
    }

    println!("{}", "[+] Extraction completed".green().bold());
    for (kind, count) in counts {
        println!("    {:<8} {}", kind, count);
    }
    println!("    output  {}", output.display());
    println!("    index   {}", metadata_path.display());
    println!("    commits {}", snapshots_index.display());

    if args.report {
        #[derive(Serialize)]
        struct Report {
            repository: String,
            total_objects: usize,
            counts: std::collections::BTreeMap<String, usize>,
            includes_unreachable_objects: bool,
        }
        let mut report_counts = std::collections::BTreeMap::<String, usize>::new();
        for object in &objects {
            *report_counts.entry(object.object_type.clone()).or_default() += 1;
        }
        let report = Report {
            repository: args.repository.to_string_lossy().into_owned(),
            total_objects: objects.len(),
            counts: report_counts,
            includes_unreachable_objects: true,
        };
        let report_file = File::create(output.join("report.json"))?;
        serde_json::to_writer_pretty(report_file, &report)?;
    }

    Ok(())
}
