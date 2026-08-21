# mater on Ableton Move

A second wrapper around [`granny-core`](../crates/granny-core), next to the CLAP one. This builds
the engine as a [Schwung](https://github.com/charlesvestal/schwung) module, so it loads into a
Signal Chain slot on an Ableton Move and plays from the pads.

Nothing from the CLAP build comes with it. `granny-core` has no host dependencies at all, so the
whole port is the ABI glue in [`crates/mater-schwung`](../crates/mater-schwung) — no nih-plug, no
egui, no symphonia. The MPE channel model used to live in `mater-plugin`; it now sits in
`granny-core` alongside the engine, so both wrappers resolve a member-channel bend the same way.

```
crates/mater-schwung/src/
  lib.rs      the instance, and the C ABI the host dlopen()s
  bridge.rs   set_param / get_param, and the chain_params the Shadow UI reads
  state.rs    the state blob: written by hand on the audio thread, read with serde_json
  midi.rs     Move's MIDI bytes into engine events, through granny-core's MpeState
  wav.rs      a WAV reader, and the same 8-bit conversion the CLAP loader does
  host.rs     the host side of the ABI, transcribed from plugin_api_v1.h
schwung/
  module.json what appears in the menus, and where each parameter sits
  build.sh    cross-compile for the Move's ARM, in Docker
  install.sh  copy onto an attached Move, for development
```

## Building

```
schwung/build.sh
```

Re-runs itself inside a `debian:bookworm` container — the same base Schwung's own `build.sh` uses,
so the module links against a glibc the device has — and leaves
`dist/mater-module.tar.gz` behind. The host needs a running Docker daemon and nothing else.

The result is a ~517 KB stripped ARM64 shared object exporting `move_plugin_init_v2`, needing
nothing but `libc`, `libm` and `libgcc_s`, with a highest symbol requirement of `GLIBC_2.34`.
Schwung's own `schwung` binary reaches `GLIBC_2.35`, so anything that can run the host can load
this. Worth re-checking with `aarch64-linux-gnu-readelf -sW --dyn-syms` if a dependency is ever
added.

Two things about the container are easy to get wrong. `libc6-dev-arm64-cross` is only a
*recommend* of the cross-gcc, so under `--no-install-recommends` it is absent and the build fails
at the final link with `cannot find crti.o` — it is named explicitly in the Dockerfile for that
reason. And the first build resolves the whole workspace, so it fetches nih-plug's git
dependencies even though `-p mater-schwung` never compiles them; they are cached under
`target/schwung-docker/cargo` afterwards.

## Verifying

```
cargo test -p mater-schwung                                   # unit + ABI tests
cargo check -p mater-schwung --target aarch64-unknown-linux-gnu   # ARM, without a linker
```

`tests/abi.rs` drives the module through the raw `plugin_api_v2` function pointers rather than
calling `Instance` directly, because the ABI is the part nothing else checks — a wrong signature is
a segfault on the device and silence in a unit test. It plays a note at a loaded sample and asserts
something came out, round-trips every parameter through the state blob, deletes a sample file and
recalls the preset anyway, and hands each entry point the nulls a teardown race produces.

Three lists have to name the same parameters — `PARAM_KEYS`, `chain_params` and `module.json`'s
`ui_hierarchy` — and a unit test reads `module.json` off disk to hold all three to it. A key in
only two of the three is a row that never appears, or one that reads blank.

To install it on a Move for testing:

```
schwung/install.sh              # or: schwung/install.sh <host>
```

That is a development loop, not the supported install path. A released module is fetched by
schwung-manager at `http://move.local:7700` from a `release.json` on the repo's default branch.

## What it does

- **A sample browser**, `sample...` at the top of the menu. It walks the module's own directory
  and `UserLibrary/{Samples,Recordings}` — where Move keeps recordings and where Schwung's
  resampler and skipback write — and offers what it finds, nested paths labelled by their path and
  the loaded one marked. Setting `sample_path` directly still works; the browser is what makes it
  reachable from the device.
- **A default sample**, shipped in the module, so a freshly-loaded slot makes a sound. Without one
  the module comes up silent with nothing on screen to say why, which is exactly how it shipped
  the first time.
- **Loads a WAV** resampled to 22050 Hz and normalised exactly as the CLAP loader does, so a
  sample sounds the same in both. YIN detection runs on load and sets the root, so playing note N
  gets you note N.
- **Plays notes** — note on/off, velocity, sustain, and per-channel bend, pressure and CC74
  resolved through `granny-core`'s MPE model. Eight voices by default.
- **The eight knobs** sit on the eight physical encoders in the firmware's order, in the firmware's
  own integer ranges. Below them are the setting bits, the tuning controls, the MPE zone, the
  three-slot mod matrix and the fidelity switches.
- **Note mode**, including `split` — one channel picking slices while the rest play in
  tune, out of the same file. A split forces the MPE zone off, because the two cannot both own the
  channel number; `mpe_active` reports the zone actually in force so a UI can grey the controls.
- **Scala** — load a `.scl` and optional `.kbm` by path. The text, not the path, goes into the
  state blob, so a preset carries its own tuning. `24edo.scl` ships alongside the default sample,
  so `scala_path` can be pointed at the module's own directory to get quarter tones without
  moving a file onto the device. Under the default keyboard mapping it puts A440 on note 69 and
  makes every note number half a semitone, which is what a controller sending one note number per
  quarter tone — a LinnStrument in note-number mode, say — already emits. `snap` reaches the same
  grid from the other direction, quantising a continuous MPE pitch onto it rather than remapping
  the note numbers, and the two are safe together because a 24-EDO scale already lands on it.
- **The hardware CC map** — CC102–109 and CC1, off by default. Unlike the CLAP build these write
  the parameters directly and the displayed value follows: a Schwung module has no restriction on
  writing its own parameters from the audio thread, so the override table the CLAP build needs
  does not exist here.
- **A state blob**, which is what slot autosave, chain patches and User Presets all ride on. See
  below.

## The state blob, and what "self-contained" costs

`docs/MODULES.md` asks for a `state` that is self-contained rather than a reference into a bank.
The host's buffer is 64 KB and base64 costs a third on top, so about 45 KB of audio fits — two
seconds at 22050 Hz.

Under that, the audio is embedded and the preset really is self-contained: it survives the sample
file being deleted, which the tests check by deleting it. Over it, only the path is written and
recall re-reads the file. The blob says which happened, in `"embedded"`, so a preset carried to
another device can explain itself instead of just coming up silent.

An absent `sample` key means the blob has no opinion — an older blob, or one from something else
— and whatever is loaded stays. Only an explicit `null` clears it. Reading absence as a clear is
how a slot that autosaved before its sample existed came back silent on every reload, wiping the
default the module had just loaded.

The writer does not predict whether the audio fits — it *tries* to embed, and rewinds to write the
path form if it did not. An estimate that was wrong in the optimistic direction would lose the whole
blob, and Schwung reacts to an empty `state` by preserving the old `slot_N.json` and quietly
giving up on autosave.

## What it does not do yet

| | |
|---|---|
| **Phase 3** | Sample loading off the audio thread — see below. Formats beyond WAV. Browsing for a sample from the device rather than setting a path. |
| **Phase 4** | Anything drawn. A waveform strip and loop handles in `ui_chain.js`, and the real editor as a `web_ui.html` Remote UI, which is where the loop points, slice divisions and playheads want to live. |

Three departures from the CLAP build are deliberate rather than pending.

**The MPE zone defaults off**, where the CLAP build defaults to the lower zone: a Move slot
receives on one channel and forwards on one channel, so plain MIDI is the normal case and a zone
would put a master channel where a Move track simply is one. MPE needs the slot set to Receive=All
+ Forward=THRU before it means anything at all. `mpe_enabled` turns it on without naming a zone,
which is what `quartertone` and anything else driving a slot generically will set.

**`voices` defaults to eight** rather than sixteen — see below.

**`vel_sensitivity` defaults to 0.4** rather than the hardware's full response. The firmware's
envelope steps a 38-entry *dB* table and velocity decides how far down it starts, which is far
steeper than the synths a Move user arrives from. Measured on a device, at full response a note at
velocity 1 lands 31 dB below one at 127 — and Move's pads, through an overtake tool that passes
velocity through, send single digits routinely (a real session's log had four notes at velocity 1
out of twenty-three). The module reads as silent.

| Vel Sens | vel 1 | vel 13 | vel 52 | vel 100 |
|---|---|---|---|---|
| 0.0 | 0 | 0 | 0 | 0 |
| 0.4 | −12 | −11 | −7 | −2 |
| 1.0 | −31 | −28 | −18 | −6 |

0.4 halves the depth rather than removing it: the worst case a pad can send is audible and twelve
dB of dynamics survive. Set it to 1.0 for the instrument's own response, or 0 to ignore velocity.

## Two things to know before changing anything here

**Everything runs on the audio thread.** Schwung calls `render_block`, `on_midi`, `set_param` *and*
`get_param` from the SPI callback, at SCHED_FIFO with about 900 µs a frame. That is stricter than
CLAP, where only `process` is realtime. So `get_param` formats into a stack buffer rather than a
`String`, and writing the state blob — 64 KB, base64 and all — goes straight into the host's own
buffer without allocating a byte.

Three things are exceptions, and all three sit on the slot-load path where a dropout is already
expected: `load_sample`, `load_scala` and restoring a state blob. Each reads a file or parses JSON
inline. Phase 3 moves the sample decode to a worker and swaps the buffer through an atomic, freeing
the old one off-thread.

**Nothing may panic across the ABI.** A panic crossing `extern "C"` aborts the process, and on a
Move that process is the audio server — a stray bounds check would take the instrument down
mid-set rather than dropping one note. Every entry point is wrapped in `catch_unwind` as a
backstop, which is a backstop and not a licence: the code inside is still written not to panic.

**A chainable synth serves its own `ui_hierarchy`.** `chain_host.c` falls back to `module.json`
for an audio FX's hierarchy but not for a synth's — `synth:ui_hierarchy` is forwarded straight to
the plugin. A module that only declares the hierarchy in `module.json` loads into a slot with no
menu at all: every parameter exists, `chain_params` describes them, and none of them can be
reached. So `UI_HIERARCHY` in `bridge.rs` is what the chain host reads, `module.json` keeps its
copy for the module manager and the standalone host, and a test holds the two to being the same
JSON.

**`chain_params` owns the ranges, `module.json` owns the layout.** The Shadow UI learns step sizes,
minima, maxima and enum options from the `chain_params` JSON that `get_param` returns; `ui_hierarchy`
in `module.json` says only which keys appear, in what order, and under which knob. Adding a
parameter means touching both, but they can never disagree about a range.

**A `step` on an `int` is dead metadata.** `shared/knob_engine.mjs` reads `step` on the float path
only — `(step / divisor) * direction`. Its int path accumulates detents and emits a single unit
once the accumulator reaches the acceleration divisor, which is 4 at a fast sweep and 16 at a slow
one, so an int moves at most one unit per four detents whatever it declares. A 0..1023 int is 4089
detents end to end. Anything that wants a real sweep is declared `float` with a step and a
`display_format` of `".0f"` to keep it reading as a whole number; `set_param` then receives
decimals on the wire, which `clamp_u16` already parses and rounds.

## Voices

Eight, against the CLAP build's sixteen. Measured on a Move, rendering granular (`grain` 30,
`shift` 200), timing `render_block` alone and net of the ~3.8 µs a ctypes round trip costs:

| voices | µs/block | of realtime |
|---|---|---|
| 0 | 2.6 | 0.1 % |
| 1 | 5.8 | 0.2 % |
| 8 | 23.0 | 0.8 % |
| 16 | 42.2 | 1.5 % |

About 2.5 µs a voice, on top of 2.6 µs fixed. Against a 2902 µs block that reads as nothing, and
it was tempting to raise the default to sixteen and be done.

The frame is the wrong denominator. `spi_timing` on the same device reports a frame total
averaging 2759 µs against the 2902 µs period, of which the SPI transfer itself is 2679 µs — so
the slack everything else shares is about 143 µs typically and was observed as low as 39 µs. The
obxd in the next slot renders in 58 µs at its worst. Sixteen voices would ask for 42 µs of a
worst case that has 39 µs in it, and this is one slot of four.

So eight stays, and the reason is now a number rather than caution. Sixteen is available and fine
on a quiet frame; it is not something to default to. If this is ever revisited, measure
`Slot render max(us)` under load rather than a percentage of realtime — realtime is not the
budget, the gap after the transfer is.
