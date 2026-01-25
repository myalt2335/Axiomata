use crate::{console, memory, timer};
use crate::console::CompositorMode;
use spin::Mutex;
use x86_64::instructions::{hlt, interrupts};

pub const USAGE: &str = "Usage: debug compdemo [toggle]";
const CDMO_APP_ID: memory::AppId = 0x43444d4f;
const CDMO_APP_OVERHEAD: usize = 8 * 1024;
const CDMO_FRAMES: usize = 90;
const CDMO_DELAY_MS: u64 = 20;
const CDMO_BLUE_COLOR: u32 = 0x3355FF;
const CDMO_GREEN_COLOR: u32 = 0x33CC66;
const CDMO_OPAQUE_COLOR: u32 = 0xFF6633;
const CDMO_BLUE_ALPHA: u8 = 128;

struct CdmoState {
    prev_mode: CompositorMode,
    blue_id: console::LayerId,
    opaque_id: console::LayerId,
    green_id: console::LayerId,
    blue_w: usize,
    blue_h: usize,
    opaque_w: usize,
    opaque_h: usize,
    opaque_x: usize,
    opaque_y: usize,
    blue_y: usize,
    blue_left: usize,
    blue_right: usize,
    frames: usize,
    half: usize,
    step: usize,
    delay_ticks: u64,
    next_tick: u64,
}

static CDMO_STATE: Mutex<Option<CdmoState>> = Mutex::new(None);

pub fn tick() {
    cdmo_tick();
}

pub fn command(args: &[&str]) {
    if args.len() == 1 && args[0] == "toggle" {
        match cdmo_toggle() {
            Ok(true) => console::write_line("cdmo: loop on"),
            Ok(false) => console::write_line("cdmo: loop off"),
            Err(msg) => console::write_line(msg),
        }
        return;
    }
    if args.is_empty() {
        if cdmo_is_active() {
            return;
        }
        if let Err(msg) = run_compositor_demo() {
            console::write_line(msg);
        }
        return;
    }
    console::write_line(USAGE);
}

fn cdmo_delay_ticks() -> u64 {
    let freq = timer::frequency() as u64;
    let ticks = (CDMO_DELAY_MS.saturating_mul(freq)) / 1000;
    ticks.max(1)
}

fn cdmo_render_frame(state: &CdmoState, step: usize) {
    let half = state.half.max(1);
    let phase = if step <= state.half { step } else { state.frames - step };
    let span = state.blue_right.saturating_sub(state.blue_left);
    let blue_x = state.blue_left + (span * phase) / half;
    console::layer_set_pos(state.blue_id, blue_x, state.blue_y);
    console::layer_fill_rect(state.green_id, 0, 0, state.opaque_w, state.opaque_h, CDMO_OPAQUE_COLOR);
    let overlap_x0 = blue_x.max(state.opaque_x);
    let overlap_y0 = state.blue_y.max(state.opaque_y);
    let overlap_x1 = blue_x.saturating_add(state.blue_w).min(state.opaque_x.saturating_add(state.opaque_w));
    let overlap_y1 = state.blue_y.saturating_add(state.blue_h).min(state.opaque_y.saturating_add(state.opaque_h));
    if overlap_x1 > overlap_x0 && overlap_y1 > overlap_y0 {
        let local_x = overlap_x0.saturating_sub(state.opaque_x);
        let local_y = overlap_y0.saturating_sub(state.opaque_y);
        let w = overlap_x1 - overlap_x0;
        let h = overlap_y1 - overlap_y0;
        console::layer_fill_rect(state.green_id, local_x, local_y, w, h, CDMO_GREEN_COLOR);
    }
}

fn cdmo_step(state: &mut CdmoState) {
    let step = state.step;
    cdmo_render_frame(state, step);
    console::present();
    state.step = if step >= state.frames { 0 } else { step + 1 };
}

fn cdmo_setup() -> Result<CdmoState, &'static str> {
    let Some(stats) = console::display_buffer_stats() else {
        return Err("cdmo: display not ready.");
    };

    let prev_mode = console::compositor_mode();
    if prev_mode != CompositorMode::Layered {
        console::set_compositor_mode(CompositorMode::Layered);
    }
    if console::compositor_mode() != CompositorMode::Layered {
        return Err("cdmo: layered compositor unavailable.");
    }

    let width = stats.width_px;
    let height = stats.height_px;
    let bpp = stats.bytes_per_pixel;
    if width == 0 || height == 0 || bpp == 0 {
        if prev_mode != CompositorMode::Layered {
            console::set_compositor_mode(prev_mode);
        }
        return Err("cdmo: display format unsupported.");
    }

    let demo_w = (width / 3).max(120).min(width.saturating_sub(16));
    let demo_h = (height / 3).max(80).min(height.saturating_sub(16));
    if demo_w < 48 || demo_h < 48 {
        if prev_mode != CompositorMode::Layered {
            console::set_compositor_mode(prev_mode);
        }
        return Err("cdmo: display too small for demo.");
    }

    let origin_x = width.saturating_sub(demo_w.saturating_add(8));
    let origin_y = 8.min(height.saturating_sub(demo_h));
    const MIN_OPAQUE_W: usize = 48;
    const MIN_OPAQUE_H: usize = 48;
    const MIN_BLUE_W: usize = 24;
    const MIN_BLUE_H: usize = 24;
    let mut opaque_w = (demo_w / 2).min(160);
    let mut opaque_h = (demo_h / 2).min(120);
    let (blue_w, blue_h) = loop {
        let blue_w = (opaque_w * 2 / 3).max(48).min(opaque_w.saturating_sub(8));
        let blue_h = (opaque_h * 2 / 3).max(32).min(opaque_h.saturating_sub(8));
        if opaque_w < MIN_OPAQUE_W || opaque_h < MIN_OPAQUE_H || blue_w < MIN_BLUE_W || blue_h < MIN_BLUE_H {
            if prev_mode != CompositorMode::Layered {
                console::set_compositor_mode(prev_mode);
            }
            return Err("cdmo: display too small for demo.");
        }

        let bytes_per_layer = opaque_w.saturating_mul(opaque_h).saturating_mul(bpp);
        let bytes_blue = blue_w.saturating_mul(blue_h).saturating_mul(bpp);
        let required = bytes_per_layer.saturating_mul(2).saturating_add(bytes_blue);
        let quota = required.saturating_add(CDMO_APP_OVERHEAD);
        let _ = memory::unregister_app(CDMO_APP_ID);
        if memory::register_app(CDMO_APP_ID, quota) {
            break (blue_w, blue_h);
        }

        if opaque_w <= MIN_OPAQUE_W || opaque_h <= MIN_OPAQUE_H {
            if prev_mode != CompositorMode::Layered {
                console::set_compositor_mode(prev_mode);
            }
            return Err("cdmo: insufficient arena memory.");
        }
        opaque_w = opaque_w.saturating_sub(16).max(MIN_OPAQUE_W);
        opaque_h = opaque_h.saturating_sub(12).max(MIN_OPAQUE_H);
    };

    let opaque_x = origin_x + (demo_w.saturating_sub(opaque_w)) / 2;
    let opaque_y = origin_y + (demo_h.saturating_sub(opaque_h)) / 2;
    let blue_y = opaque_y + (opaque_h.saturating_sub(blue_h)) / 2;
    let blue_left = origin_x;
    let blue_right = origin_x + demo_w.saturating_sub(blue_w);

    let blue_id = console::create_layer_in_app_heap(blue_w, blue_h, blue_left, blue_y, 0, CDMO_BLUE_ALPHA, CDMO_APP_ID);
    let opaque_id = console::create_layer_in_app_heap(opaque_w, opaque_h, opaque_x, opaque_y, 10, 255, CDMO_APP_ID);
    let green_id = console::create_layer_in_app_heap(opaque_w, opaque_h, opaque_x, opaque_y, 20, 180, CDMO_APP_ID);

    let (blue_id, opaque_id, green_id) = match (blue_id, opaque_id, green_id) {
        (Some(blue_id), Some(opaque_id), Some(green_id)) => (blue_id, opaque_id, green_id),
        (blue_id, opaque_id, green_id) => {
            if let Some(id) = blue_id {
                console::destroy_layer(id);
            }
            if let Some(id) = opaque_id {
                console::destroy_layer(id);
            }
            if let Some(id) = green_id {
                console::destroy_layer(id);
            }
            let _ = memory::unregister_app(CDMO_APP_ID);
            if prev_mode != CompositorMode::Layered {
                console::set_compositor_mode(prev_mode);
            }
            return Err("cdmo: failed to allocate demo layers.");
        }
    };

    console::layer_fill_rect(blue_id, 0, 0, blue_w, blue_h, CDMO_BLUE_COLOR);
    console::layer_fill_rect(opaque_id, 0, 0, opaque_w, opaque_h, CDMO_OPAQUE_COLOR);
    console::layer_fill_rect(green_id, 0, 0, opaque_w, opaque_h, CDMO_OPAQUE_COLOR);
    console::present();

    let delay_ticks = cdmo_delay_ticks();
    let next_tick = timer::ticks().saturating_add(delay_ticks);

    Ok(CdmoState {
        prev_mode,
        blue_id,
        opaque_id,
        green_id,
        blue_w,
        blue_h,
        opaque_w,
        opaque_h,
        opaque_x,
        opaque_y,
        blue_y,
        blue_left,
        blue_right,
        frames: CDMO_FRAMES,
        half: CDMO_FRAMES / 2,
        step: 0,
        delay_ticks,
        next_tick,
    })
}

fn cdmo_shutdown(state: &mut CdmoState) {
    console::destroy_layer(state.blue_id);
    console::destroy_layer(state.opaque_id);
    console::destroy_layer(state.green_id);
    console::present();

    let _ = memory::unregister_app(CDMO_APP_ID);

    if state.prev_mode != CompositorMode::Layered {
        console::set_compositor_mode(state.prev_mode);
    }
}

fn cdmo_is_active() -> bool {
    interrupts::without_interrupts(|| CDMO_STATE.lock().is_some())
}

fn cdmo_toggle() -> Result<bool, &'static str> {
    let mut result = Ok(false);
    interrupts::without_interrupts(|| {
        let mut state = CDMO_STATE.lock();
        if let Some(mut active) = state.take() {
            cdmo_shutdown(&mut active);
            result = Ok(false);
            return;
        }
        match cdmo_setup() {
            Ok(new_state) => {
                *state = Some(new_state);
                result = Ok(true);
            }
            Err(msg) => {
                result = Err(msg);
            }
        }
    });
    result
}

fn cdmo_tick() {
    interrupts::without_interrupts(|| {
        let mut state = CDMO_STATE.lock();
        let Some(state) = state.as_mut() else {
            return;
        };
        let now = timer::ticks();
        if now < state.next_tick {
            return;
        }
        state.next_tick = now.saturating_add(state.delay_ticks);
        cdmo_step(state);
    });
}

fn run_compositor_demo() -> Result<(), &'static str> {
    let mut state = cdmo_setup()?;
    let mut next_tick = timer::ticks();
    if state.half > 0 {
        for _ in 0..=state.frames {
            while timer::ticks() < next_tick {
                hlt();
            }
            cdmo_step(&mut state);
            next_tick = next_tick.saturating_add(state.delay_ticks);
        }
    }
    cdmo_shutdown(&mut state);
    Ok(())
}
