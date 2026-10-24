//! InputSwap: Ctrl+W switches the player's car to the wheel mid-session.
//!
//! Built as dinput8.dll and put in the AC folder: acs.exe imports DirectInput8Create and Windows
//! looks in the game's folder first, so the game loads it at every start. DirectInput calls go on
//! to the system dinput8.dll; inside acs.exe the DLL also installs Ctrl+W, in any other process
//! (Content Manager's showroom, …) it only forwards.
//!
//! AC reads cfg/controls.ini once, when the car loads (`CarAvatar::initControls`), and lists the
//! DirectInput devices once, at start (`DirectInput::initJoys`). It already has a hidden Ctrl+P
//! that switches the car to the gamepad (`Sim::onKeyDown` builds a `JoypadCarControl`), but no way
//! back. Ctrl+W, added to the same key handler:
//! 1. lists the game controllers attached now and adds the ones AC has not seen (a wheel plugged in
//!    after launch) to AC's DirectInput device list, with AC's own code;
//! 2. puts the devices in the order of controls.ini's [CONTROLLERS], which the JOY= indices count
//!    in, matched by instance GUID, else product GUID;
//! 3. builds a `DICarControl` (the wheel settings of controls.ini) and hands it to
//!    `CarAvatar::setControlsProvider`, which swaps it in on the physics thread, as Ctrl+P does.
//!
//! Tied to acs.exe 1.16 (addresses from acs.pdb, which ships with AC): every called address is
//! checked first, and on a mismatch nothing is touched. Log: inputswap.log next to the DLL.

use std::ffi::c_void;
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use windows_sys::Win32::Foundation::{BOOL, HMODULE, TRUE};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::Memory::{VirtualProtect, PAGE_EXECUTE_READWRITE};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows_sys::Win32::System::SystemServices::DLL_PROCESS_ATTACH;
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL, VK_W};
use windows_sys::Win32::UI::Shell::{FOLDERID_Documents, SHGetKnownFolderPath};

// ---- acs.exe 1.16 (RVAs and layouts from acs.pdb) -----------------------------------------------

const ON_KEY_DOWN: usize = 0x19a940; // Sim::onKeyDown(const OnKeyEvent&), through IKeyEventListener
const VT_SIM_KEY_LISTENER: usize = 0x4d8048; // Sim's IKeyEventListener vtable, slot 0 = onKeyDown
const DI_CAR_CONTROL_CTOR: usize = 0x7ddd0; // DICarControl::DICarControl()
const DI_CAR_CONTROL_SIZE: usize = 0xb50;
const VT_DI_CAR_CONTROL: usize = 0x4b2098;
const SET_CONTROLS_PROVIDER: usize = 0xd9f90; // CarAvatar::setControlsProvider(ICarControlsProvider*)
const OPERATOR_NEW: usize = 0x399648;
const ENUM_JOYSTICKS_CALLBACK: usize = 0x23eaa0; // EnumJoysticksCallbackDI(const DIDEVICEINSTANCEW*, DirectInput*)
const INPUT_DEVICE_CTOR: usize = 0x23f2f0; // InputDevice::InputDevice(GUID, const std::wstring&, DirectInput*)
const INPUT_DEVICE_SIZE: usize = 0x100;
const CONTROL_IS_VISIBLE: usize = 0x55480; // ksgui::Control::isVisible()
const CONSOLE_IS_VISIBLE: usize = 0x19a460; // Console::isVisible()
const ADD_MESSAGE: usize = 0x1b61c0; // SystemMessage::addMessage(const wstring& title, const wstring& text, type)
const DIRECT_INPUT_SINGLETON: usize = 0x155a5f8; // DirectInput::m_singleton
const DIRECT_INPUT_HWND: usize = 0x155a5e8; // DirectInput::hWnd
/// DirectInput::singleton(): creates it on first use (listing the attached devices), which AC
/// leaves to the first DICarControl, so a session started on the gamepad has none.
const DIRECT_INPUT_GET: usize = 0x23f290;
/// First bytes of the functions above; CSP replaces some functions' start with a jump (E9), also fine.
const ACS_FUNCTIONS: [(usize, [u8; 8]); 9] = [
    (DIRECT_INPUT_GET, [0x48, 0x83, 0xec, 0x38, 0x48, 0xc7, 0x44, 0x24]),
    (ON_KEY_DOWN, [0x48, 0x8b, 0xc4, 0x55, 0x56, 0x57, 0x41, 0x54]),
    (DI_CAR_CONTROL_CTOR, [0x48, 0x8b, 0xc4, 0x55, 0x41, 0x54, 0x41, 0x55]),
    (SET_CONTROLS_PROVIDER, [0x40, 0x53, 0x48, 0x83, 0xec, 0x60, 0x48, 0xc7]),
    (OPERATOR_NEW, [0xff, 0x25, 0x8a, 0xb7, 0x10, 0x00, 0xff, 0x25]),
    (ENUM_JOYSTICKS_CALLBACK, [0x48, 0x8b, 0xc4, 0x57, 0x48, 0x81, 0xec, 0x80]),
    (INPUT_DEVICE_CTOR, [0x48, 0x89, 0x4c, 0x24, 0x08, 0x57, 0x41, 0x56]),
    (CONTROL_IS_VISIBLE, [0x0f, 0xb6, 0x81, 0x50, 0x01, 0x00, 0x00, 0xc3]),
    (CONSOLE_IS_VISIBLE, [0x0f, 0xb6, 0x41, 0x30, 0xc3, 0xcc, 0xcc, 0xcc]),
];
const ADD_MESSAGE_PROLOGUE: [u8; 8] = [0x48, 0x8b, 0xc4, 0x44, 0x89, 0x48, 0x20, 0x56];

// Sim seen from its IKeyEventListener base (onKeyDown's `this`, Sim+0x58).
const KEYS_BLOCKING_SCREEN: usize = 0xb8; // ksgui::Control*: AC ignores its Ctrl shortcuts while shown
const KEYS_SYSTEM_MESSAGE: usize = 0x140; // SystemMessage*
const KEYS_CARS: usize = 0x1b0; // std::vector<CarAvatar*>, [0] = the player
const KEYS_CONSOLE: usize = 0x1d0; // Console*
const KEYS_PHYSICS_AVATAR: usize = 0x160; // PhysicsAvatar* (Sim::physicsAvatar, Sim+0x1b8)
const PHYSICS_AVATAR_DRIVE_THREAD: usize = 832; // PhysicsDriveThread, embedded
// PhysicsDriveThread: each physics step polls DirectInput (and its DICommandManager) only while
// useDirectInput is set and directInput is known; a session started on the gamepad has neither.
const DRIVE_THREAD_USE_DIRECT_INPUT: usize = 0x30; // bool
const DRIVE_THREAD_DIRECT_INPUT: usize = 0x88; // DirectInput*
const CARAVATAR_PHYSICS: usize = 0x1168; // Car*
const CAR_CONTROLS_PROVIDER: usize = 0x3d80; // ICarControlsProvider*
const DI_INFOS: usize = 0x08; // std::vector<std::pair<GUID, std::wstring>>
const DI_INFO_SIZE: usize = 0x30;
const DI_INFO_NAME: usize = 0x10;
const DI_DINPUT: usize = 0x20; // IDirectInput8W*
const DI_DEVICES: usize = 0x28; // std::vector<InputDevice*>, the JOY= indices

// ---- DirectInput --------------------------------------------------------------------------------

const DIDEVICEINSTANCEW_SIZE: usize = 0x44c;
const DIDI_GUID_INSTANCE: usize = 0x04;
const DIDI_GUID_PRODUCT: usize = 0x14;
const DIDI_PRODUCT_NAME: usize = 0x230;
const IDIRECTINPUT8_ENUM_DEVICES_SLOT: usize = 4;
const DI8DEVCLASS_GAMECTRL: u32 = 4;
const DIEDFL_ATTACHEDONLY: u32 = 1;

type Guid = [u8; 16];

// ---- state --------------------------------------------------------------------------------------

static MODULE: AtomicUsize = AtomicUsize::new(0);
static ACS: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_ON_KEY_DOWN: AtomicUsize = AtomicUsize::new(0);
static ADD_MESSAGE_OK: AtomicUsize = AtomicUsize::new(0);
/// The instance GUID of every device in AC's list, by `InputDevice*`, noted before the first
/// reorder (until then AC's device list and its info list are in the same order).
static DEVICE_GUIDS: Mutex<Vec<(usize, Guid)>> = Mutex::new(Vec::new());

fn log(msg: &str) {
    let mut path = [0u16; 1024];
    let n = unsafe { GetModuleFileNameW(MODULE.load(Ordering::Relaxed) as HMODULE, path.as_mut_ptr(), 1024) } as usize;
    let dll = String::from_utf16_lossy(&path[..n]);
    let file = std::path::Path::new(&dll).with_file_name("inputswap.log");
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(file) {
        let _ = writeln!(f, "{msg}");
    }
}

unsafe fn rd(addr: usize) -> usize {
    std::ptr::read_unaligned(addr as *const usize)
}

unsafe fn wr(addr: usize, value: usize) {
    std::ptr::write_volatile(addr as *mut usize, value);
}

unsafe fn write_protected(addr: usize, bytes: &[u8]) -> bool {
    let mut old = 0;
    if VirtualProtect(addr as *const c_void, bytes.len(), PAGE_EXECUTE_READWRITE, &mut old) == 0 {
        return false;
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), addr as *mut u8, bytes.len());
    VirtualProtect(addr as *const c_void, bytes.len(), old, &mut old);
    FlushInstructionCache(GetCurrentProcess(), addr as *const c_void, bytes.len());
    true
}

fn same(addr: usize, expected: &[u8]) -> bool {
    unsafe { std::slice::from_raw_parts(addr as *const u8, expected.len()) == expected }
}

/// Unchanged, or replaced by a jump (CSP hooks some functions itself).
fn callable(addr: usize, expected: &[u8; 8]) -> bool {
    same(addr, expected) || unsafe { *(addr as *const u8) } == 0xe9
}

/// `SizeOfImage` from a loaded module's PE header.
unsafe fn image_size(base: usize) -> usize {
    let pe = base + std::ptr::read_unaligned((base + 0x3c) as *const u32) as usize;
    std::ptr::read_unaligned((pe + 24 + 56) as *const u32) as usize
}

fn key_down(vk: u16) -> bool {
    unsafe { GetAsyncKeyState(vk as i32) < 0 }
}

unsafe fn acs_fn<T>(rva: usize) -> T {
    std::mem::transmute_copy(&(ACS.load(Ordering::Relaxed) + rva))
}

unsafe fn op_new(size: usize) -> usize {
    let f: unsafe extern "C" fn(usize) -> usize = acs_fn(OPERATOR_NEW);
    f(size)
}

/// The running executable is acs.exe.
fn in_game() -> bool {
    let mut path = [0u16; 1024];
    let n = unsafe { GetModuleFileNameW(std::ptr::null_mut(), path.as_mut_ptr(), 1024) } as usize;
    String::from_utf16_lossy(&path[..n]).to_ascii_lowercase().ends_with("\\acs.exe")
}

#[no_mangle]
pub extern "system" fn DllMain(module: HMODULE, reason: u32, _reserved: *mut c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH && in_game() {
        MODULE.store(module as usize, Ordering::Relaxed);
        match unsafe { install() } {
            Ok(()) => log("installed: Ctrl+W switches to the wheel (Ctrl+P, AC's own, to the gamepad)"),
            Err(e) => log(&format!("not installed: {e}")),
        }
    }
    TRUE
}

unsafe fn install() -> Result<(), String> {
    let acs = GetModuleHandleW(std::ptr::null()) as usize;
    ACS.store(acs, Ordering::Relaxed);
    // Never read outside the module: a wrong executable must fail the checks, not crash the game.
    if image_size(acs) < DIRECT_INPUT_SINGLETON + 8 {
        return Err(format!("acs.exe is too small ({:#x}): not acs.exe 1.16", image_size(acs)));
    }
    for (rva, bytes) in ACS_FUNCTIONS {
        if !callable(acs + rva, &bytes) {
            return Err(format!("acs.exe+{rva:#x} differs: not acs.exe 1.16"));
        }
    }
    if callable(acs + ADD_MESSAGE, &ADD_MESSAGE_PROLOGUE) {
        ADD_MESSAGE_OK.store(1, Ordering::Relaxed);
    }
    let slot = acs + VT_SIM_KEY_LISTENER;
    let current = rd(slot);
    if current != acs + ON_KEY_DOWN {
        log(&format!("onKeyDown's vtable slot already points elsewhere ({current:#x}), chaining to it"));
    }
    ORIGINAL_ON_KEY_DOWN.store(current, Ordering::SeqCst);
    let detour = on_key_down as *const () as usize;
    if !write_protected(slot, &detour.to_le_bytes()) {
        return Err("could not patch Sim's key listener vtable".into());
    }
    Ok(())
}

// ---- dinput8.dll exports, forwarded to the system one -------------------------------------------

static SYSTEM_DINPUT8: AtomicUsize = AtomicUsize::new(0);
const E_FAIL: i32 = 0x80004005u32 as i32;

/// `name` in the system dinput8.dll, loaded on first use (never from DllMain: loader lock).
unsafe fn system_export(name: &[u8]) -> usize {
    let mut lib = SYSTEM_DINPUT8.load(Ordering::Acquire);
    if lib == 0 {
        let mut dir = [0u16; 260];
        let n = GetSystemDirectoryW(dir.as_mut_ptr(), dir.len() as u32) as usize;
        if n == 0 || n >= dir.len() {
            return 0;
        }
        let path: Vec<u16> = dir[..n].iter().copied().chain("\\dinput8.dll".encode_utf16()).chain([0]).collect();
        lib = LoadLibraryW(path.as_ptr()) as usize;
        if lib == 0 {
            log("could not load the system dinput8.dll");
            return 0;
        }
        SYSTEM_DINPUT8.store(lib, Ordering::Release);
    }
    GetProcAddress(lib as HMODULE, name.as_ptr()).map_or(0, |f| f as usize)
}

#[no_mangle]
pub unsafe extern "system" fn DirectInput8Create(
    instance: *mut c_void,
    version: u32,
    riid: *const c_void,
    out: *mut *mut c_void,
    outer: *mut c_void,
) -> i32 {
    match system_export(b"DirectInput8Create\0") {
        0 => E_FAIL,
        f => {
            let f: unsafe extern "system" fn(*mut c_void, u32, *const c_void, *mut *mut c_void, *mut c_void) -> i32 =
                std::mem::transmute(f);
            f(instance, version, riid, out, outer)
        }
    }
}

#[no_mangle]
pub unsafe extern "system" fn DllCanUnloadNow() -> i32 {
    match system_export(b"DllCanUnloadNow\0") {
        0 => 1, // S_FALSE
        f => std::mem::transmute::<usize, unsafe extern "system" fn() -> i32>(f)(),
    }
}

#[no_mangle]
pub unsafe extern "system" fn DllGetClassObject(clsid: *const c_void, riid: *const c_void, out: *mut *mut c_void) -> i32 {
    match system_export(b"DllGetClassObject\0") {
        0 => E_FAIL,
        f => {
            let f: unsafe extern "system" fn(*const c_void, *const c_void, *mut *mut c_void) -> i32 = std::mem::transmute(f);
            f(clsid, riid, out)
        }
    }
}

#[no_mangle]
pub unsafe extern "system" fn DllRegisterServer() -> i32 {
    match system_export(b"DllRegisterServer\0") {
        0 => E_FAIL,
        f => std::mem::transmute::<usize, unsafe extern "system" fn() -> i32>(f)(),
    }
}

#[no_mangle]
pub unsafe extern "system" fn DllUnregisterServer() -> i32 {
    match system_export(b"DllUnregisterServer\0") {
        0 => E_FAIL,
        f => std::mem::transmute::<usize, unsafe extern "system" fn() -> i32>(f)(),
    }
}

#[no_mangle]
pub unsafe extern "system" fn GetdfDIJoystick() -> *const c_void {
    match system_export(b"GetdfDIJoystick\0") {
        0 => std::ptr::null(),
        f => std::mem::transmute::<usize, unsafe extern "system" fn() -> *const c_void>(f)(),
    }
}

// ---- Ctrl+W -------------------------------------------------------------------------------------

type KeyFn = unsafe extern "C" fn(usize, *const u32);

unsafe extern "C" fn on_key_down(keys: usize, event: *const u32) {
    if *event == VK_W as u32 && key_down(VK_CONTROL) && switch_to_wheel(keys) {
        return;
    }
    let original: KeyFn = std::mem::transmute(ORIGINAL_ON_KEY_DOWN.load(Ordering::SeqCst));
    original(keys, event);
}

/// True when Ctrl+W was ours (handled, even if it failed), false to leave the key to AC.
unsafe fn switch_to_wheel(keys: usize) -> bool {
    let acs = ACS.load(Ordering::Relaxed);
    let control_visible: unsafe extern "C" fn(usize) -> bool = acs_fn(CONTROL_IS_VISIBLE);
    let console_visible: unsafe extern "C" fn(usize) -> bool = acs_fn(CONSOLE_IS_VISIBLE);
    let screen = rd(keys + KEYS_BLOCKING_SCREEN);
    let console = rd(keys + KEYS_CONSOLE);
    if (screen != 0 && control_visible(screen)) || (console != 0 && console_visible(console)) {
        return false;
    }
    let cars = rd(keys + KEYS_CARS);
    if cars == 0 || cars == rd(keys + KEYS_CARS + 8) {
        log("Ctrl+W: no car");
        return false;
    }
    let avatar = rd(cars);
    let car = rd(avatar + CARAVATAR_PHYSICS);
    if car == 0 {
        log("Ctrl+W: the player's car has no physics (replay?)");
        return false;
    }
    log("Ctrl+W");
    let Some(di) = direct_input(keys) else { return true };
    let attached = attached_devices(di);
    note_device_guids(di);
    let added = add_new_devices(di, &attached);
    let moved = order_devices(di, &attached);

    let current = rd(car + CAR_CONTROLS_PROVIDER);
    if current != 0 && rd(current) == acs + VT_DI_CAR_CONTROL && added.is_empty() && !moved {
        // A second DICarControl on the same wheel would lose its force feedback when AC deletes
        // the first one (on the physics thread, after the new one set its effects up).
        log("  already on the wheel, nothing new");
        message(keys, "Controls", "Already on the wheel");
        return true;
    }
    let provider = op_new(DI_CAR_CONTROL_SIZE);
    if provider == 0 {
        log("  out of memory");
        return true;
    }
    let ctor: unsafe extern "C" fn(usize) -> usize = acs_fn(DI_CAR_CONTROL_CTOR);
    ctor(provider);
    let set_provider: unsafe extern "C" fn(usize, usize) = acs_fn(SET_CONTROLS_PROVIDER);
    set_provider(avatar, provider);
    log(&format!("  wheel controls {provider:#x} queued for car {car:#x} (was {current:#x})"));
    let text = if added.is_empty() { "Wheel".to_string() } else { format!("Wheel ({} found)", added.join(", ")) };
    message(keys, "Controls", &text);
    true
}

/// The DirectInput the physics thread polls (which is what reads the wheel's axes and buttons),
/// made AC's `DirectInput::m_singleton` too, since `DICarControl` takes its devices from that one.
/// In a session started on the gamepad the singleton can be empty while the physics thread still
/// polls the one made at start (seen with CSP), or there may be none at all.
unsafe fn direct_input(keys: usize) -> Option<usize> {
    let acs = ACS.load(Ordering::Relaxed);
    let physics = rd(keys + KEYS_PHYSICS_AVATAR);
    let thread = (physics != 0).then_some(physics + PHYSICS_AVATAR_DRIVE_THREAD);
    let thread_di = thread.map_or(0, |t| rd(t + DRIVE_THREAD_DIRECT_INPUT));
    let mut di = rd(acs + DIRECT_INPUT_SINGLETON);
    let use_di = thread.map_or(0, |t| *((t + DRIVE_THREAD_USE_DIRECT_INPUT) as *const u8));
    log(&format!("  DirectInput: AC's {di:#x}, physics thread's {thread_di:#x} (useDirectInput {use_di})"));
    if thread_di != 0 && di != thread_di {
        wr(acs + DIRECT_INPUT_SINGLETON, thread_di);
        di = thread_di;
        log("  AC's DirectInput set to the physics thread's");
    }
    if di == 0 {
        if rd(acs + DIRECT_INPUT_HWND) == 0 {
            log("  AC's DirectInput has no window yet");
            return None;
        }
        let get: unsafe extern "C" fn() -> usize = acs_fn(DIRECT_INPUT_GET);
        di = get();
        if di == 0 {
            return None;
        }
        log(&format!("  DirectInput created ({} devices)", vec_len(di + DI_DEVICES, 8)));
    }
    match thread {
        Some(t) => {
            if rd(t + DRIVE_THREAD_DIRECT_INPUT) == 0 {
                wr(t + DRIVE_THREAD_DIRECT_INPUT, di);
            }
            if use_di == 0 {
                std::ptr::write_volatile((t + DRIVE_THREAD_USE_DIRECT_INPUT) as *mut u8, 1);
            }
        }
        None => log("  no PhysicsAvatar: the physics thread may not read the wheel"),
    }
    Some(di)
}

// ---- devices ------------------------------------------------------------------------------------

/// A DIDEVICEINSTANCEW, copied.
struct Instance([u8; DIDEVICEINSTANCEW_SIZE]);

impl Instance {
    fn guid_instance(&self) -> Guid {
        self.0[DIDI_GUID_INSTANCE..DIDI_GUID_INSTANCE + 16].try_into().unwrap()
    }
    fn guid_product(&self) -> Guid {
        self.0[DIDI_GUID_PRODUCT..DIDI_GUID_PRODUCT + 16].try_into().unwrap()
    }
    fn product_name(&self) -> String {
        let name: Vec<u16> = self.0[DIDI_PRODUCT_NAME..DIDI_PRODUCT_NAME + 520]
            .chunks(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&c| c != 0)
            .collect();
        String::from_utf16_lossy(&name)
    }
}

unsafe extern "system" fn collect_device(instance: *const u8, found: *mut c_void) -> BOOL {
    let mut copy = Instance([0; DIDEVICEINSTANCEW_SIZE]);
    std::ptr::copy_nonoverlapping(instance, copy.0.as_mut_ptr(), DIDEVICEINSTANCEW_SIZE);
    (*(found as *mut Vec<Instance>)).push(copy);
    TRUE // DIENUM_CONTINUE
}

/// The game controllers attached now, listed through AC's own IDirectInput8.
unsafe fn attached_devices(di: usize) -> Vec<Instance> {
    let mut found: Vec<Instance> = Vec::new();
    let dinput = rd(di + DI_DINPUT);
    if dinput == 0 {
        return found;
    }
    type EnumDevices = unsafe extern "system" fn(usize, u32, unsafe extern "system" fn(*const u8, *mut c_void) -> BOOL, *mut c_void, u32) -> i32;
    let enum_devices: EnumDevices = std::mem::transmute(rd(rd(dinput) + IDIRECTINPUT8_ENUM_DEVICES_SLOT * 8));
    let hr = enum_devices(dinput, DI8DEVCLASS_GAMECTRL, collect_device, &mut found as *mut _ as *mut c_void, DIEDFL_ATTACHEDONLY);
    if hr < 0 {
        log(&format!("  EnumDevices failed: {hr:#x}"));
    }
    for d in &found {
        log(&format!("  attached: {} {}", d.product_name(), guid_text(&d.guid_instance())));
    }
    found
}

unsafe fn vec_len(v: usize, stride: usize) -> usize {
    (rd(v + 8) - rd(v)) / stride
}

unsafe fn info_guid(di: usize, i: usize) -> Guid {
    let entry = rd(di + DI_INFOS) + i * DI_INFO_SIZE;
    std::slice::from_raw_parts(entry as *const u8, 16).try_into().unwrap()
}

unsafe fn device(di: usize, i: usize) -> usize {
    rd(rd(di + DI_DEVICES) + i * 8)
}

/// The first time: pair AC's devices with its info list, which `initJoys` built in the same order.
unsafe fn note_device_guids(di: usize) {
    let mut guids = DEVICE_GUIDS.lock().unwrap();
    if !guids.is_empty() {
        return;
    }
    let devices = vec_len(di + DI_DEVICES, 8);
    let infos = vec_len(di + DI_INFOS, DI_INFO_SIZE);
    if devices != infos {
        log(&format!("  AC has {devices} devices but {infos} device infos: devices left in their order"));
        return;
    }
    for i in 0..devices {
        guids.push((device(di, i), info_guid(di, i)));
    }
}

/// Adds the attached devices AC does not know, the way `initJoys` adds them; their names.
unsafe fn add_new_devices(di: usize, attached: &[Instance]) -> Vec<String> {
    let mut added = Vec::new();
    for d in attached {
        let guid = d.guid_instance();
        let infos = vec_len(di + DI_INFOS, DI_INFO_SIZE);
        if (0..infos).any(|i| info_guid(di, i) == guid) {
            continue;
        }
        let callback: unsafe extern "system" fn(*const u8, usize) -> BOOL = acs_fn(ENUM_JOYSTICKS_CALLBACK);
        callback(d.0.as_ptr(), di);
        if vec_len(di + DI_INFOS, DI_INFO_SIZE) != infos + 1 || info_guid(di, infos) != guid {
            log(&format!("  {}: AC did not take it", d.product_name()));
            continue;
        }
        let input_device = op_new(INPUT_DEVICE_SIZE);
        if input_device == 0 {
            continue;
        }
        let name = rd(di + DI_INFOS) + infos * DI_INFO_SIZE + DI_INFO_NAME;
        let ctor: unsafe extern "C" fn(usize, *const Guid, usize, usize) -> usize = acs_fn(INPUT_DEVICE_CTOR);
        ctor(input_device, &guid, name, di);
        push_device(di, input_device);
        DEVICE_GUIDS.lock().unwrap().push((input_device, guid));
        log(&format!("  added {} as device {}", d.product_name(), vec_len(di + DI_DEVICES, 8) - 1));
        added.push(d.product_name());
    }
    added
}

/// Appends to AC's device vector. When it is full, the pointers move to a bigger buffer and the
/// old one is left allocated: the physics thread may be reading it right now
/// (`DICarControl::acquireControls` → `DirectInput::getDevice`).
unsafe fn push_device(di: usize, input_device: usize) {
    let v = di + DI_DEVICES;
    let (begin, end, cap) = (rd(v), rd(v + 8), rd(v + 16));
    let len = (end - begin) / 8;
    if end == cap {
        let new_cap = (len * 2).max(16);
        let buf = op_new(new_cap * 8);
        if buf == 0 {
            return;
        }
        std::ptr::write_bytes(buf as *mut u8, 0, new_cap * 8);
        if len > 0 {
            std::ptr::copy_nonoverlapping(begin as *const u8, buf as *mut u8, len * 8);
        }
        wr(v, buf);
        wr(v + 8, buf + len * 8);
        wr(v + 16, buf + new_cap * 8);
    }
    let end = rd(v + 8);
    wr(end, input_device);
    wr(v + 8, end + 8);
}

/// Moves each device to its index in controls.ini's [CONTROLLERS]; true if any moved.
unsafe fn order_devices(di: usize, attached: &[Instance]) -> bool {
    let wanted = ini_controllers();
    let guids = DEVICE_GUIDS.lock().unwrap();
    let instance_of = |dev: usize| guids.iter().find(|(d, _)| *d == dev).map(|(_, g)| *g);
    let product_of = |instance: Guid| attached.iter().find(|a| a.guid_instance() == instance).map(|a| a.guid_product());
    let len = vec_len(di + DI_DEVICES, 8);
    let mut moved = false;
    for (n, (want_instance, want_product)) in wanted.iter().enumerate().take(len) {
        let by_instance = |p: usize| want_instance.is_some() && instance_of(device(di, p)) == *want_instance;
        let by_product = |p: usize| {
            want_product.is_some() && instance_of(device(di, p)).and_then(|g| product_of(g)) == *want_product
        };
        let Some(p) = (n..len).find(|&p| by_instance(p)).or_else(|| (n..len).find(|&p| by_product(p))) else {
            continue;
        };
        if p != n {
            let slots = rd(di + DI_DEVICES);
            let (a, b) = (device(di, n), device(di, p));
            wr(slots + n * 8, b);
            wr(slots + p * 8, a);
            log(&format!("  device {p} moved to {n}, as in controls.ini"));
            moved = true;
        }
    }
    moved
}

// ---- controls.ini -------------------------------------------------------------------------------

/// [CONTROLLERS] of Documents/Assetto Corsa/cfg/controls.ini: (__IGUIDn, PGUIDn) for n = 0, 1, …
fn ini_controllers() -> Vec<(Option<Guid>, Option<Guid>)> {
    let Some(text) = documents().and_then(|d| read_text(&d.join("Assetto Corsa").join("cfg").join("controls.ini"))) else {
        log("  controls.ini not found: devices left in their order");
        return Vec::new();
    };
    let mut list: Vec<(Option<Guid>, Option<Guid>)> = Vec::new();
    let mut in_section = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_section = line.eq_ignore_ascii_case("[CONTROLLERS]");
            continue;
        }
        let Some((key, value)) = line.split_once('=').filter(|_| in_section) else { continue };
        let value = value.split(';').next().unwrap_or("").trim();
        let (index, instance) = if let Some(n) = key.trim().strip_prefix("__IGUID") {
            (n, true)
        } else if let Some(n) = key.trim().strip_prefix("PGUID") {
            (n, false)
        } else {
            continue;
        };
        let Ok(n) = index.parse::<usize>() else { continue };
        if n >= 64 {
            continue;
        }
        if list.len() <= n {
            list.resize(n + 1, (None, None));
        }
        if instance {
            list[n].0 = parse_guid(value);
        } else {
            list[n].1 = parse_guid(value);
        }
    }
    list
}

fn documents() -> Option<std::path::PathBuf> {
    unsafe {
        let mut path: *mut u16 = std::ptr::null_mut();
        if SHGetKnownFolderPath(&FOLDERID_Documents, 0, std::ptr::null_mut(), &mut path) < 0 || path.is_null() {
            return None;
        }
        let len = (0..).take_while(|&i| *path.add(i) != 0).count();
        let dir = String::from_utf16_lossy(std::slice::from_raw_parts(path, len));
        CoTaskMemFree(path as *const c_void);
        Some(dir.into())
    }
}

fn read_text(path: &std::path::Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.starts_with(&[0xff, 0xfe]) {
        let wide: Vec<u16> = bytes[2..].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        return Some(String::from_utf16_lossy(&wide));
    }
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes);
    Some(String::from_utf8_lossy(bytes).into_owned())
}

/// "35AE76A0-4795-11EF-8001-444553540000" (braces optional) → the GUID's bytes in memory.
fn parse_guid(text: &str) -> Option<Guid> {
    let parts: Vec<&str> = text.trim_matches(|c| c == '{' || c == '}').split('-').collect();
    if parts.len() != 5 || parts.iter().map(|p| p.len()).ne([8, 4, 4, 4, 12]) {
        return None;
    }
    let mut g = [0u8; 16];
    g[0..4].copy_from_slice(&u32::from_str_radix(parts[0], 16).ok()?.to_le_bytes());
    g[4..6].copy_from_slice(&u16::from_str_radix(parts[1], 16).ok()?.to_le_bytes());
    g[6..8].copy_from_slice(&u16::from_str_radix(parts[2], 16).ok()?.to_le_bytes());
    let tail = format!("{}{}", parts[3], parts[4]);
    for (i, byte) in g[8..].iter_mut().enumerate() {
        *byte = u8::from_str_radix(&tail[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(g)
}

fn guid_text(g: &Guid) -> String {
    let d1 = u32::from_le_bytes(g[0..4].try_into().unwrap());
    let d2 = u16::from_le_bytes([g[4], g[5]]);
    let d3 = u16::from_le_bytes([g[6], g[7]]);
    let tail: String = g[8..].iter().map(|b| format!("{b:02X}")).collect();
    format!("{d1:08X}-{d2:04X}-{d3:04X}-{}-{}", &tail[..4], &tail[4..])
}

// ---- in-game message ----------------------------------------------------------------------------

/// MSVC `std::wstring` over our own buffer (heap mode); AC only reads it.
#[repr(C)]
struct WString {
    ptr: *const u16,
    _pad: usize,
    len: usize,
    cap: usize,
}

/// AC's system message (the banner its own shortcuts show, e.g. "Auto Shifter ON").
unsafe fn message(keys: usize, title: &str, text: &str) {
    let system_message = rd(keys + KEYS_SYSTEM_MESSAGE);
    if system_message == 0 || ADD_MESSAGE_OK.load(Ordering::Relaxed) == 0 {
        return;
    }
    let title_buf: Vec<u16> = title.encode_utf16().chain([0]).collect();
    let text_buf: Vec<u16> = text.encode_utf16().chain([0]).collect();
    let wstring = |buf: &Vec<u16>| WString { ptr: buf.as_ptr(), _pad: 0, len: buf.len() - 1, cap: (buf.len() - 1).max(8) };
    let (title_w, text_w) = (wstring(&title_buf), wstring(&text_buf));
    let add: unsafe extern "C" fn(usize, *const WString, *const WString, u32) = acs_fn(ADD_MESSAGE);
    add(system_message, &title_w, &text_w, 0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guid_round_trip() {
        let text = "0004346E-0000-0000-0000-504944564944";
        let g = parse_guid(text).unwrap();
        assert_eq!(&g[0..4], &[0x6e, 0x34, 0x04, 0x00]);
        assert_eq!(&g[8..], b"\x00\x00PIDVID");
        assert_eq!(guid_text(&g), text);
        assert_eq!(parse_guid("{35AE76A0-4795-11EF-8001-444553540000}").map(|g| guid_text(&g)).as_deref(), Some("35AE76A0-4795-11EF-8001-444553540000"));
        assert!(parse_guid("nope").is_none());
    }
}
