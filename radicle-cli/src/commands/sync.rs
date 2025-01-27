use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::ops::ControlFlow;
use std::str::FromStr;
use std::time;

use anyhow::{anyhow, Context as _};

use radicle::node;
use radicle::node::AliasStore;
use radicle::node::Seed;
use radicle::node::{FetchResult, FetchResults, Handle as _, Node, SyncStatus};
use radicle::prelude::{NodeId, Profile, RepoId};
use radicle::storage::{ReadRepository, ReadStorage};
use radicle_term::Element;

use crate::terminal as term;
use crate::terminal::args::{Args, Error, Help};
use crate::terminal::format::Author;
use crate::terminal::{Table, TableOptions};

pub const HELP: Help = Help {
    name: "sync",
    description: "Sync repositories to the network",
    version: env!("CARGO_PKG_VERSION"),
    usage: r#"
Usage

    rad sync [--fetch | --announce] [<rid>] [<option>...]
    rad sync --inventory [<option>...]
    rad sync status [<rid>] [<option>...]

    By default, the current repository is synchronized both ways.
    If an <rid> is specified, that repository is synced instead.

    The process begins by fetching changes from connected seeds,
    followed by announcing local refs to peers, thereby prompting
    them to fetch from us.

    When `--fetch` is specified, any number of seeds may be given
    using the `--seed` option, eg. `--seed <nid>@<addr>:<port>`.

    When `--replicas` is specified, the given replication factor will try
    to be matched. For example, `--replicas 5` will sync with 5 seeds.

    When `--fetch` or `--announce` are specified on their own, this command
    will only fetch or announce.

    If `--inventory` is specified, the node's inventory is announced to
    the network. This mode does not take an `<rid>`.

Commands

    status                    Display the sync status of a repository

Options

        --sort-by   <field>   Sort the table by column (options: nid, alias, status)
    -f, --fetch               Turn on fetching (default: true)
    -a, --announce            Turn on ref announcing (default: true)
    -i, --inventory           Turn on inventory announcing (default: false)
        --timeout   <secs>    How many seconds to wait while syncing
        --seed      <nid>     Sync with the given node (may be specified multiple times)
    -r, --replicas  <count>   Sync with a specific number of seeds
    -v, --verbose             Verbose output
        --help                Print help
"#,
};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Operation {
    Synchronize(SyncMode),
    #[default]
    Status,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortBy {
    Nid,
    Alias,
    #[default]
    Status,
}

impl FromStr for SortBy {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "nid" => Ok(Self::Nid),
            "alias" => Ok(Self::Alias),
            "status" => Ok(Self::Status),
            _ => Err("invalid `--sort-by` field"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncMode {
    Repo {
        settings: RepoSync,
        direction: SyncDirection,
    },
    Inventory,
}

impl Default for SyncMode {
    fn default() -> Self {
        Self::Repo {
            settings: RepoSync::default(),
            direction: SyncDirection::default(),
        }
    }
}

/// Repository sync settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoSync {
    /// Sync with at least N replicas.
    pub replicas: usize,
    /// Sync with the given list of seeds.
    pub seeds: BTreeSet<NodeId>,
}

impl RepoSync {
    pub fn from_seeds(seeds: impl IntoIterator<Item = NodeId>) -> Self {
        let seeds = BTreeSet::from_iter(seeds);
        Self {
            replicas: seeds.len(),
            seeds,
        }
    }

    /// Use profile to populate sync settings, by adding preferred seeds if no seeds are specified,
    /// and removing the local node from the set.
    pub fn with_profile(mut self, profile: &Profile) -> Self {
        // If no seeds were specified, add up to `replica` seeds from the preferred seeds.
        if self.seeds.is_empty() {
            self.seeds = profile
                .config
                .preferred_seeds
                .iter()
                .map(|p| p.id)
                .take(self.replicas)
                .collect();
        }
        // Remove our local node from the seed set just in case it was added by mistake.
        self.seeds.remove(profile.id());
        self
    }
}

impl Default for RepoSync {
    fn default() -> Self {
        Self {
            replicas: 3,
            seeds: BTreeSet::new(),
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq, Clone)]
pub enum SyncDirection {
    Fetch,
    Announce,
    #[default]
    Both,
}

#[derive(Default, Debug)]
pub struct Options {
    pub rid: Option<RepoId>,
    pub verbose: bool,
    pub timeout: time::Duration,
    pub sort_by: SortBy,
    pub op: Operation,
}

impl Args for Options {
    fn from_args(args: Vec<OsString>) -> anyhow::Result<(Self, Vec<OsString>)> {
        use lexopt::prelude::*;

        let mut parser = lexopt::Parser::from_args(args);
        let mut verbose = false;
        let mut timeout = time::Duration::from_secs(9);
        let mut rid = None;
        let mut fetch = false;
        let mut announce = false;
        let mut inventory = false;
        let mut replicas = None;
        let mut seeds = BTreeSet::new();
        let mut sort_by = SortBy::default();
        let mut op: Option<Operation> = None;

        while let Some(arg) = parser.next()? {
            match arg {
                Long("verbose") | Short('v') => {
                    verbose = true;
                }
                Long("fetch") | Short('f') => {
                    fetch = true;
                }
                Long("replicas") | Short('r') => {
                    let val = parser.value()?;
                    let count = term::args::number(&val)?;

                    if count == 0 {
                        anyhow::bail!("value for `--replicas` must be greater than zero");
                    }
                    replicas = Some(count);
                }
                Long("seed") => {
                    let val = parser.value()?;
                    let nid = term::args::nid(&val)?;

                    seeds.insert(nid);
                }
                Long("announce") | Short('a') => {
                    announce = true;
                }
                Long("inventory") | Short('i') => {
                    inventory = true;
                }
                Long("sort-by") if matches!(op, Some(Operation::Status)) => {
                    let value = parser.value()?;
                    sort_by = value.parse()?;
                }
                Long("timeout") | Short('t') => {
                    let value = parser.value()?;
                    let secs = term::args::parse_value("timeout", value)?;

                    timeout = time::Duration::from_secs(secs);
                }
                Long("help") | Short('h') => {
                    return Err(Error::Help.into());
                }
                Value(val) if rid.is_none() => match val.to_string_lossy().as_ref() {
                    "s" | "status" => {
                        op = Some(Operation::Status);
                    }
                    _ => {
                        rid = Some(term::args::rid(&val)?);
                    }
                },
                arg => {
                    return Err(anyhow!(arg.unexpected()));
                }
            }
        }

        let sync = if inventory && (fetch || announce) {
            anyhow::bail!("`--inventory` cannot be used with `--fetch` or `--announce`");
        } else if inventory {
            SyncMode::Inventory
        } else {
            let direction = match (fetch, announce) {
                (true, true) | (false, false) => SyncDirection::Both,
                (true, false) => SyncDirection::Fetch,
                (false, true) => SyncDirection::Announce,
            };
            let settings = if seeds.is_empty() {
                RepoSync {
                    replicas: replicas.unwrap_or(3),
                    seeds,
                }
            } else {
                RepoSync {
                    replicas: replicas.unwrap_or(seeds.len()),
                    seeds,
                }
            };
            SyncMode::Repo {
                settings,
                direction,
            }
        };

        Ok((
            Options {
                rid,
                verbose,
                timeout,
                sort_by,
                op: op.unwrap_or(Operation::Synchronize(sync)),
            },
            vec![],
        ))
    }
}

pub fn run(options: Options, ctx: impl term::Context) -> anyhow::Result<()> {
    let profile = ctx.profile()?;
    let rid = match options.rid {
        Some(rid) => rid,
        None => {
            let (_, rid) =
                radicle::rad::cwd().context("Current directory is not a Radicle repository")?;

            rid
        }
    };
    let mut node = radicle::Node::new(profile.socket());
    if !node.is_running() {
        anyhow::bail!(
            "to sync a repository, your node must be running. To start it, run `rad node start`"
        );
    }

    match options.op {
        Operation::Status => {
            sync_status(rid, &mut node, &profile, &options)?;
        }
        Operation::Synchronize(SyncMode::Repo {
            settings,
            direction,
        }) => {
            let settings = settings.with_profile(&profile);

            if [SyncDirection::Fetch, SyncDirection::Both].contains(&direction) {
                if !profile.policies()?.is_seeding(&rid)? {
                    anyhow::bail!("repository {rid} is not seeded");
                }
                let results = fetch(rid, settings.clone(), options.timeout, &mut node)?;
                let success = results.success().count();
                let failed = results.failed().count();

                if results.is_empty() {
                    term::error(format!("no seeds found for {rid}"));
                } else if success == 0 {
                    term::error(format!("repository fetch from {failed} seed(s) failed"));
                } else {
                    term::success!("Fetched repository from {success} seed(s)");
                }
            }
            if [SyncDirection::Announce, SyncDirection::Both].contains(&direction) {
                announce_refs(rid, settings, options.timeout, &mut node, &profile)?;
            }
        }
        Operation::Synchronize(SyncMode::Inventory) => {
            announce_inventory(node)?;
        }
    }
    Ok(())
}

fn sync_status(
    rid: RepoId,
    node: &mut Node,
    profile: &Profile,
    options: &Options,
) -> anyhow::Result<()> {
    let mut table = Table::<7, term::Label>::new(TableOptions::bordered());
    let mut seeds: Vec<_> = node.seeds(rid)?.into();
    let local = node.nid()?;
    let aliases = profile.aliases();

    table.push([
        term::format::dim(String::from("●")).into(),
        term::format::bold(String::from("Node")).into(),
        term::Label::blank(),
        term::format::bold(String::from("Address")).into(),
        term::format::bold(String::from("Status")).into(),
        term::format::bold(String::from("Tip")).into(),
        term::format::bold(String::from("Timestamp")).into(),
    ]);
    table.divider();

    sort_seeds_by(local, &mut seeds, &aliases, &options.sort_by);

    for seed in seeds {
        let (icon, status, head, time) = match seed.sync {
            Some(SyncStatus::Synced { at }) => (
                term::format::positive("●"),
                term::format::positive(if seed.nid != local { "synced" } else { "" }),
                term::format::oid(at.oid),
                term::format::timestamp(at.timestamp),
            ),
            Some(SyncStatus::OutOfSync { remote, .. }) => (
                term::format::negative("●"),
                term::format::negative(if seed.nid != local { "out-of-sync" } else { "" }),
                term::format::oid(remote.oid),
                term::format::timestamp(remote.timestamp),
            ),
            None if options.verbose => (
                term::format::dim("●"),
                term::format::dim("unknown"),
                term::paint(String::new()),
                term::paint(String::new()),
            ),
            None => continue,
        };
        let addr = seed
            .addrs
            .first()
            .map(|a| a.addr.to_string())
            .unwrap_or_default()
            .into();
        let (alias, nid) = Author::new(&seed.nid, profile).labels();

        table.push([
            icon.into(),
            alias,
            nid,
            addr,
            status.into(),
            term::format::secondary(head).into(),
            time.dim().italic().into(),
        ]);
    }
    table.print();

    Ok(())
}

fn announce_refs(
    rid: RepoId,
    settings: RepoSync,
    timeout: time::Duration,
    node: &mut Node,
    profile: &Profile,
) -> anyhow::Result<()> {
    let Ok(repo) = profile.storage.repository(rid) else {
        return Err(anyhow!(
            "nothing to announce, repository {rid} is not available locally"
        ));
    };
    let doc = repo.identity_doc()?;
    let unsynced: Vec<_> = if doc.visibility.is_public() {
        // All seeds.
        let all = node.seeds(rid)?;
        // Seeds in sync with us.
        let synced = all.iter().filter(|s| s.is_synced());
        // Replicas not counting our local replica.
        let replicas = all
            .iter()
            .filter(|s| s.is_synced() && &s.nid != profile.id())
            .count();
        // Maximum replication factor we can achieve.
        let max_replicas = all.iter().filter(|s| &s.nid != profile.id()).count();
        // If the seeds we specified in the sync settings are all synced.
        let is_seeds_synced = {
            let synced = synced.map(|s| s.nid).collect::<BTreeSet<_>>();
            settings.seeds.iter().all(|s| synced.contains(s))
        };
        // If we met our desired replica count. Note that this can never exceed the maximum count.
        let is_replicas_synced = replicas >= settings.replicas.min(max_replicas);

        // Nothing to do if we've met our sync state.
        if is_seeds_synced && is_replicas_synced {
            term::success!("Nothing to announce, already in sync with {replicas} node(s) (see `rad sync status`)");
            return Ok(());
        }
        // Return nodes we can announce to.
        all.connected()
            .filter(|s| !s.is_synced())
            .map(|s| s.nid)
            .collect()
    } else {
        node.sessions()?
            .into_iter()
            .filter(|s| s.state.is_connected() && doc.is_visible_to(&s.nid))
            .map(|s| s.nid)
            .collect()
    };

    if unsynced.is_empty() {
        term::info!("Not connected to any seeds for {rid}.");
        return Ok(());
    }

    let mut spinner = term::spinner(format!("Syncing with {} node(s)..", unsynced.len()));
    let result = node.announce(rid, unsynced, timeout, |event, replicas| match event {
        node::AnnounceEvent::Announced => ControlFlow::Continue(()),
        node::AnnounceEvent::RefsSynced { remote } => {
            spinner.message(format!("Synced with {remote}.."));

            // We're done syncing when both of these conditions are met:
            //
            // 1. We've matched or exceeded our target replica count.
            // 2. We've synced with the seeds specified manually.
            if replicas.len() >= settings.replicas
                && settings.seeds.iter().all(|s| replicas.contains(s))
            {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        }
    })?;

    if result.synced.is_empty() {
        spinner.failed();
    } else {
        spinner.message(format!("Synced with {} node(s)", result.synced.len()));
        spinner.finish();
    }
    for seed in result.timeout {
        term::notice!("Seed {seed} timed out..");
    }
    if result.synced.is_empty() {
        anyhow::bail!("all seeds timed out");
    }
    Ok(())
}

pub fn announce_inventory(mut node: Node) -> anyhow::Result<()> {
    let peers = node.sessions()?.iter().filter(|s| s.is_connected()).count();
    let spinner = term::spinner(format!("Announcing inventory to {peers} peers.."));

    node.sync_inventory()?;
    node.announce_inventory()?;
    spinner.finish();

    Ok(())
}

pub fn fetch(
    rid: RepoId,
    settings: RepoSync,
    timeout: time::Duration,
    node: &mut Node,
) -> Result<FetchResults, node::Error> {
    let local = node.nid()?;
    // Get seeds. This consults the local routing table only.
    let seeds = node.seeds(rid)?;
    // Target replicas, clamped by the maximum replicas possible.
    let replicas = settings
        .replicas
        .min(seeds.iter().filter(|s| s.nid != local).count());
    let sessions = node.sessions()?;
    let mut results = FetchResults::default();
    let (connected, mut disconnected) = seeds.partition();

    // Fetch from specified seeds, plus our preferred seeds.
    for nid in &settings.seeds {
        if !sessions.iter().any(|s| &s.nid == nid) {
            term::warning(format!("node {nid} is not connected.. skipping"));
            continue;
        }
        let result = fetch_from(rid, nid, timeout, node)?;
        results.push(*nid, result);
    }
    if results.success().count() >= replicas {
        return Ok(results);
    }

    // Fetch from connected seeds.
    let connected = connected
        .into_iter()
        .filter(|c| !results.contains(&c.nid))
        .map(|c| c.nid)
        .take(replicas)
        .collect::<Vec<_>>();
    for nid in connected {
        let result = fetch_from(rid, &nid, timeout, node)?;
        results.push(nid, result);
    }

    // Try to connect to disconnected seeds and fetch from them.
    while results.success().count() < replicas {
        let Some(seed) = disconnected.pop() else {
            break;
        };
        if seed.nid == local {
            // Skip our own node.
            continue;
        }
        if connect(
            seed.nid,
            seed.addrs.into_iter().map(|ka| ka.addr),
            timeout,
            node,
        )? {
            let result = fetch_from(rid, &seed.nid, timeout, node)?;
            results.push(seed.nid, result);
        }
    }

    Ok(results)
}

fn connect(
    nid: NodeId,
    addrs: impl Iterator<Item = node::Address>,
    timeout: time::Duration,
    node: &mut Node,
) -> Result<bool, node::Error> {
    // Try all addresses until one succeeds.
    for addr in addrs {
        let spinner = term::spinner(format!(
            "Connecting to {}@{}..",
            term::format::tertiary(term::format::node(&nid)),
            &addr
        ));
        let cr = node.connect(
            nid,
            addr,
            node::ConnectOptions {
                persistent: false,
                timeout,
            },
        )?;

        match cr {
            node::ConnectResult::Connected => {
                spinner.finish();
                return Ok(true);
            }
            node::ConnectResult::Disconnected { .. } => {
                spinner.failed();
                continue;
            }
        }
    }
    Ok(false)
}

fn fetch_from(
    rid: RepoId,
    seed: &NodeId,
    timeout: time::Duration,
    node: &mut Node,
) -> Result<FetchResult, node::Error> {
    let spinner = term::spinner(format!(
        "Fetching {} from {}..",
        term::format::tertiary(rid),
        term::format::tertiary(term::format::node(seed))
    ));
    let result = node.fetch(rid, *seed, timeout)?;

    match &result {
        FetchResult::Success { .. } => {
            spinner.finish();
        }
        FetchResult::Failed { reason } => {
            spinner.error(reason);
        }
    }
    Ok(result)
}

fn sort_seeds_by(local: NodeId, seeds: &mut [Seed], aliases: &impl AliasStore, sort_by: &SortBy) {
    let compare = |a: &Seed, b: &Seed| match sort_by {
        SortBy::Nid => a.nid.cmp(&b.nid),
        SortBy::Alias => {
            let a = aliases.alias(&a.nid);
            let b = aliases.alias(&b.nid);
            a.cmp(&b)
        }
        SortBy::Status => match (&a.sync, &b.sync) {
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (Some(a), Some(b)) => a.cmp(b).reverse(),
            (None, None) => Ordering::Equal,
        },
    };

    // Always show our local node first.
    seeds.sort_by(|a, b| {
        if a.nid == local {
            Ordering::Less
        } else if b.nid == local {
            Ordering::Greater
        } else {
            compare(a, b)
        }
    });
}
