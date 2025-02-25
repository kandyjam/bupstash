#![cfg_attr(feature = "simd-rollsum", feature(portable_simd))]

pub mod abloom;
pub mod acache;
pub mod address;
pub mod base64;
pub mod chunk_storage;
pub mod chunker;
pub mod cksumvfs;
pub mod client;
pub mod compression;
pub mod crypto;
pub mod dir_chunk_storage;
pub mod external_chunk_storage;
pub mod fmtutil;
pub mod fprefetch;
pub mod fstx1;
pub mod fstx2;
pub mod fsutil;
pub mod hex;
pub mod htree;
pub mod index;
pub mod indexer;
pub mod ioutil;
pub mod keys;
pub mod migrate;
pub mod oplog;
pub mod pem;
pub mod protocol;
pub mod put;
pub mod query;
pub mod querycache;
pub mod repository;
pub mod rollsum;
pub mod sendlog;
pub mod server;
pub mod sodium;
pub mod vfs;
pub mod xglobset;
pub mod xid;
pub mod xtar;

use plmap::PipelineMap;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as FmtWrite;
use std::io::{BufRead, Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn die(s: String) -> ! {
    let _ = writeln!(std::io::stderr(), "{}", s);
    let _ = std::io::stderr().flush();
    std::process::exit(1);
}

fn cache_dir() -> Result<PathBuf, anyhow::Error> {
    let mut cache_dir = match std::env::var_os("XDG_CACHE_HOME") {
        Some(cache_dir) => PathBuf::from(&cache_dir),
        None => match std::env::var_os("HOME") {
            Some(home) => {
                let mut h = PathBuf::from(&home);
                h.push(".cache");
                h
            }
            None => anyhow::bail!("unable to determine cache dir from XDG_CACHE_HOME or HOME"),
        },
    };
    cache_dir.push("bupstash");
    Ok(cache_dir)
}

fn print_help_and_exit(subcommand: &str, opts: &getopts::Options) {
    let subcommand_no_alias = match subcommand {
        "remove" => "rm",
        subcommand => subcommand,
    };

    let mut brief = match subcommand_no_alias {
        "init" => include_str!("../doc/cli/init.txt"),
        "help" => include_str!("../doc/cli/help.txt"),
        "new-key" => include_str!("../doc/cli/new-key.txt"),
        "new-sub-key" => include_str!("../doc/cli/new-sub-key.txt"),
        "put" => include_str!("../doc/cli/put.txt"),
        "list" => include_str!("../doc/cli/list.txt"),
        "list-contents" => include_str!("../doc/cli/list-contents.txt"),
        "diff" => include_str!("../doc/cli/diff.txt"),
        "get" => include_str!("../doc/cli/get.txt"),
        "restore" => include_str!("../doc/cli/restore.txt"),
        "rm" => include_str!("../doc/cli/rm.txt"),
        "recover-removed" => include_str!("../doc/cli/recover-removed.txt"),
        "gc" => include_str!("../doc/cli/gc.txt"),
        "serve" => include_str!("../doc/cli/serve.txt"),
        "sync" => include_str!("../doc/cli/sync.txt"),
        "version" => include_str!("../doc/cli/version.txt"),
        "exec-with-locks" => include_str!("../doc/cli/exec-with-locks.txt"),
        "put-benchmark" => "put-benchmark tool.",
        _ => panic!(),
    }
    .to_string();

    writeln!(brief, "\n\nOnline Manual:").unwrap();
    write!(
        brief,
        "  https://bupstash.io/doc/{}/man/bupstash-{}.html",
        env!("CARGO_PKG_VERSION"),
        subcommand_no_alias
    )
    .unwrap();

    let _ = std::io::stdout().write_all(opts.usage(&brief).as_bytes());

    std::process::exit(0);
}

fn default_cli_opts() -> getopts::Options {
    let mut opts = getopts::Options::new();
    opts.parsing_style(getopts::ParsingStyle::StopAtFirstFree);
    opts.optflag("h", "help", "print this help menu.");
    opts
}

fn query_cli_opts(opts: &mut getopts::Options) {
    opts.optopt(
        "",
        "query-cache",
        "Path to the query cache (used for storing synced items before search). \
        See manual for default values and relevant environment variables.",
        "PATH",
    );
    opts.optflag(
        "",
        "query-encrypted",
        "The query will not decrypt any metadata, allowing you to \
        list items you do not have a decryption key for.\
        This option inserts the pseudo query tag 'decryption-key-id'.",
    );
    opts.optflag(
        "",
        "utc-timestamps",
        "Display and search against timestamps in utc time instead of local time.",
    );
    opts.optflag("", "no-progress", "Suppress progress indicators.");
    opts.optflag("q", "quiet", "Be quiet, implies --no-progress.");
}

fn repo_cli_opts(opts: &mut getopts::Options) {
    opts.optopt(
        "r",
        "repository",
        "Repository to interact with, if prefixed with ssh:// implies ssh access. \
         Defaults to BUPSTASH_REPOSITORY if not set. \
         See the manual for additional ways to connect to the repository.",
        "REPO",
    );
}

fn parse_cli_opts(opts: getopts::Options, args: &[String]) -> getopts::Matches {
    if args.len() >= 2 && (args[1] == "-h" || args[1] == "--help") {
        print_help_and_exit(&args[0], &opts)
    }
    let matches = opts
        .parse(&args[1..])
        .unwrap_or_else(|e| die(e.to_string()));
    if matches.opt_present("h") {
        print_help_and_exit(&args[0], &opts)
    };
    matches
}

fn cli_to_key(matches: &getopts::Matches) -> Result<keys::Key, anyhow::Error> {
    if let Some(k) = cli_to_opt_key(matches)? {
        Ok(k)
    } else {
        anyhow::bail!("please set --key, BUPSTASH_KEY or BUPSTASH_KEY_COMMAND");
    }
}

fn cli_to_opt_key(matches: &getopts::Matches) -> Result<Option<keys::Key>, anyhow::Error> {
    match matches.opt_str("key") {
        Some(k) => Ok(Some(keys::Key::load_from_file(&k)?)),
        None => {
            if let Some(k) = std::env::var_os("BUPSTASH_KEY") {
                Ok(Some(keys::Key::load_from_file(&k.into_string().unwrap())?))
            } else if let Some(cmd) = std::env::var_os("BUPSTASH_KEY_COMMAND") {
                match shlex::split(&cmd.into_string().unwrap()) {
                    Some(mut args) => {
                        if args.is_empty() {
                            anyhow::bail!("BUPSTASH_KEY_COMMAND must not be empty")
                        }
                        let bin = args.remove(0);

                        match std::process::Command::new(bin)
                            .args(args)
                            .stderr(std::process::Stdio::inherit())
                            .stdin(std::process::Stdio::inherit())
                            .output()
                        {
                            Ok(key_data) => Ok(Some(keys::Key::from_slice(&key_data.stdout)?)),
                            Err(e) => anyhow::bail!("error running BUPSTASH_KEY_COMMAND: {}", e),
                        }
                    }
                    None => anyhow::bail!("unable to parse BUPSTASH_KEY_COMMAND"),
                }
            } else {
                Ok(None)
            }
        }
    }
}

fn new_key_main(args: Vec<String>) -> Result<(), anyhow::Error> {
    let mut opts = default_cli_opts();
    opts.reqopt("o", "output", "set output file.", "PATH");
    let matches = parse_cli_opts(opts, &args[..]);
    let primary_key = keys::Key::PrimaryKeyV1(keys::PrimaryKey::gen());
    primary_key.write_to_file(&matches.opt_str("o").unwrap())
}

fn new_sub_key_main(args: Vec<String>) -> Result<(), anyhow::Error> {
    let mut opts = default_cli_opts();

    opts.optopt(
        "k",
        "key",
        "primary key to derive metadata key from.",
        "PATH",
    );

    opts.reqopt("o", "output", "output file.", "PATH");

    opts.optflag(
        "",
        "put",
        "The key is able to encrypt data for put operations.",
    );
    opts.optflag(
        "",
        "list",
        "The key will be able to decrypt metadata and perform queries.",
    );
    opts.optflag(
        "",
        "list-contents",
        "The key will be able to list item contents with 'list-contents' (implies --list).",
    );

    let matches = parse_cli_opts(opts, &args[..]);

    let allow_put = matches.opt_present("put");
    let allow_list = matches.opt_present("list");
    let allow_list_contents = matches.opt_present("list-contents");

    let k = cli_to_key(&matches)?;
    match k {
        keys::Key::PrimaryKeyV1(primary_key) => {
            let subk = keys::Key::SubKeyV1(keys::SubKey::gen(
                &primary_key,
                allow_put,
                allow_list,
                allow_list_contents,
            ));
            subk.write_to_file(&matches.opt_str("o").unwrap())
        }
        _ => anyhow::bail!("key is not a primary key"),
    }
}

fn cli_to_query_cache(matches: &getopts::Matches) -> Result<querycache::QueryCache, anyhow::Error> {
    match matches.opt_str("query-cache") {
        Some(query_cache) => querycache::QueryCache::open(&PathBuf::from(query_cache)),
        None => match std::env::var_os("BUPSTASH_QUERY_CACHE") {
            Some(query_cache) => querycache::QueryCache::open(&PathBuf::from(query_cache)),
            None => {
                let mut p = cache_dir()?;
                std::fs::create_dir_all(&p)?;
                p.push("bupstash.qcache");
                querycache::QueryCache::open(&p)
            }
        },
    }
}

fn cli_to_id_and_opt_query(
    matches: &getopts::Matches,
) -> Result<(Option<xid::Xid>, Option<query::Query>), anyhow::Error> {
    if !matches.free.is_empty() {
        match query::parse(&matches.free.join("•")) {
            Ok(query) => Ok((query::get_id_query(&query), Some(query))),
            Err(e) => {
                query::report_parse_error(e);
                anyhow::bail!("query parse error");
            }
        }
    } else {
        Ok((None, None))
    }
}

fn cli_to_id_and_query(
    matches: &getopts::Matches,
) -> Result<(Option<xid::Xid>, query::Query), anyhow::Error> {
    let (id, query) = cli_to_id_and_opt_query(matches)?;
    let query = if let Some(query) = query {
        query
    } else {
        anyhow::bail!("you must specify a query");
    };
    Ok((id, query))
}

// Define a smiple wrapper around the serve process
// the wrapper ensures we handle stderr correctly.
struct ServeProcess {
    stderr_reader: Option<std::thread::JoinHandle<()>>,
    proc: std::process::Child,
}

impl ServeProcess {
    fn wait(mut self) -> Result<(), anyhow::Error> {
        let status = self.proc.wait()?;
        if let Some(handle) = self.stderr_reader.take() {
            handle.join().unwrap();
        }
        if !status.success() {
            if let Some(code) = status.code() {
                anyhow::bail!("bupstash serve failed (exit-code={})", code);
            } else {
                anyhow::bail!("bupstash serve failed");
            }
        }
        Ok(())
    }
}

impl Drop for ServeProcess {
    fn drop(&mut self) {
        unsafe { libc::kill(self.proc.id() as i32, libc::SIGTERM) };
        if let Some(handle) = self.stderr_reader.take() {
            handle.join().unwrap();
        }
    }
}

#[derive(Clone, Copy)]
struct ServeProcessCliOpts<'a, 'b, 'c> {
    repository_arg: &'a str,
    repository_env_var: &'b str,
    repository_command_env_var: &'c str,
}

impl<'a, 'b, 'c> Default for ServeProcessCliOpts<'a, 'b, 'c> {
    fn default() -> Self {
        ServeProcessCliOpts {
            repository_arg: "repository",
            repository_env_var: "BUPSTASH_REPOSITORY",
            repository_command_env_var: "BUPSTASH_REPOSITORY_COMMAND",
        }
    }
}

fn cli_to_serve_process(
    matches: &getopts::Matches,
    progress: &indicatif::ProgressBar,
    opts: ServeProcessCliOpts,
) -> Result<ServeProcess, anyhow::Error> {
    let mut serve_cmd_args = {
        let repo = if matches.opt_present(opts.repository_arg) {
            Some(matches.opt_str(opts.repository_arg).unwrap())
        } else {
            std::env::var_os(opts.repository_env_var).map(|r| r.into_string().unwrap())
        };

        match repo {
            Some(repo) => {
                if repo.starts_with("ssh://") {
                    let re = regex::Regex::new(r"^ssh://(?:([a-zA-Z0-9]+)@)?([^/]*)(.*)$")?;
                    let caps = re.captures(&repo).unwrap();

                    let mut args = vec!["ssh".to_owned()];

                    if let Some(user) = caps.get(1) {
                        args.push("-o".to_owned());
                        args.push("User=".to_owned() + user.as_str());
                    }
                    args.push(caps[2].to_string());
                    args.push("--".to_owned());
                    args.push("bupstash".to_owned());
                    args.push("serve".to_owned());
                    let repo_path = caps[3].to_string();
                    if !repo_path.is_empty() {
                        args.push(repo_path);
                    }
                    args
                } else {
                    vec![
                        if cfg!(target_os = "openbsd") {
                            std::env::args().next().unwrap()
                        } else {
                            std::env::current_exe()?.to_string_lossy().to_string()
                        },
                        "serve".to_owned(),
                        repo,
                    ]
                }
            }
            None => {
                if let Some(connect_cmd) = std::env::var_os(opts.repository_command_env_var) {
                    match shlex::split(&connect_cmd.into_string().unwrap()) {
                        Some(args) => {
                            if args.is_empty() {
                                anyhow::bail!(
                                    "{} should have at least one element",
                                    opts.repository_command_env_var,
                                );
                            }
                            args
                        }
                        None => {
                            anyhow::bail!("unable to parse {}", opts.repository_command_env_var)
                        }
                    }
                } else {
                    anyhow::bail!(
                        "please set --{}, {} or {}",
                        opts.repository_arg,
                        opts.repository_env_var,
                        opts.repository_command_env_var,
                    );
                }
            }
        }
    };

    let bin = serve_cmd_args.remove(0);

    let mut proc = match std::process::Command::new(bin)
        .args(serve_cmd_args)
        .stderr(if progress.is_hidden() {
            std::process::Stdio::inherit()
        } else {
            std::process::Stdio::piped()
        })
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(err) => anyhow::bail!("error spawning serve command: {}", err),
    };

    let stderr_reader = if progress.is_hidden() {
        None
    } else {
        let progress = progress.clone();
        let proc_stderr = proc.stderr.take().unwrap();

        let stderr_reader = std::thread::spawn(move || {
            let buf_reader = std::io::BufReader::new(proc_stderr);
            for line in buf_reader.lines().flatten() {
                progress.println(&line);
                // Theres a tiny race condition here where we may print an
                // error line twice, I can't see how to fix this unless we
                // rewrite the progress bar library to report if the print happened.
                if progress.is_finished() || progress.is_hidden() {
                    let _ = writeln!(std::io::stderr(), "{}", line);
                }
            }
        });

        Some(stderr_reader)
    };

    Ok(ServeProcess {
        stderr_reader,
        proc,
    })
}

fn cli_to_opened_serve_process(
    matches: &getopts::Matches,
    progress: &indicatif::ProgressBar,
    serve_process_opts: ServeProcessCliOpts,
    open_mode: protocol::OpenMode,
) -> Result<ServeProcess, anyhow::Error> {
    let mut retry_delay_secs = 2;
    let mut retry_count: u64 = 0;

    loop {
        let mut remote = cli_to_serve_process(matches, progress, serve_process_opts)?;
        // Temporary borrow of the stdin/stdout so we can handle
        // server retry back pressure if the server implements this.
        let mut proc_stdin = remote.proc.stdin.take().unwrap();
        let mut proc_stdout = remote.proc.stdout.take().unwrap();

        match open_mode {
            protocol::OpenMode::ReadWrite | protocol::OpenMode::Gc => {
                progress.set_message("acquiring repository lock...");
            }
            _ => (),
        }

        match client::open_repository(&mut proc_stdin, &mut proc_stdout, open_mode) {
            Ok(()) => {
                remote.proc.stdin = Some(proc_stdin);
                remote.proc.stdout = Some(proc_stdout);
                return Ok(remote);
            }
            Err(err) => {
                if let Some(protocol::AbortError::ServerUnavailable { message, .. }) =
                    err.root_cause().downcast_ref()
                {
                    if retry_count == 1 {
                        // Print after the second retry so that DNS load balancing can resolve
                        // the problem if the server supports this.
                        progress.println(format!(
                            "server is unavailable ({}), retrying after delay.",
                            message
                        ));
                    }
                    std::thread::sleep(std::time::Duration::from_secs(retry_delay_secs));
                    retry_delay_secs = (retry_delay_secs * 2).min(180);
                    retry_count += 1;
                    if retry_count < 50 {
                        continue;
                    }
                }
                return Err(err);
            }
        }
    }
}

fn cli_to_progress_bar(
    matches: &getopts::Matches,
    style: indicatif::ProgressStyle,
) -> indicatif::ProgressBar {
    let want_visible_progress = !matches.opt_present("no-progress")
        && !matches.opt_present("quiet")
        && atty::is(atty::Stream::Stderr)
        && atty::is(atty::Stream::Stdout);
    let progress = indicatif::ProgressBar::with_draw_target(
        u64::MAX,
        if want_visible_progress {
            indicatif::ProgressDrawTarget::stderr()
        } else {
            indicatif::ProgressDrawTarget::hidden()
        },
    );
    progress.set_style(style);
    progress.set_message("connecting...");
    if want_visible_progress {
        progress.enable_steady_tick(250)
    };
    progress.tick();
    progress
}

fn help_main(args: Vec<String>) {
    let opts = default_cli_opts();
    print_help_and_exit(&args[0], &opts);
}

fn version_main(args: Vec<String>) -> Result<(), anyhow::Error> {
    let opts = default_cli_opts();
    parse_cli_opts(opts, &args[..]);
    writeln!(
        &mut std::io::stdout(),
        "bupstash-{}",
        env!("CARGO_PKG_VERSION"),
    )?;
    Ok(())
}

fn init_main(args: Vec<String>) -> Result<(), anyhow::Error> {
    let mut opts = default_cli_opts();
    repo_cli_opts(&mut opts);
    opts.optopt(
        "s",
        "storage",
        "The storage engine specification. 'dir', or a json specification. Consult the manual for details.",
        "STORAGE",
    );
    opts.optflag("", "no-progress", "Suppress progress indicators.");
    opts.optflag("q", "quiet", "Be quiet, implies --no-progress.");
    let matches = parse_cli_opts(opts, &args[..]);

    let storage_spec: Option<repository::StorageEngineSpec> = match matches.opt_str("storage") {
        Some(s) if s == "dir" => Some(repository::StorageEngineSpec::DirStore),
        Some(s) => match serde_json::from_str(&s) {
            Ok(s) => Some(s),
            Err(err) => anyhow::bail!("unable to parse storage engine spec: {}", err),
        },
        None => None,
    };

    let progress = cli_to_progress_bar(
        &matches,
        indicatif::ProgressStyle::default_spinner().template("[{elapsed_precise}] {wide_msg}"),
    );

    let mut serve_proc = cli_to_serve_process(&matches, &progress, ServeProcessCliOpts::default())?;
    let mut serve_out = serve_proc.proc.stdout.as_mut().unwrap();
    let mut serve_in = serve_proc.proc.stdin.as_mut().unwrap();

    client::init_repository(&mut serve_out, &mut serve_in, storage_spec)?;
    client::hangup(&mut serve_in)?;
    serve_proc.wait()?;

    Ok(())
}

enum ListFormat {
    Human,
    Jsonl1,
    Bare,
}

fn list_main(args: Vec<String>) -> Result<(), anyhow::Error> {
    let mut opts = default_cli_opts();
    repo_cli_opts(&mut opts);
    opts.optopt(
        "k",
        "key",
        "primary or metadata key to decrypt item metadata with during listing.",
        "PATH",
    );
    opts.optopt(
        "",
        "format",
        "Output format, valid values are 'human' or 'jsonl1'.",
        "FORMAT",
    );
    query_cli_opts(&mut opts);

    let matches = parse_cli_opts(opts, &args[..]);

    let list_format = match matches.opt_str("format") {
        Some(f) => match &f[..] {
            "jsonl1" => ListFormat::Jsonl1,
            "human" => ListFormat::Human,
            _ => anyhow::bail!("invalid --format, expected one of 'human' or 'jsonl1'"),
        },
        None => ListFormat::Human,
    };

    let (primary_key_id, metadata_dctx) = match cli_to_opt_key(&matches)? {
        Some(key) => {
            if !key.is_list_key() {
                anyhow::bail!(
                    "only main keys and sub keys created with '--list' can be used for listing"
                )
            }

            let primary_key_id = key.primary_key_id();
            let metadata_dctx = match key {
                keys::Key::PrimaryKeyV1(k) => {
                    crypto::DecryptionContext::new(k.metadata_sk, k.metadata_psk)
                }
                keys::Key::SubKeyV1(k) => {
                    crypto::DecryptionContext::new(k.metadata_sk.unwrap(), k.metadata_psk.unwrap())
                }
                _ => unreachable!(),
            };

            (Some(primary_key_id), Some(metadata_dctx))
        }
        None => {
            if !matches.opt_present("query-encrypted") {
                anyhow::bail!("please set --key, BUPSTASH_KEY, BUPSTASH_KEY_COMMAND or pass --query-encrypted");
            }
            (None, None)
        }
    };

    let query = if !matches.free.is_empty() {
        match query::parse(&matches.free.join("•")) {
            Ok(query) => Some(query),
            Err(e) => {
                query::report_parse_error(e);
                anyhow::bail!("query parse error");
            }
        }
    } else {
        None
    };

    let progress = cli_to_progress_bar(
        &matches,
        indicatif::ProgressStyle::default_spinner().template("[{elapsed_precise}] {wide_msg}"),
    );

    let mut query_cache = cli_to_query_cache(&matches)?;

    let mut serve_proc = cli_to_opened_serve_process(
        &matches,
        &progress,
        ServeProcessCliOpts::default(),
        protocol::OpenMode::Read,
    )?;
    let mut serve_out = serve_proc.proc.stdout.as_mut().unwrap();
    let mut serve_in = serve_proc.proc.stdin.as_mut().unwrap();

    client::sync_query_cache(progress, &mut query_cache, &mut serve_out, &mut serve_in)?;
    client::hangup(&mut serve_in)?;
    serve_proc.wait()?;

    let out = std::io::stdout();
    let mut out = out.lock();

    let mut on_match =
        |item_id: xid::Xid,
         tags: &std::collections::BTreeMap<String, String>,
         metadata: &oplog::VersionedItemMetadata,
         secret_metadata: Option<&oplog::DecryptedItemMetadata>| {
            let mut tags: Vec<(&String, &String)> = tags.iter().collect();

            // Custom sort to be more human friendly.
            tags.sort_by(|(k1, _), (k2, _)| match (k1.as_str(), k2.as_str()) {
                ("id", _) => std::cmp::Ordering::Less,
                (_, "id") => std::cmp::Ordering::Greater,
                ("name", _) => std::cmp::Ordering::Less,
                (_, "name") => std::cmp::Ordering::Greater,
                _ => k1.partial_cmp(k2).unwrap(),
            });

            match list_format {
                ListFormat::Human => {
                    for (i, (k, v)) in tags.iter().enumerate() {
                        if i != 0 {
                            write!(out, " ")?;
                        }
                        write!(
                            out,
                            "{}=\"{}\"",
                            k,
                            v.replace('\\', "\\\\").replace('\"', "\\\"")
                        )?;
                    }
                    writeln!(out)?;
                }
                ListFormat::Jsonl1 => {
                    write!(out, "{{")?;
                    write!(
                        out,
                        "\"id\":{}",
                        serde_json::to_string(&item_id.to_string())?
                    )?;
                    write!(
                        out,
                        ",\"decryption_key_id\":{}",
                        serde_json::to_string(&metadata.primary_key_id().to_string())?
                    )?;
                    let data_tree = metadata.data_tree();
                    write!(out, ",\"data_tree\":{{")?;
                    write!(
                        out,
                        "\"address\":{}",
                        serde_json::to_string(&data_tree.address.to_string())?
                    )?;
                    write!(out, ",\"height\":{}", data_tree.height.0)?;
                    write!(
                        out,
                        ",\"data_chunk_count\":{}",
                        data_tree.data_chunk_count.0
                    )?;
                    write!(out, "}}")?;
                    if let Some(index_tree) = metadata.index_tree() {
                        write!(out, ",\"index_tree\":{{")?;
                        write!(
                            out,
                            "\"address\":{}",
                            serde_json::to_string(&index_tree.address.to_string())?
                        )?;
                        write!(out, ",\"height\":{}", index_tree.height.0)?;
                        write!(
                            out,
                            ",\"data_chunk_count\":{}",
                            index_tree.data_chunk_count.0
                        )?;
                        write!(out, "}}")?;
                    }
                    if let Some(secret_metadata) = secret_metadata {
                        write!(out, ",\"data_size\":{}", secret_metadata.data_size.0)?;
                        write!(out, ",\"index_size\":{}", secret_metadata.index_size.0)?;
                        write!(out, ",\"put_key_id\":\"{:x}\"", secret_metadata.send_key_id)?;
                        write!(
                            out,
                            ",\"data_hash_key_part\":\"{:x}\"",
                            secret_metadata.data_hash_key_part_2
                        )?;
                        write!(
                            out,
                            ",\"index_hash_key_part\":\"{:x}\"",
                            secret_metadata.index_hash_key_part_2
                        )?;
                    }
                    let unix_timestamp_millis = match metadata {
                        oplog::VersionedItemMetadata::V1(_) => {
                            secret_metadata.map(|secret_metadata| {
                                secret_metadata.timestamp.timestamp_millis() as u64
                            })
                        }
                        oplog::VersionedItemMetadata::V2(ref metadata) => {
                            Some(metadata.plain_text_metadata.unix_timestamp_millis)
                        }
                        oplog::VersionedItemMetadata::V3(ref metadata) => {
                            Some(metadata.plain_text_metadata.unix_timestamp_millis)
                        }
                        _ => anyhow::bail!("metadata is from a future version of bupstash"),
                    };
                    if let Some(unix_timestamp_millis) = unix_timestamp_millis {
                        write!(out, ",\"unix_timestamp_millis\":{}", unix_timestamp_millis)?;
                    }
                    write!(out, ",\"tags\":{{")?;
                    for (i, (k, v)) in tags.iter().enumerate() {
                        if i != 0 {
                            write!(out, ", ")?;
                        }
                        write!(
                            out,
                            "{}:{}",
                            serde_json::to_string(&k)?,
                            serde_json::to_string(&v)?
                        )?;
                    }
                    write!(out, "}}")?;
                    writeln!(out, "}}")?;
                }
                ListFormat::Bare => anyhow::bail!("unsupported list format"),
            }

            Ok(())
        };

    let mut tx = query_cache.transaction()?;
    tx.list(
        querycache::ListOptions {
            primary_key_id,
            query,
            metadata_dctx,
            list_encrypted: matches.opt_present("query-encrypted"),
            utc_timestamps: matches.opt_present("utc-timestamps"),
            now: chrono::Utc::now(),
        },
        &mut on_match,
    )?;

    Ok(())
}

fn put_main(args: Vec<String>) -> Result<(), anyhow::Error> {
    let mut opts = default_cli_opts();
    repo_cli_opts(&mut opts);
    opts.optopt(
        "k",
        "key",
        "Primary or put key to encrypt data with.",
        "PATH",
    );
    opts.optopt(
        "",
        "compression",
        "Compression algorithm, one of 'none', 'lz4' or 'zstd'[:$level]. Defaults to 'zstd:3'.",
        "COMPRESS",
    );
    opts.optflag("", "no-default-tags", "Disable the default tag(s) 'name'.");
    opts.optflag(
        "v",
        "verbose",
        "Be verbose, implies for --print-file-actions and --print-stats.",
    );
    opts.optflag(
        "",
        "print-file-actions",
        "Print file actions in the form '$a $t $path' to stderr when processing directories, see the manual for details on the format.",
    );
    opts.optflag(
        "",
        "print-stats",
        "Print put statistics to stderr on completion.",
    );

    opts.optflag("", "no-progress", "Suppress progress indicators.");
    opts.optflag("q", "quiet", "Be quiet, implies --no-progress.");

    opts.optflag(
        "e",
        "exec",
        "Treat arguments as a command to run, ensuring it succeeds before committing the item.",
    );
    opts.optflag(
        "",
        "no-stat-caching",
        "Do not use stat caching to skip sending files to the repository.",
    );
    opts.optflag(
        "",
        "no-send-log",
        "Disable logging of previously sent data, implies --no-stat-caching.",
    );
    opts.optflag(
        "",
        "xattrs",
        "Save directory entry xattrs (at some performance cost).",
    );
    opts.optopt(
        "",
        "send-log",
        "Use the file at PATH as a 'send log', used to skip data that was previously sent to the repository.",
        "PATH",
    );
    opts.optmulti(
        "",
        "exclude",
        "Exclude directory entries matching the given glob pattern when saving a directory, may be passed multiple times.\
        Paths are absolute as they are currently mounted, and must start with a slash and not end on one even for directories.\
        Patterns without any slashes will match any file (`--exclude foo` is equivalent to `--exclude '/**/foo'`).",
        "PATTERN",
    );
    opts.optmulti(
        "",
        "exclude-if-present",
        "Exclude a directory's content if it contains a file with the given name. May be passed multiple times.
  This will still backup the folder itself, containing the marker file. Common marker file names are `CACHEDIR.TAG`, `.backupexclude`
  or `.no-backup`.",
        "FILENAME",
    );
    opts.optflag(
        "",
        "ignore-permission-errors",
        "Ignore permission denied errors, skipping those files or directories.",
    );
    opts.optflag(
        "",
        "one-file-system",
        "Do not cross mount points when traversing the file system.",
    );
    opts.optopt(
        "",
        "indexer-threads",
        "Number of processor threads to use for pipelined parallel file metadata reads. Defaults to 1.",
        "N",
    );
    opts.optopt(
        "",
        "threads",
        "Number of processor threads to use for pipelined parallel reading, hashing, compression and encryption. Defaults to the number of processors.",
        "N",
    );

    let matches = parse_cli_opts(opts, &args);

    let tag_re = regex::Regex::new(r"^([a-zA-Z0-9\\-_]+)=(.+)$").unwrap();

    let mut tags = BTreeMap::<String, String>::new();
    let mut source_args = Vec::new();

    {
        let mut collecting_tags = true;

        for a in &matches.free {
            if collecting_tags && a == "::" {
                collecting_tags = false;
                continue;
            }
            if collecting_tags {
                match tag_re.captures(a) {
                    Some(caps) => {
                        let t = &caps[1];
                        let v = &caps[2];
                        tags.insert(t.to_string(), v.to_string());
                    }
                    None => {
                        collecting_tags = false;
                        source_args.push(a.to_string());
                    }
                }
            } else {
                source_args.push(a.to_string());
            }
        }
    }

    let want_xattrs = matches.opt_present("xattrs");
    let ignore_permission_errors = matches.opt_present("ignore-permission-errors");
    let one_file_system = matches.opt_present("one-file-system");
    let print_stats = matches.opt_present("print-stats") || matches.opt_present("verbose");
    let print_file_actions =
        matches.opt_present("print-file-actions") || matches.opt_present("verbose");
    let use_stat_cache =
        !(matches.opt_present("no-stat-caching") || matches.opt_present("no-send-log"));

    let indexer_threads = matches
        .opt_str("indexer-threads")
        .as_deref()
        .map(|n| {
            n.parse::<usize>()
                .map_err(|err| anyhow::format_err!("error parsing --indexer-threads: {}", err))
        })
        .unwrap_or_else(|| Ok(1))?;

    let threads = matches
        .opt_str("threads")
        .as_deref()
        .map(|n| {
            n.parse::<usize>()
                .map_err(|err| anyhow::format_err!("error parsing --threads: {}", err))
        })
        .unwrap_or_else(|| Ok(num_cpus::get_physical()))?;

    let compression = {
        let scheme = matches
            .opt_str("compression")
            .unwrap_or_else(|| "zstd:3".to_string());
        compression::parse_scheme(&scheme)?
    };

    let mut exclusions = globset::GlobSetBuilder::new();
    for mut e in matches.opt_strs("exclude") {
        /* Sanity checks. Beware, the order of the checks matters */

        /* An exclude path ending on / won't match anything. */
        if e.ends_with('/') {
            anyhow::bail!(
                "--exclude option '{}' ends with '/', so it won't match anything",
                e
            );
        }

        /* This check is technically redundant, but it gives a nicer error message. */
        if e.starts_with("./") || e.starts_with("../") {
            anyhow::bail!("relative paths are not allowed in --exclude patterns");
        }

        /* Start with a / to match a path, and leave out slashes to match any file. */
        if e.starts_with('/') || e.starts_with("**/") {
            /* pass */
        } else if e.contains('/') && !e.contains('[') {
            /* This is just to help the user with a nicer error message, we do
             * not take this branch if the pattern contains an escape character. */
            anyhow::bail!(
                "--exclude option '{}' contains '/' so must be absolute to match anything",
                e
            );
        } else {
            /* Just a file name */
            e = format!("**/{}", e);
        }

        /* Check for unnormalized segments, as they too won't match anything. Skip this
         * check if the pattern contains the escape character. */
        if (e.contains("/./") || e.contains("/../") || e.contains("//")) && !e.contains('[') {
            anyhow::bail!(
                "--exclude option '{}' must be normalized (no '.', '..' or '//' path segments)",
                e
            );
        }

        let mut pattern = globset::GlobBuilder::new(&e);
        /* For some reason, the default doesn't give us the common behavior one would expect. */
        pattern.literal_separator(true);
        pattern.backslash_escape(true);
        let pattern = pattern.build().map_err(|err| {
            anyhow::format_err!("--exclude option '{}' is not a valid glob: {}", e, err)
        })?;
        exclusions.add(pattern);
    }
    let exclusions = exclusions.build()?;

    let exclusion_markers = matches
        .opt_strs("exclude-if-present")
        .drain(..)
        .map(std::ffi::OsString::from)
        .collect();

    let checkpoint_seconds: u64 = match std::env::var("BUPSTASH_CHECKPOINT_SECONDS") {
        Ok(v) => match v.parse() {
            Ok(v) => v,
            Err(err) => anyhow::bail!("unable to parse BUPSTASH_CHECKPOINT_SECONDS: {}", err),
        },
        Err(_) => 600, /* Default value 10 minutes */
    };

    let key = cli_to_key(&matches)?;

    if !key.is_put_key() {
        anyhow::bail!(
            "can only send data with a primary key or a sub key created with '--allow-put'."
        );
    }

    let primary_key_id = key.primary_key_id();
    let send_key_id = key.id();

    let (idx_hash_key, data_hash_key, gear_tab, data_ectx, metadata_ectx, idx_ectx) = match key {
        keys::Key::PrimaryKeyV1(k) => {
            let idx_hash_key =
                crypto::derive_hash_key(&k.idx_hash_key_part_1, &k.idx_hash_key_part_2);
            let data_hash_key =
                crypto::derive_hash_key(&k.data_hash_key_part_1, &k.data_hash_key_part_2);
            let gear_tab = k.rollsum_key.gear_tab();
            let data_ectx = crypto::EncryptionContext::new(&k.data_pk, &k.data_psk);
            let metadata_ectx = crypto::EncryptionContext::new(&k.metadata_pk, &k.metadata_psk);
            let idx_ectx = crypto::EncryptionContext::new(&k.idx_pk, &k.idx_psk);
            (
                idx_hash_key,
                data_hash_key,
                gear_tab,
                data_ectx,
                metadata_ectx,
                idx_ectx,
            )
        }
        keys::Key::SubKeyV1(k) => {
            let idx_hash_key = crypto::derive_hash_key(
                &k.idx_hash_key_part_1.unwrap(),
                &k.idx_hash_key_part_2.unwrap(),
            );
            let data_hash_key = crypto::derive_hash_key(
                &k.data_hash_key_part_1.unwrap(),
                &k.data_hash_key_part_2.unwrap(),
            );
            let gear_tab = k.rollsum_key.unwrap().gear_tab();
            let data_ectx =
                crypto::EncryptionContext::new(&k.data_pk.unwrap(), &k.data_psk.unwrap());
            let metadata_ectx =
                crypto::EncryptionContext::new(&k.metadata_pk.unwrap(), &k.metadata_psk.unwrap());
            let idx_ectx = crypto::EncryptionContext::new(&k.idx_pk.unwrap(), &k.idx_psk.unwrap());
            (
                idx_hash_key,
                data_hash_key,
                gear_tab,
                data_ectx,
                metadata_ectx,
                idx_ectx,
            )
        }
        _ => unreachable!(),
    };

    let default_tags = !matches.opt_present("no-default-tags");

    let data_source: client::DataSource;

    let progress = cli_to_progress_bar(
        &matches,
        indicatif::ProgressStyle::default_spinner()
            .template("[{elapsed_precise}] {wide_msg} [{bytes} sent, {bytes_per_sec}]"),
    );

    let file_action_log_fn = if print_file_actions {
        let log_fn: Arc<index::FileActionLogFn> = if progress.is_hidden() {
            Arc::new(Box::new(move |action: char, ty: char, path: &Path| {
                writeln!(
                    std::io::stderr(),
                    "{} {} {}",
                    action,
                    ty,
                    path.as_os_str().to_string_lossy()
                )?;
                Ok(())
            }))
        } else {
            let progress = progress.clone();
            Arc::new(Box::new(move |action: char, ty: char, path: &Path| {
                progress.println(format!(
                    "{} {} {}",
                    action,
                    ty,
                    path.as_os_str().to_string_lossy()
                ));
                Ok(())
            }))
        };
        Some(log_fn)
    } else {
        None
    };

    if matches.opt_present("exec") {
        data_source = client::DataSource::Subprocess(source_args)
    } else if source_args.is_empty() {
        anyhow::bail!("data sources should be a file, directory, or command (use '-' for stdin).");
    } else if source_args.len() == 1 {
        if source_args[0] == "-" {
            // Dup stdin so we can read the data from an unbuffered file.
            let inf = unsafe {
                std::fs::File::from_raw_fd(nix::unistd::dup(std::io::stdin().as_raw_fd())?)
            };
            data_source = client::DataSource::Readable {
                description: "<stdin>".to_string(),
                data: Box::new(inf),
            };
        } else {
            let input_path: PathBuf = std::convert::From::from(&source_args[0]);
            let input_path = fsutil::absolute_path(&input_path)?;

            let md = match std::fs::metadata(&input_path) {
                Ok(md) => md,
                Err(err) => anyhow::bail!("unable to put {:?}: {}", input_path, err),
            };

            let name = match input_path.file_name() {
                Some(name) => name.to_string_lossy().to_string(),
                None => "rootfs".to_string(),
            };

            if md.is_dir() {
                if default_tags && !tags.contains_key("name") {
                    tags.insert("name".to_string(), name + ".tar");
                }

                data_source = client::DataSource::Filesystem {
                    paths: vec![input_path],
                    exclusions,
                    exclusion_markers,
                };
            } else if md.is_file() {
                if default_tags && !tags.contains_key("name") {
                    tags.insert("name".to_string(), name);
                }

                data_source = client::DataSource::Readable {
                    description: input_path.to_string_lossy().to_string(),
                    data: Box::new(std::fs::File::open(input_path)?),
                };
            } else {
                anyhow::bail!("{} is not a file or a directory", source_args[0]);
            }
        }
    } else {
        // Gather absolute paths.
        let mut absolute_paths = Vec::new();
        for input_path in source_args.iter() {
            let input_path = match fsutil::absolute_path(input_path) {
                Ok(p) => p,
                Err(err) => anyhow::bail!("unable to put {:?}: {}", input_path, err),
            };
            absolute_paths.push(input_path)
        }

        if default_tags && !tags.contains_key("name") {
            // We should always have at least "/" in common.
            let base_path = fsutil::common_path_all(&absolute_paths).unwrap();

            let name = match base_path.file_name() {
                Some(name) => name.to_string_lossy().to_string() + ".tar",
                None => "rootfs.tar".to_string(),
            };

            tags.insert("name".to_string(), name);
        }

        data_source = client::DataSource::Filesystem {
            paths: absolute_paths,
            exclusions,
            exclusion_markers,
        };
    };

    // No easy way to compute the tag set length without actually encoding it due
    // to var ints in the bare encoding.
    if serde_bare::to_vec(&tags)?.len() > oplog::MAX_TAG_SET_SIZE {
        anyhow::bail!("tags must not exceed {} bytes", oplog::MAX_TAG_SET_SIZE);
    }

    let send_log = if matches.opt_present("no-send-log") {
        None
    } else {
        progress.set_message("acquiring exclusive lock on send log...");
        match matches.opt_str("send-log") {
            Some(send_log) => Some(sendlog::SendLog::open(&PathBuf::from(send_log))?),
            None => match std::env::var_os("BUPSTASH_SEND_LOG") {
                Some(send_log) => Some(sendlog::SendLog::open(&PathBuf::from(send_log))?),
                None => {
                    let mut p = cache_dir()?;
                    std::fs::create_dir_all(&p)?;
                    p.push("bupstash.sendlog");
                    Some(sendlog::SendLog::open(&p)?)
                }
            },
        }
    };

    let mut serve_proc = cli_to_opened_serve_process(
        &matches,
        &progress,
        ServeProcessCliOpts::default(),
        protocol::OpenMode::ReadWrite,
    )?;
    let mut serve_out = serve_proc.proc.stdout.as_mut().unwrap();
    let mut serve_in = serve_proc.proc.stdin.as_mut().unwrap();

    let ctx = client::PutContext {
        progress: progress.clone(),
        compression,
        checkpoint_seconds,
        use_stat_cache,
        primary_key_id,
        send_key_id,
        gear_tab,
        data_hash_key,
        data_ectx,
        metadata_ectx,
        idx_hash_key,
        idx_ectx,
        want_xattrs,
        one_file_system,
        file_action_log_fn,
        ignore_permission_errors,
        send_log,
        indexer_threads,
        threads,
    };

    let (id, stats) = client::put(ctx, &mut serve_out, &mut serve_in, tags, data_source)?;
    client::hangup(&mut serve_in)?;
    serve_proc.wait()?;

    progress.finish_and_clear();

    if print_stats {
        let duration = stats.end_time.signed_duration_since(stats.start_time);
        let total_uncompressed_size = stats.uncompressed_data_size + stats.uncompressed_index_size;
        writeln!(
            std::io::stderr(),
            "{}m{}.{}s elapsed",
            duration.num_minutes(),
            duration.num_seconds() % 60,
            duration.num_milliseconds() % 1000
        )?;
        writeln!(
            std::io::stderr(),
            "{} chunk(s), {} processed",
            stats.total_chunks,
            fmtutil::format_size(total_uncompressed_size)
        )?;
        writeln!(
            std::io::stderr(),
            "{} chunk(s), {} (compressed) sent",
            stats.transferred_chunks,
            fmtutil::format_size(stats.transferred_bytes),
        )?;
        writeln!(
            std::io::stderr(),
            "{} chunk(s), {} (compressed) added",
            stats.added_chunks,
            fmtutil::format_size(stats.added_bytes)
        )?;
        if stats.added_bytes == 0 {
            writeln!(std::io::stderr(), "infinite effective compression ratio")?;
        } else {
            let ecr = (total_uncompressed_size as f64) / (stats.added_bytes as f64);
            writeln!(
                std::io::stderr(),
                "{:.1}:1 effective compression ratio",
                ecr
            )?;
        }
    }

    writeln!(std::io::stdout(), "{:x}", id)?;
    Ok(())
}

fn get_main(args: Vec<String>) -> Result<(), anyhow::Error> {
    let mut opts = default_cli_opts();
    repo_cli_opts(&mut opts);
    query_cli_opts(&mut opts);
    opts.optopt("k", "key", "Key to decrypt data with.", "PATH");
    opts.optopt(
        "",
        "pick",
        "Pick a single file or sub-directory from a directory snapshot.",
        "PATH",
    );

    let matches = parse_cli_opts(opts, &args[..]);

    let key = cli_to_key(&matches)?;
    let primary_key_id = key.primary_key_id();
    let (idx_hash_key_part_1, data_hash_key_part_1, data_dctx, metadata_dctx, idx_dctx) = match key
    {
        keys::Key::PrimaryKeyV1(k) => {
            let idx_hash_key_part_1 = k.idx_hash_key_part_1.clone();
            let data_hash_key_part_1 = k.data_hash_key_part_1.clone();
            let data_dctx = crypto::DecryptionContext::new(k.data_sk, k.data_psk.clone());
            let metadata_dctx = crypto::DecryptionContext::new(k.metadata_sk, k.metadata_psk);
            let idx_dctx = crypto::DecryptionContext::new(k.idx_sk, k.idx_psk);
            (
                idx_hash_key_part_1,
                data_hash_key_part_1,
                data_dctx,
                metadata_dctx,
                idx_dctx,
            )
        }
        _ => anyhow::bail!("provided key is not a data decryption key"),
    };

    let progress = cli_to_progress_bar(
        &matches,
        indicatif::ProgressStyle::default_spinner().template("[{elapsed_precise}] {wide_msg}"),
    );

    let (id, query) = cli_to_id_and_query(&matches)?;
    let mut serve_proc = cli_to_opened_serve_process(
        &matches,
        &progress,
        ServeProcessCliOpts::default(),
        protocol::OpenMode::Read,
    )?;
    let mut serve_out = serve_proc.proc.stdout.as_mut().unwrap();
    let mut serve_in = serve_proc.proc.stdin.as_mut().unwrap();

    let id = match (id, query) {
        (Some(id), _) => id,
        (_, query) => {
            let mut query_cache = cli_to_query_cache(&matches)?;

            // Only sync the client if we have a non id query.
            client::sync_query_cache(
                progress.clone(),
                &mut query_cache,
                &mut serve_out,
                &mut serve_in,
            )?;

            let mut n_matches: u64 = 0;
            let mut id = xid::Xid::default();

            let mut on_match =
                |item_id: xid::Xid,
                 _tags: &std::collections::BTreeMap<String, String>,
                 _metadata: &oplog::VersionedItemMetadata,
                 _secret_metadata: Option<&oplog::DecryptedItemMetadata>| {
                    n_matches += 1;
                    id = item_id;

                    if n_matches > 1 {
                        anyhow::bail!(
                            "the provided query matched {} items, need a single match",
                            n_matches
                        );
                    }

                    Ok(())
                };

            let mut tx = query_cache.transaction()?;
            tx.list(
                querycache::ListOptions {
                    primary_key_id: Some(primary_key_id),
                    metadata_dctx: Some(metadata_dctx.clone()),
                    list_encrypted: matches.opt_present("query-encrypted"),
                    utc_timestamps: matches.opt_present("utc-timestamps"),
                    query: Some(query),
                    now: chrono::Utc::now(),
                },
                &mut on_match,
            )?;

            id
        }
    };

    progress.set_message("fetching item metadata...");
    let metadata = client::request_metadata(id, &mut serve_out, &mut serve_in)?;

    let mut get_index = if metadata.index_tree().is_some() {
        Some(client::request_index(
            client::IndexRequestContext {
                primary_key_id,
                idx_hash_key_part_1,
                idx_dctx,
                metadata_dctx: metadata_dctx.clone(),
            },
            id,
            &metadata,
            &mut serve_out,
            &mut serve_in,
        )?)
    } else {
        None
    };

    let mut get_data_map = None;

    if matches.opt_present("pick") {
        progress.set_message("picking content...");

        if let Some(ref index) = get_index {
            let pick_path: PathBuf = matches.opt_str("pick").unwrap().into();
            let (pick_index, pick_data_map) = index::pick(&pick_path, index)?;
            get_index = pick_index;
            get_data_map = Some(pick_data_map);
        } else {
            anyhow::bail!("requested item does not have a content index (tarball was not created by bupstash)")
        }
    };

    progress.finish_and_clear();

    // rust line buffers stdin and stdout unconditionally, so bypass it.
    let mut stdout_unbuffered = unsafe { std::fs::File::from_raw_fd(libc::STDOUT_FILENO) };

    let result = client::request_data_stream(
        client::DataRequestContext {
            primary_key_id,
            data_hash_key_part_1,
            data_dctx,
            metadata_dctx,
        },
        id,
        &metadata,
        get_data_map,
        get_index,
        &mut serve_out,
        &mut serve_in,
        &mut stdout_unbuffered,
    );

    // Prevent stdout from being closed prematurely.
    stdout_unbuffered.into_raw_fd();

    // Now that we dropped out stdout handle, it is safe to return on error.
    result?;

    client::hangup(&mut serve_in)?;
    serve_proc.wait()?;

    Ok(())
}

fn list_contents_main(args: Vec<String>) -> Result<(), anyhow::Error> {
    let mut opts = default_cli_opts();
    repo_cli_opts(&mut opts);
    query_cli_opts(&mut opts);
    opts.optopt("k", "key", "Key to decrypt data with.", "PATH");
    opts.optopt(
        "",
        "format",
        "Output format, valid values are 'human' or 'jsonl1'.",
        "FORMAT",
    );
    opts.optopt(
        "",
        "pick",
        "Pick a sub-directory from a directory snapshot.",
        "PATH",
    );

    let matches = parse_cli_opts(opts, &args[..]);

    let list_format = match matches.opt_str("format") {
        Some(f) => match &f[..] {
            "jsonl1" => ListFormat::Jsonl1,
            "human" => ListFormat::Human,
            "BARE" => ListFormat::Bare,
            _ => anyhow::bail!("invalid --format, expected one of 'human' or 'jsonl1'"),
        },
        None => ListFormat::Human,
    };

    let key = cli_to_key(&matches)?;

    if !key.is_list_key() || !key.is_list_contents_key() {
        anyhow::bail!(
            "only primary keys and sub keys created with '--list-contents' can list contents"
        );
    }

    let primary_key_id = key.primary_key_id();
    let (idx_hash_key_part_1, metadata_dctx, idx_dctx) = match key {
        keys::Key::PrimaryKeyV1(k) => {
            let idx_hash_key_part_1 = k.idx_hash_key_part_1.clone();
            let metadata_dctx = crypto::DecryptionContext::new(k.metadata_sk, k.metadata_psk);
            let idx_dctx = crypto::DecryptionContext::new(k.idx_sk, k.idx_psk);
            (idx_hash_key_part_1, metadata_dctx, idx_dctx)
        }
        keys::Key::SubKeyV1(k) => {
            let idx_hash_key_part_1 = k.idx_hash_key_part_1.unwrap();
            let metadata_dctx =
                crypto::DecryptionContext::new(k.metadata_sk.unwrap(), k.metadata_psk.unwrap());
            let idx_dctx = crypto::DecryptionContext::new(k.idx_sk.unwrap(), k.idx_psk.unwrap());
            (idx_hash_key_part_1, metadata_dctx, idx_dctx)
        }
        _ => unreachable!(),
    };

    let progress = cli_to_progress_bar(
        &matches,
        indicatif::ProgressStyle::default_spinner().template("[{elapsed_precise}] {wide_msg}"),
    );

    let (id, query) = cli_to_id_and_query(&matches)?;
    let mut serve_proc = cli_to_opened_serve_process(
        &matches,
        &progress,
        ServeProcessCliOpts::default(),
        protocol::OpenMode::Read,
    )?;
    let mut serve_out = serve_proc.proc.stdout.as_mut().unwrap();
    let mut serve_in = serve_proc.proc.stdin.as_mut().unwrap();

    let id = match (id, query) {
        (Some(id), _) => id,
        (_, query) => {
            let mut query_cache = cli_to_query_cache(&matches)?;

            // Only sync the client if we have a non id query.
            client::sync_query_cache(
                progress.clone(),
                &mut query_cache,
                &mut serve_out,
                &mut serve_in,
            )?;

            let mut n_matches: u64 = 0;
            let mut id = xid::Xid::default();

            let mut on_match =
                |item_id: xid::Xid,
                 _tags: &std::collections::BTreeMap<String, String>,
                 _metadata: &oplog::VersionedItemMetadata,
                 _secret_metadata: Option<&oplog::DecryptedItemMetadata>| {
                    n_matches += 1;
                    id = item_id;

                    if n_matches > 1 {
                        anyhow::bail!(
                            "the provided query matched {} items, need a single match",
                            n_matches
                        );
                    }

                    Ok(())
                };

            let mut tx = query_cache.transaction()?;
            tx.list(
                querycache::ListOptions {
                    primary_key_id: Some(primary_key_id),
                    metadata_dctx: Some(metadata_dctx.clone()),
                    list_encrypted: matches.opt_present("query-encrypted"),
                    utc_timestamps: matches.opt_present("utc-timestamps"),
                    query: Some(query),
                    now: chrono::Utc::now(),
                },
                &mut on_match,
            )?;

            id
        }
    };

    progress.set_message("fetching item metadata...");
    let metadata = client::request_metadata(id, &mut serve_out, &mut serve_in)?;

    if metadata.index_tree().is_none() {
        anyhow::bail!(
            "list-contents is only supported for directory snapshots created by bupstash"
        );
    }

    progress.set_message("fetching content index...");
    let mut content_index = client::request_index(
        client::IndexRequestContext {
            primary_key_id,
            idx_hash_key_part_1,
            idx_dctx,
            metadata_dctx,
        },
        id,
        &metadata,
        &mut serve_out,
        &mut serve_in,
    )?;

    if matches.opt_present("pick") {
        progress.set_message("picking content...");
        let pick_path: PathBuf = matches.opt_str("pick").unwrap().into();
        content_index = index::pick_dir_without_data(&pick_path, &content_index)?;
    }

    client::hangup(&mut serve_in)?;
    serve_proc.wait()?;

    progress.finish_and_clear();

    let utc_timestamps = matches.opt_present("utc-timestamps");

    let out = std::io::stdout();
    let mut out = out.lock();

    match list_format {
        ListFormat::Human => {
            let widths = fmtutil::estimate_index_human_display_widths(&content_index)?;
            for ent in content_index.iter() {
                let ent = ent?;
                writeln!(
                    out,
                    "{}",
                    fmtutil::format_human_content_listing(&ent, utc_timestamps, &widths),
                )?;
            }
        }
        ListFormat::Jsonl1 => {
            for ent in content_index.iter() {
                let ent = ent?;
                writeln!(out, "{}", fmtutil::format_jsonl1_content_listing(&ent)?)?;
            }
        }
        ListFormat::Bare => {
            for ent in content_index.iter() {
                let ent = ent?;
                out.write_all(&serde_bare::to_vec(&ent)?)?;
            }
        }
    }

    out.flush()?;

    Ok(())
}

fn diff_main(args: Vec<String>) -> Result<(), anyhow::Error> {
    let mut opts = default_cli_opts();
    repo_cli_opts(&mut opts);
    query_cli_opts(&mut opts);
    opts.optopt("k", "key", "Key to decrypt data with.", "PATH");
    opts.optopt(
        "i",
        "ignore",
        "Comma separated list of file attributes to ignore in comparisons. Valid values are 'content,dev,devnos,inode,type,perms,nlink,uid,gid,times,xattrs'.",
        "IGNORE",
    );
    opts.optflag(
        "",
        "relaxed",
        "Shortcut for --ignore dev,inode,nlink,uid,gid,times,xattrs.",
    );
    opts.optflag(
        "",
        "xattrs",
        "Fetch xattrs when indexing a local directories.",
    );
    opts.optopt(
        "",
        "left-pick",
        "Perform diff on a sub-directory of the left query.",
        "PATH",
    );
    opts.optopt(
        "",
        "right-pick",
        "Perform diff on a sub-directory of the right query.",
        "PATH",
    );
    opts.optopt(
        "",
        "format",
        "Output format, valid values are 'human' or 'jsonl'.",
        "FORMAT",
    );
    opts.optopt(
        "",
        "indexer-threads",
        "Number of processor threads to use for pipelined parallel file hashing and metadata reads. Defaults to the number of processors.",
        "N",
    );

    let matches = parse_cli_opts(opts, &args[..]);
    let utc_timestamps = matches.opt_present("utc-timestamps");
    let list_format = match matches.opt_str("format") {
        Some(f) => match &f[..] {
            "jsonl1" => ListFormat::Jsonl1,
            "human" => ListFormat::Human,
            _ => anyhow::bail!("invalid --format, expected one of 'human' or 'jsonl'"),
        },
        None => ListFormat::Human,
    };

    let indexer_threads = matches
        .opt_str("indexer-threads")
        .as_deref()
        .map(|n| {
            n.parse::<usize>()
                .map_err(|err| anyhow::format_err!("error parsing --indexer-threads: {}", err))
        })
        .unwrap_or_else(|| Ok(num_cpus::get_physical()))?;

    let mut diff_mask = index::INDEX_COMPARE_MASK_DATA_CURSORS;

    if matches.opt_present("relaxed") {
        diff_mask |= index::INDEX_COMPARE_MASK_DEV;
        diff_mask |= index::INDEX_COMPARE_MASK_INO;
        diff_mask |= index::INDEX_COMPARE_MASK_NLINK;
        diff_mask |= index::INDEX_COMPARE_MASK_MTIME | index::INDEX_COMPARE_MASK_CTIME;
        diff_mask |= index::INDEX_COMPARE_MASK_UID;
        diff_mask |= index::INDEX_COMPARE_MASK_GID;
        diff_mask |= index::INDEX_COMPARE_MASK_XATTRS;
    }

    if let Some(fields) = matches.opt_str("ignore") {
        let mut to_toggle = 0;
        for f in fields.split(',') {
            match f {
                "dev" => to_toggle |= index::INDEX_COMPARE_MASK_DEV,
                "devnos" => to_toggle |= index::INDEX_COMPARE_MASK_DEVNOS,
                "uid" => to_toggle |= index::INDEX_COMPARE_MASK_UID,
                "gid" => to_toggle |= index::INDEX_COMPARE_MASK_GID,
                "inode" => to_toggle |= index::INDEX_COMPARE_MASK_INO,
                "nlink" => to_toggle |= index::INDEX_COMPARE_MASK_NLINK,
                "type" => to_toggle |= index::INDEX_COMPARE_MASK_TYPE,
                "perms" => to_toggle |= index::INDEX_COMPARE_MASK_PERMS,
                "file-content" => {
                    to_toggle |=
                        index::INDEX_COMPARE_MASK_SIZE | index::INDEX_COMPARE_MASK_DATA_HASH
                }
                "times" => {
                    to_toggle |= index::INDEX_COMPARE_MASK_MTIME | index::INDEX_COMPARE_MASK_CTIME
                }
                "xattrs" => to_toggle |= index::INDEX_COMPARE_MASK_XATTRS,
                _ => anyhow::bail!("'{}' is not a valid ignore value", f),
            }
        }
        diff_mask |= to_toggle
    }

    let mut queries = vec![vec![]];
    {
        for a in &matches.free {
            if a == "::" {
                queries.push(vec![]);
                continue;
            }
            queries.last_mut().unwrap().push(a.to_string());
        }

        if queries.len() != 2 {
            anyhow::bail!("expected two queries separated by '::'");
        }
    }

    let key = cli_to_key(&matches)?;

    if !key.is_list_key() || !key.is_list_contents_key() {
        anyhow::bail!(
            "only primary keys and sub keys created with '--list-contents' can diff contents"
        );
    }

    let primary_key_id = key.primary_key_id();
    let (idx_hash_key_part_1, metadata_dctx, idx_dctx) = match key {
        keys::Key::PrimaryKeyV1(k) => {
            let idx_hash_key_part_1 = k.idx_hash_key_part_1.clone();
            let metadata_dctx = crypto::DecryptionContext::new(k.metadata_sk, k.metadata_psk);
            let idx_dctx = crypto::DecryptionContext::new(k.idx_sk, k.idx_psk);
            (idx_hash_key_part_1, metadata_dctx, idx_dctx)
        }
        keys::Key::SubKeyV1(k) => {
            let idx_hash_key_part_1 = k.idx_hash_key_part_1.unwrap();
            let metadata_dctx =
                crypto::DecryptionContext::new(k.metadata_sk.unwrap(), k.metadata_psk.unwrap());
            let idx_dctx = crypto::DecryptionContext::new(k.idx_sk.unwrap(), k.idx_psk.unwrap());
            (idx_hash_key_part_1, metadata_dctx, idx_dctx)
        }
        _ => unreachable!(),
    };

    let progress = cli_to_progress_bar(
        &matches,
        indicatif::ProgressStyle::default_spinner().template("[{elapsed_precise}] {wide_msg}"),
    );

    let mut serve_proc = cli_to_opened_serve_process(
        &matches,
        &progress,
        ServeProcessCliOpts::default(),
        protocol::OpenMode::Read,
    )?;
    let mut serve_out = serve_proc.proc.stdout.as_mut().unwrap();
    let mut serve_in = serve_proc.proc.stdin.as_mut().unwrap();

    let mut already_synced = false;
    let mut to_diff = vec![];

    for query in queries {
        if !query.is_empty() && (query[0].starts_with("./") || query[0].starts_with('/')) {
            let paths: Vec<PathBuf> = query.iter().map(PathBuf::from).collect();
            let mut ciw = index::CompressedIndexWriter::new();
            for ent in indexer::FsIndexer::new(
                &paths,
                indexer::FsIndexerOptions {
                    exclusions: globset::GlobSet::empty(),
                    exclusion_markers: std::collections::HashSet::new(),
                    want_xattrs: matches.opt_present("xattrs"),
                    want_sparseness: false,
                    want_hash: true,
                    one_file_system: false,
                    ignore_permission_errors: false,
                    file_action_log_fn: None,
                    threads: indexer_threads,
                },
            )? {
                ciw.add(&ent?.1);
            }
            to_diff.push(ciw.finish())
        } else {
            let (id, query) = match query::parse(&query.join("•")) {
                Ok(query) => (query::get_id_query(&query), query),
                Err(e) => {
                    query::report_parse_error(e);
                    anyhow::bail!("query parse error");
                }
            };

            let id = match (id, query) {
                (Some(id), _) => id,
                (_, query) => {
                    let mut query_cache = cli_to_query_cache(&matches)?;

                    if !already_synced {
                        client::sync_query_cache(
                            progress.clone(),
                            &mut query_cache,
                            &mut serve_out,
                            &mut serve_in,
                        )?;
                        already_synced = true;
                    }

                    let mut n_matches: u64 = 0;
                    let mut id = xid::Xid::default();

                    let mut on_match =
                        |item_id: xid::Xid,
                         _tags: &std::collections::BTreeMap<String, String>,
                         _metadata: &oplog::VersionedItemMetadata,
                         _secret_metadata: Option<&oplog::DecryptedItemMetadata>| {
                            n_matches += 1;
                            id = item_id;

                            if n_matches > 1 {
                                anyhow::bail!(
                                    "provided query matched {} items, need a single match",
                                    n_matches
                                );
                            }

                            Ok(())
                        };

                    let mut tx = query_cache.transaction()?;
                    tx.list(
                        querycache::ListOptions {
                            primary_key_id: Some(primary_key_id),
                            metadata_dctx: Some(metadata_dctx.clone()),
                            list_encrypted: matches.opt_present("query-encrypted"),
                            query: Some(query),
                            now: chrono::Utc::now(),
                            utc_timestamps,
                        },
                        &mut on_match,
                    )?;

                    id
                }
            };

            progress.set_message("fetching item metadata...");
            let metadata = client::request_metadata(id, &mut serve_out, &mut serve_in)?;

            if metadata.index_tree().is_none() {
                anyhow::bail!("diff is only supported for directory snapshots created by bupstash");
            }

            progress.set_message("fetching content index...");
            let content_index = client::request_index(
                client::IndexRequestContext {
                    primary_key_id,
                    idx_hash_key_part_1: idx_hash_key_part_1.clone(),
                    metadata_dctx: metadata_dctx.clone(),
                    idx_dctx: idx_dctx.clone(),
                },
                id,
                &metadata,
                &mut serve_out,
                &mut serve_in,
            )?;

            to_diff.push(content_index);
        }
    }

    client::hangup(&mut serve_in)?;
    serve_proc.wait()?;

    for (i, pick_opt) in ["left-pick", "right-pick"].iter().enumerate() {
        if matches.opt_present(pick_opt) {
            progress.set_message("picking content...");
            let pick_path: PathBuf = matches.opt_str(pick_opt).unwrap().into();
            to_diff[i] = index::pick_dir_without_data(&pick_path, &to_diff[i])?;
        }
    }

    progress.finish_and_clear();

    let out = std::io::stdout();
    let mut out = out.lock();

    match list_format {
        ListFormat::Human => {
            let lwidths = fmtutil::estimate_index_human_display_widths(&to_diff[0])?;
            let rwidths = fmtutil::estimate_index_human_display_widths(&to_diff[1])?;
            let widths = fmtutil::IndexHumanDisplayWidths {
                human_size_digits: std::cmp::max(
                    lwidths.human_size_digits,
                    rwidths.human_size_digits,
                ),
            };
            index::diff(
                &to_diff[0],
                &to_diff[1],
                diff_mask,
                &mut |st: index::DiffStat, e: &index::IndexEntry| -> Result<(), anyhow::Error> {
                    let op = match st {
                        index::DiffStat::Unchanged => return Ok(()),
                        index::DiffStat::Added => '+',
                        index::DiffStat::Removed => '-',
                    };

                    writeln!(
                        std::io::stdout(),
                        "{} {}",
                        op,
                        fmtutil::format_human_content_listing(e, utc_timestamps, &widths)
                    )?;
                    Ok(())
                },
            )?;
        }
        ListFormat::Jsonl1 => {
            index::diff(
                &to_diff[0],
                &to_diff[1],
                diff_mask,
                &mut |st: index::DiffStat, e: &index::IndexEntry| -> Result<(), anyhow::Error> {
                    let op = match st {
                        index::DiffStat::Unchanged => return Ok(()),
                        index::DiffStat::Added => '+',
                        index::DiffStat::Removed => '-',
                    };

                    writeln!(
                        std::io::stdout(),
                        "{} {}",
                        op,
                        fmtutil::format_jsonl1_content_listing(e)?
                    )?;
                    Ok(())
                },
            )?;
        }
        ListFormat::Bare => {
            anyhow::bail!("unsupported diff format");
        }
    }

    out.flush()?;

    Ok(())
}

fn remove_main(args: Vec<String>) -> Result<(), anyhow::Error> {
    let mut opts = default_cli_opts();
    repo_cli_opts(&mut opts);
    query_cli_opts(&mut opts);

    opts.optopt("k", "key", "Key to decrypt metadata with.", "PATH");

    opts.optflag(
        "",
        "ids-from-stdin",
        "Remove items with IDs read from stdin, one per line, instead of executing a query.",
    );

    opts.optflag("", "allow-many", "Allow multiple removals.");

    let matches = parse_cli_opts(opts, &args[..]);

    let progress = cli_to_progress_bar(
        &matches,
        indicatif::ProgressStyle::default_spinner().template("[{elapsed_precise}] {wide_msg}"),
    );

    let n_removed;

    if matches.opt_present("ids-from-stdin") {
        let mut ids = Vec::new();

        for l in std::io::stdin().lock().lines() {
            let l = l?;
            if l.is_empty() {
                continue;
            }
            match xid::Xid::parse(&l) {
                Ok(id) => ids.push(id),
                Err(err) => anyhow::bail!("error id parsing {:?}: {}", l, err),
            };
        }

        let mut serve_proc = cli_to_opened_serve_process(
            &matches,
            &progress,
            ServeProcessCliOpts::default(),
            protocol::OpenMode::ReadWrite,
        )?;
        let mut serve_out = serve_proc.proc.stdout.as_mut().unwrap();
        let mut serve_in = serve_proc.proc.stdin.as_mut().unwrap();

        n_removed = client::remove(progress.clone(), ids, &mut serve_out, &mut serve_in)?;
        client::hangup(&mut serve_in)?;
        serve_proc.wait()?;
    } else {
        let mut serve_proc = cli_to_opened_serve_process(
            &matches,
            &progress,
            ServeProcessCliOpts::default(),
            protocol::OpenMode::ReadWrite,
        )?;
        let mut serve_out = serve_proc.proc.stdout.as_mut().unwrap();
        let mut serve_in = serve_proc.proc.stdin.as_mut().unwrap();

        let ids: Vec<xid::Xid> = match cli_to_id_and_query(&matches)? {
            (Some(id), _) => vec![id],
            (_, query) => {
                let mut query_cache = cli_to_query_cache(&matches)?;

                // Only sync the client if we have a non id query.
                client::sync_query_cache(
                    progress.clone(),
                    &mut query_cache,
                    &mut serve_out,
                    &mut serve_in,
                )?;

                let (primary_key_id, metadata_dctx) = match cli_to_opt_key(&matches)? {
                    Some(key) => {
                        if !key.is_list_key() {
                            anyhow::bail!("only primary keys and sub keys created with '--list' can be used for listing")
                        }

                        let primary_key_id = key.primary_key_id();
                        let metadata_dctx = match key {
                            keys::Key::PrimaryKeyV1(k) => {
                                crypto::DecryptionContext::new(k.metadata_sk, k.metadata_psk)
                            }
                            keys::Key::SubKeyV1(k) => crypto::DecryptionContext::new(
                                k.metadata_sk.unwrap(),
                                k.metadata_psk.unwrap(),
                            ),
                            _ => unreachable!(),
                        };

                        (Some(primary_key_id), Some(metadata_dctx))
                    }
                    None => {
                        if !matches.opt_present("query-encrypted") {
                            anyhow::bail!("please set --key, BUPSTASH_KEY, BUPSTASH_KEY_COMMAND or pass --query-encrypted");
                        }
                        (None, None)
                    }
                };

                let mut ids = Vec::new();

                let mut on_match =
                    |item_id: xid::Xid,
                     _tags: &std::collections::BTreeMap<String, String>,
                     _metadata: &oplog::VersionedItemMetadata,
                     _secret_metadata: Option<&oplog::DecryptedItemMetadata>| {
                        ids.push(item_id);
                        Ok(())
                    };

                let mut tx = query_cache.transaction()?;
                tx.list(
                    querycache::ListOptions {
                        primary_key_id,
                        metadata_dctx,
                        list_encrypted: matches.opt_present("query-encrypted"),
                        utc_timestamps: matches.opt_present("utc-timestamps"),
                        query: Some(query),
                        now: chrono::Utc::now(),
                    },
                    &mut on_match,
                )?;

                if ids.len() > 1 && !matches.opt_present("allow-many") {
                    anyhow::bail!(
                        "the provided query matched {} items, need a single match unless --allow-many is specified",
                        ids.len()
                    );
                };

                ids
            }
        };
        n_removed = client::remove(progress.clone(), ids, &mut serve_out, &mut serve_in)?;
        client::hangup(&mut serve_in)?;
        serve_proc.wait()?;
    };

    progress.finish_and_clear();

    writeln!(std::io::stdout(), "{} item(s) removed", n_removed)?;

    Ok(())
}

fn sync_main(args: Vec<String>) -> Result<(), anyhow::Error> {
    let mut opts = default_cli_opts();
    repo_cli_opts(&mut opts);
    query_cli_opts(&mut opts);

    opts.optopt("k", "key", "Key to decrypt metadata with.", "PATH");

    opts.optflag(
        "",
        "ids-from-stdin",
        "Sync items with IDs read from stdin, one per line, instead of executing a query.",
    );

    opts.optopt(
        "",
        "to",
        "Repository to sync items to, if prefixed with ssh:// implies ssh access. \
         Defaults to BUPSTASH_TO_REPOSITORY if not set. \
         See the manual for additional ways to connect to the repository.",
        "REPO",
    );

    let matches = parse_cli_opts(opts, &args[..]);

    let progress = cli_to_progress_bar(
        &matches,
        indicatif::ProgressStyle::default_spinner().template("[{elapsed_precise}] {wide_msg}"),
    );

    let mut ids = Vec::new();

    let mut source_serve_proc = cli_to_opened_serve_process(
        &matches,
        &progress,
        ServeProcessCliOpts::default(),
        protocol::OpenMode::Read,
    )?;
    let mut source_serve_out = source_serve_proc.proc.stdout.as_mut().unwrap();
    let mut source_serve_in = source_serve_proc.proc.stdin.as_mut().unwrap();

    let mut ids_to_metadata: Option<HashMap<xid::Xid, oplog::VersionedItemMetadata>> = None;

    if matches.opt_present("ids-from-stdin") {
        for l in std::io::stdin().lock().lines() {
            let l = l?;
            if l.is_empty() {
                continue;
            }
            match xid::Xid::parse(&l) {
                Ok(id) => ids.push(id),
                Err(err) => anyhow::bail!("error id parsing {:?}: {}", l, err),
            };
        }
    } else {
        match cli_to_id_and_opt_query(&matches)? {
            (Some(id), _) => ids.push(id),
            (_, query) => {
                let mut query_cache = cli_to_query_cache(&matches)?;

                // Only sync the client if we have a non id query.
                client::sync_query_cache(
                    progress.clone(),
                    &mut query_cache,
                    &mut source_serve_out,
                    &mut source_serve_in,
                )?;

                let (primary_key_id, metadata_dctx) = match cli_to_opt_key(&matches)? {
                    Some(key) => {
                        if !key.is_list_key() {
                            anyhow::bail!("only primary keys and sub keys created with '--list' can be used for listing")
                        }

                        let primary_key_id = key.primary_key_id();
                        let metadata_dctx = match key {
                            keys::Key::PrimaryKeyV1(k) => {
                                crypto::DecryptionContext::new(k.metadata_sk, k.metadata_psk)
                            }
                            keys::Key::SubKeyV1(k) => crypto::DecryptionContext::new(
                                k.metadata_sk.unwrap(),
                                k.metadata_psk.unwrap(),
                            ),
                            _ => unreachable!(),
                        };

                        (Some(primary_key_id), Some(metadata_dctx))
                    }
                    None => (None, None),
                };

                ids_to_metadata = Some(HashMap::new());

                let mut on_match =
                    |item_id: xid::Xid,
                     _tags: &std::collections::BTreeMap<String, String>,
                     metadata: &oplog::VersionedItemMetadata,
                     _secret_metadata: Option<&oplog::DecryptedItemMetadata>| {
                        ids.push(item_id);
                        ids_to_metadata
                            .as_mut()
                            .unwrap()
                            .insert(item_id, metadata.clone());
                        Ok(())
                    };

                let mut tx = query_cache.transaction()?;
                tx.list(
                    querycache::ListOptions {
                        primary_key_id,
                        metadata_dctx,
                        list_encrypted: matches.opt_present("query-encrypted") || query.is_none(),
                        utc_timestamps: matches.opt_present("utc-timestamps"),
                        query,
                        now: chrono::Utc::now(),
                    },
                    &mut on_match,
                )?;
            }
        };
    };

    let mut dest_serve_proc = cli_to_opened_serve_process(
        &matches,
        &progress,
        ServeProcessCliOpts {
            repository_arg: "to",
            repository_env_var: "BUPSTASH_TO_REPOSITORY",
            repository_command_env_var: "BUPSTASH_TO_REPOSITORY_COMMAND",
        },
        protocol::OpenMode::ReadWrite,
    )?;
    let mut dest_serve_out = dest_serve_proc.proc.stdout.as_mut().unwrap();
    let mut dest_serve_in = dest_serve_proc.proc.stdin.as_mut().unwrap();

    client::repo_sync(
        &progress,
        ids,
        ids_to_metadata,
        &mut source_serve_out,
        &mut source_serve_in,
        &mut dest_serve_out,
        &mut dest_serve_in,
    )?;

    client::hangup(&mut source_serve_in)?;
    source_serve_proc.wait()?;

    client::hangup(&mut dest_serve_in)?;
    dest_serve_proc.wait()?;

    progress.finish_and_clear();

    Ok(())
}

fn gc_main(args: Vec<String>) -> Result<(), anyhow::Error> {
    let mut opts = default_cli_opts();
    opts.optflag("", "no-progress", "Suppress progress indicators.");
    opts.optflag("q", "quiet", "Be quiet, implies --no-progress.");

    repo_cli_opts(&mut opts);
    let matches = parse_cli_opts(opts, &args[..]);

    let progress = cli_to_progress_bar(
        &matches,
        indicatif::ProgressStyle::default_spinner().template("[{elapsed_precise}] {wide_msg}"),
    );

    let mut serve_proc = cli_to_opened_serve_process(
        &matches,
        &progress,
        ServeProcessCliOpts::default(),
        protocol::OpenMode::Gc,
    )?;
    let mut serve_out = serve_proc.proc.stdout.as_mut().unwrap();
    let mut serve_in = serve_proc.proc.stdin.as_mut().unwrap();

    let stats = client::gc(progress.clone(), &mut serve_out, &mut serve_in)?;
    client::hangup(&mut serve_in)?;
    serve_proc.wait()?;

    progress.finish_and_clear();

    let out = std::io::stdout();
    let mut out = out.lock();

    if let Some(chunks_deleted) = stats.chunks_deleted {
        writeln!(out, "{} chunks deleted", chunks_deleted)?;
    }
    if let Some(chunks_remaining) = stats.chunks_remaining {
        writeln!(out, "{} chunks remaining", chunks_remaining)?;
    }
    if let Some(bytes_deleted) = stats.bytes_deleted {
        writeln!(out, "{} deleted", fmtutil::format_size(bytes_deleted))?;
    }
    if let Some(bytes_remaining) = stats.bytes_remaining {
        writeln!(out, "{} remaining", fmtutil::format_size(bytes_remaining))?;
    }

    Ok(())
}

fn recover_removed_main(args: Vec<String>) -> Result<(), anyhow::Error> {
    let mut opts = default_cli_opts();
    opts.optflag("", "no-progress", "Suppress progress indicators.");
    opts.optflag("q", "quiet", "Be quiet, implies --no-progress.");

    repo_cli_opts(&mut opts);
    let matches = parse_cli_opts(opts, &args[..]);

    let progress = cli_to_progress_bar(
        &matches,
        indicatif::ProgressStyle::default_spinner().template("[{elapsed_precise}] {wide_msg}"),
    );

    let mut serve_proc = cli_to_opened_serve_process(
        &matches,
        &progress,
        ServeProcessCliOpts::default(),
        protocol::OpenMode::ReadWrite,
    )?;
    let mut serve_out = serve_proc.proc.stdout.as_mut().unwrap();
    let mut serve_in = serve_proc.proc.stdin.as_mut().unwrap();

    let n_recovered = client::recover_removed(progress.clone(), &mut serve_out, &mut serve_in)?;
    client::hangup(&mut serve_in)?;
    serve_proc.wait()?;

    progress.finish_and_clear();

    writeln!(std::io::stdout(), "{} item(s) recovered", n_recovered)?;

    Ok(())
}

fn put_benchmark_main(args: Vec<String>) -> Result<(), anyhow::Error> {
    let mut opts = default_cli_opts();
    opts.optopt("", "compression", "Compression algorithm.", "ALGO");
    opts.optflag("", "compress", "Do chunk compression.");
    opts.optflag("", "address", "Compute chunk content addresses.");
    opts.optflag("", "encrypt", "Encrypt chunks.");
    opts.optflag("", "print", "Print data to stdout.");
    opts.optflag("", "pipelining", "Do parallel pipelining.");
    opts.optflag("", "print-chunk-size", "Print chunk sizes.");
    opts.optopt(
        "",
        "address-threads",
        "Number of processor threads to use for computing addresses, Defaults to 0.",
        "N",
    );
    opts.optopt(
        "",
        "compress-threads",
        "Number of processor threads to use for compression, Defaults to 0.",
        "N",
    );
    opts.optopt(
        "",
        "encrypt-threads",
        "Number of processor threads to use for encryption, Defaults to 0.",
        "N",
    );

    let matches = parse_cli_opts(opts, &args[..]);

    let compression_scheme = {
        let scheme = matches
            .opt_str("compression")
            .unwrap_or_else(|| "zstd:3".to_string());
        compression::parse_scheme(&scheme)?
    };

    let mut threads = HashMap::new();

    for kind in ["address-threads", "compress-threads", "encrypt-threads"] {
        let n_threads = matches
            .opt_str(kind)
            .as_deref()
            .map(|n| {
                n.parse::<usize>()
                    .map_err(|err| anyhow::format_err!("error parsing --{}: {}", kind, err))
            })
            .unwrap_or_else(|| Ok(0))?;

        threads.insert(kind, n_threads);
    }

    let do_compress = matches.opt_present("compress");
    let do_address = matches.opt_present("address");
    let do_encrypt = matches.opt_present("encrypt");
    let do_print = matches.opt_present("print");

    let (pk, _) = crypto::box_keypair();
    let psk = crypto::BoxPreSharedKey::new();
    let mut ectx = crypto::EncryptionContext::new(&pk, &psk);

    let hk = crypto::derive_hash_key(
        &crypto::PartialHashKey::new(),
        &crypto::PartialHashKey::new(),
    );

    let inf = std::io::stdin();
    let mut inf = inf.lock();

    let mut outf = std::io::stdout();
    {
        let mut outf = outf.lock();

        let chunker = chunker::RollsumChunker::new(
            crypto::GearHashKey::new().gear_tab(),
            put::CHUNK_MIN_SIZE,
            put::CHUNK_MAX_SIZE,
        );

        let chunks = put::ChunkIter::new(chunker, &mut inf);

        let chunks = chunks.map(|chunk| chunk.unwrap());

        let chunks = chunks.plmap(
            *threads.get("address-threads").unwrap(),
            move |mut chunk: Vec<u8>| {
                if do_address {
                    let address = crypto::keyed_content_address(&chunk, &hk);
                    // use address to ensure compiler can't eliminate it.
                    if address.bytes[0] == 0 {
                        chunk[0] = 0;
                    }
                }
                chunk
            },
        );

        let chunks = chunks.plmap(
            *threads.get("compress-threads").unwrap(),
            move |mut chunk: Vec<u8>| {
                if do_compress {
                    chunk = compression::compress(compression_scheme, chunk);
                }
                chunk
            },
        );

        let chunks = chunks.plmap(
            *threads.get("encrypt-threads").unwrap(),
            move |mut chunk: Vec<u8>| {
                if do_encrypt {
                    chunk = ectx.encrypt_data(chunk);
                }
                chunk
            },
        );

        for chunk in chunks {
            if do_print {
                outf.write_all(&chunk)?;
            }
        }
    }

    outf.flush()?;

    Ok(())
}

fn rollsum_benchmark_main(args: Vec<String>) -> Result<(), anyhow::Error> {
    let opts = default_cli_opts();
    let matches = parse_cli_opts(opts, &args[..]);

    if matches.free.len() != 1 {
        anyhow::bail!("expected an algorithm to run.");
    }

    let gear_tab = crypto::GearHashKey::new().gear_tab();

    let mut rs: Box<dyn rollsum::RollsumSplitter> = match matches.free[0].as_str() {
        "GearHasher" => Box::new(rollsum::GearHasher::new(gear_tab)),
        "InterleavedGearHasher<4>" => Box::new(rollsum::InterleavedGearHasher::<4>::new(gear_tab)),
        "InterleavedGearHasher<8>" => Box::new(rollsum::InterleavedGearHasher::<8>::new(gear_tab)),
        "InterleavedGearHasher<16>" => {
            Box::new(rollsum::InterleavedGearHasher::<16>::new(gear_tab))
        }
        #[cfg(feature = "simd-rollsum")]
        "SimdInterleavedGearHasher<4>" => {
            Box::new(rollsum::SimdInterleavedGearHasher::<4>::new(gear_tab))
        }
        #[cfg(feature = "simd-rollsum")]
        "SimdInterleavedGearHasher<8>" => {
            Box::new(rollsum::SimdInterleavedGearHasher::<8>::new(gear_tab))
        }
        #[cfg(feature = "simd-rollsum")]
        "SimdInterleavedGearHasher<16>" => {
            Box::new(rollsum::SimdInterleavedGearHasher::<16>::new(gear_tab))
        }
        _ => anyhow::bail!("expected a supported algorithm"),
    };

    let mut buf = vec![0; 1024 * 1024];

    let inf = std::io::stdin();
    let mut inf = inf.lock();

    loop {
        match inf.read(&mut buf)? {
            0 => break,
            n_read => {
                let mut buf = &buf[..n_read];
                while !buf.is_empty() {
                    match rs.roll_bytes(buf) {
                        Some(n) => buf = &buf[n..],
                        None => break,
                    }
                }
            }
        }
    }

    Ok(())
}

fn indexer_benchmark_main(args: Vec<String>) -> Result<(), anyhow::Error> {
    let mut opts = default_cli_opts();
    opts.optflag("", "one-file-system", "Don't traverse mount points.");
    opts.optflag("", "want-hash", "Want hash.");
    opts.optflag("", "sparseness", "Want spareness.");
    opts.optflag("", "xattrs", "Want xattrs.");
    opts.optflag("", "hash", "Want hash.");
    opts.optflag("", "ignore-permission-errors", "Ignore permission errors.");
    opts.optopt(
        "",
        "threads",
        "Number of processor threads to use for pipelined parallel file hashing and metadata reads. Defaults to 0.",
        "N",
    );
    let matches = parse_cli_opts(opts, &args[..]);

    let indexer_threads = matches
        .opt_str("threads")
        .as_deref()
        .map(|n| {
            n.parse::<usize>()
                .map_err(|err| anyhow::format_err!("error parsing --threads: {}", err))
        })
        .unwrap_or_else(|| Ok(0))?;

    let indexer_opts = indexer::FsIndexerOptions {
        exclusions: globset::GlobSetBuilder::new().build().unwrap(),
        exclusion_markers: std::collections::HashSet::new(),
        one_file_system: matches.opt_present("one-file-system"),
        ignore_permission_errors: matches.opt_present("ignore-permission-errors"),
        want_hash: matches.opt_present("hash"),
        want_sparseness: matches.opt_present("sparseness"),
        want_xattrs: matches.opt_present("xattrs"),
        threads: indexer_threads,
        file_action_log_fn: None,
    };

    let dirs: Vec<PathBuf> = matches.free.iter().map(PathBuf::from).collect();

    let indexer = indexer::FsIndexer::new(&dirs, indexer_opts).unwrap();

    for ent in indexer {
        println!("{}", ent?.0.display());
    }

    Ok(())
}

fn restore_main(args: Vec<String>) -> Result<(), anyhow::Error> {
    let mut opts = default_cli_opts();
    repo_cli_opts(&mut opts);
    query_cli_opts(&mut opts);
    opts.optopt("k", "key", "Key to decrypt data with.", "PATH");
    opts.optopt(
        "",
        "pick",
        "Pick a sub-directory of the snapshot to restore.",
        "PATH",
    );
    opts.optflag("", "ownership", "Set uids and gids.");
    opts.optflag("", "xattrs", "Set xattrs.");
    opts.optopt(
        "",
        "into",
        "Directory to restore files into, defaults to BUPSTASH_RESTORE_DIR.",
        "PATH",
    );
    opts.optopt(
        "",
        "indexer-threads",
        "Number of processor threads to use for pipelined parallel file hashing and metadata reads. Defaults to the number of processors.",
        "N",
    );

    let matches = parse_cli_opts(opts, &args[..]);

    let indexer_threads = matches
        .opt_str("indexer-threads")
        .as_deref()
        .map(|n| {
            n.parse::<usize>()
                .map_err(|err| anyhow::format_err!("error parsing --indexer-threads: {}", err))
        })
        .unwrap_or_else(|| Ok(num_cpus::get_physical()))?;

    let key = cli_to_key(&matches)?;
    let primary_key_id = key.primary_key_id();
    let (idx_hash_key_part_1, data_hash_key_part_1, data_dctx, metadata_dctx, idx_dctx) = match key
    {
        keys::Key::PrimaryKeyV1(k) => {
            let idx_hash_key_part_1 = k.idx_hash_key_part_1.clone();
            let data_hash_key_part_1 = k.data_hash_key_part_1.clone();
            let data_dctx = crypto::DecryptionContext::new(k.data_sk, k.data_psk.clone());
            let metadata_dctx = crypto::DecryptionContext::new(k.metadata_sk, k.metadata_psk);
            let idx_dctx = crypto::DecryptionContext::new(k.idx_sk, k.idx_psk);
            (
                idx_hash_key_part_1,
                data_hash_key_part_1,
                data_dctx,
                metadata_dctx,
                idx_dctx,
            )
        }
        _ => anyhow::bail!("provided key is not a data decryption key"),
    };

    let progress = cli_to_progress_bar(
        &matches,
        indicatif::ProgressStyle::default_spinner().template("[{elapsed_precise}] {wide_msg}"),
    );

    let into_dir: PathBuf = if let Some(into) = matches.opt_str("into") {
        into.into()
    } else if let Some(into) = std::env::var_os("BUPSTASH_RESTORE_DIR") {
        into.into()
    } else {
        anyhow::bail!("please set --into or BUPSTASH_RESTORE_DIR to the restore target directory.")
    };

    let into_dir = fsutil::absolute_path(&into_dir)?;

    if !into_dir.exists() {
        anyhow::bail!("{} does not exist", into_dir.display())
    }

    if !into_dir.is_dir() {
        anyhow::bail!("{} is not a directory", into_dir.display())
    }

    let (item_id, query) = cli_to_id_and_query(&matches)?;
    let mut serve_proc = cli_to_opened_serve_process(
        &matches,
        &progress,
        ServeProcessCliOpts::default(),
        protocol::OpenMode::Read,
    )?;
    let mut serve_out = serve_proc.proc.stdout.as_mut().unwrap();
    let mut serve_in = serve_proc.proc.stdin.as_mut().unwrap();

    let item_id = match (item_id, query) {
        (Some(item_id), _) => item_id,
        (_, query) => {
            let mut query_cache = cli_to_query_cache(&matches)?;

            // Only sync the client if we have a non id query.
            client::sync_query_cache(
                progress.clone(),
                &mut query_cache,
                &mut serve_out,
                &mut serve_in,
            )?;

            let mut n_matches: u64 = 0;
            let mut item_id = xid::Xid::default();

            let mut on_match =
                |id: xid::Xid,
                 _tags: &std::collections::BTreeMap<String, String>,
                 _metadata: &oplog::VersionedItemMetadata,
                 _secret_metadata: Option<&oplog::DecryptedItemMetadata>| {
                    n_matches += 1;
                    item_id = id;

                    if n_matches > 1 {
                        anyhow::bail!(
                            "the provided query matched {} items, need a single match",
                            n_matches
                        );
                    }

                    Ok(())
                };

            let mut tx = query_cache.transaction()?;
            tx.list(
                querycache::ListOptions {
                    primary_key_id: Some(primary_key_id),
                    metadata_dctx: Some(metadata_dctx.clone()),
                    list_encrypted: matches.opt_present("query-encrypted"),
                    utc_timestamps: matches.opt_present("utc-timestamps"),
                    query: Some(query),
                    now: chrono::Utc::now(),
                },
                &mut on_match,
            )?;

            item_id
        }
    };

    progress.set_message("fetching item index...");
    let metadata = client::request_metadata(item_id, &mut serve_out, &mut serve_in)?;

    let content_index = if metadata.index_tree().is_some() {
        client::request_index(
            client::IndexRequestContext {
                primary_key_id,
                idx_hash_key_part_1,
                idx_dctx,
                metadata_dctx: metadata_dctx.clone(),
            },
            item_id,
            &metadata,
            &mut serve_out,
            &mut serve_in,
        )?
    } else {
        anyhow::bail!("restore is only supported for directory snapshots created by bupstash");
    };

    client::restore_to_local_dir(
        &progress,
        client::RestoreContext {
            item_id,
            metadata,
            data_ctx: client::DataRequestContext {
                primary_key_id,
                data_hash_key_part_1,
                data_dctx,
                metadata_dctx,
            },
            restore_ownership: matches.opt_present("ownership"),
            restore_xattrs: matches.opt_present("xattrs"),
            indexer_threads,
        },
        content_index,
        matches.opt_str("pick").map(|x| x.into()),
        &mut serve_out,
        &mut serve_in,
        &into_dir,
    )?;

    client::hangup(&mut serve_in)?;
    serve_proc.wait()?;
    progress.finish_and_clear();

    Ok(())
}

fn serve_main(args: Vec<String>) -> Result<(), anyhow::Error> {
    let mut opts = default_cli_opts();

    opts.optflag(
        "",
        "allow-init",
        "Allow the client to initialize new repositories.",
    );
    opts.optflag(
        "",
        "allow-put",
        "Allow client to put more entries into the repository.",
    );
    opts.optflag(
        "",
        "allow-remove",
        "Allow client to remove repository items, implies --allow-list.",
    );
    opts.optflag(
        "",
        "allow-gc",
        "Allow client to run the repository garbage collector.",
    );
    opts.optflag(
        "",
        "allow-get",
        "Allow client to retrieve data from the repository, implies --allow-list.",
    );
    opts.optflag(
        "",
        "allow-list",
        "Allow client to list items and file lists.",
    );
    opts.optflag(
        "",
        "allow-sync",
        "Allow client to sync items into the repository, i.e. be the destination of a repository sync.",
    );

    let matches = parse_cli_opts(opts, &args[..]);

    if matches.free.len() != 1 {
        die("Expected a single repository path to serve.".to_string());
    }

    let mut allow_init = true;
    let mut allow_put = true;
    let mut allow_remove = true;
    let mut allow_gc = true;
    let mut allow_get = true;
    let mut allow_list = true;
    let mut allow_sync = true;

    if matches.opt_present("allow-init")
        || matches.opt_present("allow-put")
        || matches.opt_present("allow-remove")
        || matches.opt_present("allow-gc")
        || matches.opt_present("allow-get")
        || matches.opt_present("allow-list")
        || matches.opt_present("allow-sync")
    {
        allow_init = matches.opt_present("allow-init");
        allow_remove = matches.opt_present("allow-remove");
        allow_gc = matches.opt_present("allow-gc");
        allow_get = matches.opt_present("allow-get");
        allow_sync = matches.opt_present("allow-sync");
        allow_put = matches.opt_present("allow-put");
        // --allow-get and --allow-remove implies --allow-list because they
        // are essentially useless without being able to list items.
        allow_list = allow_get || allow_remove || matches.opt_present("allow-list");
    }

    if atty::is(atty::Stream::Stdout) {
        let _ = writeln!(
            std::io::stderr(),
            "'bupstash serve' running on stdin/stdout..."
        );
    }

    // rust line buffers stdin and stdout unconditionally, so bypass it.
    let mut stdin_unbuffered = unsafe { std::fs::File::from_raw_fd(libc::STDIN_FILENO) };
    let mut stdout_unbuffered = unsafe { std::fs::File::from_raw_fd(libc::STDOUT_FILENO) };

    // preserve result until we have disposed of our stdin/stdout files.
    let result = server::serve(
        server::ServerConfig {
            allow_init,
            allow_put,
            allow_remove,
            allow_gc,
            allow_get,
            allow_list,
            allow_sync,
            repo_path: Path::new(&matches.free[0]).to_path_buf(),
        },
        &mut stdin_unbuffered,
        &mut stdout_unbuffered,
    );

    // Prevent stdin/stdout form being closed.
    stdin_unbuffered.into_raw_fd();
    stdout_unbuffered.into_raw_fd();

    result
}

fn exec_with_locks_main(args: Vec<String>) -> Result<(), anyhow::Error> {
    let mut opts = default_cli_opts();
    opts.optopt(
        "r",
        "repository",
        "Repository to lock. \
         Defaults to BUPSTASH_REPOSITORY if not set. \
         Unlike other commands, does not support remote repository access.",
        "REPO",
    );

    let matches = parse_cli_opts(opts, &args[..]);

    let repo = match matches.opt_str("repository") {
        Some(repo) => repo,
        None => match std::env::var("BUPSTASH_REPOSITORY") {
            Ok(repo) => repo,
            Err(_) => anyhow::bail!("you must specify either -r or BUPSTASH_REPOSITORY"),
        },
    };

    if !repo.starts_with("./") && !repo.starts_with("../") && !repo.starts_with('/') {
        anyhow::bail!("exec-with-locks does not support remote or uri repositories.");
    }

    if matches.free.is_empty() {
        die("Expected a command to exec.".to_string());
    }

    let mut fstx_lock_path = PathBuf::from(repo.clone());
    fstx_lock_path.push("tx.lock");
    let mut repo_lock_path = PathBuf::from(repo);
    repo_lock_path.push("repo.lock");

    // We open files with unsafe so we can disable CLOEXEC, in this
    // case we explicitly want our child to inherit these locks.
    let fstx_lock_file = unsafe {
        std::fs::File::from_raw_fd(nix::fcntl::open(
            &fstx_lock_path,
            nix::fcntl::OFlag::O_RDWR,
            nix::sys::stat::Mode::empty(),
        )?)
    };
    let repo_lock_file = unsafe {
        std::fs::File::from_raw_fd(nix::fcntl::open(
            &repo_lock_path,
            nix::fcntl::OFlag::O_RDWR,
            nix::sys::stat::Mode::empty(),
        )?)
    };

    let lock_opts = libc::flock {
        l_type: libc::F_WRLCK as libc::c_short,
        l_whence: libc::SEEK_SET as libc::c_short,
        l_start: 0,
        l_len: 0,
        l_pid: 0,
        #[cfg(target_os = "freebsd")]
        l_sysid: 0,
    };

    nix::fcntl::fcntl(
        fstx_lock_file.as_raw_fd(),
        nix::fcntl::FcntlArg::F_SETLKW(&lock_opts),
    )?;
    nix::fcntl::fcntl(
        repo_lock_file.as_raw_fd(),
        nix::fcntl::FcntlArg::F_SETLKW(&lock_opts),
    )?;

    let bin = std::ffi::CString::new(matches.free[0].clone()).unwrap();
    let args: Vec<std::ffi::CString> = matches
        .free
        .into_iter()
        .map(|a| std::ffi::CString::new(a).unwrap())
        .collect();

    nix::unistd::execvp(&bin, &args)?;

    // Ensure these are alive until after exec call.
    std::mem::drop(fstx_lock_file);
    std::mem::drop(repo_lock_file);
    anyhow::bail!("exec failed");
}

fn main() {
    crypto::init();
    cksumvfs::register_cksumvfs();

    let mut args: Vec<String> = std::env::args().collect();
    let program = args[0].clone();
    args.remove(0);
    if args.is_empty() {
        die(format!(
            "Expected at least a single subcommand, try '{} help'.",
            program
        ))
    }
    let subcommand = args[0].clone();

    let result = match subcommand.as_str() {
        "init" => init_main(args),
        "new-key" => new_key_main(args),
        "new-sub-key" => new_sub_key_main(args),
        "list" => list_main(args),
        "list-contents" => list_contents_main(args),
        "diff" => diff_main(args),
        "put" => put_main(args),
        "get" => get_main(args),
        "restore" => restore_main(args),
        "gc" => gc_main(args),
        "remove" | "rm" => remove_main(args),
        "serve" => serve_main(args),
        "recover-removed" => recover_removed_main(args),
        "put-benchmark" => put_benchmark_main(args),
        "rollsum-benchmark" => rollsum_benchmark_main(args),
        "indexer-benchmark" => indexer_benchmark_main(args),
        "sync" => sync_main(args),
        "exec-with-locks" => exec_with_locks_main(args),
        "version" | "--version" => {
            args[0] = "version".to_string();
            version_main(args)
        }
        "help" | "--help" | "-h" => {
            args[0] = "help".to_string();
            help_main(args);
            Ok(())
        }
        _ => die(format!(
            "Unknown subcommand '{}', try  '{} help'.",
            subcommand, program
        )),
    };

    if let Err(err) = result {
        // Support unix style pipelines, don't print an error on EPIPE.
        match err.root_cause().downcast_ref::<std::io::Error>() {
            Some(io_error) if io_error.kind() == std::io::ErrorKind::BrokenPipe => {
                std::process::exit(1)
            }
            _ => die(format!("bupstash {}: {}", subcommand, err)),
        }
    }
}
