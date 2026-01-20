use std::sync::Arc;

use color_eyre::{Result, eyre::WrapErr as _};
use egui::ColorImage;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::info;

pub type Image = Arc<ColorImage>;

#[derive(derive_more::Debug)]
pub struct MyApp {
	cancel: CancellationToken,
	image_rx: mpsc::Receiver<Image>,
	#[debug(skip)]
	texture: Option<egui::TextureHandle>,
}

impl Drop for MyApp {
	fn drop(&mut self) {
		self.cancel.cancel();
	}
}

impl MyApp {
	pub fn new(cancel: CancellationToken, image_rx: mpsc::Receiver<Image>) -> Self {
		Self {
			cancel,
			image_rx,
			texture: None,
		}
	}

	fn ui(&mut self, ui: &mut egui::Ui) {
		let texture = self.texture.get_or_insert_with(|| {
			// Load the texture only once.
			ui.ctx().load_texture(
				"spectrogram-texture",
				egui::ColorImage::example(),
				Default::default(),
			)
		});
		if let Ok(img) = self.image_rx.try_recv() {
			texture.set(img, Default::default());
		}
		ui.heading("My egui Application");
		ui.label("Hello World");
		ui.image((texture.id(), texture.size_vec2()));
	}
}

impl eframe::App for MyApp {
	fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
		egui::CentralPanel::default().show(ctx, |ui| self.ui(ui));
	}

	fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
		info!("exiting egui");
		self.cancel.cancel();
	}
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct FromEguiError(String);

impl From<eframe::Error> for FromEguiError {
	fn from(value: eframe::Error) -> Self {
		Self(value.to_string())
	}
}

pub fn run_egui(
	cancel: CancellationToken,
	image_rx: mpsc::Receiver<Image>,
) -> Result<()> {
	let options = eframe::NativeOptions::default();
	let my_app = MyApp::new(cancel, image_rx);
	eframe::run_native("My egui App", options, Box::new(|_cc| Ok(Box::new(my_app))))
		.map_err(FromEguiError::from)
		.wrap_err("failed to run egui app")
}
