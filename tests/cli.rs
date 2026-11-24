//! The command line, as far as it works without a compositor.

use std::process::{Command, Output};

fn hacek(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hacek"))
        .args(args)
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("WAYLAND_SOCKET")
        .env("XDG_RUNTIME_DIR", "/nonexistent")
        .output()
        .unwrap()
}

fn lists(args: &[&str], want: &str) {
    let out = hacek(args);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8(out.stdout).unwrap(), want);
}

fn fails(args: &[&str], why: &str) {
    let out = hacek(args);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains(why), "{err:?} lacks {why:?}");
}

#[test]
fn lists_slovenian() {
    lists(&["--layout", "si", "--list"], "c  č ć\nd  đ\ns  š\nz  ž\n");
}

#[test]
fn layouts_and_extras_combine() {
    lists(
        &["-l", "si", "-l", "de", "-x", "e=éè", "--list"],
        "a  ä\nc  č ć\nd  đ\ne  é è\no  ö\ns  š ß\nu  ü\nz  ž\n",
    );
}

#[test]
fn layout_variants_are_understood() {
    let out = hacek(&["-l", "us(intl)", "--list"]);
    assert!(String::from_utf8(out.stdout).unwrap().contains('é'));
}

#[test]
fn nothing_to_list() {
    lists(&["--layout", "us", "--list"], "");
}

#[test]
fn bad_options() {
    fails(&["--nope"], "unknown option --nope");
    fails(&["--layout"], "--layout needs a value");
    fails(&["--layout", "no_such_layout", "--list"], "no such keyboard layout");
    fails(&["--delay", "soon"], "milliseconds");
    fails(&["--extra", "éè", "--list"], "e=éè");
    fails(&["--extra", "ab=éè", "--list"], "single letter");
}

#[test]
fn no_compositor() {
    fails(&[], "cannot connect to Wayland");
}
