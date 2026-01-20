// #![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release

use std::time::Duration;

use clap::Parser;
use color_eyre::{Result, eyre::WrapErr as _};
use cpal::SampleRate;
use sp_app::{
	egui::Image,
	workers::{analysis::AnalysisWorker, audio::AudioWorker},
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

const BUFFER_DURATION: Duration = Duration::from_millis(100);
const MIN_SAMPLE_RATE: SampleRate = 44_000;

#[derive(Parser)]
#[clap(version, author, about)]
struct Args {}

fn main() -> Result<()> {
	tracing_subscriber::fmt::init();
	color_eyre::install()?;

	let _args = Args::parse();

	let (cancel_tx, cancel_rx) = oneshot::channel();
	let (image_tx, image_rx) = mpsc::channel(1);
	let async_handle = std::thread::spawn(|| {
		let rt = tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.expect("failed to initialize tokio runtime");

		rt.block_on(async_main(cancel_tx, image_tx))
	});

	let cancel = cancel_rx
		.blocking_recv()
		.expect("failed to receive cancellation token");
	let cancel_clone = cancel.clone();
	let cancel_guard = cancel.drop_guard();
	sp_app::egui::run_egui(cancel_clone, image_rx)?;
	info!("after run egui");
	drop(cancel_guard);

	async_handle.join().expect("async runtime panicked")
}

async fn async_main(
	cancel_tx: oneshot::Sender<CancellationToken>,
	_image_tx: mpsc::Sender<Image>,
) -> Result<()> {
	let cancel = ctrl_c();
	cancel_tx
		.send(cancel.clone())
		.expect("failed to send cancel");
	info!("started app");

	let audio_worker = AudioWorker::spawn(cancel.child_token())
		.await
		.wrap_err("failed to spawn audio worker")?;

	let (stream, sample_rx) = audio_worker
		.create_stream(BUFFER_DURATION, MIN_SAMPLE_RATE)
		.await
		.wrap_err("failed to create stream")?;
	let sample_rate = stream.sample_rate();
	info!("created stream with sample rate {}", sample_rate);

	let analysis_worker =
		AnalysisWorker::spawn(cancel.child_token(), stream, BUFFER_DURATION, sample_rx);

	tokio::time::sleep(Duration::from_millis(1000)).await;
	info!("stopping stream");
	analysis_worker.kill();
	if let Some(result) = analysis_worker.join().await {
		result?;
	};

	cancel.cancelled().await;
	debug!("joining on audio worker");
	audio_worker
		.join()
		.await
		.expect("infallible becuase there is only 1 join")
		.wrap_err("error in audio worker")?;

	Ok(())
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
