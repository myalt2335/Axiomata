#![allow(unused_unsafe)]
#![allow(static_mut_refs)]

use core::{
    hint::spin_loop,
    sync::atomic::{AtomicBool, AtomicU8, Ordering},
};
use spin::Mutex;
use x86_64::{
    instructions::port::Port,
    registers::model_specific::Msr,
};
use heapless::String as HString;
use alloc::format;
use raw_cpuid::{CpuId, Hypervisor};
use x86::time::rdtsc;

use crate::{timer, memory};

pub static DISPLAY_24H: AtomicBool = AtomicBool::new(false);

static BASE_TIME: Mutex<Option<DateTime>> = Mutex::new(None);
static UPTIME_SECONDS: Mutex<u64> = Mutex::new(0);
const SYNC_LOG_CAP: usize = 10;
#[derive(Copy, Clone, PartialEq, Eq)]
enum TimeSource {
    Rtc = 0,
    PvClock = 1,
}

static TIME_SOURCE: AtomicU8 = AtomicU8::new(TimeSource::Rtc as u8);
static PV_ENABLED: AtomicBool = AtomicBool::new(false);
static PV_OFFSET_NS: Mutex<Option<i128>> = Mutex::new(None);

const MSR_KVM_WALL_CLOCK: u32 = 0x4b56_4d00;
const MSR_KVM_SYSTEM_TIME: u32 = 0x4b56_4d01;

#[repr(align(4096))]
struct AlignedPage([u8; 4096]);

#[repr(C, align(4))]
struct PvClockWallClock {
    version: u32,
    sec: u32,
    nsec: u32,
}

#[repr(C, align(4))]
struct PvClockVcpuTimeInfo {
    version: u32,
    pad0: u32,
    tsc_timestamp: u64,
    system_time: u64,
    tsc_to_system_mul: u32,
    tsc_shift: i8,
    flags: u8,
    pad1: [u8; 2],
}

static mut PVCLOCK_WALL: AlignedPage = AlignedPage([0; 4096]);
static mut PVCLOCK_TIME: AlignedPage = AlignedPage([0; 4096]);

#[derive(Copy, Clone)]
enum SyncReason {
    Auto,
    Manual,
}

#[derive(Copy, Clone)]
struct SyncEntry {
    reason: SyncReason,
    drift_secs: i64,
    since_last_secs: u64,
    before_secs: u64,
    after_secs: u64,
}

struct SyncLog {
    entries: [Option<SyncEntry>; SYNC_LOG_CAP],
    next: usize,
    count: usize,
}

impl SyncLog {
    fn push(&mut self, entry: SyncEntry) {
        self.entries[self.next] = Some(entry);
        self.next = (self.next + 1) % SYNC_LOG_CAP;
        self.count = (self.count + 1).min(SYNC_LOG_CAP);
    }

    fn clear(&mut self) {
        self.entries = [None; SYNC_LOG_CAP];
        self.next = 0;
        self.count = 0;
    }

    fn iter(&self) -> impl Iterator<Item = SyncEntry> + '_ {
        (0..self.count).filter_map(move |i| {
            let idx = (self.next + SYNC_LOG_CAP - self.count + i) % SYNC_LOG_CAP;
            self.entries[idx]
        })
    }
}

static SYNC_LOG: Mutex<SyncLog> = Mutex::new(SyncLog {
    entries: [None; SYNC_LOG_CAP],
    next: 0,
    count: 0,
});

static LAST_SYNC_TICK: Mutex<Option<u64>> = Mutex::new(None);
static NEXT_SYNC_TICK: Mutex<u64> = Mutex::new(0);

#[derive(Copy, Clone)]
pub struct DateTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

fn is_leap_year(year: u64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

const MONTH_DAYS: [u64; 12] = [31,28,31,30,31,30,31,31,30,31,30,31];

fn days_in_year(year: u64) -> u64 {
    if is_leap_year(year) { 366 } else { 365 }
}

fn days_in_month(year: u64, month: u64) -> u64 {
    let mut days = MONTH_DAYS[(month - 1) as usize];
    if month == 2 && is_leap_year(year) {
        days += 1;
    }
    days
}

fn read_rtc_register(reg: u8) -> u8 {
    unsafe {
        let mut cmos_address = Port::<u8>::new(0x70);
        let mut cmos_data = Port::<u8>::new(0x71);
        cmos_address.write(reg);
        cmos_data.read()
    }
}

fn read_rtc_status_a() -> u8 {
    read_rtc_register(0x0A)
}

fn read_rtc_status_b() -> u8 {
    read_rtc_register(0x0B)
}

fn bcd_to_binary(value: u8) -> u8 {
    ((value / 16) * 10) + (value & 0xF)
}

#[derive(Copy, Clone)]
struct RawRtc {
    sec: u8,
    min: u8,
    hour: u8,
    day: u8,
    month: u8,
    year: u8,
    status_b: u8,
}

fn read_rtc_snapshot() -> RawRtc {
    let sec = read_rtc_register(0x00);
    let min = read_rtc_register(0x02);
    let hour = read_rtc_register(0x04);
    let day = read_rtc_register(0x07);
    let month = read_rtc_register(0x08);
    let year = read_rtc_register(0x09);
    let status_b = read_rtc_status_b();

    RawRtc { sec, min, hour, day, month, year, status_b }
}

fn decode_rtc(raw: RawRtc) -> DateTime {
    let bcd_mode = (raw.status_b & 0x04) == 0;
    let hour_24 = (raw.status_b & 0x02) != 0;

    let mut sec = raw.sec;
    let mut min = raw.min;
    let mut hour = raw.hour;
    let mut day = raw.day;
    let mut month = raw.month;
    let mut year = raw.year;

    if bcd_mode {
        sec = bcd_to_binary(sec);
        min = bcd_to_binary(min);
        hour = bcd_to_binary(hour & 0x7F); 
        day = bcd_to_binary(day);
        month = bcd_to_binary(month);
        year = bcd_to_binary(year);
    }

    if !hour_24 {
        let pm = (raw.hour & 0x80) != 0;
        if pm && hour < 12 {
            hour = hour.wrapping_add(12);
        } else if !pm && hour == 12 {
            hour = 0;
        }
    }

    DateTime {
        year: year as u16 + 2000,
        month,
        day,
        hour,
        minute: min,
        second: sec,
    }
}

fn read_rtc_time() -> DateTime {
    const MAX_TRIES: usize = 5;

    for _ in 0..MAX_TRIES {
        
        for _ in 0..1000 {
            if read_rtc_status_a() & 0x80 == 0 {
                break;
            }
            spin_loop();
        }

        let a = read_rtc_snapshot();
        let b = read_rtc_snapshot();
        if a.sec == b.sec
            && a.min == b.min
            && a.hour == b.hour
            && a.day == b.day
            && a.month == b.month
            && a.year == b.year
        {
            return decode_rtc(a);
        }
    }

    
    decode_rtc(read_rtc_snapshot())
}

fn read_pvclock_time() -> Option<DateTime> {
    if !PV_ENABLED.load(Ordering::Acquire) {
        return None;
    }
    let mono_ns = read_pvclock_monotonic_ns()?;
    let offset = PV_OFFSET_NS.lock().as_ref().copied()?;
    let now_ns = (mono_ns as i128).checked_add(offset)?;
    if now_ns < 0 {
        return None;
    }
    let secs = (now_ns / 1_000_000_000) as u64;
    let (year, month, day, hour, minute, second) = secs_to_ymd_hms(secs);
    Some(DateTime {
        year: year as u16,
        month: month as u8,
        day: day as u8,
        hour: hour as u8,
        minute: minute as u8,
        second: second as u8,
    })
}

fn read_wall_clock() -> DateTime {
    if current_source() == TimeSource::PvClock {
        if let Some(dt) = read_pvclock_time() {
            return dt;
        }
        disable_pvclock();
        TIME_SOURCE.store(TimeSource::Rtc as u8, Ordering::Relaxed);
    }
    read_rtc_time()
}

fn lcg32(x: u64) -> u64 {
    let v = x.wrapping_mul(6364136223846793005).wrapping_add(1);
    v >> 32
}

fn current_source() -> TimeSource {
    match TIME_SOURCE.load(Ordering::Relaxed) {
        1 => TimeSource::PvClock,
        _ => TimeSource::Rtc,
    }
}

fn set_source(src: TimeSource) {
    match src {
        TimeSource::Rtc => {
            disable_pvclock();
            TIME_SOURCE.store(TimeSource::Rtc as u8, Ordering::Relaxed);
        }
        TimeSource::PvClock => {
            if enable_pvclock_if_possible().is_ok() {
                TIME_SOURCE.store(TimeSource::PvClock as u8, Ordering::Relaxed);
            } else {
                TIME_SOURCE.store(TimeSource::Rtc as u8, Ordering::Relaxed);
            }
        }
    }
}

fn pick_next_interval_secs(now_tick: u64, drift_abs_secs: u64) -> u64 {
    let base = if drift_abs_secs > 10 {
        25
    } else if drift_abs_secs > 5 {
        35
    } else if drift_abs_secs > 2 {
        45
    } else {
        60
    };

    let jitter_span = 15u64;
    let seed = now_tick ^ drift_abs_secs.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let jitter = lcg32(seed) % (jitter_span + 1);

    base + jitter
}

fn compute_next_sync_tick(now_tick: u64, drift_abs_secs: u64) -> u64 {
    let interval_secs = pick_next_interval_secs(now_tick, drift_abs_secs);
    now_tick + interval_secs * timer::frequency() as u64
}

fn hypervisor_is_kvm_like() -> bool {
    let cpuid = CpuId::new();
    let Some(info) = cpuid.get_hypervisor_info() else { return false; };
    match info.identify() {
        Hypervisor::KVM | Hypervisor::QEMU => true,
        _ => false,
    }
}

fn disable_pvclock() {
    PV_ENABLED.store(false, Ordering::Release);
    *PV_OFFSET_NS.lock() = None;
    unsafe {
        let mut msr = Msr::new(MSR_KVM_SYSTEM_TIME);
        msr.write(0);
    }
}

fn pvclock_phys(ptr: *const u8) -> Option<u64> {
    memory::phys_addr_of(ptr)
}

fn read_pvclock_wall_ns() -> Option<u64> {
    let wall: &PvClockWallClock = unsafe { &*(PVCLOCK_WALL.0.as_ptr() as *const PvClockWallClock) };
    for _ in 0..100 {
        let v1 = unsafe { core::ptr::read_volatile(&wall.version) };
        if v1 & 1 != 0 {
            spin_loop();
            continue;
        }
        let sec = unsafe { core::ptr::read_volatile(&wall.sec) } as u64;
        let nsec = unsafe { core::ptr::read_volatile(&wall.nsec) } as u64;
        let v2 = unsafe { core::ptr::read_volatile(&wall.version) };
        if v1 == v2 && v1 != 0 {
            return Some(sec.saturating_mul(1_000_000_000).saturating_add(nsec));
        }
    }
    None
}

fn tsc_delta_to_ns(delta: u64, mul: u32, shift: i8) -> u64 {
    let mut val = delta as i128;
    if shift < 0 {
        val >>= (-shift) as usize;
    } else {
        val <<= shift as usize;
    }
    let prod = (val * mul as i128) >> 32;
    if prod < 0 { 0 } else { prod as u64 }
}

fn read_pvclock_monotonic_ns() -> Option<u64> {
    let info: &PvClockVcpuTimeInfo = unsafe { &*(PVCLOCK_TIME.0.as_ptr() as *const PvClockVcpuTimeInfo) };
    for _ in 0..100 {
        let v1 = unsafe { core::ptr::read_volatile(&info.version) };
        if v1 & 1 != 0 {
            spin_loop();
            continue;
        }
        let tsc_ts = unsafe { core::ptr::read_volatile(&info.tsc_timestamp) };
        let sys_time = unsafe { core::ptr::read_volatile(&info.system_time) };
        let mul = unsafe { core::ptr::read_volatile(&info.tsc_to_system_mul) };
        let shift = unsafe { core::ptr::read_volatile(&info.tsc_shift) };
        let v2 = unsafe { core::ptr::read_volatile(&info.version) };
        if v1 != v2 || v1 == 0 {
            continue;
        }
        let tsc_now = unsafe { rdtsc() };
        let delta = tsc_now.wrapping_sub(tsc_ts);
        let ns = sys_time.saturating_add(tsc_delta_to_ns(delta, mul, shift));
        return Some(ns);
    }
    None
}

fn enable_pvclock_if_possible() -> Result<(), &'static str> {
    if !hypervisor_is_kvm_like() {
        return Err("hypervisor not KVM/QEMU");
    }

    let wall_ptr = unsafe { PVCLOCK_WALL.0.as_ptr() };
    let time_ptr = unsafe { PVCLOCK_TIME.0.as_ptr() };
    let wall_phys = pvclock_phys(wall_ptr).ok_or("no phys offset")?;
    let time_phys = pvclock_phys(time_ptr).ok_or("no phys offset")?;

    
    unsafe {
        core::ptr::write_bytes(PVCLOCK_WALL.0.as_mut_ptr(), 0, PVCLOCK_WALL.0.len());
        core::ptr::write_bytes(PVCLOCK_TIME.0.as_mut_ptr(), 0, PVCLOCK_TIME.0.len());
    }

    unsafe {
        let mut wall_msr = Msr::new(MSR_KVM_WALL_CLOCK);
        let mut time_msr = Msr::new(MSR_KVM_SYSTEM_TIME);
        wall_msr.write(wall_phys);
        
        time_msr.write(time_phys | 1);
    }

    let wall_ns = read_pvclock_wall_ns().ok_or("wall clock not populated")?;
    let mono_ns = read_pvclock_monotonic_ns().ok_or("vcpu time not populated")?;
    let offset = wall_ns as i128 - mono_ns as i128;
    *PV_OFFSET_NS.lock() = Some(offset);
    PV_ENABLED.store(true, Ordering::Release);
    Ok(())
}
pub fn init_time() {
    let rtc = read_wall_clock();
    let mut base = BASE_TIME.lock();
    *base = Some(rtc);
    let mut uptime = UPTIME_SECONDS.lock();
    *uptime = 0;
    let mut log = SYNC_LOG.lock();
    log.clear();
    let now_tick = timer::ticks();
    *LAST_SYNC_TICK.lock() = Some(now_tick);
    *NEXT_SYNC_TICK.lock() = compute_next_sync_tick(now_tick, 0);
}

pub fn tick_second() {
    {
        let mut uptime = UPTIME_SECONDS.lock();
        *uptime += 1;
    } 
    maybe_auto_resync();
}

pub fn current_time_secs() -> Option<u64> {
    let base = BASE_TIME.lock();
    base.as_ref().map(|b| {
        let base_seconds = ymd_hms_to_secs(b.year as u64, b.month as u64, b.day as u64, b.hour as u64, b.minute as u64, b.second as u64);
        let uptime = *UPTIME_SECONDS.lock();
        base_seconds + uptime
    })
}

fn perform_resync(reason: SyncReason, now_tick: u64) -> Option<u64> {
    let before_secs = current_time_secs()?;
    let rtc = read_wall_clock();
    let rtc_secs = ymd_hms_to_secs(
        rtc.year as u64,
        rtc.month as u64,
        rtc.day as u64,
        rtc.hour as u64,
        rtc.minute as u64,
        rtc.second as u64,
    );

    let drift = rtc_secs as i64 - before_secs as i64;
    let drift_abs = if drift < 0 { (-drift) as u64 } else { drift as u64 };

    let since_last_secs = {
        let mut last = LAST_SYNC_TICK.lock();
        let freq = timer::frequency() as u64;
        let since = last.map(|prev| now_tick.saturating_sub(prev) / freq).unwrap_or(0);
        *last = Some(now_tick);
        since
    };

    {
        let mut base = BASE_TIME.lock();
        *base = Some(rtc);
    }
    {
        let mut uptime = UPTIME_SECONDS.lock();
        *uptime = 0;
    }
    {
        let mut log = SYNC_LOG.lock();
        log.push(SyncEntry {
            reason,
            drift_secs: drift,
            since_last_secs,
            before_secs,
            after_secs: rtc_secs,
        });
    }

    Some(drift_abs)
}

fn maybe_auto_resync() {
    let now_tick = timer::ticks();
    let mut next = NEXT_SYNC_TICK.lock();
    if *next == 0 {
        *next = compute_next_sync_tick(now_tick, 0);
        return;
    }
    if now_tick < *next {
        return;
    }
    drop(next);

    if let Some(drift_abs) = perform_resync(SyncReason::Auto, now_tick) {
        let next_tick = compute_next_sync_tick(now_tick, drift_abs);
        *NEXT_SYNC_TICK.lock() = next_tick;
    } else {
        *NEXT_SYNC_TICK.lock() = compute_next_sync_tick(now_tick, 0);
    }
}

fn ymd_hms_to_secs(y: u64, m: u64, d: u64, h: u64, min: u64, s: u64) -> u64 {
    let mut days = 0u64;

    for year in 1970..y {
        days += days_in_year(year);
    }

    for month in 1..m {
        days += days_in_month(y, month);
    }

    days += d - 1;

    days * 86400 + h * 3600 + min * 60 + s
}

fn secs_to_ymd_hms(mut secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    let mut year = 1970u64;
    let mut days = secs / 86400;
    secs %= 86400;

    loop {
        let dy = days_in_year(year);
        if days >= dy {
            days -= dy;
            year += 1;
        } else {
            break;
        }
    }

    let mut month = 1u64;
    loop {
        let dm = days_in_month(year, month);
        if days >= dm {
            days -= dm;
            month += 1;
        } else {
            break;
        }
    }

    let day = days + 1;

    let hour = secs / 3600;
    secs %= 3600;
    let minute = secs / 60;
    let second = secs % 60;

    (year, month, day, hour, minute, second)
}


pub fn time_cmd(args: &[&str]) {
    match args.get(0).copied() {
        Some("help") => {
            crate::console::write_line("Usage: os time [12hr|24hr|sync|log|source|help]");
            crate::console::write_line("  12hr   Set display format to 12-hour mode");
            crate::console::write_line("  24hr   Set display format to 24-hour mode");
            crate::console::write_line("  sync   Resync OS time to RTC time if drift detected");
            crate::console::write_line("  log    Show last 10 RTC re-syncs");
            crate::console::write_line("  source [rtc|pvclock|show]  Set or show time source");
            crate::console::write_line("  help   Show this message");
        }
        Some("24hr") => {
            DISPLAY_24H.store(true, Ordering::Relaxed);
            crate::console::write_line("Set time format: 24-hour");
        }
        Some("12hr") => {
            DISPLAY_24H.store(false, Ordering::Relaxed);
            crate::console::write_line("Set time format: 12-hour");
        }
        Some("sync") => {
            if let Some(current_secs) = current_time_secs() {
                let rtc = read_rtc_time();
                let rtc_secs = ymd_hms_to_secs(
                    rtc.year as u64,
                    rtc.month as u64,
                    rtc.day as u64,
                    rtc.hour as u64,
                    rtc.minute as u64,
                    rtc.second as u64,
                );
                let drift = if rtc_secs > current_secs {
                    rtc_secs - current_secs
                } else {
                    current_secs - rtc_secs
                };

                if drift > 2 {
                    let now_tick = timer::ticks();
                    if let Some(drift_abs) = perform_resync(SyncReason::Manual, now_tick) {
                        let mut next = NEXT_SYNC_TICK.lock();
                        *next = compute_next_sync_tick(now_tick, drift_abs);
                    }
                    crate::console::write_line("Time re-synced to RTC.");
                } else {
                    crate::console::write_line("Clock is in sync with RTC.");
                }
            } else {
                crate::console::write_line("Time not initialized yet, initializing...");
                init_time();
            }
        }
        Some("log") => {
            print_sync_log();
        }
        Some("source") => {
            match args.get(1).copied() {
                Some("rtc") => {
                    set_source(TimeSource::Rtc);
                    crate::console::write_line("Time source set to RTC.");
                }
                Some("pvclock") => {
                    match enable_pvclock_if_possible() {
                        Ok(_) => {
                            TIME_SOURCE.store(TimeSource::PvClock as u8, Ordering::Relaxed);
                            crate::console::write_line("Time source set to pvclock.");
                        }
                        Err(reason) => {
                            TIME_SOURCE.store(TimeSource::Rtc as u8, Ordering::Relaxed);
                            crate::console::write_line(&format!("pvclock not available: {}; staying on RTC.", reason));
                        }
                    }
                }
                Some("show") | None => {
                    let src = match current_source() {
                        TimeSource::Rtc => "RTC",
                        TimeSource::PvClock => "pvclock",
                    };
                    crate::console::write_line(&format!("Current time source: {}", src));
                }
                _ => {
                    crate::console::write_line("Usage: os time source [rtc|pvclock|show]");
                }
            }
        }
        _ => {
            let current = current_time_secs();
            match current {
                Some(secs) => {
                    let (y, m, d, h, min, s) = secs_to_ymd_hms(secs);
                    let mut buf = heapless::String::<32>::new();
                    unsafe {
                        if DISPLAY_24H.load(Ordering::Relaxed) {
                            use core::fmt::Write;
                            let _ = write!(&mut buf, "{y:04}-{m:02}-{d:02} {h:02}:{min:02}:{s:02}");
                        } else {
                            let (disp_h, ampm) = if h == 0 {
                                (12, "AM")
                            } else if h < 12 {
                                (h, "AM")
                            } else if h == 12 {
                                (12, "PM")
                            } else {
                                (h - 12, "PM")
                            };
                            use core::fmt::Write;
                            let _ = write!(
                                &mut buf,
                                "{y:04}-{m:02}-{d:02} {disp_h:02}:{min:02}:{s:02} {ampm}"
                            );
                        }
                    }
                    crate::console::write_line(buf.as_str());
                }
                None => crate::console::write_line("Time not initialized yet."),
            }
        }
    }
}

pub fn format_hud_time() -> HString<32> {
    let mut out: HString<32> = HString::new();

    if let Some(secs) = current_time_secs() {
        let (y, m, d, h, min, s) = secs_to_ymd_hms(secs);
        let is_24 = DISPLAY_24H.load(Ordering::Relaxed);

        if is_24 {
            let _ = core::fmt::write(&mut out, format_args!("{:02}/{:02}/{:04} {:02}:{:02}:{:02}", m, d, y, h, min, s));
        } else {
            let (disp_h, ampm) = if h == 0 {
                (12, "AM")
            } else if h < 12 {
                (h, "AM")
            } else if h == 12 {
                (12, "PM")
            } else {
                (h - 12, "PM")
            };
            let _ = core::fmt::write(&mut out, format_args!("{:02}/{:02}/{:04} {:02}:{:02}:{:02} {}", m, d, y, disp_h, min, s, ampm));
        }
    } else {
        let _ = out.push_str("--/--/---- --:--:--");
    }

    out
}

pub fn print_sync_log() {
    let log = SYNC_LOG.lock();
    if log.count == 0 {
        crate::console::write_line("No resyncs recorded yet.");
        return;
    }

    crate::console::write_line("Recent RTC resyncs (oldest -> newest):");
    for entry in log.iter() {
        let (by, bm, bd, bh, bmin, bs) = secs_to_ymd_hms(entry.before_secs);
        let (ay, am, ad, ah, amin, as_) = secs_to_ymd_hms(entry.after_secs);
        let reason = match entry.reason {
            SyncReason::Auto => "auto",
            SyncReason::Manual => "manual",
        };

        use core::fmt::Write;
        let mut line = HString::<128>::new();
        let _ = write!(
            &mut line,
            "{}: drift {:+}s, {}s since last, {:04}-{:02}-{:02} {:02}:{:02}:{:02} -> {:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            reason,
            entry.drift_secs,
            entry.since_last_secs,
            by,
            bm,
            bd,
            bh,
            bmin,
            bs,
            ay,
            am,
            ad,
            ah,
            amin,
            as_
        );
        crate::console::write_line(line.as_str());
    }
}
