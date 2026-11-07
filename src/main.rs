//! PA120 数显。四格：CPU 温度、CPU 占用、GPU 温度、GPU 占用。
//! 颜色跟着各自的占用走。开机后台运行。
//! USB HID 0416:8001，84 颗灯。CPU 封装温度走 PawnIO 的 IntelMSR 模块。

#![windows_subsystem = "windows"]

use std::env;
use std::mem::MaybeUninit;
use std::thread;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use hidapi::{HidApi, HidDevice, HidError};

const VID: u16 = 0x0416;
const PID: u16 = 0x8001;
const LEDS: usize = 84;
const MSR_MODULE: &[u8] = include_bytes!("../third_party/IntelMSR.bin");

const MSR_TEMP_TARGET: u64 = 0x1A2;
const MSR_PKG_THERM: u64 = 0x1B1;

// Wire index <- logical index. Physical LED i gets logical color REMAP[i].
const REMAP: [u8; LEDS] = [
    1, 0, 15, 10, 11, 16, 14, 13, 12, 22, 17, 18, 23, 21, 20, 19, 29, 24, 25, 30, 28, 27, 26, 4, 5,
    81, 80, 36, 31, 32, 37, 35, 34, 33, 43, 38, 39, 44, 42, 41, 40, 6, 9, 75, 76, 77, 79, 74, 73,
    78, 68, 69, 70, 72, 67, 66, 71, 82, 83, 7, 8, 61, 62, 63, 65, 60, 59, 64, 54, 55, 56, 58, 53,
    52, 57, 47, 48, 49, 51, 46, 45, 50, 2, 3,
];

// 7-segment bit0=a .. bit6=g.
const GLYPH: [u8; 10] = [
    0b011_1111, 0b000_0110, 0b101_1011, 0b100_1111, 0b110_0110, 0b110_1101, 0b111_1101, 0b000_0111,
    0b111_1111, 0b110_1111,
];

const CPU_TEMP: [[u8; 7]; 3] = [
    [10, 11, 12, 13, 14, 15, 16],
    [17, 18, 19, 20, 21, 22, 23],
    [24, 25, 26, 27, 28, 29, 30],
];
const CPU_LOAD: [[u8; 7]; 2] = [[31, 32, 33, 34, 35, 36, 37], [38, 39, 40, 41, 42, 43, 44]];
const GPU_TEMP: [[u8; 7]; 3] = [
    [45, 46, 47, 48, 49, 50, 51],
    [52, 53, 54, 55, 56, 57, 58],
    [59, 60, 61, 62, 63, 64, 65],
];
const GPU_LOAD: [[u8; 7]; 2] = [[66, 67, 68, 69, 70, 71, 72], [73, 74, 75, 76, 77, 78, 79]];

const TEMP_EDGE: usize = 8;
const ACTIVITY_GFX: usize = 19;

fn main() -> ExitCode {
    if already_running() {
        return ExitCode::SUCCESS;
    }
    let brightness = match brightness_arg(env::args().skip(1)) {
        Ok(v) => v,
        Err(e) => {
            complain(&e);
            return ExitCode::from(2);
        }
    };

    let api = match HidApi::new() {
        Ok(api) => api,
        Err(e) => {
            complain(&format!("HID 初始化失败：{e}"));
            return ExitCode::from(1);
        }
    };
    let mut panel = match Panel::open(&api) {
        Ok(p) => p,
        Err(e) => {
            complain(&e);
            return ExitCode::from(1);
        }
    };
    if let Some(pm) = panel.handshake() {
        if !(16..=31).contains(&pm) {
            complain(&format!("这不是 PA120 数显（型号码 {pm}），没有往屏幕写数据"));
            return ExitCode::from(1);
        }
    }

    install_autostart();
    let cpu = CpuTemp::open().ok();
    let gpu = Gpu::open().ok();
    let mut app = App {
        panel,
        cpu,
        gpu,
        prev: Times::now(),
        brightness,
        tip: "pa120-lite".into(),
    };
    // First sample needs a one-second gap so CPU 占用不是 0。
    thread::sleep(Duration::from_secs(1));
    if let Err(e) = app.tick() {
        complain(&format!("写屏失败：{e}"));
        return ExitCode::from(1);
    }

    let hwnd = match create_tray_window() {
        Ok(hwnd) => hwnd,
        Err(()) => {
            // ponytail: no tray if the window class fails. Quit from Task Manager.
            hidden_loop(&mut app, core::ptr::null_mut());
            return ExitCode::SUCCESS;
        }
    };
    let _ = tray(hwnd, &app.tip, 0);
    hidden_loop(&mut app, hwnd);
    let _ = tray(hwnd, "", 2);
    ExitCode::SUCCESS
}

struct App {
    panel: Panel,
    cpu: Option<CpuTemp>,
    gpu: Option<Gpu>,
    prev: Times,
    brightness: u8,
    tip: String,
}

impl App {
    fn tick(&mut self) -> Result<(), HidError> {
        let now = Times::now();
        let cpu_c = self.cpu.as_ref().and_then(|c| c.celsius());
        let cpu_pct = now.busy_percent(&self.prev);
        let (gpu_c, gpu_pct) = self.gpu.as_ref().map(|g| g.sample()).unwrap_or((None, None));
        let frame = render(cpu_c, cpu_pct, gpu_c, gpu_pct);
        self.panel.send(&frame, self.brightness)?;
        self.tip = format!(
            "CPU {}°C {}%  GPU {}°C {}%",
            fmt_opt(cpu_c),
            fmt_opt(cpu_pct),
            fmt_opt(gpu_c),
            fmt_opt(gpu_pct),
        );
        self.prev = now;
        Ok(())
    }
}

fn fmt_opt(v: Option<impl std::fmt::Display>) -> String {
    v.map(|n| n.to_string()).unwrap_or_else(|| "--".into())
}

fn brightness_arg(args: impl Iterator<Item = String>) -> Result<u8, String> {
    let args: Vec<String> = args.collect();
    let mut brightness = 80u8;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--brightness" => {
                i += 1;
                let raw = args.get(i).ok_or("缺少亮度，范围 0-100")?;
                brightness = raw.parse().map_err(|_| format!("亮度不是数字：{raw}"))?;
                if brightness > 100 {
                    return Err("亮度范围是 0-100".into());
                }
            }
            "-h" | "--help" => {
                return Err("pa120-lite [--brightness 0-100]\n后台运行。托盘图标右键退出。颜色随占用变化。".into());
            }
            other => return Err(format!("不认识的参数 {other}")),
        }
        i += 1;
    }
    Ok(brightness)
}

fn load_color(pct: u32) -> [u8; 3] {
    match pct {
        0..=29 => [0, 255, 255],
        30..=49 => [0, 255, 0],
        50..=69 => [255, 255, 0],
        70..=89 => [255, 110, 0],
        _ => [255, 0, 0],
    }
}

fn render(
    cpu_c: Option<i32>,
    cpu_pct: Option<u32>,
    gpu_c: Option<i32>,
    gpu_pct: Option<u32>,
) -> [[u8; 3]; LEDS] {
    let cpu_color = load_color(cpu_pct.unwrap_or(0));
    let gpu_color = load_color(gpu_pct.unwrap_or(0));
    let mut logical = [[0u8; 3]; LEDS];
    if cpu_c.is_some() || cpu_pct.is_some() {
        logical[0] = cpu_color;
        logical[1] = cpu_color;
    }
    if let Some(t) = cpu_c {
        logical[4] = cpu_color;
        paint3(&mut logical, CPU_TEMP, t, cpu_color);
    }
    if let Some(p) = cpu_pct {
        logical[6] = cpu_color;
        paint_pct(&mut logical, CPU_LOAD, [80, 81], p, cpu_color);
    }
    if gpu_c.is_some() || gpu_pct.is_some() {
        logical[2] = gpu_color;
        logical[3] = gpu_color;
    }
    if let Some(t) = gpu_c {
        logical[7] = gpu_color;
        paint3(&mut logical, GPU_TEMP, t, gpu_color);
    }
    if let Some(p) = gpu_pct {
        logical[9] = gpu_color;
        paint_pct(&mut logical, GPU_LOAD, [82, 83], p, gpu_color);
    }
    let mut wire = [[0u8; 3]; LEDS];
    for (dst, src) in wire.iter_mut().zip(REMAP) {
        *dst = logical[src as usize];
    }
    wire
}

fn paint3(buf: &mut [[u8; 3]; LEDS], digits: [[u8; 7]; 3], value: i32, color: [u8; 3]) {
    let v = value.clamp(0, 999) as u32;
    let chars = [v / 100, (v / 10) % 10, v % 10];
    let mut skip = true;
    for (i, leds) in digits.into_iter().enumerate() {
        if skip && i < 2 && chars[i] == 0 {
            continue;
        }
        skip = false;
        paint_digit(buf, leds, chars[i] as u8, color);
    }
}

fn paint_pct(
    buf: &mut [[u8; 3]; LEDS],
    digits: [[u8; 7]; 2],
    partial: [u8; 2],
    value: u32,
    color: [u8; 3],
) {
    let v = value.min(199);
    if v >= 100 {
        buf[partial[0] as usize] = color;
        buf[partial[1] as usize] = color;
        paint_digit(buf, digits[0], ((v - 100) / 10) as u8, color);
        paint_digit(buf, digits[1], ((v - 100) % 10) as u8, color);
    } else if v >= 10 {
        paint_digit(buf, digits[0], (v / 10) as u8, color);
        paint_digit(buf, digits[1], (v % 10) as u8, color);
    } else {
        paint_digit(buf, digits[1], v as u8, color);
    }
}

fn paint_digit(buf: &mut [[u8; 3]; LEDS], leds: [u8; 7], digit: u8, color: [u8; 3]) {
    let bits = GLYPH[digit as usize];
    for (seg, led) in leds.into_iter().enumerate() {
        if bits & (1 << seg) != 0 {
            buf[led as usize] = color;
        }
    }
}

fn scale(channel: u8, brightness: u8) -> u8 {
    (channel as u32 * brightness as u32 * 4 / 1000) as u8
}

struct Panel {
    dev: HidDevice,
    prefix_report_id: bool,
}

impl Panel {
    fn open(api: &HidApi) -> Result<Self, String> {
        let mut found: Vec<_> = api
            .device_list()
            .filter(|d| d.vendor_id() == VID && d.product_id() == PID)
            .collect();
        if found.is_empty() {
            return Err("没找到 PA120（USB 0416:8001）。确认数显线插上了。".into());
        }
        found.sort_by_key(|d| if d.usage_page() == 0xFF00 { 0 } else { 1 });
        let dev = found[0].open_device(api).map_err(|e| {
            format!("打不开数显屏：{e}。先完全退出 ModernTRCC 和 TRCC，它们会占着这块屏。")
        })?;
        Ok(Self {
            dev,
            prefix_report_id: true,
        })
    }

    fn handshake(&mut self) -> Option<u8> {
        let mut pkt = [0u8; 64];
        pkt[0..4].copy_from_slice(&[0xDA, 0xDB, 0xDC, 0xDD]);
        pkt[12] = 0x01;
        thread::sleep(Duration::from_millis(50));
        self.write_report(&pkt).ok()?;
        thread::sleep(Duration::from_millis(200));
        let mut buf = [0u8; 128];
        let n = self.dev.read_timeout(&mut buf, 400).ok()?;
        buf[..n]
            .windows(4)
            .position(|w| w == [0xDA, 0xDB, 0xDC, 0xDD])
            .and_then(|i| buf.get(i + 5).copied())
    }

    fn send(&mut self, wire: &[[u8; 3]; LEDS], brightness: u8) -> Result<(), HidError> {
        let mut body = [0u8; LEDS * 3];
        for (i, rgb) in wire.iter().enumerate() {
            body[i * 3] = scale(rgb[0], brightness);
            body[i * 3 + 1] = scale(rgb[1], brightness);
            body[i * 3 + 2] = scale(rgb[2], brightness);
        }
        let mut packet = [0u8; 20 + LEDS * 3];
        packet[0..4].copy_from_slice(&[0xDA, 0xDB, 0xDC, 0xDD]);
        packet[12] = 0x02;
        let len = (LEDS * 3) as u16;
        packet[16] = len as u8;
        packet[17] = (len >> 8) as u8;
        packet[20..].copy_from_slice(&body);
        for chunk in packet.chunks(64) {
            let mut report = [0u8; 64];
            report[..chunk.len()].copy_from_slice(chunk);
            self.write_report(&report)?;
        }
        thread::sleep(Duration::from_millis(30));
        Ok(())
    }

    fn write_report(&mut self, report: &[u8; 64]) -> Result<(), HidError> {
        if self.prefix_report_id {
            let mut buf = [0u8; 65];
            buf[1..].copy_from_slice(report);
            if self.dev.write(&buf).is_ok() {
                return Ok(());
            }
            self.prefix_report_id = false;
        }
        self.dev.write(report).map(|_| ())
    }
}

struct Times {
    idle: u64,
    total: u64,
}

impl Times {
    fn now() -> Self {
        let mut idle = 0u64;
        let mut kernel = 0u64;
        let mut user = 0u64;
        unsafe {
            GetSystemTimes(&mut idle, &mut kernel, &mut user);
        }
        Self {
            idle,
            total: kernel.saturating_add(user),
        }
    }

    fn busy_percent(&self, prev: &Self) -> Option<u32> {
        let dt = self.total.saturating_sub(prev.total);
        if dt == 0 {
            return None;
        }
        let idle = self.idle.saturating_sub(prev.idle).min(dt);
        Some(((dt - idle) * 100 / dt) as u32)
    }
}

struct CpuTemp {
    handle: *mut core::ffi::c_void,
}

impl CpuTemp {
    fn open() -> Result<Self, String> {
        let path: Vec<u16> = "\\\\?\\GLOBALROOT\\Device\\PawnIO"
            .encode_utf16()
            .chain([0])
            .collect();
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                0xC000_0000,
                0x3,
                core::ptr::null_mut(),
                3,
                0,
                core::ptr::null_mut(),
            )
        };
        if handle.is_null() || handle as usize == usize::MAX {
            return Err(format!(
                "PawnIO 没打开（错误 {}）。需要已安装的 PawnIO 驱动",
                unsafe { GetLastError() }
            ));
        }
        let mut returned = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                handle,
                ioctl_load(),
                MSR_MODULE.as_ptr(),
                MSR_MODULE.len() as u32,
                core::ptr::null_mut(),
                0,
                &mut returned,
                core::ptr::null_mut(),
            )
        };
        if ok == 0 {
            let err = unsafe { GetLastError() };
            unsafe { CloseHandle(handle) };
            return Err(format!("IntelMSR 模块加载失败（错误 {err}）"));
        }
        Ok(Self { handle })
    }

    fn celsius(&self) -> Option<i32> {
        let thread = unsafe { GetCurrentThread() };
        let old = unsafe { SetThreadAffinityMask(thread, 1) };
        let target = self.rdmsr(MSR_TEMP_TARGET);
        let status = self.rdmsr(MSR_PKG_THERM);
        if old != 0 {
            unsafe { SetThreadAffinityMask(thread, old) };
        }
        let target = target?;
        let status = status?;
        if status & (1 << 31) == 0 {
            return None;
        }
        let tj = ((target >> 16) & 0xFF) as i32;
        let delta = ((status >> 16) & 0x7F) as i32;
        (tj > delta && (70..=120).contains(&tj)).then_some(tj - delta)
    }

    fn rdmsr(&self, msr: u64) -> Option<u64> {
        let mut input = [0u8; 32 + 8];
        input[..b"ioctl_read_msr".len()].copy_from_slice(b"ioctl_read_msr");
        input[32..40].copy_from_slice(&msr.to_le_bytes());
        let mut output = [0u8; 8];
        let mut returned = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                self.handle,
                ioctl_exec(),
                input.as_ptr(),
                input.len() as u32,
                output.as_mut_ptr(),
                output.len() as u32,
                &mut returned,
                core::ptr::null_mut(),
            )
        };
        (ok != 0 && returned >= 8).then(|| u64::from_le_bytes(output))
    }
}

impl Drop for CpuTemp {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.handle) };
    }
}

fn ioctl_load() -> u32 {
    (41394 << 16) | (0x821 << 2)
}

fn ioctl_exec() -> u32 {
    (41394 << 16) | (0x841 << 2)
}

#[repr(C)]
struct AdapterInfo {
    size: i32,
    adapter_index: i32,
    udid: [u8; 256],
    bus: i32,
    device: i32,
    function: i32,
    vendor_id: i32,
    name: [u8; 256],
    display: [u8; 256],
    present: i32,
    exist: i32,
    driver_path: [u8; 256],
    driver_path_ext: [u8; 256],
    pnp: [u8; 256],
    os_display_index: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Sensor {
    supported: i32,
    value: i32,
}

#[repr(C)]
struct PmLog {
    size: i32,
    sensors: [Sensor; 256],
}

struct Gpu {
    lib: *mut core::ffi::c_void,
    ctx: *mut core::ffi::c_void,
    destroy: unsafe extern "C" fn(*mut core::ffi::c_void) -> i32,
    query: unsafe extern "C" fn(*mut core::ffi::c_void, i32, *mut PmLog) -> i32,
    adapter: i32,
}

impl Gpu {
    fn open() -> Result<Self, String> {
        let wide: Vec<u16> = "atiadlxx.dll".encode_utf16().chain([0]).collect();
        let lib = unsafe { LoadLibraryW(wide.as_ptr()) };
        if lib.is_null() {
            return Err("系统里没有 atiadlxx.dll".into());
        }
        let create: unsafe extern "C" fn(
            unsafe extern "C" fn(i32) -> *mut core::ffi::c_void,
            i32,
            *mut *mut core::ffi::c_void,
        ) -> i32 = proc(lib, b"ADL2_Main_Control_Create\0")
            .ok_or("ADL 里没有 Main_Control_Create")?;
        let destroy: unsafe extern "C" fn(*mut core::ffi::c_void) -> i32 =
            proc(lib, b"ADL2_Main_Control_Destroy\0").ok_or("ADL 缺少 Destroy")?;
        let count_fn: unsafe extern "C" fn(*mut core::ffi::c_void, *mut i32) -> i32 =
            proc(lib, b"ADL2_Adapter_NumberOfAdapters_Get\0").ok_or("ADL 缺少适配器数量接口")?;
        let info_fn: unsafe extern "C" fn(*mut core::ffi::c_void, *mut AdapterInfo, i32) -> i32 =
            proc(lib, b"ADL2_Adapter_AdapterInfo_Get\0").ok_or("ADL 缺少适配器信息接口")?;
        let query: unsafe extern "C" fn(*mut core::ffi::c_void, i32, *mut PmLog) -> i32 =
            proc(lib, b"ADL2_New_QueryPMLogData_Get\0").ok_or("ADL 缺少 PMLog")?;

        let mut ctx = core::ptr::null_mut();
        let rc = unsafe { create(adl_alloc, 1, &mut ctx) };
        if rc != 0 || ctx.is_null() {
            unsafe { FreeLibrary(lib) };
            return Err(format!("ADL 初始化失败（{rc}）"));
        }
        let mut n = 0i32;
        if unsafe { count_fn(ctx, &mut n) } != 0 || n <= 0 {
            unsafe { destroy(ctx) };
            unsafe { FreeLibrary(lib) };
            return Err("ADL 没有枚举到显卡".into());
        }
        let mut infos = Vec::with_capacity(n as usize);
        for _ in 0..n {
            infos.push(unsafe { MaybeUninit::<AdapterInfo>::zeroed().assume_init() });
        }
        for info in &mut infos {
            info.size = core::mem::size_of::<AdapterInfo>() as i32;
        }
        let bytes = (n as usize * core::mem::size_of::<AdapterInfo>()) as i32;
        if unsafe { info_fn(ctx, infos.as_mut_ptr(), bytes) } != 0 {
            unsafe { destroy(ctx) };
            unsafe { FreeLibrary(lib) };
            return Err("读不到显卡信息".into());
        }
        let mut chosen = None;
        for info in &infos {
            // ADL stores the PCI vendor id as decimal 1002, not 0x1002.
            if info.vendor_id != 1002 || info.present == 0 {
                continue;
            }
            let mut log = unsafe { MaybeUninit::<PmLog>::zeroed().assume_init() };
            log.size = core::mem::size_of::<PmLog>() as i32;
            if unsafe { query(ctx, info.adapter_index, &mut log) } != 0 {
                continue;
            }
            if gpu_temp(&log).is_some() {
                chosen = Some(info.adapter_index);
                break;
            }
        }
        let Some(adapter) = chosen else {
            unsafe { destroy(ctx) };
            unsafe { FreeLibrary(lib) };
            return Err("AMD 驱动没有报出 GPU 温度".into());
        };
        Ok(Self {
            lib,
            ctx,
            destroy,
            query,
            adapter,
        })
    }

    fn sample(&self) -> (Option<i32>, Option<u32>) {
        let mut log = unsafe { MaybeUninit::<PmLog>::zeroed().assume_init() };
        log.size = core::mem::size_of::<PmLog>() as i32;
        if unsafe { (self.query)(self.ctx, self.adapter, &mut log) } != 0 {
            return (None, None);
        }
        (gpu_temp(&log), activity_pct(log.sensors[ACTIVITY_GFX]))
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        unsafe {
            (self.destroy)(self.ctx);
            FreeLibrary(self.lib);
        }
    }
}

fn gpu_temp(log: &PmLog) -> Option<i32> {
    // Edge, then the die, then the hotspot. 6700 XT fills whichever the driver has.
    for index in [TEMP_EDGE, 28, 27] {
        if let Some(t) = temp_c(log.sensors[index]) {
            return Some(t);
        }
    }
    None
}

fn temp_c(sensor: Sensor) -> Option<i32> {
    if sensor.supported == 0 {
        return None;
    }
    let v = sensor.value;
    if (1..=125).contains(&v) {
        Some(v)
    } else if (1000..=125_000).contains(&v) {
        Some(v / 1000)
    } else {
        None
    }
}

fn activity_pct(sensor: Sensor) -> Option<u32> {
    if sensor.supported == 0 || sensor.value < 0 {
        return None;
    }
    // ponytail: most ADL builds report 0-100. A few report centipercent, so only scale past 100.
    let v = sensor.value as u32;
    Some(if v <= 100 { v } else { (v / 100).min(100) })
}

fn already_running() -> bool {
    let name = wide("Local\\pa120-lite");
    let handle = unsafe { CreateMutexW(core::ptr::null_mut(), 1, name.as_ptr()) };
    if handle.is_null() || handle as usize == usize::MAX {
        return false;
    }
    // Keep the mutex open for the life of the process.
    unsafe { GetLastError() == 183 }
}

fn install_autostart() {
    let Ok(exe) = env::current_exe() else {
        return;
    };
    let value = wide(&format!("\"{}\"", exe.display()));
    let sub = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    let name = wide("pa120-lite");
    let mut key = core::ptr::null_mut();
    let rc = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            sub.as_ptr(),
            0,
            core::ptr::null(),
            0,
            0x0002,
            core::ptr::null_mut(),
            &mut key,
            core::ptr::null_mut(),
        )
    };
    if rc != 0 {
        return;
    }
    unsafe {
        RegSetValueExW(
            key,
            name.as_ptr(),
            0,
            1,
            value.as_ptr() as *const u8,
            (value.len() * 2) as u32,
        );
        RegCloseKey(key);
    }
}

fn complain(text: &str) {
    let text = wide(text);
    let title = wide("pa120-lite");
    unsafe {
        MessageBoxW(
            core::ptr::null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            0x10,
        );
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn hidden_loop(app: &mut App, hwnd: *mut core::ffi::c_void) {
    let mut last = Instant::now();
    loop {
        unsafe {
            MsgWaitForMultipleObjects(0, core::ptr::null(), 0, 1000, 0x04FF);
        }
        loop {
            let mut msg = unsafe { MaybeUninit::<Msg>::zeroed().assume_init() };
            if unsafe { PeekMessageW(&mut msg, core::ptr::null_mut(), 0, 0, 1) } == 0 {
                break;
            }
            if msg.message == 0x0012 {
                return;
            }
            unsafe {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        if last.elapsed() >= Duration::from_secs(1) {
            if app.tick().is_ok() && !hwnd.is_null() {
                let _ = tray(hwnd, &app.tip, 1);
            }
            last = Instant::now();
        }
    }
}

fn create_tray_window() -> Result<*mut core::ffi::c_void, ()> {
    let class_name = wide("pa120-lite-tray");
    let instance = unsafe { GetModuleHandleW(core::ptr::null()) };
    let mut wc = unsafe { MaybeUninit::<WndClassW>::zeroed().assume_init() };
    wc.lpfn = Some(wndproc);
    wc.instance = instance;
    wc.class_name = class_name.as_ptr();
    let atom = unsafe { RegisterClassW(&wc) };
    if atom == 0 && unsafe { GetLastError() } != 1410 {
        return Err(());
    }
    let title = wide("pa120-lite");
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class_name.as_ptr(),
            title.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            instance,
            core::ptr::null_mut(),
        )
    };
    if hwnd.is_null() { Err(()) } else { Ok(hwnd) }
}

fn tray(hwnd: *mut core::ffi::c_void, tip: &str, nim: u32) -> bool {
    let mut data = unsafe { MaybeUninit::<NotifyIconData>::zeroed().assume_init() };
    data.cb_size = core::mem::size_of::<NotifyIconData>() as u32;
    data.hwnd = hwnd;
    data.id = 1;
    if nim != 2 {
        data.flags = 1 | 2 | 4;
        data.callback = 0x401;
        data.icon = unsafe { LoadIconW(core::ptr::null_mut(), 32512 as *const u16) };
        let shown: String = tip.chars().take(120).collect();
        for (i, unit) in wide(&shown).into_iter().take(127).enumerate() {
            data.tip[i] = unit;
        }
    }
    unsafe { Shell_NotifyIconW(nim, &data) != 0 }
}

unsafe extern "system" fn wndproc(
    hwnd: *mut core::ffi::c_void,
    msg: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    if msg == 0x401 && lparam as u32 == 0x0205 {
        let menu = unsafe { CreatePopupMenu() };
        let label = wide("退出");
        unsafe {
            AppendMenuW(menu, 0, 1, label.as_ptr());
            let mut pt = Point { x: 0, y: 0 };
            GetCursorPos(&mut pt);
            SetForegroundWindow(hwnd);
            let cmd = TrackPopupMenu(menu, 0x0100, pt.x, pt.y, 0, hwnd, core::ptr::null());
            PostMessageW(hwnd, 0, 0, 0);
            DestroyMenu(menu);
            if cmd == 1 {
                PostQuitMessage(0);
            }
        }
        return 0;
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

#[repr(C)]
struct WndClassW {
    style: u32,
    lpfn: Option<
        unsafe extern "system" fn(*mut core::ffi::c_void, u32, usize, isize) -> isize,
    >,
    cls_extra: i32,
    wnd_extra: i32,
    instance: *mut core::ffi::c_void,
    icon: *mut core::ffi::c_void,
    cursor: *mut core::ffi::c_void,
    background: *mut core::ffi::c_void,
    menu_name: *const u16,
    class_name: *const u16,
}

#[repr(C)]
struct NotifyIconData {
    cb_size: u32,
    hwnd: *mut core::ffi::c_void,
    id: u32,
    flags: u32,
    callback: u32,
    icon: *mut core::ffi::c_void,
    tip: [u16; 128],
    state: u32,
    state_mask: u32,
    info: [u16; 256],
    version: u32,
    info_title: [u16; 64],
    info_flags: u32,
    guid: [u8; 16],
    balloon_icon: *mut core::ffi::c_void,
}

#[repr(C)]
struct Msg {
    hwnd: *mut core::ffi::c_void,
    message: u32,
    wparam: usize,
    lparam: isize,
    time: u32,
    x: i32,
    y: i32,
}

#[repr(C)]
struct Point {
    x: i32,
    y: i32,
}

const HKEY_CURRENT_USER: isize = 0x8000_0001u32 as isize;

fn proc<T>(module: *mut core::ffi::c_void, name: &[u8]) -> Option<T> {
    let p = unsafe { GetProcAddress(module, name.as_ptr()) };
    if p.is_null() {
        None
    } else {
        Some(unsafe { core::mem::transmute_copy(&p) })
    }
}

unsafe extern "C" fn adl_alloc(size: i32) -> *mut core::ffi::c_void {
    unsafe { malloc(size.max(0) as usize) }
}

unsafe extern "C" {
    fn malloc(size: usize) -> *mut core::ffi::c_void;
    fn GetSystemTimes(idle: *mut u64, kernel: *mut u64, user: *mut u64) -> i32;
    fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        security: *mut core::ffi::c_void,
        disposition: u32,
        flags: u32,
        template: *mut core::ffi::c_void,
    ) -> *mut core::ffi::c_void;
    fn DeviceIoControl(
        device: *mut core::ffi::c_void,
        code: u32,
        input: *const u8,
        input_len: u32,
        output: *mut u8,
        output_len: u32,
        returned: *mut u32,
        overlapped: *mut core::ffi::c_void,
    ) -> i32;
    fn CloseHandle(handle: *mut core::ffi::c_void) -> i32;
    fn GetLastError() -> u32;
    fn GetCurrentThread() -> *mut core::ffi::c_void;
    fn SetThreadAffinityMask(thread: *mut core::ffi::c_void, mask: usize) -> usize;
    fn LoadLibraryW(name: *const u16) -> *mut core::ffi::c_void;
    fn FreeLibrary(module: *mut core::ffi::c_void) -> i32;
    fn GetProcAddress(module: *mut core::ffi::c_void, name: *const u8) -> *mut core::ffi::c_void;
    fn CreateMutexW(security: *mut core::ffi::c_void, owner: i32, name: *const u16) -> *mut core::ffi::c_void;
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegCreateKeyExW(
        key: isize,
        sub: *const u16,
        reserved: u32,
        class: *const u16,
        options: u32,
        sam: u32,
        security: *mut core::ffi::c_void,
        result: *mut *mut core::ffi::c_void,
        disposition: *mut u32,
    ) -> i32;
    fn RegSetValueExW(
        key: *mut core::ffi::c_void,
        name: *const u16,
        reserved: u32,
        kind: u32,
        data: *const u8,
        len: u32,
    ) -> i32;
    fn RegCloseKey(key: *mut core::ffi::c_void) -> i32;
}

#[link(name = "user32")]
unsafe extern "system" {
    fn MessageBoxW(hwnd: *mut core::ffi::c_void, text: *const u16, title: *const u16, kind: u32) -> i32;
    fn GetModuleHandleW(name: *const u16) -> *mut core::ffi::c_void;
    fn RegisterClassW(class: *const WndClassW) -> u16;
    fn CreateWindowExW(
        ex: u32,
        class: *const u16,
        window: *const u16,
        style: u32,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        parent: *mut core::ffi::c_void,
        menu: *mut core::ffi::c_void,
        instance: *mut core::ffi::c_void,
        param: *mut core::ffi::c_void,
    ) -> *mut core::ffi::c_void;
    fn DefWindowProcW(hwnd: *mut core::ffi::c_void, msg: u32, wparam: usize, lparam: isize) -> isize;
    fn PeekMessageW(msg: *mut Msg, hwnd: *mut core::ffi::c_void, min: u32, max: u32, remove: u32) -> i32;
    fn TranslateMessage(msg: *const Msg) -> i32;
    fn DispatchMessageW(msg: *const Msg) -> isize;
    fn PostQuitMessage(code: i32);
    fn PostMessageW(hwnd: *mut core::ffi::c_void, msg: u32, wparam: usize, lparam: isize) -> i32;
    fn MsgWaitForMultipleObjects(count: u32, handles: *const *mut core::ffi::c_void, wait_all: i32, ms: u32, mask: u32) -> u32;
    fn CreatePopupMenu() -> *mut core::ffi::c_void;
    fn AppendMenuW(menu: *mut core::ffi::c_void, flags: u32, id: usize, text: *const u16) -> i32;
    fn TrackPopupMenu(menu: *mut core::ffi::c_void, flags: u32, x: i32, y: i32, reserved: i32, hwnd: *mut core::ffi::c_void, rect: *const core::ffi::c_void) -> i32;
    fn DestroyMenu(menu: *mut core::ffi::c_void) -> i32;
    fn GetCursorPos(pt: *mut Point) -> i32;
    fn SetForegroundWindow(hwnd: *mut core::ffi::c_void) -> i32;
    fn LoadIconW(instance: *mut core::ffi::c_void, name: *const u16) -> *mut core::ffi::c_void;
}

#[link(name = "shell32")]
unsafe extern "system" {
    fn Shell_NotifyIconW(message: u32, data: *const NotifyIconData) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remap_is_a_permutation() {
        let mut seen = [0u8; LEDS];
        for &i in &REMAP {
            seen[i as usize] += 1;
        }
        assert!(seen.iter().all(|&n| n == 1));
    }

    fn logical(frame: [[u8; 3]; LEDS]) -> [[u8; 3]; LEDS] {
        let mut logical = [[0u8; 3]; LEDS];
        for (wire_i, &logical_i) in REMAP.iter().enumerate() {
            logical[logical_i as usize] = frame[wire_i];
        }
        logical
    }

    #[test]
    fn one_degree_lights_only_bc_of_the_ones_digit() {
        let logical = logical(render(Some(1), None, None, None));
        let cyan = [0, 255, 255];
        assert_eq!(logical[25], cyan);
        assert_eq!(logical[26], cyan);
        assert_eq!(logical[24], [0, 0, 0]);
        assert_eq!(logical[10], [0, 0, 0]);
    }

    #[test]
    fn load_picks_the_color_and_the_hundreds_mark() {
        assert_eq!(load_color(10), [0, 255, 255]);
        assert_eq!(load_color(40), [0, 255, 0]);
        assert_eq!(load_color(60), [255, 255, 0]);
        assert_eq!(load_color(80), [255, 110, 0]);
        assert_eq!(load_color(95), [255, 0, 0]);
        let logical = logical(render(None, Some(100), None, Some(95)));
        assert_eq!(logical[80], [255, 0, 0]);
        assert_eq!(logical[81], [255, 0, 0]);
        assert_eq!(logical[66], [255, 0, 0]);
        assert_eq!(logical[82], [0, 0, 0]);
    }
}
