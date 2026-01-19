use std::{fmt::Debug, sync::Arc, time::Duration};

use color_eyre::{
	Result,
	eyre::{OptionExt as _, WrapErr as _, ensure},
};
use cpal::{
	InputCallbackInfo, SampleRate, Stream, StreamConfig, StreamError,
	traits::{DeviceTrait as _, HostTrait, StreamTrait},
};
use ringbuf::{
	HeapRb,
	traits::{Producer as _, Split},
};
use thiserror::Error;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Receiving side: pops samples from the ringbuffer
#[derive(derive_more::From, derive_more::Into)]
pub struct Consumer(pub ringbuf::CachingCons<Arc<HeapRb<f32>>>);

impl Debug for Consumer {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_tuple("Consumer").finish_non_exhaustive()
	}
}

#[derive(Debug)]
struct Joins {
	supervisor: tokio::task::JoinHandle<Result<()>>,
}

impl Joins {
	/// Joins on all tokio tasks and aggregates their results
	async fn join(self) -> Result<()> {
		self.supervisor
			.await
			.wrap_err("supervisor task panicked")?
			.wrap_err("supervisor task returned error")
	}
}

/// A stream is killed when all of its `StreamHandle`s are dropped.
#[derive(Debug, Clone)]
pub struct StreamHandle {
	_guard: Arc<tokio_util::sync::DropGuard>,
	config: StreamConfig,
}

impl StreamHandle {
	pub fn sample_rate(&self) -> SampleRate {
		self.config.sample_rate
	}
}

#[derive(Debug)]
pub enum Request {
	Ping,
	StartStream {
		buffer_duration: Duration,
		sample_rate: SampleRate,
	},
}

#[derive(Debug)]
pub enum Response {
	Pong,
	StartStreamResponse(Result<(StreamHandle, Consumer)>),
}

/// A handle to the audio worker and its associates subtasks
#[derive(Debug, Clone)]
pub struct AudioWorker {
	rpc_tx: flume::Sender<(Request, oneshot::Sender<Response>)>,
	join_rx: flume::Receiver<Result<()>>,
	cancel: CancellationToken,
}

impl AudioWorker {
	/// Creates audio stream then spawns subtasks
	pub async fn spawn(cancel: CancellationToken) -> Result<AudioWorker> {
		let (rpc_tx, rpc_rx) = flume::unbounded();
		let supervisor_task = tokio::task::spawn(supervisor(cancel.clone(), rpc_rx));

		let joins = Joins {
			supervisor: supervisor_task,
		};
		let (join_tx, join_rx) = flume::bounded(1);
		let cancel_clone = cancel.clone();
		tokio::task::spawn(async move {
			cancel_clone.cancelled().await;
			let result = joins.join().await;
			join_tx.try_send(result).ok();
		});

		Ok(Self {
			rpc_tx,
			join_rx,
			cancel,
		})
	}

	/// Returns the results of subtasks. Will wait for them to be cancelled.
	/// This only returns `Some` once, and its not guaranted which joiner will
	/// return Some. If you want to be sure, only call .join once.
	pub async fn join(self) -> Option<Result<()>> {
		self.join_rx.recv().ok()
	}

	/// Errors if the underlying worker was already killed
	async fn send(&self, request: Request) -> Result<Response, RpcError> {
		let (response_tx, response_rx) = oneshot::channel();
		self.rpc_tx
			.send_async((request, response_tx))
			.await
			.map_err(|_| RpcError)?;
		let response = response_rx.await.map_err(|_| RpcError)?;

		Ok(response)
	}

	/// Simple test to make sure the worker task is alive
	pub async fn ping(&self) -> Result<()> {
		let Response::Pong = self
			.send(Request::Ping)
			.await
			.wrap_err("failed to send ping command")?
		else {
			unreachable!()
		};

		Ok(())
	}

	/// Starts an audio stream, which will push all of its samples to a ringbuffer.
	///
	/// `buffer_duration` is the maximum amount of time the [`Consumer`] will be able to stall
	/// for before data will start getting dropped.
	pub async fn create_stream(
		&self,
		buffer_duration: Duration,
		sample_rate: SampleRate,
	) -> Result<(StreamHandle, Consumer)> {
		let Response::StartStreamResponse(result) = self
			.send(Request::StartStream {
				buffer_duration,
				sample_rate,
			})
			.await
			.wrap_err("failed to send create_stream command")?
		else {
			unreachable!()
		};

		result
	}

	pub fn kill(&self) {
		self.cancel.cancel();
	}
}

#[derive(thiserror::Error, Debug)]
#[error("worker is killed")]
pub struct RpcError;

/// The broker manages communication with the underlying tasks and handling errors
async fn supervisor(
	cancel: CancellationToken,
	rpc_rx: flume::Receiver<(Request, oneshot::Sender<Response>)>,
) -> Result<()> {
	let rpc_fut = async {
		while let Ok((request, response_tx)) = rpc_rx.recv_async().await {
			debug!("request: {request:?}");
			let response = match request {
				Request::Ping => Response::Pong,
				Request::StartStream {
					buffer_duration,
					sample_rate,
				} => {
					let f = move || {
						stream_setup(buffer_duration, sample_rate)
							.wrap_err("failed to create stream")
							.and_then(|(stream, consumer, config)| {
								stream.play().wrap_err("failed to play stream")?;
								let cancel = CancellationToken::new();
								let cancel_cloned = cancel.clone();
								tokio::task::spawn(async move {
									cancel_cloned.cancelled().await;
									debug!("stopping stream");
									drop(stream)
								});

								Ok((
									StreamHandle {
										_guard: Arc::new(cancel.drop_guard()),
										config,
									},
									consumer,
								))
							})
					};

					Response::StartStreamResponse(
						tokio::task::spawn_blocking(f)
							.await
							.wrap_err("task panicked")
							.flatten(),
					)
				}
			};
			let _ = response_tx.send(response);
		}
		debug!("no AudioWorker handles left, shutting down supervisor");
	};

	tokio::select! {
		() = cancel.cancelled() => debug!("supervisor cancellation token triggered, shutting down"),
		() = rpc_fut => (),
	};

	Ok(())
}

/// Blocking function, sets up a stream
fn stream_setup(
	buffer_duration: Duration,
	sample_rate: SampleRate,
) -> Result<(Stream, Consumer, StreamConfig)> {
	ensure!(
		!buffer_duration.is_zero(),
		"`buffer_duration` cannot be zero"
	);

	for h in cpal::available_hosts() {
		debug!("host: {h:?}");
	}
	let host = cpal::default_host();
	info!("using host: {}", host.id());

	for d in host
		.devices()
		.wrap_err("failed to enumerate input devices")?
	{
		debug!("input device: {:?}", d.description());
	}
	let device = host
		.default_input_device()
		.ok_or_eyre("no input device available")?;
	info!(
		"using device: {}",
		device
			.description()
			.map(|d| d.to_string())
			.unwrap_or_else(|_| format!("{:?}", device.description()))
	);

	let mut supported_configs_range = device
		.supported_input_configs()
		.wrap_err("error while querying configs")?
		.filter(|c| {
			c.channels() == 1
				&& c.max_sample_rate() >= sample_rate
				&& c.min_sample_rate() <= sample_rate
		});
	let supported_config = supported_configs_range
		.next()
		.ok_or_eyre("no supported config?!")?
		.with_sample_rate(sample_rate);

	let capacity_exponent =
		compute_capacity_exponent(supported_config.sample_rate(), buffer_duration)
			.expect("infallible: precondition already checked");
	let capacity = 1usize << capacity_exponent;
	debug!("ringbuffer capacity: {capacity}");
	let (mut tx, rx) = ringbuf::HeapRb::new(capacity).split();

	let data_cb = move |data: &[f32], _info: &InputCallbackInfo| {
		tx.push_slice(data);
	};
	let error_cb = move |err: StreamError| warn!(?err, "got error");
	let stream = device
		.build_input_stream(&supported_config.config(), data_cb, error_cb, None)
		.wrap_err("failed to create input stream")?;

	Ok((stream, Consumer(rx), supported_config.config()))
}

#[derive(Error, Debug, Eq, PartialEq)]
#[error("duration was zero")]
struct DurationWasZero;

const fn compute_capacity_exponent(
	sample_rate: SampleRate,
	buffer_duration: Duration,
) -> Result<u32, DurationWasZero> {
	if buffer_duration.is_zero() {
		return Err(DurationWasZero);
	}
	let capacity = buffer_duration
		.as_micros()
		.checked_mul(sample_rate as u128)
		.expect("overflow")
		.div_ceil(Duration::from_secs(1).as_micros());
	let exp = capacity.ilog2();
	let exp = if 1 << exp == capacity { exp } else { exp + 1 };
	assert!(
		(1u32 << exp) as u128 >= capacity,
		"sanity: should be impossible to get lower than required capacity"
	);

	Ok(exp)
}

#[cfg(test)]
mod test_compute_capacity_exponent {
	use super::*;

	#[test]
	fn zero_latency_should_error() {
		assert!(compute_capacity_exponent(1337, Duration::ZERO).is_err());
		assert!(compute_capacity_exponent(0, Duration::ZERO).is_err());
	}

	#[test]
	fn known_values() {
		let examples = [
			(44_000, Duration::from_secs(1), 16),
			(16, Duration::from_secs(1), 4),
			(16, Duration::from_secs(1) - Duration::from_millis(1), 4),
			(16, Duration::from_secs(1) + Duration::from_millis(1), 5),
			(44_000, Duration::from_millis(100), 13),
		];
		for (sample_rate, buffer_duration, buffer_capacity_exponent) in examples {
			assert_eq!(
				compute_capacity_exponent(sample_rate, buffer_duration),
				Ok(buffer_capacity_exponent),
				"failed example with sr={sample_rate}, bd={buffer_duration:?}"
			);
		}
	}
}
