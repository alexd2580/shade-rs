use std::{mem, path::PathBuf, rc::Rc};

use error::Error;

mod audio;
mod dft;
mod error;
mod vulkan;
mod window;

struct App {
    audio: audio::Audio,
    dft: dft::Dft,
    dft_buffer: Rc<vulkan::multi_buffer::MultiBuffer>,
    vulkan: vulkan::Vulkan,
}

impl window::App for App {
    fn run_frame(&mut self) -> winit::event_loop::ControlFlow {
        self.audio.write_to_slice(self.dft.get_input_vec());
        self.dft.run_transform();
        self.dft
            .write_to_pointer(self.dft_buffer.mapped(self.vulkan.binding_index));
        self.vulkan.run_frame()
    }

    fn handle_resize(&mut self, new_size: (u32, u32)) -> Result<(), error::Error> {
        self.vulkan.handle_resize(new_size)
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.vulkan.wait_idle();
    }
}

fn run_main() -> Result<(), Error> {
    let mut window = window::Window::new(1280, 1024)?;
    let compute_shader_path = PathBuf::from("shaders/debug.comp");
    let vulkan = vulkan::Vulkan::new(&window, &compute_shader_path)?;

    let audio = audio::Audio::new();
    let dft = dft::Dft::new();
    let dft_result_size = mem::size_of_val(dft.get_output_vec()) as u64;
    let dft_buffer = vulkan.new_multi_buffer("dft", dft_result_size)?;

    log::info!("Running...");
    {
        let mut app = App {
            audio,
            dft,
            dft_buffer,
            vulkan,
        };
        window.run_main_loop(&mut app);
    }

    Ok(())
}

fn main() {
    simple_logger::init_with_level(log::Level::Debug).unwrap();
    log::info!("Initializing...");
    run_main().unwrap();
    log::info!("Terminating...");
}
