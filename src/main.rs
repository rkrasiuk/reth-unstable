// We use jemalloc for performance reasons.
#[cfg(all(feature = "jemalloc", unix))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

pub mod cli;

/// Parameters for configuring the engine
#[derive(Debug, Clone, clap::Args, PartialEq, Eq, Default)]
#[command(next_help_heading = "Engine")]
pub struct EngineArgs {
    /// Enable the engine2 experimental features on reth binary
    #[arg(long = "engine.experimental", default_value = "false")]
    pub experimental: bool,
}

fn main() {
    use clap::Parser;
    use reth::args::utils::DefaultChainSpecParser;
    use reth_node_builder::EngineNodeLauncher;
    use reth_node_ethereum::{node::EthereumAddOns, EthereumNode};
    use reth_provider::providers::BlockchainProvider2;

    reth_cli_util::sigsegv_handler::install();

    // Enable backtraces unless a RUST_BACKTRACE value has already been explicitly provided.
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        std::env::set_var("RUST_BACKTRACE", "1");
    }

    if let Err(err) = cli::Cli::<DefaultChainSpecParser, EngineArgs>::parse().run(
        |builder, engine_args| async move {
            let enable_engine2 = engine_args.experimental;
            match enable_engine2 {
                true => {
                    let handle = builder
                        .with_types_and_provider::<EthereumNode, BlockchainProvider2<_>>()
                        .with_components(EthereumNode::components())
                        .with_add_ons::<EthereumAddOns>()
                        .launch_with_fn(|builder| {
                            let launcher = EngineNodeLauncher::new(
                                builder.task_executor().clone(),
                                builder.config().datadir(),
                            );
                            builder.launch_with(launcher)
                        })
                        .await?;
                    handle.node_exit_future.await
                }
                false => {
                    let handle = builder.launch_node(EthereumNode::default()).await?;
                    handle.node_exit_future.await
                }
            }
        },
    ) {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
