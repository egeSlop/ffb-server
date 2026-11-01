# FFB server protocol (v1)

The FFB server drives a force feedback wheel for games that can't. A mod, plugin or small
adapter program reads the game's vehicle data and sends it to the server over a local
WebSocket. The server turns it into forces on the wheel.

It runs inside [V987 Wheel Studio](https://github.com/egeSlop/v987-wheel-studio), or on its own as
`ffb-server`. It works with any wheel the OS exposes as a force feedback joystick, not only the V987.

## Connecting

| | |
|---|---|
| Send game data to | `ws://127.0.0.1:47820/telemetry` (`ws://127.0.0.1:47820/` works too) |
| Watch what the server is doing | `ws://127.0.0.1:47820/stream` |

Every message, in both directions, is one JSON object in a WebSocket text frame. Field names are
camelCase. Unknown fields are ignored, so you can send extra data without breaking anything.

A session looks like this:

1. Connect. The server sends a `welcome`.
2. Send one `hello` saying which game you are. This is optional, but it's what the app shows.
3. Send a `telemetry` or `forces` message every frame, ideally 30 to 120 times a second.
4. Send `"active": false` when the player is in a menu, has paused, or isn't in a vehicle. The
   wheel goes slack.
5. Disconnect whenever you like. If no message arrives for 400 ms (`stale_ms` in the settings),
   the server switches the forces off by itself, so a crashed game never leaves the wheel pulling.

Only one game drives the wheel at a time. If two are connected, the most recent message wins.

## Messages you send

### hello

```json
{"type": "hello", "game": "Farming Simulator 22", "source": "FS22_WheelFFB 1.0", "protocol": 1}
```

`game` is a human-readable name. `source` is your mod or program and its version.

### telemetry: let the server work out the forces

Send what the vehicle is doing and the server's force model does the rest: centering that grows
with speed, steering weight, bumps from the suspension, impacts, surface texture and engine
vibration. The user tunes how strong each part is in the app. A message without `"type"` is read
as telemetry.

```json
{
  "type": "telemetry",
  "active": true,
  "speed": 8.4,
  "engine": true,
  "rpm": 1450,
  "maxRpm": 2200,
  "load": 0.6,
  "vFwd": 8.4, "vLat": 0.2, "vUp": -0.05,
  "yawRate": 0.03,
  "wheels": [
    {"i": 1, "steered": true,  "x":  1, "y": 0.012, "surface": "field"},
    {"i": 2, "steered": true,  "x": -1, "y": 0.004, "surface": "field"},
    {"i": 3, "steered": false, "x":  1, "y": 0.010, "surface": "field"},
    {"i": 4, "steered": false, "x": -1, "y": 0.008, "surface": "field"}
  ]
}
```

| Field | Unit | Needed | What it does |
|---|---|---|---|
| `active` | bool | no, default `true` | `false` turns the forces off (menu, pause, on foot) |
| `speed` | m/s | yes | centering and texture grow with it |
| `engine` | bool | yes | engine off and stopped makes the steering heavy, like losing power steering |
| `rpm` | rev/min | no | engine vibration frequency; without it there is no engine vibration |
| `maxRpm` | rev/min | no | reserved |
| `load` | 0..1 | no | engine vibration gets stronger under load |
| `vFwd` | m/s, forward | no | sudden changes become impacts |
| `vUp` | m/s, up | no | hard landings become a short kick |
| `vLat` | m/s, left | no | reserved |
| `yawRate` | rad/s | no | picks the direction of impact kicks |
| `t` | seconds | no | your own clock. Leave it out and the server timestamps each message |
| `game` | string | no | same as `hello.game`, for clients that can't send a hello |
| `wheels` | list | yes | see below. An empty list still gives centering and weight |

Each wheel:

| Field | Meaning |
|---|---|
| `i` | any number that stays the same for this wheel between messages |
| `steered` | `true` for wheels that steer. Bumps and grip come from these |
| `x` | which side: positive is left, negative is right. Only the sign matters |
| `y` | suspension position in metres, larger means more compressed. Only changes matter, so any zero point works |
| `surface` | what the tyre is on, see below |

When the left and right steered wheels move differently, the server twists the wheel, the way
hitting a pothole with one front wheel does.

Surfaces set how rough the ground feels and how much grip is left for centering:

| Surface | Names the server understands |
|---|---|
| smooth road | `road`, `asphalt`, `concrete`, `tarmac` |
| firm and rough | `hard`, `gravel`, `dirt`, `cobbles`, `kerb` |
| soft | `soft`, `grass`, `sand` |
| ploughed or muddy | `field`, `mud` |
| snow | `snow` |
| ice | `ice` |
| wheel in the air | `air` |

Any other name counts as a firm, slightly rough surface.

### forces: send your own

If your game already knows what the wheel should feel (it has its own force feedback maths, or
you want full control), send forces instead. The server only limits them to their ranges, applies
the user's overall strength and runs the 400 ms safety timeout.

```json
{"type": "forces", "spring": 0.4, "damper": 0.2, "constant": -0.15,
 "texture": 0.1, "textureHz": 30, "engine": 0.05, "engineHz": 25}
```

| Field | Range | Default | Effect |
|---|---|---|---|
| `active` | bool | `true` | `false` turns the forces off |
| `spring` | 0..1 | 0 | pull toward center |
| `damper` | 0..1 | 0 | resistance to turning quickly (weight) |
| `constant` | -1..1 | 0 | steady push. Positive turns the wheel one way, negative the other |
| `texture`, `textureHz` | 0..1, 1..200 Hz | 0, 30 | first vibration channel |
| `engine`, `engineHz` | 0..1, 1..200 Hz | 0, 25 | second vibration channel |

To stream a single steering torque the way most racing games do, send only `constant` and
update it every frame.

## Messages you receive

On `/telemetry` the server sends very little, and you don't have to read any of it.

```json
{"type": "welcome", "server": "V987 Wheel Studio", "protocol": 1, "client": 3, "wheel": "LITE STAR FFB WHEEL PACE V987"}
{"type": "error", "message": "bad telemetry message: invalid type: string \"fast\", expected f32"}
```

`wheel` is `null` when no wheel is plugged in. The server answers the first five bad messages on
a connection with an `error` and ignores the rest, so a broken client can't flood its own socket.

## Watching: /stream

Anything can connect to `/stream`: a dashboard, a logger, or your own debugging script. It
receives a `status` message twice a second, and a `frame` message about 30 times a second while
a game is driving.

```json
{"type": "status", "server": "V987 Wheel Studio", "protocol": 1, "listen": "127.0.0.1:47820",
 "wheel": {"name": "LITE STAR FFB WHEEL PACE V987", "path": "/dev/input/event30"}, "wheelError": null,
 "mode": "telemetry", "game": "Farming Simulator 22",
 "forces": {"spring": 0.27, "damper": 0.25, "constant": 0.08, "texture": 0.17, "textureHz": 22, "engine": 0.04, "engineHz": 50},
 "clients": [{"id": 1, "game": "Farming Simulator 22", "source": "FS22_WheelFFB 1.0", "kind": "telemetry", "frames": 5120, "errors": 0}]}

{"type": "frame", "mode": "telemetry", "game": "Farming Simulator 22", "telemetry": {...}, "forces": {...}}
```

`mode` is `idle`, `telemetry`, `forces` or `test`.

## Examples

### Python

```python
import asyncio, json, websockets

async def main():
    async with websockets.connect("ws://127.0.0.1:47820/telemetry") as ws:
        await ws.send(json.dumps({"type": "hello", "game": "My Game", "source": "my_adapter.py"}))
        while True:
            car = read_game()  # your code
            await ws.send(json.dumps({"type": "telemetry", "speed": car.speed, "engine": car.engine_on,
                                      "rpm": car.rpm, "wheels": car.wheels}))
            await asyncio.sleep(1 / 60)

asyncio.run(main())
```

### JavaScript (browser or Node 22+)

```js
const ws = new WebSocket("ws://127.0.0.1:47820/telemetry");
ws.onopen = () => {
  ws.send(JSON.stringify({ type: "hello", game: "My Game", source: "web-demo" }));
  setInterval(() => ws.send(JSON.stringify({ type: "forces", constant: Math.sin(Date.now() / 300) * 0.3 })), 16);
};
```

### Lua, when the game has no sockets

Many game Lua sandboxes have no network access. Farming Simulator 22 is one. The FS22 mod writes
JSON lines to a Windows named pipe with `io.open`, and a tiny relay program forwards them to the
server. See [FS22_WheelFFB](https://github.com/egeSlop/FS22_WheelFFB) for both parts; the relay
works for any game that can write lines to a file or pipe.

```lua
local pipe = io.open("\\\\.\\pipe\\WheelFFB", "w")
if pipe then
    pipe:setvbuf("no")
    pipe:write('{"type":"hello","game":"My Game","source":"MyMod 1.0"}\n')
    -- every frame:
    pipe:write(string.format('{"speed":%.2f,"engine":true,"wheels":[]}\n', speed))
end
```

If your game's Lua does have LuaSocket or a WebSocket library, connect to the server directly.

## Settings

The user's settings live in `~/.config/v987-wheel-studio/ffb-server.toml` on Linux and
`%APPDATA%\v987-wheel-studio\ffb-server.toml` on Windows. The server reloads the file within half a
second of a change. Wheel Studio's FFB server page edits it for you.

```toml
master_gain = 0.8          # overall strength, 0..1
invert = false             # flip bumps and impacts
stale_ms = 400             # silence after which forces switch off
listen = "127.0.0.1:47820"
device = "auto"            # or an evdev path such as /dev/input/event30
autostart = true           # Wheel Studio starts the server when it opens

[feel]                     # used for telemetry only, all 0..1
centering = 0.55
centering_parked = 0.08
damping = 0.30
damping_engine_off = 0.65
bumps = 0.60
impacts = 0.80
texture = 0.25
engine = 0.10
```

## Versioning

This is protocol 1. New optional fields can appear without a version change. Anything that
would break an existing client gets a new number, and the server will keep accepting version 1.
