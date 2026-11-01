# ffb-server

Force feedback for games that can't drive your wheel themselves. A game mod sends vehicle
telemetry, or forces it computed itself, to `ws://127.0.0.1:47820/telemetry`. The server turns
that into spring, damper, constant and vibration effects on the wheel.

It started as the Linux force feedback bridge for Farming Simulator 22 and is now separate from
any one game. [V987 Wheel Studio](https://github.com/egeSlop/v987-wheel-studio) embeds it and
shows live status. This repository is the server on its own: a Rust library and a small
command-line program.

- **Linux:** evdev force feedback (`EVIOCSFF`). Works with any wheel the kernel exposes as a force
  feedback joystick. Tested with a LiteStar / Rampage Pace V987.
- **Windows:** DirectInput 8. **Untested**: it compiles, but nobody has run it on a real wheel yet.
  Reports are welcome.

Writing a mod for a game? The whole protocol, with examples in Python, JavaScript and Lua, is in
[docs/FFB-SERVER.md](docs/FFB-SERVER.md).

## Running it

```sh
cargo build --release
./target/release/ffb-server          # serve and drive the wheel until Ctrl+C
./target/release/ffb-server test     # 12 s hardware check: push, centering, weight, vibration
./target/release/ffb-server config   # show the settings file and its values
```

On Linux your user needs write access to the wheel's `/dev/input/event*` node. Most distributions
grant it to the logged-in user through `uaccess`.

Settings live in `~/.config/v987-wheel-studio/ffb-server.toml` (`%APPDATA%\v987-wheel-studio\` on
Windows) and are reloaded while the server runs.

## As a library

```rust
let server = ffb_server::FfbServer::start("My App", ffb_server::config::load())?;
println!("{:?}", server.status());
let lines = server.subscribe();   // the same JSON lines /stream sends
server.stop();                     // removes every effect; the wheel goes back to its idle spring
```

## How the force model works

For telemetry, the server keeps five effects loaded and updates them in place:

| Effect | Comes from |
|---|---|
| spring | speed (centering grows up to about 40 km/h), lower on loose surfaces |
| damper | base steering weight, much heavier with the engine off while stopped |
| constant | left/right suspension difference on the steered wheels (bumps), sudden deceleration (impacts) |
| sine 1 | surface roughness, faster with speed |
| sine 2 | engine firing frequency from rpm, fades with speed |

Every part is scaled by a value in the `[feel]` table of the settings file.

## License

MIT
