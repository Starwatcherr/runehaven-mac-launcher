<p align="center"><img src="assets/runehaven-r.gif" width="128" alt="Runehaven R"></p>

# Runehaven Launcher

An **unofficial** launcher that lets you play the Windows version of [Runehaven](https://store.steampowered.com/app/2676030/) on Apple Silicon Macs. Project by Starwatcherr.

> Not affiliated with or endorsed by Cikoria Studio. You need your own copy of Runehaven on Steam.

**Status:** early development. There is no app yet, only the core and a command-line test tool (`rhl`).

## How it works

1. Steam on macOS normally refuses to install Windows-only games. The launcher asks your own Mac Steam client to install the Windows build of Runehaven, then switches Steam back to normal. Steam keeps the game updated from then on.
2. The game runs through Wine with DXMT (Direct3D 11 → Metal) and Rosetta 2.
3. A small input fix (`version.dll`) translates mouse input into the form Unity expects, which Wine does not provide yet.

The launcher never touches your Steam login or game files beyond placing the input fix next to the game.

## Building the test tool

Requires [Rust](https://rustup.rs) on macOS.

```sh
cargo build --release
./target/release/rhl doctor
```

| Command | What it does |
|---|---|
| `rhl doctor` | Checks Mac, Rosetta, Steam and the game installation |
| `rhl steam wait` | Starts Steam if needed and waits until it answers |
| `rhl steam platform` | Shows Steam's platform setting |
| `rhl steam status` | Steam's own status of Runehaven |
| `rhl steam recover` | Restores Steam's platform after an interrupted install |
| `rhl game locate` | Where Steam keeps Runehaven and its state |
| `rhl game install` | Installs the Windows build through Steam |
| `rhl game launch` | Starts the game (needs a Wine runtime in the config) |
| `rhl config show` / `set` | Shows or changes the configuration |

## Credits

- [Wine](https://www.winehq.org), [Sikarugir](https://github.com/Sikarugir-App) WineCX builds, [Gcenx](https://github.com/Gcenx) macOS Wine builds
- [DXMT](https://github.com/3Shain/dxmt) by 3Shain
- [kaon](https://github.com/natbro/kaon) for documenting Steam's platform override on macOS
- Runehaven by Cikoria Studio

## License

GNU General Public License, version 3 or later, see [LICENSE](LICENSE), with additional terms: the credit "Project by Starwatcherr" stays visible, and the name and logo are not licensed, so distributed forks need their own. The logo, the app icon, the Runehaven screenshot and the fonts are under other terms. Details in [NOTICE.md](NOTICE.md).

Runehaven itself and its assets belong to Cikoria Studio.
