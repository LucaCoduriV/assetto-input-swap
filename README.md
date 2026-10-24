# InputSwap

Switch Assetto Corsa between the gamepad and the wheel without leaving the session, even with a wheel
plugged in after the game started. Personal local tool: nothing on the servers.

- **Ctrl+P** (AC's own, hidden): switches the car to the gamepad (`[X360]` settings of controls.ini).
  Works without this tool.
- **Ctrl+W** (this tool): switches the car to the wheel (the wheel settings of controls.ini).

Both are ignored while the pause menu or the console is open. AC shows "Controls: Wheel" when it worked.

Each switch also writes `INPUT_METHOD` in controls.ini's `[HEADER]` (`WHEEL` on Ctrl+W, `X360` on Ctrl+P;
only that value changes), so whatever reads the file follows: a server HUD that labels records with it (the
Touge Shobu HUD reads it when you connect and at each time attack start), and the next game start.

## How it works

AC reads `Documents/Assetto Corsa/cfg/controls.ini` once, when the car loads (`CarAvatar::initControls`
builds a `DICarControl`, `JoypadCarControl` or `KeyboardCarControl` from `INPUT_METHOD`), and lists the
DirectInput devices once, at start (`DirectInput::initJoys`). Its hidden Ctrl+P (`Sim::onKeyDown`) builds
a `JoypadCarControl` and hands it to `CarAvatar::setControlsProvider`, which swaps it in on the physics
thread and deletes the old one there.

InputSwap is built as `dinput8.dll` and sits in the AC folder. `acs.exe` imports `DirectInput8Create` from
`dinput8.dll` and Windows looks in the game's folder first, so the game loads it at every start (CSP loads
itself the same way, as `dwrite.dll`). Every dinput8 export goes on to the system `dinput8.dll`. Inside
`acs.exe` (any other process only gets the forwarding), it takes over the slot of `Sim::onKeyDown` in Sim's
key listener vtable and, on Ctrl+W:

0. makes `DirectInput::m_singleton` (where `DICarControl` takes its devices) the DirectInput the physics
   thread polls (`PhysicsDriveThread::directInput`, polled each step while `useDirectInput` is set). In a
   session started on the gamepad the singleton was seen empty (with CSP) while the physics thread still
   polled the one made at start; with neither, `DirectInput::singleton()` creates one;
1. lists the game controllers attached now through AC's own `IDirectInput8` and adds the ones AC has not
   seen, with AC's code (`EnumJoysticksCallbackDI`, `InputDevice::InputDevice`);
2. moves the devices to their place in controls.ini's `[CONTROLLERS]` (`__IGUIDn`, else `PGUIDn`), since
   the `JOY=` indices count in AC's device list;
3. builds a `DICarControl` and hands it to `CarAvatar::setControlsProvider`, as Ctrl+P does.

Pressing Ctrl+W while already on the wheel, with no new device, does nothing (a second `DICarControl` on
the same wheel would lose its force feedback when AC deletes the first one). AC's device vector is never
freed while growing: the physics thread may be reading it. Addresses come from `acs.pdb`, which ships with
AC; every called address is checked first, and on a mismatch nothing changes and `inputswap.log` (next to
the DLL) says why.

## Build and install

```bash
cargo build --release
```

With the game closed, copy `target/release/dinput8.dll` into the AC folder (next to `acs.exe`, e.g.
`C:\Program Files (x86)\Steam\steamapps\common\assettocorsa`). The log is `inputswap.log` in that folder.
To uninstall, delete `dinput8.dll` from there.

## Limits

- Tied to acs.exe 1.16 (the Steam build of March 2026); another build refuses to install.
- Keep both setups in the same controls.ini: the wheel bindings and the `[X360]` section (Content
  Manager keeps both when you switch the input method).
- Unplugging a device mid-session does not remove it from AC's list; plugging the same one back in works.
