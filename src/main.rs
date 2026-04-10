use std::cmp::min;
use std::time::{Duration, Instant};

use calloop::timer::{TimeoutAction, Timer};
use calloop::{EventLoop, LoopHandle};
use calloop_wayland_source::WaylandSource;
use client::globals::registry_queue_init;
use client::protocol::{wl_keyboard, wl_output, wl_region, wl_seat, wl_shm, wl_surface};
use client::{Connection, Dispatch, QueueHandle};
use protocols::ext::background_effect::v1::client::ext_background_effect_surface_v1;
use sctk::background_effect::{BackgroundEffectHandler, BackgroundEffectState};
use sctk::compositor::{CompositorHandler, CompositorState};
use sctk::output::{OutputHandler, OutputState};
use sctk::reexports::{calloop, calloop_wayland_source, client, protocols};
use sctk::registry::{ProvidesRegistryState, RegistryState};
use sctk::seat::keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers};
use sctk::seat::{Capability, SeatHandler, SeatState};
use sctk::shell::WaylandSurface;
use sctk::shell::xdg::XdgShell;
use sctk::shell::xdg::window::{Window, WindowConfigure, WindowDecorations, WindowHandler};
use sctk::shm::slot::{Buffer, SlotPool};
use sctk::shm::{Shm, ShmHandler};
use sctk::{
    delegate_background_effect, delegate_compositor, delegate_keyboard, delegate_output,
    delegate_registry, delegate_seat, delegate_shm, delegate_xdg_shell, delegate_xdg_window,
    registry_handlers,
};

const RLE: &[u8] = include_bytes!("../resources/bad_apple.rle");

struct Decoder<'a> {
    width: u16,
    height: u16,
    fps: u16,
    data: &'a [u8],
}

struct Frame<'a, 'b> {
    decoder: &'b mut Decoder<'a>,
    color: bool,
}

impl<'a> Decoder<'a> {
    fn new(data: &'a [u8]) -> Self {
        let width = u16::from_le_bytes([data[0], data[1]]);
        let height = u16::from_le_bytes([data[2], data[3]]);
        let fps = u16::from_le_bytes([data[4], data[5]]);
        let data = &data[6..];

        Self {
            width,
            height,
            fps,
            data,
        }
    }

    fn next_frame<'b>(&'b mut self) -> Frame<'a, 'b> {
        let color = self.data[0] != 0;
        self.data = &self.data[1..];

        Frame {
            decoder: self,
            color,
        }
    }
}

impl<'a, 'b> Frame<'a, 'b> {
    fn next_run(&mut self) -> (usize, bool) {
        let color = self.color;
        self.color = !self.color;

        let mut len = 0;
        let mut b = self.decoder.data[0];
        self.decoder.data = &self.decoder.data[1..];
        while b == 0 {
            len += 255;
            b = self.decoder.data[0];
            self.decoder.data = &self.decoder.data[1..];
        }
        len += usize::from(b);

        (len, color)
    }
}

fn main() {
    let decoder = Decoder::new(RLE);

    let conn = Connection::connect_to_env().unwrap();
    let (globals, event_queue) = registry_queue_init(&conn).unwrap();
    let qh = event_queue.handle();
    let mut event_loop: EventLoop<App> = EventLoop::try_new().unwrap();
    let loop_handle = event_loop.handle();
    WaylandSource::new(conn.clone(), event_queue)
        .insert(loop_handle.clone())
        .unwrap();

    let compositor = CompositorState::bind(&globals, &qh).unwrap();
    let xdg_shell = XdgShell::bind(&globals, &qh).unwrap();
    let shm = Shm::bind(&globals, &qh).unwrap();
    let bg_effect = BackgroundEffectState::new(&globals, &qh);

    let surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(surface, WindowDecorations::RequestServer, &qh);
    window.set_title("bad_apple");

    let width = u32::from(decoder.width);
    let height = u32::from(decoder.height);
    let size = (width, height);
    window.set_min_size(Some(size));
    window.set_max_size(Some(size));

    window.commit();

    let pool = SlotPool::new((width * height * 4) as usize, &shm).unwrap();

    let bg_effect_surface = bg_effect
        .get_background_effect(window.wl_surface(), &qh)
        .expect("ext-background-effect is missing");

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        compositor,
        shm,
        bg_effect,
        loop_handle,
        qh: qh.clone(),

        exit: false,
        first_configure: true,
        pool,
        window_buffer: None,
        window,
        bg_effect_surface,
        keyboard: None,

        decoder,
        frame_num: 0,
        start_time: Instant::now(),
    };

    println!("Q/Escape to quit.");

    loop {
        event_loop.dispatch(None, &mut app).unwrap();
        if app.exit {
            break;
        }
    }
}

struct App {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    compositor: CompositorState,
    shm: Shm,
    bg_effect: BackgroundEffectState,
    loop_handle: LoopHandle<'static, Self>,
    qh: QueueHandle<App>,

    exit: bool,
    first_configure: bool,
    pool: SlotPool,
    window_buffer: Option<Buffer>,
    window: Window,
    bg_effect_surface: ext_background_effect_surface_v1::ExtBackgroundEffectSurfaceV1,
    keyboard: Option<wl_keyboard::WlKeyboard>,

    decoder: Decoder<'static>,
    frame_num: u32,
    start_time: Instant,
}

impl App {
    fn attach_buffer(&mut self) {
        let width = self.decoder.width;
        let height = self.decoder.height;
        let stride = width as i32 * 4;

        let (buffer, canvas) = self
            .pool
            .create_buffer(
                width as i32,
                height as i32,
                stride,
                wl_shm::Format::Argb8888,
            )
            .unwrap();

        // Fully transparent.
        canvas.fill(0);

        self.window
            .wl_surface()
            .damage_buffer(0, 0, width as i32, height as i32);
        buffer.attach_to(self.window.wl_surface()).unwrap();
        self.window_buffer = Some(buffer);
    }

    /// Returns false when there are no more frames.
    fn advance_frame(&mut self) -> bool {
        if self.decoder.data.is_empty() {
            return false;
        }

        let region = self.compositor.wl_compositor().create_region(&self.qh, ());

        let w = usize::from(self.decoder.width);
        let h = usize::from(self.decoder.height);
        let frame_len = w * h;

        let mut frame = self.decoder.next_frame();
        let mut total_len = 0;

        let mut x = 0;
        let mut y = 0;

        loop {
            let (len, color) = frame.next_run();
            // println!("{len}, {color}");

            {
                let mut len = len;
                loop {
                    let height = min(len, h - y);
                    // println!("{x}, {y}, {height}");
                    if !color {
                        region.add(x, y as i32, 1, height as i32);
                    }
                    len -= height;
                    y += height;
                    if y == h {
                        x += 1;
                        y = 0;
                    }

                    if len == 0 {
                        break;
                    }
                }
            }

            total_len += len;
            assert!(total_len <= frame_len);
            if total_len == frame_len {
                break;
            }
        }

        self.bg_effect_surface.set_blur_region(Some(&region));
        region.destroy();

        self.window.wl_surface().commit();
        true
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl WindowHandler for App {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _window: &Window,
        _configure: WindowConfigure,
        _serial: u32,
    ) {
        if self.first_configure {
            self.first_configure = false;
            self.attach_buffer();
            self.advance_frame();

            self.start_time = Instant::now();
            let frame_time = Duration::from_secs_f64(1. / self.decoder.fps as f64);
            let timer = Timer::from_duration(frame_time);
            self.loop_handle
                .insert_source(timer, move |_event, _metadata, app: &mut App| {
                    if !app.advance_frame() {
                        println!("done");
                        app.exit = true;
                        return TimeoutAction::Drop;
                    }

                    app.frame_num += 1;
                    TimeoutAction::ToInstant(app.start_time + frame_time * app.frame_num)
                })
                .unwrap();
        }
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            let keyboard = self.seat_state.get_keyboard(qh, &seat, None).unwrap();
            self.keyboard = Some(keyboard);
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard
            && let Some(kbd) = self.keyboard.take()
        {
            kbd.release();
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for App {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _keysyms: &[Keysym],
    ) {
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _: u32,
    ) {
    }

    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        match event.keysym {
            Keysym::Escape | Keysym::q => {
                self.exit = true;
            }
            _ => {}
        }
    }

    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _event: KeyEvent,
    ) {
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _event: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _modifiers: Modifiers,
        _raw_modifiers: RawModifiers,
        _layout: u32,
    ) {
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl BackgroundEffectHandler for App {
    fn background_effect_state(&mut self) -> &mut BackgroundEffectState {
        &mut self.bg_effect
    }

    fn update_capabilities(&mut self) {
        println!(
            "Background effect capabilities: {:?}",
            self.bg_effect.capabilities()
        );
    }
}

impl Dispatch<wl_region::WlRegion, ()> for App {
    fn event(
        _: &mut Self,
        _: &wl_region::WlRegion,
        _: wl_region::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
    }
}

delegate_compositor!(App);
delegate_output!(App);
delegate_shm!(App);
delegate_seat!(App);
delegate_keyboard!(App);
delegate_xdg_shell!(App);
delegate_xdg_window!(App);
delegate_background_effect!(App);
delegate_registry!(App);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState,];
}
