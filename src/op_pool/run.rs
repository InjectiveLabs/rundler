use ethers::types::{Address, U256};
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tonic::transport::Server;

use crate::common::protos::op_pool::op_pool_server::OpPoolServer;
use crate::common::protos::op_pool::OP_POOL_FILE_DESCRIPTOR_SET;
use crate::op_pool::mempool::uo_pool::UoPool;
use crate::op_pool::server::OpPoolImpl;

pub struct Args {
    pub port: u16,
    pub host: String,
    pub entry_point: Address,
    pub chain_id: U256,
}

pub async fn run(
    args: Args,
    mut shutdown_rx: broadcast::Receiver<()>,
    _shutdown_scope: mpsc::Sender<()>,
) -> anyhow::Result<()> {
    let addr = format!("{}:{}", args.host, args.port).parse()?;
    tracing::info!("Starting server on {}", addr);
    tracing::info!("Entry point: {}", args.entry_point);
    tracing::info!("Chain id: {}", args.chain_id);

    let mp = UoPool::new(args.entry_point, args.chain_id);
    let op_pool_server = OpPoolServer::new(OpPoolImpl::new(args.chain_id, mp));
    let reflection_service = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(OP_POOL_FILE_DESCRIPTOR_SET)
        .build()?;

    Server::builder()
        .add_service(op_pool_server)
        .add_service(reflection_service)
        .serve_with_shutdown(addr, async move {
            shutdown_rx
                .recv()
                .await
                .expect("should have received shutdown signal")
        })
        .await?;
    tracing::info!("Op pool server shutdown");
    Ok(())
}