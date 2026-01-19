use std::time::Duration;

use clap::Parser;
use color_eyre::{Result, eyre::WrapErr as _};
use cpal::SampleRate;
use ringbuf::traits::Consumer;
use sp_app::workers::audio::AudioWorker;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

const BUFFER_DURATION: Duration = Duration::from_millis(100);
const MIN_SAMPLE_RATE: SampleRate = 44_000;

#[derive(Parser)]
#[clap(version, author, about)]
struct Args {}

impl Args {
	async fn run(self) -> Result<()> {
		let cancel = ctrl_c();

		let audio_worker = AudioWorker::spawn(cancel.child_token())
			.await
			.wrap_err("failed to spawn audio worker")?;

		let (stream, mut rx) = audio_worker
			.create_stream(BUFFER_DURATION, MIN_SAMPLE_RATE)
			.await
			.wrap_err("failed to create stream")?;
		info!("created stream with sample rate {}", stream.sample_rate());

		let read_task_cancel = cancel.child_token();
		tokio::task::spawn(read_task_cancel.clone().run_until_cancelled_owned(
			async move {
				let _stream = stream;
				// Pop at 4x the rate that would be needed to prevent the buffer from filling.
				let mut interval = tokio::time::interval(BUFFER_DURATION / 4);
				loop {
					interval.tick().await;
					let n_samples = rx.0.clear();
					debug!("encountered {n_samples} samples");
				}
			},
		));

		tokio::time::sleep(Duration::from_millis(1000)).await;
		info!("stopping stream");
		read_task_cancel.cancel();

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
