extern crate futures;
extern crate raft_consensus;
extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate toml;

#[macro_use]
extern crate clap;

#[macro_use]
extern crate slog;
extern crate tokio;

extern crate slog_async;
extern crate slog_term;

extern crate net2;
extern crate raft_tokio;

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use clap::Arg;
use slog::{Drain, Level, Logger};

use tokio::prelude::future::*;
use tokio::runtime::current_thread::Runtime;

use raft_consensus::persistent_log::mem::MemLog;
use raft_consensus::state::ConsensusState;
use raft_consensus::state_machine::null::NullStateMachine;
use raft_consensus::ServerId;

use raft::Notifier;
use raft_tokio::raft::{BiggerIdSolver, ConnectionSolver};
use raft_tokio::start_raft_tcp;
use raft_tokio::{Notifier, RaftOptions};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
struct NodeConfig {
    listen: SocketAddr,
    id: ServerId,
}
impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:0".parse().unwrap(),
            id: 0.into(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
struct Config {
    verbosity: String,
    node: HashMap<String, NodeConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            verbosity: "trace".to_string(),
            node: HashMap::new(),
        }
    }
}

#[derive(Clone)]
struct LeaderSave(Arc<Mutex<Option<bool>>>, Logger);

impl Notifier for LeaderSave {
    fn state_changed(&mut self, old: ConsensusState, new: ConsensusState) {
        let mut is_leader = self.0.lock().unwrap();
        if old != new {
            if new == ConsensusState::Leader {
                warn!(self.1, "leader now") * is_leader = Some(true);
            } else if old == ConsensusState::Leader {
                warn!(self.1, "lost leader") * is_leader = Some(false);
            }
        }
    }
}

impl ConnectionSolver for LeaderSave {
    fn solve(&self, is_client: bool, local_id: ServerId, remote_id: ServerId) -> bool {
        if let Some(is_leader) = self.0.lock().unwrap() {
            trace!(self.1, "leader is defined"; "is_leader" => is_leader);
            return is_leader;
        } else {
            trace!(self.1, "leader is undefined");
            return BiggerIdSolver.solve(is_client, local_id, remote_id);
        }
    }
}

fn main() {
    let app = app_from_crate!()
        .arg(
            Arg::with_name("config")
                .help("configuration file path")
                .long("config")
                .short("c")
                .required(true)
                .takes_value(true)
                .default_value("config.toml"),
        ).arg(
            Arg::with_name("verbosity")
                .short("v")
                .help("logging level")
                .default_value("warn")
                .takes_value(true),
        ).arg(
            Arg::with_name("id")
                .index(1)
                .required(true)
                .help("id of current node (must exist in config)"),
        ).get_matches();

    let config = value_t!(app.value_of("config"), String).expect("config file must be string");
    let id = value_t!(app.value_of("id"), String).expect("ID must be string");

    let verbosity = value_t!(app.value_of("verbosity"), Level).expect("bad verbosity");
    //let verbosity = Level::from_str(&verbosity).expect("bad verbosity");

    let mut file = File::open(&config).expect(&format!("opening config file at {}", &config));
    let mut config_str = String::new();
    file.read_to_string(&mut config_str)
        .expect("reading config file");
    let mut system: Config = toml::de::from_str(&config_str).expect("parsing config");

    if let Some(v) = app.value_of("verbosity") {
        system.verbosity = v.into()
    }
    //nodes.insert(4.into(), "127.0.0.1:9994".parse().unwrap());

    println!("{:?}", system);
    let this = system
        .node
        .get(&id)
        .expect("ID must exist in config")
        .clone();

    let nodes: HashMap<ServerId, SocketAddr> = system
        .node
        .into_iter()
        .map(|(_, spec)| (spec.id, spec.listen))
        .collect();

    let id = this.id;
    // Set logging
    let decorator = slog_term::TermDecorator::new().build();
    let drain = slog_term::FullFormat::new(decorator).build().fuse();
    let filter = slog::LevelFilter::new(drain, verbosity).fuse();
    let drain = slog_async::Async::new(filter).build().fuse();
    let rlog = slog::Logger::root(drain, o!("program"=>"test"));

    let log = rlog.new(o!("id" => format!("{:?}", id), "local_addr" => this.listen.to_string()));
    // prepare logger
    let log = log.clone();

    // prepare consensus
    let raft_log = MemLog::new();
    let sm = NullStateMachine;
    //let notifier = LeaderNotifier(log.clone());
    let options = RaftOptions::default();

    let is_leader: Arc<Mutex<Option<bool>>> = Arc::new(Mutex::new(None));
    let notifier = LeaderSave(is_leader.clone(), log.clone());
    let solver = notifier.clone();
    // Create the runtime
    let mut runtime = Runtime::new().expect("creating runtime");

    let this_listen = this.listen.clone();
    let raft = lazy(move || {
        if id == ServerId(1) {
            start_raft_tcp(
                id,
                nodes,
                raft_log,
                sm,
                notifier,
                options,
                log,
                move |socket| {
                    use net2::TcpBuilder;
                    use std::os::unix::io::{AsRawFd, FromRawFd};
                    let builder = unsafe { TcpBuilder::from_raw_fd(socket.as_raw_fd()) };
                    builder.bind(this_listen)?;
                    *socket = builder.to_tcp_stream()?; // ensure the ownership is passed back from builder
                    Ok(())
                },
            );
        } else {
            start_raft_tcp(id, nodes, raft_log, sm, notifier, options, log, |_| Ok(()));
        }
        Ok::<(), ()>(())
    });

    runtime.spawn(raft);
    runtime.block_on(empty::<(), ()>()).expect("runtime");
}
