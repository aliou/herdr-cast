# herdr-cast

Cast (`ad.cast`) is my personal [Herdr](https://herdr.dev) plugin. It is published for reference, not as a supported plugin: it fits my machines and changes without notice.

What it adds to Herdr:

- **Agent notifications.** Notifications when an agent in a pane becomes blocked or finishes. On macOS, clicking one focuses the pane and raises its Ghostty window and tab.
- **Remote bridge.** When Herdr runs on another machine over ssh, notifications, clipboard copies, and link clicks still reach the Mac you are looking at.
- **Spaces and titles.** Extra sidebar metadata for each Space, and a terminal window title built from the machine, session, and focused pane.
- **Popups.** Pickers, a command palette, a new-workspace wizard, and lazygit and hunk launchers, opened from key bindings.

It runs on macOS 26 or newer, or Linux with a terminal that supports Herdr notifications. It needs Herdr 0.8.0 or newer; forwarding link clicks needs Herdr 0.9.4 or newer.

## Install

```sh
git clone https://github.com/aliou/herdr-cast.git
cd herdr-cast
cargo build --release
herdr plugin link "$PWD"
```

[`docs/setup.md`](docs/setup.md) covers requirements, macOS notification and Automation permissions, and the Herdr configuration Cast expects.

## Documentation

- [`docs/setup.md`](docs/setup.md): install, permissions, Herdr config, shell integration.
- [`docs/notifications.md`](docs/notifications.md): when Cast notifies and how notifications are composed.
- [`docs/bridge.md`](docs/bridge.md): the remote bridge protocol and its fallbacks.
- [`docs/spaces-and-titles.md`](docs/spaces-and-titles.md): sidebar tokens and window titles.
- [`docs/popups.md`](docs/popups.md): popup entry points and sizing.
- [`docs/releases.md`](docs/releases.md): CI binaries and releases.
- [`CONTEXT.md`](CONTEXT.md): the terms used across the code and docs.

## Development

```sh
cargo fmt -- --check
cargo test
cargo build --release
```

`AGENTS.md` has the rules for testing against a live Herdr. Runtime tests change real state, so they run only in a disposable Herdr session.
