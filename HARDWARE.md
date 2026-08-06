# Mater Hardware

A standalone build of the Mater engine: an ESP32-P4 running `granny-core` directly, with a 48-pad
RGB grid, eight endless encoders and a workspace display, in place of the CLAP host and the egui
editor.

The sampling algorithms do not change — `crates/granny-core` stays the shared engine, and
`crates/mater-plugin` is what gets replaced. Core gains two things for this target: a fixed-point
read head (§5) and a mono render path (§5). It loses nothing.

Two deliberate simplifications shape the rest of this document: **output is mono**, and **there is
no MPE**. Neither costs the instrument anything it was actually using — see §5.

## 1. Physical layout

Three [Adafruit NeoTrellis](https://www.adafruit.com/product/3954) 4×4 RGB driver PCBs tiled
left-to-right. Each is 60 × 60 mm, designed to butt edge-to-edge, so the grid is **180 × 60 mm,
12 columns × 4 rows, 48 pads**.

```
        col  0    1    2    3    4    5    6    7    8    9   10   11
row 0        P1   P2   P3   P4   P5   P6   T1   T2   T3   T4  SHF  MENU   <- control
row 1        .    .    .    .    .    .    .    .    .    .    .    .     <- string 3
row 2        .    .    .    .    .    .    .    .    .    .    .    .     <- string 2
row 3        .    .    .    .    .    .    .    .    .    .    .    .     <- string 1
```

Eight encoders sit above the grid, the display above those. Rough panel envelope: 220 × 180 mm.

> The silicone pad you linked, [Adafruit 1611](https://www.adafruit.com/product/1611), is the
> **pad only** — $4.95, no driver PCB, and its LED provision is a single 3 mm monochrome LED per
> key. It cannot do colour-with-shade. It is still the right pad; it just needs the NeoTrellis
> underneath it rather than the monochrome Trellis. Buy 3 of each.

## 2. Control row (row 0)

| Pads | Function |
|---|---|
| 0–5 | Preset select, P1–P6 |
| 6–9 | Toggles T1–T4 |
| 10 | SHIFT (momentary, held) |
| 11 | MENU |

**Unshifted toggles** map to the firmware setting bits already modelled in `params.rs`: Legato,
Repeat, Sync, Random Shift.

**Shifted toggles** (bank B) map to: Hold, Match Input Pitch, Interpolate, Quantize Seeks.

SHIFT held also swaps the encoder bank (§4). SHIFT is momentary, not latching — latching a
modifier that changes twelve meanings at once is how you lose track of what mode you are in.

MENU enters the row-root / tuning / file browser screens on the display; the grid stays live so
you can hear edits.

### HUD

Pad meanings change with SHIFT, so silkscreen legends are useless. The display carries a **12-cell
strip along its bottom edge that mirrors row 0 one-to-one**, same left-to-right order, same colours
as the physical pads, relabelling live when SHIFT is pressed. This is the reason the display is a
requirement rather than a luxury, and it costs nothing extra — it reuses the panel already needed
for the waveform.

## 3. Note rows and tuning

### The arithmetic problem

One quarter-tone octave is 24 steps. A row is 12 pads. **A row cannot hold a full quarter-tone
octave.** Two ways out:

- **(A, recommended) One pad = one quarter tone, rows offset like a guitar.** A row spans 12
  quarter tones = 6 semitones. Row roots default to a perfect 4th apart (10 quarter steps), giving
  a 1-semitone overlap between adjacent rows — the same relationship that makes a guitar neck
  playable. Three rows span ~16 semitones total.
- **(B) One pad = one semitone**, so a row is a full chromatic octave, and SHIFT offsets the whole
  grid by one quarter tone.

Go with A. It is what you described, it is isomorphic (a shape is the same shape anywhere on the
grid), and B makes quarter tones a mode you toggle rather than notes you play.

Row roots are independently settable, so guitar tuning, all-4ths, or anything else is a config
value, not a code change.

### Mapping to the engine

`Tuning::effective_note(note: u8, offset_semitones: f32) -> Option<f32>` already takes a fractional
semitone offset, and `granny-core` already snaps bends to quarter tones. So a pad press becomes:

```
note_u8 = row_root[row]
offset  = col as f32 * 0.5     // one quarter tone per column, mode A
```

No new tuning maths. This is a mapping layer, not an engine change.

### LED colour scheme

Per pad, hue carries function and brightness carries tone class:

| Tone class | Brightness | Notes |
|---|---|---|
| Row root | full, distinct hue | one per row, the anchor |
| In-scale semitone | full | from the loaded Scala scale, else diatonic |
| Chromatic semitone | ~40% | even column index, out of scale |
| Quarter tone | ~15% | odd column index |
| Sounding now | white flash, decays to base | driven by `Engine::active_voices` |

Two cautions. WS2812s have poor low-end resolution and visibly shift hue below ~10/255, so run a
gamma LUT and hold the dimmest tier at 10–12/255 rather than 3. And the silicone pads diffuse
enough to blur adjacent colours, so keep the three tiers far apart in brightness rather than
relying on subtle hue differences.

## 4. Encoders

Eight endless encoders, two banks via SHIFT:

- **Bank A** — the eight hardware knobs: Rate, Crush, Attack, Release, Grain Size, Shift, Start, End.
- **Bank B** — Voices, Root Adjust, Velocity Depth, Velocity Curve, Pad Velocity, Master Gain,
  Soft Clip, Envelope Reverse. See §6 for the four amplitude controls.

Each [Adafruit 4991 breakout](https://www.adafruit.com/product/4991) carries its own NeoPixel, so
the bank shows on the encoder itself — a colour per bank, no display glance needed.

Buy the **$5.95 version without the encoder soldered** and fit your own detentless PEC11-pinout
encoder. Adafruit's pre-soldered option is 24-detent, and at 24 detents per revolution the Rate
knob's 0–1023 range takes 43 turns. Detentless plus firmware acceleration is the difference between
usable and infuriating. Budget acceleration work either way.

## 5. Electronics

### I²C address map — check this before ordering

The QT encoder breakouts occupy **0x36–0x3D and only 0x36–0x3D**. Eight units is the hard maximum
on one bus, which happens to be exactly eight. NeoTrellis spans 0x2E–0x4D, which **overlaps**. So:

| Device | Bus | Address |
|---|---|---|
| NeoTrellis ×3 | I²C0 | 0x2E, 0x2F, 0x30 |
| QT encoders ×8 | I²C1 | 0x36–0x3D |
| ES8311 codec | onboard | 0x18 |

Split across the P4's two buses anyway — eleven seesaw devices on one bus polled fast enough to
feel responsive is a needless bottleneck. Wire the NeoTrellis INT pins (3 GPIOs) and wire-OR the
eight encoder INTs to a single GPIO, then scan on interrupt rather than polling.

### GPIO budget

The dev kit's 2×20 header brings out 28 free GPIOs:

| Use | Pins |
|---|---|
| I²C0 + I²C1 | 4 |
| NeoTrellis INT ×3 | 3 |
| Encoder INT (wire-OR) | 1 |
| DIN MIDI in/out (UART) | 2 |
| **Total** | **10 of 28** |

Comfortable — a third of the header, with the audio codec and SD on dedicated connectors that cost
nothing from this budget.

### Audio path — mono, by choice

The onboard ES8311 is a **mono** codec, and this build accepts that rather than working around it.
No external DAC, no second I²S bus, four fewer GPIOs, one less board to wire.

Mono simplifies the engine too, not just the BOM. `Voice::render` (`voice.rs:440`) currently writes
a stereo pair with per-voice pan gains. A mono path drops one multiply-add per voice per sample,
halves the output buffer and DMA traffic, and makes `pan_gains` (`voice.rs:57`), `Voice::pan` and
`ModDest::Pan` dead weight on this target.

Pleasant side effect: the dev kit's speaker connector and NS4150B amp become the entire output
stage, so the instrument is self-contained — no external monitoring needed to play it.

Fixed-point conversion of `Voice::pos` / `Voice::step` (currently `f64`, `voice.rs:74,88`) is still
required — the P4 is RV32IMAFC, single-precision only, so `f64` is soft-float here exactly as it is
on the S3.

### MIDI

The USB-A OTG port is host/device switchable **by jumper**, so it is one or the other, not both.
That makes the $10 of DIN hardware worth it: DIN in and out give you an always-available path
independent of whatever the USB port is doing. Default the USB port to device mode so a DAW sees
the instrument.

### No MPE — and quarter tones do not need it

Elastomer pads are plain switches: no velocity, no aftertouch, no pressure. That is fine, because
**MPE was never the mechanism that made quarter tones work.**

The fractional-pitch path is `Expression::note_tuning`, summed with bend in
`Expression::pitch_offset()` (`params.rs:214`) and passed to `Tuning::effective_note` at
`voice.rs:245`. MPE was one way to populate that field; CLAP note expressions were another. A pad
populates it directly:

```
expr.note_tuning = col as f32 * 0.5;   // one quarter tone per column
expr.bend_semitones = 0.0;
```

This is *more* exact than the MPE route, not less — no 14-bit bend quantisation, no per-channel
voice allocation, no bend-range negotiation between member and master channels.

`granny-core` needs no change for this. `mpe.rs` lives entirely in `mater-plugin`, so it is deleted
along with the rest of that crate rather than ported.

**The mod matrix is dropped on this target.** It was designed as an expression router, and without
expression `ModSource` (`params.rs:115`) has nothing left to route: Pressure, Slide and Bend are
permanently zero and Velocity is a constant.

Dropping it costs nothing and requires **no change to `granny-core`**. `resolve()` skips any slot
whose `dest` is `ModDest::None` (`params.rs:254`), so the hardware simply passes an empty
`&[ModSlot]` and the loop never runs. No dead code, no runtime cost, no fork of the engine.

Note this is a hardware-surface decision, not a deletion. `ModSlot`, `ModSource` and `ModDest` stay
in `granny-core` because `mater-plugin` still uses them. Deleting them outright would also change
the plugin's parameter list, its editor and the `.mater` state format — a separate call, and a
breaking one.

Keep `bend_semitones` live even so. An external keyboard's pitch wheel over DIN MIDI is still worth
having, and it costs one field.

## 6. Amplitude — velocity response and master gain

With the matrix gone, these become the amplitude controls. Four new parameters, all on encoder
bank B.

### Velocity to amplitude: it already exists, and it is one line

Velocity never went through the mod matrix. It is `envelope.rs:59`:

```rust
self.vel_atten = 31 - (velocity >> 2) as i16;
```

Velocity picks the **sustain attenuation index** into the 38-entry `DB_TO_MULT` table: 0 is full
volume, 31 is quiet. Because that table is dB-to-multiplier, the stock response is already *linear
in dB*, which is a musical default rather than an accident.

That single line is the whole insertion point. Replace it with a function that still returns 0..31
and every downstream stage — `attenuation()`, `vol_mult_offset()`, `dac_word()` — is untouched and
still covered by the existing tests, including `dac_word_never_exceeds_twelve_bits`.

| Parameter | Range | Default | Effect |
|---|---|---|---|
| **Velocity Depth** | 0–127 | 127 | Scales the attenuation span. At 0 velocity is ignored and every note plays at full. At 127, stock behaviour. |
| **Velocity Curve** | 0–127 | 64 | Gamma on normalised velocity before the map. 64 is linear, below is concave (easier to play loud), above is convex (more range at the top). |
| **Pad Velocity** | 0–127 | 100 | The fixed velocity the pads send, since elastomer switches have none. |

```rust
fn vel_atten(velocity: u8, depth: u16, curve: u16) -> i16 {
    let gamma = f32::exp2((curve as f32 - 64.0) / 32.0);   // 0.25x .. 4x
    let v = (velocity as f32 / 127.0).powf(gamma);
    let span = 31.0 * (depth as f32 / 127.0);
    (span * (1.0 - v)).round() as i16
}
```

**Velocity Depth at 0 is the setting that matters here.** The pads have no velocity, so a fixed
Pad Velocity through an unmodified curve is just a constant attenuation — Depth 0 makes that
explicit and gets the envelope out of the way entirely. Depth exists for the DIN MIDI input.

One caveat: the range is quantised to 32 dB steps no matter what the curve does, so extreme gamma
settings will produce audible flat spots at one end. If that bites, widening the span from 31 to
the table's full 37 buys a little more resolution at the cost of exact hardware fidelity. Leave it
at 31 by default.

### Master gain

The engine sums voices with no limiting (`engine.rs:238`) — sixteen voices at level 1.0 will clip
hard — and there is currently only a *per-voice* level (`Resolved::level`, applied in
`Voice::render`). A global gain after summation is a different control and is the one missing.

Do it in the audio task, not in `granny-core`. `Engine::process` *adds* into its output slices, so
scaling inside it would also scale whatever the caller had already put there. The hardware owns its
buffer, so the correct and trivial version is: zero the buffer, call `process`, then one pass over
the block.

```rust
engine.process_mono(&scene, &mut buf);
for s in buf.iter_mut() {
    *s = soft_clip(*s * master_gain, clip_amount);
}
```

| Parameter | Range | Default | Effect |
|---|---|---|---|
| **Master Gain** | 0–127 | 96 (unity) | −∞ to +24 dB after summation. Below unity matters as much as above — it is the headroom control for high voice counts. |
| **Soft Clip** | 0–127 | 0 (off) | Drive amount into a cubic soft clipper. |

Soft clip is worth the few cycles. Gain above unity on 8-bit granular material wants somewhere to
go, and hard-clipping a summed voice stack sounds like a bug rather than an effect. One multiply
and one polynomial per output sample, once per block rather than per voice — the cost does not
scale with polyphony.

## 7. Power

Estimated draw at 5 V:

| Load | Typical | Peak |
|---|---|---|
| ESP32-P4 @ 400 MHz + PSRAM, radio off | 350 mA | 450 mA |
| 48 NeoPixels, brightness-capped | 450 mA | 2.9 A (all white, full) |
| 8 encoder NeoPixels | 60 mA | 480 mA |
| 4.3" DSI panel + backlight | 250 mA | 350 mA |
| Codec + amp | 50 mA | 250 mA |
| **Total** | **~1.15 A / 5.8 W** | **4+ A** |

Cap global LED brightness in firmware. Unclamped, the grid alone can pull 2.9 A.

### On the 9 V idea

Don't. A 9 V alkaline is roughly 500 mAh at 9 V — about 4.5 Wh, so under 45 minutes here even
before buck losses, and 9 V cells sag badly above a few hundred mA. A 9 V NiMH is worse, around
200 mAh. The form factor is appealing and the energy density is not there.

**Single-cell Li-ion with a 5 V boost** is the right answer. 2× 18650 in parallel is ~25 Wh, so
4–6 hours, and the cells are cheap and replaceable.

- **Phase 1:** a 10000 mAh USB-C power bank. Zero engineering, and it lets you measure real draw
  before committing to a design. Buy a USB inline power meter alongside it — this is the single
  most useful $12 in the build.
- **Phase 2:** 2× 18650 + holder + an IP5306-based charge/boost module. Pick IP5306 (2.1–2.4 A,
  pass-through charging) over the Adafruit PowerBoost 1000C — the PowerBoost's 1 A ceiling is below
  our typical draw and would brown out on LED transients.

## 8. Bill of materials

### Control surface

| Item | Qty | Unit | Total |
|---|---|---|---|
| Adafruit NeoTrellis 4×4 RGB driver (3954) | 3 | $12.50 | $37.50 |
| Silicone elastomer 4×4 keypad (1611) | 3 | $4.95 | $14.85 |
| Adafruit I²C QT rotary encoder breakout, no encoder (4991) | 8 | $5.95 | $47.60 |
| Detentless PEC11-pinout rotary encoder | 8 | $3.00 | $24.00 |
| Encoder knobs | 8 | $2.00 | $16.00 |
| STEMMA QT / Qwiic cables | 12 | $0.95 | $11.40 |
| | | | **$151.35** |

### Compute and audio

| Item | Qty | Unit | Total |
|---|---|---|---|
| ESP32-P4-WIFI6-DEV-KIT (Amazon, est.) | 1 | $40.00 | $40.00 |
| microSD card, 32 GB | 1 | $8.00 | $8.00 |
| | | | **$48.00** |

Audio output is the onboard ES8311 codec, amp, speaker connector and 3.5 mm jack. No external DAC.

### Display

| Item | Qty | Unit | Total |
|---|---|---|---|
| Waveshare 4.3" DSI LCD, 800×480 IPS | 1 | $50.00 | $50.00 |

### MIDI

| Item | Qty | Unit | Total |
|---|---|---|---|
| H11L1 optocoupler, 2× DIN-5 jacks, passives | 1 | $10.00 | $10.00 |

USB MIDI over the onboard OTG port costs nothing.

### Power

| Item | Qty | Unit | Total |
|---|---|---|---|
| 10000 mAh USB-C power bank (phase 1) | 1 | $25.00 | $25.00 |
| USB inline power meter | 1 | $12.00 | $12.00 |
| 18650 cells + holder + IP5306 module (phase 2) | 1 | $38.00 | $38.00 |

### Assembly

| Item | Total |
|---|---|
| Protoboard, wire, headers | $15.00 |
| Standoffs, M2.5/M3 hardware | $10.00 |
| Laser-cut or 3D-printed panel and enclosure (phase 2) | $50.00 |

### Totals

| | |
|---|---|
| **Phase 1** — bench build, power bank, no enclosure | **~$311** |
| Shipping across Adafruit + Amazon + Waveshare | ~$30 |
| **Phase 1 delivered** | **~$341** |
| **Phase 2** — add enclosure and integrated battery, drop the power bank | **~$426** |

### Where the money is, if you need it cheaper

- SPI ST7789 2.8" instead of the DSI panel: **−$38**. Slower waveform redraws, smaller HUD strip.
- 2× MCP23017 + bare encoders instead of 8 QT breakouts: **−$50**. You lose the per-encoder
  NeoPixel (so bank state moves to the display) and you write quadrature decoding over I²C
  interrupts yourself.

### One thing to verify before ordering

Waveshare also sells an **ESP32-P4-WIFI6-Touch-LCD-4.3** at $37–42 that integrates the P4 with a
4.3" display — cheaper than this dev kit plus a separate panel ($90). It is a *different board*.
Confirm it still exposes a 2×20 GPIO header and a host-capable USB-A OTG port before substituting;
if it does, that is $50 saved for free.

## 9. Firmware work against the existing crates

`granny-core` ports essentially as-is. The work is all in what replaces `mater-plugin`:

- **Fixed-point position.** `Voice::pos` / `Voice::step` from `f64` to Q32.32. Do this in the
  desktop plugin first and verify against the existing tests — debugging precision loss over JTAG
  is not how you want to spend a weekend.
- **Pad → note mapping.** New layer: `(row, col)` plus three row roots to a note and an
  `Expression::note_tuning` of `col * 0.5`. Feeds the existing `Tuning::effective_note`
  unmodified — no engine change, and no MIDI bend anywhere in the path.
- **Mono render path.** A `render_mono` alongside `Voice::render`, and a matching `Engine::process`
  variant taking one buffer. The plugin keeps the stereo path; only the hardware target uses mono.
- **Velocity curve.** Replace the fixed `31 - (velocity >> 2)` at `envelope.rs:59` with a
  depth-and-gamma function returning the same 0..31 index. Needs `Envelope::start` to take the two
  new parameters. The existing envelope and DAC tests should pass unchanged at the defaults —
  that is the check that the port is faithful.
- **Master gain and soft clip.** Post-summation, in the audio task's block loop. No `granny-core`
  change.
- **Mod matrix.** Nothing to write — pass an empty `&[ModSlot]` to `resolve()`.
- **Encoder banks.** `HwParams` has eight knobs; SHIFT gives sixteen assignable slots. Needs a bank
  concept that does not exist yet. Bank B now holds real parameters (§6) rather than matrix slots.
- **LED state derivation.** Tone class per pad from the active `ScalaTuning`, plus a per-pad
  activity decay driven by `Engine::voices()`.
- **Voice cap.** 48 pads means 48 possible simultaneous presses. Cap `set_voice_capacity` at 8–16
  and let the existing stealing logic handle it. This is what makes master gain a headroom control
  rather than a luxury.
- **Drop.** nih-plug, the egui editor, symphonia, `rfd`, `parking_lot`, and `mpe.rs` entire. Sample
  loading becomes raw bytes off the SD card, which is what the original hardware did; convert on
  the host.
