//! Output capture through wlr-screencopy, and window capture through Hyprland's
//! toplevel export, on a Wayland connection of its own that stays open in a
//! worker thread: a capture costs one frame of the compositor instead of
//! starting grim and going through a PNG file.
use anyhow::{Context, Result, bail};
use gtk::glib;
use std::{
    fs,
    os::{fd::AsFd, unix::fs::FileExt},
    path::PathBuf,
    sync::mpsc,
    thread,
};
use toplevel_export::{
    hyprland_toplevel_export_frame_v1::{self, HyprlandToplevelExportFrameV1},
    hyprland_toplevel_export_manager_v1::HyprlandToplevelExportManagerV1,
};
use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle, WEnum,
    globals::{GlobalList, GlobalListContents, registry_queue_init},
    protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool},
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

/// Hyprland's copy of one window as it renders it, whatever lies above it.
#[allow(non_upper_case_globals, clippy::all)]
mod toplevel_export {
    use wayland_client;
    use wayland_client::protocol::*;
    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/hyprland-toplevel-export-v1.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("protocols/hyprland-toplevel-export-v1.xml");
}

/// The pixels of one output in B, G, R, A byte order, opaque, top row first.
#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub data: glib::Bytes,
}
impl Frame {
    pub fn texture(&self) -> gtk::gdk::MemoryTexture {
        gtk::gdk::MemoryTexture::new(
            self.width as i32,
            self.height as i32,
            gtk::gdk::MemoryFormat::B8g8r8a8,
            &self.data,
            self.stride as usize,
        )
    }
}
/// What to capture: an output by name, or a window by its Hyprland handle
/// (the low 32 bits of its address).
#[derive(Clone)]
enum Target {
    Output(String),
    Window(u32),
}
impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::Output(name) => f.write_str(name),
            Self::Window(handle) => write!(f, "window {handle:#x}"),
        }
    }
}
type Reply = async_channel::Sender<Result<Vec<Frame>, String>>;
pub struct Capturer {
    jobs: mpsc::Sender<(Vec<Target>, Reply)>,
}
impl Capturer {
    /// Connects right away, so that the first capture does not wait for it.
    pub fn start() -> Self {
        let (jobs, rx) = mpsc::channel::<(Vec<Target>, Reply)>();
        thread::spawn(move || {
            let mut session = Session::connect().ok();
            for (targets, reply) in rx {
                let mut result = Err(anyhow::anyhow!("No Wayland connection"));
                // A connection that failed is replaced once before giving up.
                for _ in 0..2 {
                    if session.is_none() {
                        session = Session::connect().ok();
                    }
                    if let Some(s) = session.as_mut() {
                        result = s.capture(&targets);
                        if result.is_ok() {
                            break;
                        }
                        session = None;
                    }
                }
                let _ = reply.send_blocking(result.map_err(|e| format!("{e:#}")));
            }
        });
        Self { jobs }
    }
    /// Frames of the outputs named, in the same order.
    pub async fn capture(&self, outputs: Vec<String>) -> Result<Vec<Frame>, String> {
        self.run(outputs.into_iter().map(Target::Output).collect())
            .await
    }
    /// The window with this Hyprland handle, without what covers it.
    pub async fn capture_window(&self, handle: u32) -> Result<Frame, String> {
        Ok(self.run(vec![Target::Window(handle)]).await?.remove(0))
    }
    async fn run(&self, targets: Vec<Target>) -> Result<Vec<Frame>, String> {
        let (tx, rx) = async_channel::bounded(1);
        self.jobs
            .send((targets, tx))
            .map_err(|_| "The capture thread stopped".to_string())?;
        rx.recv()
            .await
            .map_err(|_| "The capture thread stopped".to_string())?
    }
}

struct Output {
    global: u32,
    proxy: wl_output::WlOutput,
    name: Option<String>,
}
#[derive(Default)]
struct Pending {
    /// Format, width, height and stride of the first shared-memory buffer offered.
    buffer: Option<(wl_shm::Format, u32, u32, u32)>,
    described: bool,
    y_invert: bool,
    ready: bool,
    failed: bool,
}
impl Pending {
    /// Keeps the first shared-memory buffer offered in a format we read.
    fn offer(&mut self, format: wl_shm::Format, width: u32, height: u32, stride: u32) {
        if self.buffer.is_none()
            && matches!(
                format,
                wl_shm::Format::Xrgb8888
                    | wl_shm::Format::Argb8888
                    | wl_shm::Format::Xbgr8888
                    | wl_shm::Format::Abgr8888
            )
        {
            self.buffer = Some((format, width, height, stride));
        }
    }
}
#[derive(Default)]
struct State {
    outputs: Vec<Output>,
    pending: Vec<Pending>,
}
struct Session {
    queue: EventQueue<State>,
    state: State,
    shm: wl_shm::WlShm,
    manager: ZwlrScreencopyManagerV1,
    /// Hyprland only.
    export: Option<HyprlandToplevelExportManagerV1>,
    _globals: GlobalList,
}
/// A frame being copied, from either protocol.
enum Copy {
    Output(ZwlrScreencopyFrameV1),
    Window(HyprlandToplevelExportFrameV1),
}
impl Copy {
    fn copy(&self, buffer: &wl_buffer::WlBuffer) {
        match self {
            Self::Output(frame) => frame.copy(buffer),
            // Now, rather than at the window's next damage.
            Self::Window(frame) => frame.copy(buffer, 1),
        }
    }
    fn destroy(&self) {
        match self {
            Self::Output(frame) => frame.destroy(),
            Self::Window(frame) => frame.destroy(),
        }
    }
}
impl Session {
    fn connect() -> Result<Self> {
        let conn = Connection::connect_to_env()?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
        let qh = queue.handle();
        let shm = globals.bind(&qh, 1..=1, ())?;
        let manager = globals
            .bind(&qh, 3..=3, ())
            .context("The compositor does not offer wlr-screencopy")?;
        let export = globals.bind(&qh, 1..=1, ()).ok();
        let mut state = State::default();
        globals.contents().with_list(|list| {
            for g in list.iter().filter(|g| g.interface == "wl_output") {
                state.add_output(globals.registry(), g.name, g.version, &qh);
            }
        });
        // Output names arrive after binding.
        queue.roundtrip(&mut state)?;
        Ok(Self {
            queue,
            state,
            shm,
            manager,
            export,
            _globals: globals,
        })
    }
    fn capture(&mut self, targets: &[Target]) -> Result<Vec<Frame>> {
        // Picks up outputs plugged in since the last capture.
        self.queue.roundtrip(&mut self.state)?;
        let qh = self.queue.handle();
        self.state.pending = targets.iter().map(|_| Pending::default()).collect();
        let mut frames = Vec::new();
        for (i, target) in targets.iter().enumerate() {
            frames.push(match target {
                Target::Output(name) => {
                    let output = self
                        .state
                        .outputs
                        .iter()
                        .find(|o| o.name.as_deref() == Some(name))
                        .with_context(|| format!("No output named {name}"))?;
                    Copy::Output(self.manager.capture_output(0, &output.proxy, &qh, i))
                }
                Target::Window(handle) => Copy::Window(
                    self.export
                        .as_ref()
                        .context("The compositor cannot capture single windows")?
                        .capture_toplevel(0, *handle, &qh, i),
                ),
            });
        }
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let mut buffers: Vec<Option<(fs::File, wl_shm_pool::WlShmPool, wl_buffer::WlBuffer)>> =
            targets.iter().map(|_| None).collect();
        let result = (|| {
            loop {
                for (i, p) in self.state.pending.iter().enumerate() {
                    if p.failed {
                        bail!("The compositor could not capture {}", targets[i]);
                    }
                    if !p.described || buffers[i].is_some() {
                        continue;
                    }
                    let (format, width, height, stride) =
                        p.buffer.context("No shared-memory buffer format offered")?;
                    let size = stride as u64 * height as u64;
                    let path = runtime.join(format!("nexus-screencopy-{}-{i}", std::process::id()));
                    let file = fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create(true)
                        .truncate(true)
                        .open(&path)?;
                    fs::remove_file(&path)?;
                    file.set_len(size)?;
                    let pool = self.shm.create_pool(file.as_fd(), size as i32, &qh, ());
                    let buffer = pool.create_buffer(
                        0,
                        width as i32,
                        height as i32,
                        stride as i32,
                        format,
                        &qh,
                        (),
                    );
                    frames[i].copy(&buffer);
                    buffers[i] = Some((file, pool, buffer));
                }
                if self.state.pending.iter().all(|p| p.ready) {
                    break;
                }
                self.queue.blocking_dispatch(&mut self.state)?;
            }
            let mut out = Vec::new();
            for (p, slot) in self.state.pending.iter().zip(&buffers) {
                let (format, width, height, stride) = p.buffer.unwrap();
                let (file, ..) = slot.as_ref().unwrap();
                let mut data = vec![0; stride as usize * height as usize];
                file.read_exact_at(&mut data, 0)?;
                normalize(&mut data, format, width, stride, p.y_invert);
                out.push(Frame {
                    width,
                    height,
                    stride,
                    data: glib::Bytes::from_owned(data),
                });
            }
            Ok(out)
        })();
        for frame in frames {
            frame.destroy();
        }
        for (_, pool, buffer) in buffers.into_iter().flatten() {
            buffer.destroy();
            pool.destroy();
        }
        let _ = self.queue.flush();
        result
    }
}
/// Brings a supported format to opaque B, G, R, A with the top row first.
fn normalize(data: &mut [u8], format: wl_shm::Format, width: u32, stride: u32, y_invert: bool) {
    let swap = matches!(format, wl_shm::Format::Xbgr8888 | wl_shm::Format::Abgr8888);
    for row in data.chunks_exact_mut(stride as usize) {
        for px in row[..width as usize * 4].as_chunks_mut::<4>().0 {
            if swap {
                px.swap(0, 2);
            }
            px[3] = 255;
        }
    }
    if y_invert {
        let rows = data.len() / stride as usize;
        for i in 0..rows / 2 {
            let (top, bottom) = data.split_at_mut((rows - 1 - i) * stride as usize);
            top[i * stride as usize..(i + 1) * stride as usize]
                .swap_with_slice(&mut bottom[..stride as usize]);
        }
    }
}
impl State {
    fn add_output(
        &mut self,
        registry: &wl_registry::WlRegistry,
        global: u32,
        version: u32,
        qh: &QueueHandle<Self>,
    ) {
        // Version 4 tells the connector name, which is how Hyprland and GDK name outputs.
        let proxy = registry.bind(global, version.min(4), qh, global);
        self.outputs.push(Output {
            global,
            proxy,
            name: None,
        });
    }
}
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == "wl_output" => state.add_output(registry, name, version, qh),
            wl_registry::Event::GlobalRemove { name } => {
                state.outputs.retain(|o| o.global != name);
            }
            _ => {}
        }
    }
}
impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event
            && let Some(o) = state.outputs.iter_mut().find(|o| o.global == *global)
        {
            o.name = Some(name);
        }
    }
}
impl Dispatch<HyprlandToplevelExportFrameV1, usize> for State {
    fn event(
        state: &mut Self,
        _: &HyprlandToplevelExportFrameV1,
        event: hyprland_toplevel_export_frame_v1::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(p) = state.pending.get_mut(*index) else {
            return;
        };
        use hyprland_toplevel_export_frame_v1::Event;
        match event {
            Event::Buffer {
                format: WEnum::Value(format),
                width,
                height,
                stride,
            } => p.offer(format, width, height, stride),
            Event::BufferDone => p.described = true,
            Event::Flags {
                flags: WEnum::Value(flags),
            } => p.y_invert = flags.contains(hyprland_toplevel_export_frame_v1::Flags::YInvert),
            Event::Ready { .. } => p.ready = true,
            Event::Failed => p.failed = true,
            _ => {}
        }
    }
}
impl Dispatch<ZwlrScreencopyFrameV1, usize> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(p) = state.pending.get_mut(*index) else {
            return;
        };
        use zwlr_screencopy_frame_v1::Event;
        match event {
            Event::Buffer {
                format: WEnum::Value(format),
                width,
                height,
                stride,
            } => p.offer(format, width, height, stride),
            Event::BufferDone => p.described = true,
            Event::Flags {
                flags: WEnum::Value(flags),
            } => p.y_invert = flags.contains(zwlr_screencopy_frame_v1::Flags::YInvert),
            Event::Ready { .. } => p.ready = true,
            Event::Failed => p.failed = true,
            _ => {}
        }
    }
}
wayland_client::delegate_noop!(State: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(State: wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(State: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(State: ZwlrScreencopyManagerV1);
wayland_client::delegate_noop!(State: HyprlandToplevelExportManagerV1);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rows_are_flipped_and_made_opaque() {
        // Two rows of one pixel plus padding, upside down, in R, G, B order.
        let mut data = vec![1, 2, 3, 0, 9, 9, 4, 5, 6, 0, 9, 9];
        normalize(&mut data, wl_shm::Format::Xbgr8888, 1, 6, true);
        assert_eq!(data, vec![6, 5, 4, 255, 9, 9, 3, 2, 1, 255, 9, 9]);
    }
}
