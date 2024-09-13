//! CLI definition and entrypoint to executable

use clap::{value_parser, Parser};
use reth::{
    args::LogArgs,
    cli::Commands,
    version::{LONG_VERSION, SHORT_VERSION},
};
use reth_chainspec::ChainSpec;
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_commands::node::NoArgs;
use reth_cli_runner::CliRunner;
use reth_db::DatabaseEnv;
use reth_node_builder::{NodeBuilder, WithLaunchContext};
use reth_node_core::args::utils::DefaultChainSpecParser;
use reth_node_ethereum::{EthExecutorProvider, EthereumNode};
use reth_tracing::FileWorkerGuard;
use rolling_file::{RollingConditionBasic, RollingFileAppender};
use std::{ffi::OsString, fmt, future::Future, path::Path, sync::Arc};
use tracing::info;
use tracing_subscriber::{
    filter::Directive, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer, Registry,
};

/// The main reth cli interface.
///
/// This is the entrypoint to the executable.
#[derive(Debug, Parser)]
#[command(author, version = SHORT_VERSION, long_version = LONG_VERSION, about = "Reth", long_about = None)]
pub struct Cli<C: ChainSpecParser = DefaultChainSpecParser, Ext: clap::Args + fmt::Debug = NoArgs> {
    /// The command to run
    #[command(subcommand)]
    command: Commands<C, Ext>,

    /// The chain this node is running.
    ///
    /// Possible values are either a built-in chain or the path to a chain specification file.
    #[arg(
        long,
        value_name = "CHAIN_OR_PATH",
        long_help = C::help_message(),
        default_value = C::SUPPORTED_CHAINS[0],
        value_parser = C::parser(),
        global = true,
    )]
    chain: Arc<C::ChainSpec>,

    /// Add a new instance of a node.
    ///
    /// Configures the ports of the node to avoid conflicts with the defaults.
    /// This is useful for running multiple nodes on the same machine.
    ///
    /// Max number of instances is 200. It is chosen in a way so that it's not possible to have
    /// port numbers that conflict with each other.
    ///
    /// Changes to the following port numbers:
    /// - `DISCOVERY_PORT`: default + `instance` - 1
    /// - `AUTH_PORT`: default + `instance` * 100 - 100
    /// - `HTTP_RPC_PORT`: default - `instance` + 1
    /// - `WS_RPC_PORT`: default + `instance` * 2 - 2
    #[arg(long, value_name = "INSTANCE", global = true, default_value_t = 1, value_parser = value_parser!(u16).range(..=200))]
    instance: u16,

    #[command(flatten)]
    logs: LogArgs,
}

impl Cli {
    /// Parsers only the default CLI arguments
    pub fn parse_args() -> Self {
        Self::parse()
    }

    /// Parsers only the default CLI arguments from the given iterator
    pub fn try_parse_args_from<I, T>(itr: I) -> Result<Self, clap::error::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        Self::try_parse_from(itr)
    }
}

impl<C: ChainSpecParser<ChainSpec = ChainSpec>, Ext: clap::Args + fmt::Debug> Cli<C, Ext> {
    /// Execute the configured cli command.
    ///
    /// This accepts a closure that is used to launch the node via the
    /// [`NodeCommand`](node::NodeCommand).
    pub fn run<L, Fut>(mut self, launcher: L) -> eyre::Result<()>
    where
        L: FnOnce(WithLaunchContext<NodeBuilder<Arc<DatabaseEnv>>>, Ext) -> Fut,
        Fut: Future<Output = eyre::Result<()>>,
    {
        // add network name to logs dir
        self.logs.log_file_directory = self
            .logs
            .log_file_directory
            .join(self.chain.chain.to_string());

        let _guard = self.init_tracing()?;
        info!(target: "reth::cli", "Initialized tracing, debug log directory: {}", self.logs.log_file_directory);

        let runner = CliRunner::default();
        match self.command {
            Commands::Node(command) => {
                runner.run_command_until_exit(|ctx| command.execute(ctx, launcher))
            }
            Commands::Init(command) => {
                runner.run_blocking_until_ctrl_c(command.execute::<EthereumNode>())
            }
            Commands::InitState(command) => {
                runner.run_blocking_until_ctrl_c(command.execute::<EthereumNode>())
            }
            Commands::Import(command) => runner.run_blocking_until_ctrl_c(
                command.execute::<EthereumNode, _, _>(EthExecutorProvider::ethereum),
            ),
            Commands::DumpGenesis(command) => runner.run_blocking_until_ctrl_c(command.execute()),
            Commands::Db(command) => {
                runner.run_blocking_until_ctrl_c(command.execute::<EthereumNode>())
            }
            Commands::Stage(command) => runner.run_command_until_exit(|ctx| {
                command.execute::<EthereumNode, _, _>(ctx, EthExecutorProvider::ethereum)
            }),
            Commands::P2P(command) => runner.run_until_ctrl_c(command.execute()),
            Commands::Config(command) => runner.run_until_ctrl_c(command.execute()),
            Commands::Debug(command) => {
                runner.run_command_until_exit(|ctx| command.execute::<EthereumNode>(ctx))
            }
            Commands::Recover(command) => {
                runner.run_command_until_exit(|ctx| command.execute::<EthereumNode>(ctx))
            }
            Commands::Prune(command) => runner.run_until_ctrl_c(command.execute::<EthereumNode>()),
        }
    }

    /// Initializes tracing with the configured options.
    ///
    /// If file logging is enabled, this function returns a guard that must be kept alive to ensure
    /// that all logs are flushed to disk.
    pub fn init_tracing(&self) -> eyre::Result<Option<FileWorkerGuard>> {
        let mut layers = Vec::<Box<dyn Layer<Registry> + Send + Sync>>::new();

        // stdout
        let stdout_filter = build_env_filter(
            Some(self.logs.verbosity.directive().to_string().parse()?),
            &self.logs.log_stdout_filter,
        )?;
        let stdout_layer = self.logs.log_stdout_format.apply(
            stdout_filter,
            Some(self.logs.color.to_string()),
            None,
        );
        layers.push(stdout_layer.boxed());

        // journald
        if self.logs.journald {
            let journald_filter = build_env_filter(None, &self.logs.journald_filter)?;
            let journald_layer = tracing_journald::layer()?.with_filter(journald_filter);
            layers.push(journald_layer.boxed());
        }

        // file
        let mut file_guard = None;
        if self.logs.log_file_max_files > 0 {
            let log_dir: &Path = self.logs.log_file_directory.as_ref();
            if !log_dir.exists() {
                std::fs::create_dir_all(log_dir).expect("Could not create log directory");
            }
            let (writer, guard) = tracing_appender::non_blocking(
                RollingFileAppender::new(
                    log_dir.join(&"reth.log"),
                    RollingConditionBasic::new()
                        .max_size(self.logs.log_file_max_size * 1024 * 1024),
                    self.logs.log_file_max_files,
                )
                .expect("Could not initialize file logging"),
            );

            let file_filter = build_env_filter(None, &self.logs.log_file_filter)?;
            let layer = self
                .logs
                .log_file_format
                .apply(file_filter, None, Some(writer));
            layers.push(layer);

            file_guard = Some(guard);
        }

        // tokio-console
        layers.push(Box::new(console_subscriber::spawn()));

        // The error is returned if the global default subscriber is already set,
        // so it's safe to ignore it
        let _ = tracing_subscriber::registry().with(layers).try_init();

        Ok(file_guard)
    }
}

fn build_env_filter(
    default_directive: Option<Directive>,
    directives: &str,
) -> eyre::Result<EnvFilter> {
    let env_filter = if let Some(default_directive) = default_directive {
        EnvFilter::builder()
            .with_default_directive(default_directive)
            .from_env_lossy()
    } else {
        EnvFilter::builder().from_env_lossy()
    };

    [
        "hyper::proto::h1=off",
        "trust_dns_proto=off",
        "trust_dns_resolver=off",
        "discv5=off",
        "jsonrpsee-server=off",
    ]
    .into_iter()
    .chain(directives.split(',').filter(|d| !d.is_empty()))
    .try_fold(env_filter, |env_filter, directive| {
        Ok(env_filter.add_directive(directive.parse()?))
    })
}
