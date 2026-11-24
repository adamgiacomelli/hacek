# hacek

Hold a letter, get its accented variants: the macOS press-and-hold accent menu, for Wayland.

![Menu with č and ć, numbered 1 and 2](docs/screenshot.png)

Named after the *háček*, the little hook on č, š and ž.

- Hold `c`, press `2`, get `ć`. Capitals work the same.
- Opens only on letters that have variants in your keyboard layouts. Other keys repeat as usual.
- Left/Right and Enter or Space also pick; Escape closes; any other key closes and types.
- Never opens in fields marked as passwords.
- No root, no `input` group. It is an ordinary Wayland input method, gets keys only while a
  text field is focused, stores nothing and has no network code.

## Install

Needs Rust, Pango, Cairo and libxkbcommon.

    cargo install --path .

## Use

    hacek --layout si

In Sway's config (with `~/.cargo/bin` on the PATH Sway sees, or the full path):

    exec hacek --layout si

Layouts set up in the compositor are always included, so with `xkb_layout us,si` plain `hacek`
is enough. `--layout` adds layouts you do not otherwise use, `--extra e=éè` adds single letters,
and `--list` shows what the options give you. See `hacek --help`.

Letters behind AltGr count only for layouts with no accents on plain keys (Polish, US-intl).
`--altgr` includes them always. Accents typed with dead keys (é on Spanish) are not detected;
use `--extra`.

## Where it works

The compositor must offer input-method-v2, virtual-keyboard-v1 and layer-shell. Tested on
Sway 1.12; other wlroots-style compositors may work. GNOME and KDE do not offer these.

Tested in foot, Alacritty, GTK 4, Qt 6 and Chrome. Applications need Wayland text input, so
XWayland applications do not get the menu. Neither did a GTK popover in our tests.

It cannot run next to fcitx5 or ibus, as a seat has one input method.

In terminals, letters with variants do not repeat when held, also not in vim.

`--at-caret` puts the menu at the text cursor. It can crash Sway 1.12, and your session with it,
because of a bug in Sway's input popup handling (`input_popup_set_focus`).

## Develop

`cargo test` covers the state machine and variant detection. `cargo run --example type` is a fake
keyboard for end-to-end tests in a nested compositor.

MIT licensed.
