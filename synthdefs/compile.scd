// Compile all Performative SynthDefs to binary .scsyndef files.
// Run: sclang compile.scd
// Output: .scsyndef files in this directory.

(
var outputDir = PathName(thisProcess.nowExecutingPath).pathOnly;

// ── DeckPlayer ───────────────────────────────────────────────────────────────
// Plays a stereo buffer at variable rate with gain control.
// Outputs to a configurable private bus pair.
//
// Controls:
//   buf      - buffer number
//   rate     - playback rate multiplier (1.0 = original speed)
//   rate_lag - lag time in seconds for rate changes (0.0 = instant)
//   gain     - linear gain (1.0 = 0dB, 0.0 = silence)
//   out_bus  - output bus (private audio bus)
//   loop_    - 1 = loop, 0 = play once then free node
//   pos      - start position in frames (used for seek)

SynthDef(\deck_player, { |buf=0, rate=1.0, rate_lag=0.0, gain=1.0, gain_lag=0.0, out_bus=10, loop_=0, pos=0, gate=1|
    var sig, env, smoothRate;
    smoothRate = VarLag.kr(rate, rate_lag);
    sig = PlayBuf.ar(2, buf,
        rate: BufRateScale.kr(buf) * smoothRate,
        trigger: 1,
        startPos: pos,
        loop: loop_,
        doneAction: Done.none
    );
    env = EnvGen.kr(Env.asr(0, 1, 0.01), gate, doneAction: Done.freeSelf);
    sig = sig * VarLag.kr(gain, gain_lag) * env;
    Out.ar(out_bus, sig);
}).writeDefFile(outputDir);

// ── DeckEQ ───────────────────────────────────────────────────────────────────
// 3-band parametric EQ on a private bus. Reads from in_bus, writes to out_bus.
//
// Controls:
//   in_bus   - input private audio bus
//   out_bus  - output bus (usually same as in_bus for in-place EQ)
//   hi_gain  - high shelf gain (linear, 1.0 = unity)
//   mid_gain - mid peak gain (linear, 1.0 = unity)
//   lo_gain  - low shelf gain (linear, 1.0 = unity)

SynthDef(\deck_eq, { |in_bus=10, out_bus=10, lo_gain=1.0, mid_gain=1.0, hi_gain=1.0,
                        lo_lag=0.0, mid_lag=0.0, hi_lag=0.0|
    var sig, lo, mid, hi, lo_g, mid_g, hi_g;
    sig = In.ar(in_bus, 2);
    // Band-split: LPF for lows, HPF for highs, remainder for mids.
    // Gains are pure linear multipliers — no ampdb, so kill (0.0) is safe.
    lo  = LPF.ar(sig, 300);
    hi  = HPF.ar(sig, 3000);
    mid = sig - lo - hi;
    lo_g  = VarLag.kr(lo_gain,  lo_lag);
    mid_g = VarLag.kr(mid_gain, mid_lag);
    hi_g  = VarLag.kr(hi_gain,  hi_lag);
    sig = (lo * lo_g) + (mid * mid_g) + (hi * hi_g);
    ReplaceOut.ar(out_bus, sig);
}).writeDefFile(outputDir);

// ── MasterMix ────────────────────────────────────────────────────────────────
// Reads from up to 2 deck buses, sums to hardware output 0-1.
//
// Controls:
//   in_bus1  - deck 1 bus
//   in_bus2  - deck 2 bus

SynthDef(\master_mix, { |in_bus1=10, in_bus2=12|
    var deck1, deck2, mix;
    deck1 = In.ar(in_bus1, 2);
    deck2 = In.ar(in_bus2, 2);
    mix = (deck1 + deck2) * 0.5;
    Out.ar(0, mix);
}).writeDefFile(outputDir);

// ── CueMix ───────────────────────────────────────────────────────────────────
// Routes a selected deck bus to hardware output 2-3 (cue/headphones).
// blend: 0.0 = pure cue deck, 1.0 = 50/50 cue+master
//
// Controls:
//   cue_bus  - the deck to preview
//   blend    - 0.0–1.0 cue-to-master blend

SynthDef(\cue_mix, { |cue_bus=10, blend=0.0|
    var cue_sig, master_sig, mix;
    cue_sig    = In.ar(cue_bus, 2);
    master_sig = In.ar(0, 2);  // reads hardware output bus (tap)
    mix = (cue_sig * (1.0 - blend)) + (master_sig * blend);
    Out.ar(2, mix);
}).writeDefFile(outputDir);

"SynthDef compilation complete.".postln;
0.exit;
)
