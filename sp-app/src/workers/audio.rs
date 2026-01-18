use color_eyre::{
	Result,
	eyre::{OptionExt as _, WrapErr as _},
};
use cpal::{
	InputCallbackInfo, Stream, StreamError,
	traits::{DeviceTrait as _, HostTrait as _, StreamTrait},
};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace, warn};

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

#[derive(Debug)]
pub enum Request {
	Ping,
}

#[derive(Debug)]
pub enum Response {
	Pong,
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
		let stream = tokio::task::spawn_blocking(stream_setup)
			.await
			.wrap_err("task panicked")?
			.wrap_err("failed to set up steam")?;

		let (rpc_tx, rpc_rx) = flume::unbounded();
		let supervisor_task =
			tokio::task::spawn(supervisor(stream, cancel.clone(), rpc_rx));

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

	pub async fn ping(&self) -> Result<(), RpcError> {
		let Response::Pong = self.send(Request::Ping).await?;

		Ok(())
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
	stream: Stream,
	cancel: CancellationToken,
	rpc_rx: flume::Receiver<(Request, oneshot::Sender<Response>)>,
) -> Result<()> {
	stream.play().wrap_err("failed to play stream")?;

	let rpc_fut = async {
		while let Ok((request, response_tx)) = rpc_rx.recv_async().await {
			debug!("request: {request:?}");
			let response = match request {
				Request::Ping => Response::Pong,
			};
			let _ = response_tx.send(response);
		}
		debug!("no AudioEngine handles left, shutting down supervisor");
	};

	tokio::select! {
		() = cancel.cancelled() => debug!("supervisor cancellation token triggered, shutting down"),
		() = rpc_fut => (),
	};

	Ok(())
}

/// Blocking function, sets up a stream
fn stream_setup() -> Result<Stream> {
	let host = cpal::default_host();
	let device = host
		.default_input_device()
		.ok_or_eyre("no input device available")?;

	let mut supported_configs_range = device
		.supported_input_configs()
		.wrap_err("error while querying configs")?;
	let supported_config = supported_configs_range
		.next()
		.ok_or_eyre("no supported config?!")?
		.with_max_sample_rate();

	let data_cb = move |data: &[f32], info: &InputCallbackInfo| {
		trace!(?info, "sample buffer len:{}", data.len());
	};
	let error_cb = move |err: StreamError| warn!(?err, "got error");
	let stream = device
		.build_input_stream(&supported_config.config(), data_cb, error_cb, None)
		.wrap_err("failed to create input stream")?;

	Ok(stream)
}
