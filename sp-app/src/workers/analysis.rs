//! Handles audio analysis, and generates a spectrogram + additional data.

use std::{sync::Arc, time::Duration};

use color_eyre::{Result, eyre::Context};
use colorgrad::Gradient;
use egui::Color32;
use ringbuf::traits::{Consumer as _, Observer as _};
use tokio_util::sync::{CancellationToken, DropGuard};
use tracing::debug;

use crate::{
	DefaultStftSettings, Spectrogram, StftSettings, StftSettingsExt as _,
	workers::audio::{Consumer, SampleFormat as AudioWorkerSampleFormat, StreamHandle},
};

#[derive(Debug)]
struct Joins {
	analysis: tokio::task::JoinHandle<Result<()>>,
}

impl Joins {
	/// Joins on all tokio tasks and aggregates their results
	async fn join(self) -> Result<()> {
		self.analysis
			.await
			.wrap_err("analysis task panicked")?
			.wrap_err("analysis task returned error")
	}
}

#[derive(Debug, Clone)]
pub struct AnalysisWorker {
	cancel: CancellationToken,
	join_rx: flume::Receiver<Result<()>>,
	_drop_guard: Arc<DropGuard>,
}

impl AnalysisWorker {
	pub fn spawn(
		cancel: CancellationToken,
		stream: StreamHandle,
		ringbuf_duration: Duration,
		sample_rx: Consumer,
	) -> Self {
		let (join_tx, join_rx) = flume::bounded(1);
		let cancel_clone = cancel.clone();
		let analysis_task = tokio::task::spawn_blocking(move || {
			entry(cancel_clone, stream, ringbuf_duration, sample_rx)
		});
		let joins = Joins {
			analysis: analysis_task,
		};
		let cancel_clone = cancel.clone();
		tokio::task::spawn(async move {
			cancel_clone.cancelled().await;
			debug!("cancelling AnalysisWorker");
			let result = joins.join().await;
			join_tx.send(result)
		});

		Self {
			join_rx,
			_drop_guard: Arc::new(cancel.clone().drop_guard()),
			cancel,
		}
	}

	/// Returns the results of subtasks. Will wait for them to be cancelled.
	/// This only returns `Some` once, and its not guaranted which joiner will
	/// return Some. If you want to be sure, only call .join once.
	pub async fn join(self) -> Option<Result<()>> {
		self.join_rx.recv().ok()
	}

	pub fn kill(&self) {
		self.cancel.cancel();
	}
}

fn entry(
	cancel: CancellationToken,
	stream: StreamHandle,
	ringbuf_duration: Duration,
	mut sample_rx: Consumer,
) -> Result<()> {
	// Pop at 4x the rate that would be needed to prevent the buffer from filling.
	let mut interval = tokio::time::interval(ringbuf_duration / 4);
	type Settings = DefaultStftSettings;
	const MAX_CHUNKS_TO_POP: u8 = 8;
	const MAX_SAMPLES: usize = MAX_CHUNKS_TO_POP as usize * Settings::FFT_SIZE;
	let mut f32_samples: [AudioWorkerSampleFormat; MAX_SAMPLES] = [0.; _];
	let mut f64_samples = [0f64; MAX_SAMPLES];
	let mut spec = Spectrogram::<Settings>::new(stream.sample_rate());
	let rt = tokio::runtime::Handle::current();
	while !cancel.is_cancelled() {
		rt.block_on(interval.tick());

		// Note: Since we are not locking, this can later increase as more
		// data gets pushed. Thats ok though - we won't pop more than this.
		let chunks_to_pop = sample_rx.0.occupied_len() / Settings::FFT_SIZE;
		if chunks_to_pop == 0 {
			continue;
		}
		let chunks_to_pop = chunks_to_pop.min(MAX_CHUNKS_TO_POP as usize);
		let n_samples = chunks_to_pop * Settings::FFT_SIZE;
		let f32_samples = &mut f32_samples[..n_samples];
		let f64_samples = &mut f64_samples[..n_samples];

		let n_popped = sample_rx.0.pop_slice(f32_samples);
		assert_eq!(n_popped, n_samples, "sanity");
		debug!("popped {n_popped} samples");

		for (idx, s) in f32_samples.iter().copied().enumerate() {
			f64_samples[idx] = f64::from(s);
		}
		let n_chunks = spec.push_samples(f64_samples);
		debug_assert_eq!(n_chunks, chunks_to_pop);

		// extract_column(spec, column_index, out, gradient)
	}

	Ok(())
}

/// Returns whether a colum was extracted or not. Guaranteed to extract as long as
/// column index is in-bounds of the spec.
#[expect(dead_code)]
fn extract_column<T: StftSettings>(
	spec: &Spectrogram<T>,
	column_index: usize,
	out: &mut Vec<Color32>,
	gradient: impl Gradient,
) -> bool {
	let Some(mags) = spec.magnitudes.get(column_index) else {
		return false;
	};
	out.clear();
	let max_mag = mags
		.0
		.iter()
		.copied()
		.max_by(|a, b| my_f64_cmp(*a, *b))
		.expect("infallible: spectrogram should never have 0 frequency bins");
	dbg!(max_mag);
	let colors = mags.0.iter().map(|m| {
		let c = gradient.at(*m as f32).to_linear_rgba_u8();
		Color32::from_rgb(c[0], c[1], c[2])
	});
	out.extend(colors);

	true
}

fn my_f64_cmp(a: f64, b: f64) -> std::cmp::Ordering {
	if b >= a {
		std::cmp::Ordering::Greater
	} else {
		std::cmp::Ordering::Less
	}
}
