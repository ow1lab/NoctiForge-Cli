use anyhow::Result;

mod command;

mod api {
    pub mod registry {
        tonic::include_proto!("noctiforge.registry");
    }
    pub mod controlplane {
        tonic::include_proto!("noctiforge.controlplane");
    }
    pub mod worker {
        tonic::include_proto!("noctiforge.worker");
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    command::run().await
}
