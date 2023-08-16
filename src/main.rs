use std::{mem, rc::Rc, sync::Arc, time};

use clap::Parser;

use error::Error;
use log::{error, info, warn};
use poem::{web::sse, EndpointExt};
use tokio::{runtime, sync::broadcast};

mod audio;
mod beat_analysis;
mod dft;
mod error;
mod ring_buffer;
mod thread_shared;
mod timer;
mod utils;
mod vulkan;
mod window;

type Message = Vec<f32>;

/// Note the reverse drop order.
struct Visualizer {
    epoch: time::Instant,

    available_samples: usize,
    avg_available_samples: f32,
    avg_available_samples_alpha: f32,

    _frequency_band_border_indices: [usize; 8],
    beat_analysis: beat_analysis::BeatAnalysis,

    audio: audio::Audio,
    signal_gpu: Rc<vulkan::multi_buffer::MultiBuffer>,
    signal_dft: dft::Dft,
    signal_dft_gpu: Rc<vulkan::multi_buffer::MultiBuffer>,

    low_pass: audio::low_pass::LowPass,
    low_pass_gpu: Rc<vulkan::multi_buffer::MultiBuffer>,
    low_pass_dft: dft::Dft,
    low_pass_dft_gpu: Rc<vulkan::multi_buffer::MultiBuffer>,

    high_pass: audio::high_pass::HighPass,
    high_pass_gpu: Rc<vulkan::multi_buffer::MultiBuffer>,
    high_pass_dft: dft::Dft,
    high_pass_dft_gpu: Rc<vulkan::multi_buffer::MultiBuffer>,

    images: Vec<Rc<vulkan::multi_image::MultiImage>>,

    vulkan: vulkan::Vulkan,

    timer: timer::Timer,

    broadcast: Arc<broadcast::Sender<Message>>,
}

fn dft_index_of_frequency(frequency: usize, sample_rate: usize, dft_size: usize) -> usize {
    // For reference see
    // https://stackoverflow.com/questions/4364823/how-do-i-obtain-the-frequencies-of-each-value-in-an-fft
    // 0:   0 * 44100 / 1024 =     0.0 Hz
    // 1:   1 * 44100 / 1024 =    43.1 Hz
    // 2:   2 * 44100 / 1024 =    86.1 Hz
    // 3:   3 * 44100 / 1024 =   129.2 Hz
    (frequency as f32 * dft_size as f32 / sample_rate as f32).round() as usize
}

impl Visualizer {
    fn run_dft(
        buffer: &ring_buffer::RingBuffer<f32>,
        dft: &mut dft::Dft,
        dft_gpu: &vulkan::multi_buffer::MultiBuffer,
    ) {
        buffer.write_to_buffer(dft.get_input_vec());
        dft.run_transform();
        dft.write_to_pointer(dft_gpu.mapped(0));
    }

    fn reinitialize_images(&mut self) -> Result<(), error::Error> {
        // Drop old images.
        self.images.clear();

        let vulkan = &mut self.vulkan;
        let image_size = vulkan.surface_info.surface_resolution;

        let intermediate = vulkan.new_multi_image("intermediate", image_size, None)?;
        let intermediate_prev = vulkan.prev_shift(&intermediate, "intermediate_prev");
        self.images.push(intermediate);
        self.images.push(intermediate_prev);

        let highlights = vulkan.new_multi_image("highlights", image_size, None)?;
        self.images.push(highlights);
        let bloom_h = vulkan.new_multi_image("bloom_h", image_size, None)?;
        self.images.push(bloom_h);
        let bloom_hv = vulkan.new_multi_image("bloom_hv", image_size, None)?;
        self.images.push(bloom_hv);
        let result = vulkan.new_multi_image("result", image_size, None)?;
        let result_prev = vulkan.prev_shift(&result, "result_prev");
        self.images.push(result);
        self.images.push(result_prev);

        Ok(())
    }

    fn new(
        window: &window::Window,
        args: &Args,
        broadcast: &Arc<broadcast::Sender<Message>>,
    ) -> Result<Visualizer, Error> {
        let mut vulkan = vulkan::Vulkan::new(window, &args.shader_paths, args.vsync)?;
        let images = Vec::new();

        // TODO dynamic?
        let frame_rate = 60;

        let audio = audio::Audio::new(args.audio_buffer_sec, args.passthrough)?;
        let audio_buffer_size = audio.buffer_size();
        let audio_buffer_bytes =
            audio_buffer_size * mem::size_of::<f32>() + 2 * mem::size_of::<i32>();
        let signal_gpu = vulkan.new_multi_buffer("signal", audio_buffer_bytes, Some(1))?;

        let low_pass = audio::low_pass::LowPass::new(audio_buffer_size, 0.02);
        let low_pass_gpu = vulkan.new_multi_buffer("low_pass", audio_buffer_bytes, Some(1))?;

        let high_pass = audio::high_pass::HighPass::new(audio_buffer_size, 0.1);
        let high_pass_gpu = vulkan.new_multi_buffer("high_pass", audio_buffer_bytes, Some(1))?;

        let dft_size = args.dft_size;
        let dft_window_per_s = audio.sample_rate as f32 / dft_size as f32;
        let dft_min_fq = dft_window_per_s * 1f32;
        let dft_max_fq = dft_window_per_s * dft_size as f32 / 2f32;
        info!("DFT can analyze frequencies in the range: {dft_min_fq} hz - {dft_max_fq} hz");

        let frequency_band_borders = [16, 60, 250, 500, 2000, 4000, 6000, 22000];
        let frequency_band_border_indices = frequency_band_borders
            .map(|frequency| dft_index_of_frequency(frequency, audio.sample_rate, dft_size));

        let dft_result_size = dft::Dft::output_byte_size(args.dft_size) + mem::size_of::<i32>();

        let signal_dft = dft::Dft::new(args.dft_size);
        let signal_dft_gpu = vulkan.new_multi_buffer("signal_dft", dft_result_size, Some(1))?;

        let low_pass_dft = dft::Dft::new(args.dft_size);
        let low_pass_dft_gpu = vulkan.new_multi_buffer("low_pass_dft", dft_result_size, Some(1))?;

        let high_pass_dft = dft::Dft::new(args.dft_size);
        let high_pass_dft_gpu =
            vulkan.new_multi_buffer("high_pass_dft", dft_result_size, Some(1))?;

        let beat_analysis = beat_analysis::BeatAnalysis::new(&mut vulkan)?;

        let broadcast = broadcast.clone();

        let mut visualizer = Self {
            broadcast,
            epoch: time::Instant::now(),
            timer: timer::Timer::new(0.9),
            available_samples: 0,
            avg_available_samples: 44100f32 / 60f32,
            avg_available_samples_alpha: 0.95,
            audio,
            signal_gpu,
            signal_dft,
            signal_dft_gpu,
            low_pass,
            low_pass_gpu,
            low_pass_dft,
            low_pass_dft_gpu,
            high_pass,
            high_pass_gpu,
            high_pass_dft,
            high_pass_dft_gpu,
            _frequency_band_border_indices: frequency_band_border_indices,
            beat_analysis,
            images,
            vulkan,
        };

        visualizer.reinitialize_images()?;
        Ok(visualizer)
    }

    /// Returns the read index (start of data to read), write index (index at which new data will
    /// be written (end of data to read) and the size of the ring buffer.
    fn data_indices(&mut self) -> (usize, usize, usize) {
        let read_index = self.low_pass.write_index;
        let write_index = self.audio.left.write_index;
        let buf_size = self.audio.left.data.len();

        // Total available samples.
        let available_samples = if write_index < read_index {
            write_index + buf_size - read_index
        } else {
            write_index - read_index
        };

        // New available in this frame.
        let new_available = available_samples - self.available_samples;
        self.avg_available_samples = self.avg_available_samples * self.avg_available_samples_alpha
            + new_available as f32 * (1f32 - self.avg_available_samples_alpha);

        // `+5` makes it so that i try to display more frames without lagging behind too much.
        // This is a magic number, might be different for different FPS.
        let mut consume_samples = self.avg_available_samples as usize + 2;
        let (sample_underrun, ok) = consume_samples.overflowing_sub(available_samples);
        let sample_underrun_pct = 100f32 * sample_underrun as f32 / consume_samples as f32;
        if !ok && consume_samples > available_samples {
            if sample_underrun_pct > 50f32 {
                warn!("Sample underrun by {sample_underrun} ({sample_underrun_pct:.2}%)");
            }
            consume_samples = available_samples;
        }

        let sample_overrun_pct =
            100f32 * available_samples as f32 / (consume_samples as f32 + 1f32);
        if ok && sample_overrun_pct > 2000f32 {
            warn!("Sample overrun by {available_samples} ({sample_overrun_pct:.2}%)");
        }

        self.available_samples = available_samples - consume_samples;

        let write_index = (read_index + consume_samples) % buf_size;

        (read_index, write_index, buf_size)
    }

    fn run_vulkan(&mut self) -> Result<(), Error> {
        use vulkan::Value::{Bool, F32};

        let mut push_constant_values = std::collections::HashMap::new();

        let is_beat = self.beat_analysis.is_beat;
        push_constant_values.insert("is_beat".to_owned(), Bool(is_beat));
        let now = self.epoch.elapsed().as_secs_f32();
        push_constant_values.insert("now".to_owned(), F32(now));

        match unsafe { self.vulkan.tick(&push_constant_values)? } {
            None => (),
            Some(vulkan::Event::Resized) => self.reinitialize_images()?,
        }
        Ok(())
    }

    fn tick(&mut self) -> winit::event_loop::ControlFlow {
        self.timer.section("Outside of loop");

        let (read_index, write_index, buf_size) = self.data_indices();

        if write_index < read_index {
            for index in read_index..buf_size {
                let x = self.audio.left.data[index];
                self.low_pass.sample(x);
                self.high_pass.sample(x);
            }
            for index in 0..write_index {
                let x = self.audio.left.data[index];
                self.low_pass.sample(x);
                self.high_pass.sample(x);
            }
        } else {
            for index in read_index..write_index {
                let x = self.audio.left.data[index];
                self.low_pass.sample(x);
                self.high_pass.sample(x);
            }
        }

        self.timer.section("Filters");

        self.audio
            .left
            .write_to_pointer(read_index, write_index, self.signal_gpu.mapped(0));

        self.low_pass
            .write_to_pointer(read_index, write_index, self.low_pass_gpu.mapped(0));

        self.high_pass
            .write_to_pointer(read_index, write_index, self.high_pass_gpu.mapped(0));

        self.timer.section("Filters to GPU");

        Self::run_dft(&self.audio.left, &mut self.signal_dft, &self.signal_dft_gpu);

        Self::run_dft(
            &self.low_pass,
            &mut self.low_pass_dft,
            &self.low_pass_dft_gpu,
        );

        Self::run_dft(
            &self.high_pass,
            &mut self.high_pass_dft,
            &self.high_pass_dft_gpu,
        );

        let beat_dft = &self.low_pass_dft;
        let beat_dft_lower = dft_index_of_frequency(35, self.audio.sample_rate, beat_dft.size());
        let beat_dft_upper = dft_index_of_frequency(125, self.audio.sample_rate, beat_dft.size());
        let beat_dft_sum_size = beat_dft_upper - beat_dft_lower;
        let bass_frequencies = &beat_dft.simple[beat_dft_lower..beat_dft_upper];

        self.broadcast
            .send(bass_frequencies.to_owned())
            .expect("Failed to broadcast frame bass frequencies");

        let beat_dft_sum = bass_frequencies.iter().fold(0f32, |a, b| a + b);
        self.beat_analysis
            .sample(beat_dft_sum / beat_dft_sum_size as f32);

        self.timer.section("DFTs and DFTs to GPU");

        let result = match self.run_vulkan() {
            Ok(()) => winit::event_loop::ControlFlow::Poll,
            Err(err) => {
                error!("{err}");
                winit::event_loop::ControlFlow::ExitWithCode(1)
            }
        };

        self.timer.section("Vulkan");

        if self.vulkan.num_frames % 600 == 0 {
            self.timer.print();
        }

        result
    }
}

impl Drop for Visualizer {
    fn drop(&mut self) {
        self.vulkan.wait_idle();
    }
}

impl window::App for Visualizer {
    fn loop_body(&mut self) -> winit::event_loop::ControlFlow {
        self.tick()
    }
}

/// Run an audio visualizer.
#[derive(Parser, Debug, Clone)]
struct Args {
    /// The shader module path
    #[arg(short, long, num_args = 0.., default_value = "shaders/debug.comp")]
    shader_paths: Vec<std::path::PathBuf>,

    /// The DFT size
    #[arg(short, long, default_value = "2048")]
    dft_size: usize,

    /// The audio buffer size
    #[arg(short, long, default_value = "4")]
    audio_buffer_sec: u32,

    /// Enable vsync
    #[arg(short, long, default_value = "true", action = clap::ArgAction::Set)]
    vsync: bool,

    #[arg(short, long, default_value = "true", action = clap::ArgAction::Set)]
    passthrough: bool,
}

#[poem::handler]
fn event(channel: poem::web::Data<&Arc<broadcast::Sender<Message>>>) -> sse::SSE {
    let mut receiver = channel.subscribe();
    let stream = futures::stream::unfold(receiver, |mut receiver| async move {
        let val = receiver.recv().await.unwrap();
        let val = serde_json::to_string_pretty(&val).unwrap();
        Some((sse::Event::message(val), receiver))
    });
    sse::SSE::new(stream).keep_alive(time::Duration::from_secs(5))
}

async fn run_server(sender: Arc<broadcast::Sender<Message>>) {
    let cors = poem::middleware::Cors::new().allow_method(poem::http::Method::GET);
    let app = poem::Route::new()
        .at("/event", poem::get(event))
        .with(poem::middleware::AddData::new(sender.clone()))
        .with(cors);
    let _ = poem::Server::new(poem::listener::TcpListener::bind("127.0.0.1:3000"))
        .run(app)
        .await;
}

fn run_main(args: &Args) -> Result<(), Error> {
    let (sender, _receiver) = broadcast::channel(10);
    let sender = Arc::new(sender);

    // Start server.
    let runtime = runtime::Runtime::new()?;
    let server = runtime.spawn(run_server(sender.clone()));

    // Run visualizer.
    let mut window = window::Window::new()?;

    {
        let mut visualizer = Visualizer::new(&window, &args, &sender)?;
        log::info!("Running...");
        window.run_main_loop(&mut visualizer);
    }

    server.abort();
    Ok(())
}

fn main() {
    simple_logger::init_with_level(log::Level::Debug).unwrap();
    log::info!("Initializing...");
    let args = Args::parse();
    if let Err(err) = run_main(&args) {
        error!("{}", err);
    }
    log::info!("Terminating...");
}
