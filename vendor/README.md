# Patched dependencies

## crossterm

`crossterm/` is crossterm 0.29.0 from crates.io (its `src`, licence and readme; the examples
and their entries in `Cargo.toml` left out, and its compiler warnings turned off there, as cargo
does for crates from crates.io), with one change on top. The root `Cargo.toml`
puts it in place of the published crate with `[patch.crates-io]`.

**The change:** Konsole's default keytab sends F1–F4 with Shift, Alt or Ctrl held as SS3 with
the modifier in between: Alt+F1 is `ESC O 3 P`, Shift+F4 `ESC O 2 S`. xterm and Windows
Terminal send `ESC [ 1 ; 3 P`. crossterm 0.29.0 knew only the second: it dropped `ESC O 3` and
read the `P` as a key of its own, so in Konsole Alt+F1 and Alt+F2 typed P and Q instead of
opening the drive picker, and Shift+F2 and Shift+F4 typed Q and S.
`parse_ss3_modifier_key_code` in `src/event/sys/unix/parse.rs` reads them; the tests are
`test_parse_ss3_with_modifier` there and `konsole_alt_f1_is_one_key_and_no_stray_letter` in
`src/event/source/unix/mio.rs`.

**Testing it:** crossterm isn't part of cm's own tests (its tests need dev-dependencies cm
doesn't otherwise pull in). From this folder:

```bash
cd vendor/crossterm
CARGO_TARGET_DIR=/tmp/crossterm-target cargo test --lib -- ss3 konsole
rm -f Cargo.lock
```

**Dropping it:** once a crossterm release reads `ESC O <modifier> <key>`, delete this folder
and the `[patch.crates-io]` section, and run `cargo update -p crossterm`.
