//! How the window is drawn: with OpenGL, except on Windows, where wgpu draws it on
//! DirectX 12 or Vulkan. `crates/ui/Cargo.toml` compiles in exactly one renderer per
//! system and says why.
//!
//! Copied from `genetic_algo`'s `ui/src/renderer.rs`, where the choice was measured.

use eframe::{NativeOptions, egui::ViewportBuilder};

/// The options every window of the app opens with.
pub fn native_options(viewport: ViewportBuilder) -> NativeOptions {
    NativeOptions {
        viewport,
        #[cfg(windows)]
        wgpu_options: frugal_wgpu(),
        ..Default::default()
    }
}

/// wgpu reserves GPU memory in large blocks by default, for speed: `genetic_algo`'s window
/// committed about 400 MB, measured, and about half of that when wgpu was asked to save
/// memory. A 2D interface gains nothing from the larger blocks.
#[cfg(windows)]
fn frugal_wgpu() -> eframe::egui_wgpu::WgpuConfiguration {
    use eframe::{
        egui_wgpu::{WgpuConfiguration, WgpuSetup, WgpuSetupCreateNew},
        wgpu,
    };

    let mut setup = WgpuSetupCreateNew::without_display_handle();
    let defaults = setup.device_descriptor.clone();
    setup.device_descriptor = std::sync::Arc::new(move |adapter| wgpu::DeviceDescriptor {
        memory_hints: wgpu::MemoryHints::MemoryUsage,
        ..defaults(adapter)
    });
    WgpuConfiguration {
        wgpu_setup: WgpuSetup::CreateNew(setup),
        ..Default::default()
    }
}
