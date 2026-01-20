pub mod workers;

use std::{collections::VecDeque, marker::PhantomData};

use aus::{WindowType, spectrum::rstft};

pub type SampleFormat = f64;

/// The short time fourier transform (STFT) settings.
///
/// # Example
/// ```
/// struct MySettings;
/// impl StftSettings for MySettings {
///     const FFT_SIZE_EXPONENT = 11;
///     const N_FFT_WINDOWS: 4;
/// }
///
/// assert_eq!(MySettings::FFT_SIZE, 2048)
/// assert_eq!(MySettings::CHUNK_SIZE, 2048 * 4);
/// ```
pub trait StftSettings {
	const FFT_SIZE_EXPONENT: u8;
}

/// Additional settings computed from [`StftSettings`].
pub trait StftSettingsExt: StftSettings + private::Sealed {
	const FFT_SIZE: usize = (1usize << Self::FFT_SIZE_EXPONENT as usize);
	const HOP_SIZE: usize = Self::FFT_SIZE / 2;
}

mod private {
	use super::*;
	pub trait Sealed {}
	impl<T: StftSettings> Sealed for T {}
}

impl<T: StftSettings> StftSettingsExt for T {}

/// The frequency magnitudes of a particular timestep
#[derive(Debug, Clone)]
pub struct FrequencyMagnitudes(Vec<SampleFormat>);

impl FrequencyMagnitudes {
	pub fn as_slice(&self) -> &[SampleFormat] {
		self.0.as_slice()
	}
}

/// The "y axis" of the spectrogram. Represents the Hz value of each frequency
/// bin.
#[derive(Debug, Clone)]
pub struct FrequencyBins<T: StftSettings> {
	hz: Vec<SampleFormat>,
	_phantom: PhantomData<T>,
}

impl<T: StftSettings> FrequencyBins<T> {
	pub fn new(sample_rate: u32) -> Self {
		let hz = aus::spectrum::rfftfreq(T::FFT_SIZE, sample_rate);

		Self {
			hz,
			_phantom: PhantomData,
		}
	}

	/// Get the hz value of each bin
	pub fn hz(&self) -> &[SampleFormat] {
		self.hz.as_slice()
	}
}

/// A Spectrogram can be thought of as an image: its y axis is [`Self::frequency_bins`],
/// its x axis is time, and its pixel values are [`Self::frequency_magnitudes`].
#[derive(Debug, Clone)]
pub struct Spectrogram<T: StftSettings> {
	freqs: FrequencyBins<T>,
	magnitudes: VecDeque<FrequencyMagnitudes>,
}

impl<T: StftSettings> Spectrogram<T> {
	pub fn new(sample_rate: u32) -> Self {
		let freqs = FrequencyBins::new(sample_rate);

		Self {
			freqs,
			magnitudes: VecDeque::new(),
		}
	}

	pub fn frequency_bins(&self) -> &FrequencyBins<T> {
		&self.freqs
	}

	/// Array index corresponds to each timestep of the STFT. Two slices are returned since
	/// they are non-contiguous due to the inner VecDeque.
	pub fn frequency_magnitudes(
		&self,
	) -> (&[FrequencyMagnitudes], &[FrequencyMagnitudes]) {
		self.magnitudes.as_slices()
	}

	/// Iterates over each timestep of the STFT.
	pub fn frequency_magnitudes_iter(
		&self,
	) -> impl Iterator<Item = &FrequencyMagnitudes> {
		self.magnitudes.iter()
	}

	/// # Panics
	/// *May* panic (not guaranteed) if `waveform_samples.len()` is not a multiple of
	/// `T::FFT_SIZE`.
	///
	/// Returns the number of chunks of size `T::FFT_SIZE` processed.
	pub fn push_samples(&mut self, waveform_samples: &[SampleFormat]) -> usize {
		let it = analysis::<T>(waveform_samples);
		let n_chunks = it.len();
		self.magnitudes.extend(it);

		n_chunks
	}
}

/// # Panics
/// *May* panic (not guaranteed) if the waveform length is not a multiple of
/// `2^T::FFT_SIZE_EXPONENT`
fn analysis<T: StftSettings>(
	waveform: &[SampleFormat],
) -> impl ExactSizeIterator<Item = FrequencyMagnitudes> {
	// ideally this would have been enforced by consts but I could not figure
	// out how to do it
	debug_assert_eq!(
		waveform.len() % T::FFT_SIZE,
		0,
		"expected a waveform length which is a multiple of the fft size",
	);
	let window_type = WindowType::Hanning;
	// TODO: Consider using rayon thread pool to parallelize analysis per-chunk
	rstft(waveform, T::FFT_SIZE, T::HOP_SIZE, window_type)
		.into_iter()
		.map(|complex_spectrum| {
			let (magnitude_spectrum, _phase_spectrum) =
				aus::spectrum::complex_to_polar_rfft(complex_spectrum.as_slice());

			FrequencyMagnitudes(magnitude_spectrum)
		})
}

#[derive(Debug, Clone, Copy)]
pub struct DefaultStftSettings;

impl StftSettings for DefaultStftSettings {
	const FFT_SIZE_EXPONENT: u8 = 11;
}
