use std::time::Duration;

use clap::Parser;
use color_eyre::{Result, eyre::WrapErr as _};
use sp_app::workers::audio::AudioWorker;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

#[derive(Parser)]
#[clap(version, author, about)]
struct Args {}

impl Args {
	async fn run(self) -> Result<()> {
		let cancel = ctrl_c();

		let audio_worker = AudioWorker::spawn(cancel.child_token())
			.await
			.wrap_err("failed to spawn audio worker")?;

		for _ in 0..4 {
			let Ok(()) = audio_worker.ping().await else {
				break;
			};
			tokio::time::sleep(Duration::from_millis(1000)).await;
		}

		cancel.cancelled().await;
		debug!("joining on audio worker");
		audio_worker
			.join()
			.await
			.expect("infallible becuase there is only 1 join")
			.wrap_err("error in audio worker")?;

		Ok(())
	}
}

fn main() -> Result<()> {
	tracing_subscriber::fmt::init();
	color_eyre::install()?;

	let args = Args::parse();

	let rt = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.expect("failed to initialize tokio runtime");

	rt.block_on(async_main(args))
}

async fn async_main(args: Args) -> Result<()> {
	info!("started app");

	args.run().await
}

fn ctrl_c() -> CancellationToken {
	let cancel = CancellationToken::new();

	let cancel_clone = cancel.clone();
	tokio::spawn(async move {
		tokio::signal::ctrl_c().await.ok();
		debug!("detected ctrlc");
		cancel_clone.cancel();
	});

	cancel
}
