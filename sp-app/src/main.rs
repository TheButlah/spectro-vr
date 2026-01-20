use std::time::Duration;

use clap::Parser;
use color_eyre::{Result, eyre::WrapErr as _};
use cpal::SampleRate;
use ringbuf::traits::{Consumer, Observer};
use sp_app::{
	DefaultStftSettings, Spectrogram, StftSettingsExt as _,
	workers::audio::{AudioWorker, SampleFormat},
};
use tokio::sync::oneshot;
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
		let sample_rate = stream.sample_rate();
		info!("created stream with sample rate {}", sample_rate);

		let read_task_cancel = cancel.child_token();
		let read_task_cancel_clone = read_task_cancel.clone();
		let (read_task_tx, read_task_rx) = oneshot::channel();
		tokio::task::spawn(async move {
			let cancel = read_task_cancel_clone;
			let _stream_guard = stream; // ensures its droped at end of scope
			// Pop at 4x the rate that would be needed to prevent the buffer from filling.
			let mut interval = tokio::time::interval(BUFFER_DURATION / 4);
			type Settings = DefaultStftSettings;
			const MAX_CHUNKS_TO_POP: u8 = 8;
			const MAX_SAMPLES: usize = MAX_CHUNKS_TO_POP as usize * Settings::FFT_SIZE;
			let mut f32_samples: [SampleFormat; MAX_SAMPLES] = [0.; _];
			let mut f64_samples = [0f64; MAX_SAMPLES];
			let mut spec = Spectrogram::<Settings>::new(sample_rate);
			while !cancel.is_cancelled() {
				interval.tick().await;

				// Note: Since we are not locking, this can later increase as more
				// data gets pushed. Thats ok though - we won't pop more than this.
				let chunks_to_pop = rx.0.occupied_len() / Settings::FFT_SIZE;
				if chunks_to_pop == 0 {
					continue;
				}
				let chunks_to_pop = chunks_to_pop.min(MAX_CHUNKS_TO_POP as usize);
				let n_samples = chunks_to_pop * Settings::FFT_SIZE;
				let f32_samples = &mut f32_samples[..n_samples];
				let f64_samples = &mut f64_samples[..n_samples];

				let n_popped = rx.0.pop_slice(f32_samples);
				assert_eq!(n_popped, n_samples, "sanity");
				debug!("popped {n_popped} samples");

				for (idx, s) in f32_samples.iter().copied().enumerate() {
					f64_samples[idx] = f64::from(s);
				}
				let n_pushed = spec.push_samples(f64_samples);
				debug_assert_eq!(n_pushed, n_popped);
			}
			let _ = read_task_tx.send(spec);
		});

		tokio::time::sleep(Duration::from_millis(1000)).await;
		info!("stopping stream");
		read_task_cancel.cancel();
		if let Ok(spec) = read_task_rx.await {
			info!("spec: {spec:?}");
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
