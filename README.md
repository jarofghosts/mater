# Mater

A CLAP instrument that ports the **Bastl microGranny 2.5** — a monophonic 8-bit granular sampler
built on an ATmega328 — and makes it polyphonic, MPE-capable and microtonal.

The DSP is a faithful port, quirks included. One plugin instance is one hardware *preset*: a single
sample, one set of the eight knob values, one set of setting bits, all stored in the plugin's state
and saveable to a self-contained `.mater` file.

```
cargo xtask bundle mater --release      # -> target/bundled/Mater.clap
```

Copy `Mater.clap` to `~/.clap/` (Linux), `~/Library/Audio/Plug-Ins/CLAP/` (macOS) or
`%COMMONPROGRAMFILES%\CLAP\` (Windows).

There is also a standalone build for playing it without a host:

```
cargo run -p mater --features standalone --bin mater-standalone -- --backend jack
```

## The eight knobs

Parameters keep the hardware's raw integer ranges, so automation and preset values line up
one-to-one with a real microGranny. The derived value is shown in brackets.

| Parameter | Range | Default | What it does |
|---|---|---|---|
| Rate | 0–1023 | 877 | Playback rate. In Pitch mode the note sets the rate and this becomes a transpose (−35.9 to +5.0 st, zero at the default). |
| Crush | 0–127 | 0 | Bitcrush. A mask OR'd into the 12-bit DAC word — it *sets* low bits rather than clearing them. |
| Attack | 0–127 | 0 | Milliseconds **per envelope step**, not total time. 37 steps, or 13 below 30 ms. |
| Release | 0–127 | 0 | Same, for the release. |
| Grain Size | 0–127 | 0 | Grain length, 0–3942 ms. Zero switches the granular engine off. |
| Shift | 0–255 | 128 | Bytes the read head jumps each grain, ±16000. Dead zone at the centre. |
| Start | 0–1023 | 0 | Loop start, in 1/1024ths of the file. |
| End | 0–1023 | 1022 | Loop end. 1000 and above means "end of sample". |

All eight support CLAP polyphonic modulation.

### Settings

`Note Mode` is the firmware's TUNED bit: **Pitch** (notes set the rate, Start sets the loop start) or
**Slice** (notes select one of 60 slices, the Rate knob sets the pitch). `Legato`, `Repeat`, `Sync`
and `Random Shift` are the remaining setting bits. `Hold`, `Level` and `Voices` replace hardware
gestures. `Sync` follows the host transport where the hardware follows MIDI clock.

## Matching the keyboard

A sampler only plays in tune if it knows what pitch the sample already is. On load, Mater runs YIN
pitch detection over the audio and uses the result as the **root** — the note that plays the sample
back untransposed. Playing note N then transposes by `N - root`, so N is what you actually hear, and
a microtonal MPE bend lands where it was asked to.

- **Match Input Pitch** (default on) — use the detected root. Turn it off for the hardware's
  behaviour, where B3 plays the file at its recorded speed and the sounding pitch is whatever the
  sample happens to be.
- **Root Adjust** — ±24 semitones on top of detection, in hundredths of a semitone. Use it when the
  sample is unpitched, when detection lands an octave out, or to detune deliberately.

The editor states what it found: `detected A3 +1 ¢ at 220.2 Hz (100 % confident)`, or
`no clear pitch in this sample` for a drum hit or a noise wash, in which case the root falls back to
B3 and Root Adjust is yours to set by ear.

**Pitch Table defaults to Equal temperament** for this reason. The hardware's own note table is a
rounded approximation — up to +10 ¢ sharp at the bottom of its range — so it cannot track an
incoming note exactly. Switch it to *Hardware table* if you want the instrument's own slightly
stretched tuning back; every other fidelity default is unchanged.

## Polyphony, MPE and tuning

Sixteen voices, each with its own read head, grain clock and envelope; the eight knobs stay global
and are re-read every control tick, exactly as the firmware's `renderTweaking` does.

- **CLAP note expressions** — tuning, pressure, brightness, volume and pan, per voice.
- **MPE** — lower or upper zone, or off for plain global MIDI. Per-channel bend, channel pressure
  and CC74. Two bend ranges, as MPE specifies: `mpe bend range` (±48 by default) for member
  channels, and `midi bend range` (±2) for the master channel and for plain MIDI with the zone off.
  Either follows RPN 0 for its own class of channel when `Follow RPN 0` is on.
- **Snap** — quantise the final pitch, after bend, to 24-EDO (quarter tones) or 12-EDO.
- **Root** — everything above is relative to the detected sample root, so bends and scale steps are
  measured from a pitch that is actually correct.
- **Scala** — load a `.scl` scale and optional `.kbm` keyboard map. The text is stored in the
  plugin state, so a preset carries its own tuning. Unmapped keys stay silent.
- **Mod matrix** — three slots of source (pressure, slide, velocity, bend, random) → destination
  (any knob, level, pan) with a bipolar depth, applied per voice on top of the knob values.

## Fidelity switches

Every one of these defaults to the hardware's behaviour. They exist because polyphony and MPE
already depart from the instrument, so it seemed worth being able to keep going.

- **Curve Maps** — `map16()` in the firmware does its arithmetic in `uint16_t`, and on AVR `int` is
  16 bits, so several segments of the 2.5 shift curve overflow: the shift knob is genuinely
  non-monotonic in its outer regions, and grain size collapses from 3942 ms to 2070 ms at full
  travel. *Hardware* reproduces the fold; *Extended* redoes it in 32 bits. The editor points out
  when the two disagree at the current setting.
- **Interpolate** — the hardware transposes by changing the DAC clock, so it drops and repeats
  samples. That aliasing is most of the character.
- **Block-Quantise Seeks** — `WaveRP::seek` floors every seek to a 512-byte SD block, so grain start
  points land on a ~23 ms grid at 22050 Hz.
- **Grain Fade** — grains start with a hard discontinuity. This adds a short ramp if you want one.

Two further hardware behaviours are reproduced without a switch, because they are load-bearing:

- Positions are byte offsets into the *whole file*, header included, and seeks refuse to go below
  one SD block — so the first 468 samples of any sample are unreachable, exactly as on the device.
- The envelope steps through a 38-entry dB table on a millisecond clock, and velocity sets the
  sustain attenuation rather than scaling a multiplier.

## The editor

**Save project…** and **Load project…** sit next to the title; the sample controls follow them.
The waveform is the interface: drag the `s` and `e` handles to move the loop points, the shaded band
at the loop start is one grain, and every sounding voice draws its own playhead. Anything that is
simply on or off is a checkbox, anything that is a choice between named alternatives — note mode,
pitch table, snap, the MPE zone, curve maps, and the mod matrix's sources and destinations — is a
row of radio buttons with every option named, and everything with a range is a slider.

A plugin window cannot resize itself, so **UI Scale** in the top right draws the whole interface
larger in steps from 100 % to 250 %, and the bottom-right corner drags the window out to fit it. The
scale is stored with the instance.

## Loading samples

Click **Load sample…**, or drop an audio file (`.wav`, `.aiff`, `.flac`, `.mp3`, `.ogg`, mp4
family) anywhere on the editor. Dropping a `.scl` or `.kbm` loads it as a tuning, and dropping a
`.mater` loads a whole project.

The decoded 8-bit mono buffer is embedded in the plugin state, so a project or preset is
self-contained. Two load-time options:

- **Resample On Load** (default on) resamples to 22050 Hz so MIDI note 59 plays the file at its
  original speed. Turn it off for the literal hardware behaviour, where the file's own rate is
  ignored entirely and only the DAC clock matters.
- **Normalise On Load** (default on) — the microGranny manual itself recommends loud samples.

## Saving a project

Everything is in the plugin's state, so a host that saves its project has already saved the sample,
the scale text, the eight knobs and every switch. **Save project…** writes that same state to a
`.mater` file of your own, and **Load project…** — or dropping the file on the editor — puts it
back: parameters, sample and tuning together, in a host or in the standalone build.

The file is JSON with the audio base64'd inside it, so it is self-contained and needs nothing else
present to open. It carries no reference to the original audio file beyond the path it came from,
which is only used by **reload**.

Loading a project replaces the whole instance, the window size and UI scale included, exactly as
reopening a host project would.

## Hardware CC map

Off by default. When on, CC102–109 map to the eight knobs in firmware order and CC1 maps to crush,
so an existing microGranny MIDI rig drives the plugin unchanged. These *override* the parameter
values rather than automating them, because a plugin cannot write to its own parameters from the
audio thread — the displayed parameter will not move.

## What is not here

- **Sample slots and banks.** One instance is one preset. A `.mater` file saves and recalls that
  preset; browsing a library of them is the host's job.
- **Instant loop.** It is a performance gesture for capturing a sub-loop live. With Start, End and
  Repeat all available as parameters, a static version of it would add a control without adding a
  capability.
- **Recording, MIDI channel selection, the display and the button combinations.** All artefacts of
  having six buttons and four digits.

## Layout

```
crates/granny-core   the port: tables, curves, envelope, DAC, granular engine, tuning, Scala
                     no host dependencies, and where the tests live
crates/mater-plugin  the CLAP wrapper: parameters, voices, MPE, sample loading, projects, editor
```

## Verifying

```
cargo test --workspace
cargo run -p granny-core --example render -- in.wav out.wav note=69 grain=30 shift=200 seconds=3
cargo xtask bundle mater --release && clap-validator validate target/bundled/Mater.clap
```

The `render` example drives the engine offline and writes a WAV, which is the quickest way to hear
a parameter change without opening a host. It reports both the root it detected and the pitch of its
own output, so tuning can be checked without ears:

```
detected root: A3 +1 ¢ (220.2 Hz, 100 % confident)
output pitch: A4 -1 ¢ (439.9 Hz) — note 69 wants 440.0 Hz, -0.5 cents off
```

`--help` lists every knob it accepts.

## Credit and licensing

The microGranny and its firmware are by **Václav Pelousek** for **Bastl Instruments**. The
algorithms and constant tables here were reimplemented in Rust from the published firmware
(`bastl-instruments/bastlMicroGranny`, `examples/microGranny2_5/*.ino` and `WaveRP.cpp`), not
copied. Neither Bastl repository carries an explicit licence, and `WaveRP` derives from GPL'd
sdfatlib, so settle the licensing question before publishing this anywhere.

This project is not affiliated with or endorsed by Bastl Instruments.
