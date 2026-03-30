use std::fmt;

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Load  { deck: usize, path: String },
    Play  { deck: usize },
    Pause { deck: usize },
    Gain  { deck: usize, target: f32, ramp: Option<RampDuration> },
    Eq    { deck: usize, band: EqBand, target: f32, ramp: Option<RampDuration> },
    /// Set jog-wheel focus to the given deck (0-indexed). Subsequent scroll
    /// events will drive that deck's playback rate.
    Jog   { deck: usize },
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EqBand { Lo, Mid, Hi }

/// Unresolved ramp duration — engine converts to seconds using current BPM.
#[derive(Debug, Clone, PartialEq)]
pub enum RampDuration {
    Bars(f32),
    Beats(f32),
    Seconds(f32),
}

impl RampDuration {
    pub fn to_secs(&self, bpm: f32) -> f32 {
        match self {
            RampDuration::Bars(b)    => b * 4.0 * 60.0 / bpm,
            RampDuration::Beats(b)   => b * 60.0 / bpm,
            RampDuration::Seconds(s) => *s,
        }
    }
}

// ── Composition AST types ────────────────────────────────────────────────────

/// A full script: one or more statements separated by `;`.
/// Each statement executes independently (fire-and-forget per statement).
#[derive(Debug, Clone, PartialEq)]
pub struct Script {
    pub statements: Vec<Statement>,
}

/// A statement: one or more sequential steps separated by `>`.
/// Each step waits for the previous step's ramps to finish before starting.
#[derive(Debug, Clone, PartialEq)]
pub struct Statement {
    pub steps: Vec<ParallelStep>,
}

/// A parallel step: one or more commands separated by `&` that all fire simultaneously.
#[derive(Debug, Clone, PartialEq)]
pub struct ParallelStep {
    pub commands: Vec<Command>,
}

// ── ParseError ────────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
pub enum ParseError {
    Empty,
    UnknownCommand(String),
    InvalidDeck(String),
    MissingArg { cmd: &'static str, arg: &'static str },
    InvalidValue(String),
    InvalidDuration(String),
    /// A pipe-context deck number that is out of range (must be 1 or 2).
    PipeContext(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Empty => write!(f, ""),
            ParseError::UnknownCommand(s) =>
                write!(f, "unknown command '{s}' — try load, play, pause, gain, eq, jog, fadein, fadeout, kill, quit (fade/cut not yet supported)"),
            ParseError::InvalidDeck(s) =>
                write!(f, "invalid deck '{s}' — use 1 or 2"),
            ParseError::MissingArg { cmd, arg } =>
                write!(f, "usage: {cmd} <deck> {arg}"),
            ParseError::InvalidValue(s) =>
                write!(f, "invalid value '{s}' — use dB (e.g. -6db, +6db max), 0.0-2.0, kill, or reset"),
            ParseError::InvalidDuration(s) =>
                write!(f, "invalid duration '{s}' — use e.g. 4bars, 8beats, 30s"),
            ParseError::PipeContext(s) =>
                write!(f, "invalid pipe context '{s}' — use 1 or 2 (e.g. '2 | fadein 16bars')"),
        }
    }
}

// ── Entry point: single-command parser ───────────────────────────────────────

/// Parse a single command string into a `Command`.
pub fn parse(input: &str) -> Result<Command, ParseError> {
    let tokens: Vec<&str> = input.split_whitespace().collect();
    if tokens.is_empty() {
        return Err(ParseError::Empty);
    }
    match tokens[0].to_lowercase().as_str() {
        "load"  => parse_load(&tokens),
        "play"  => Ok(Command::Play  { deck: parse_deck(tokens.get(1), "play")? }),
        "pause" => Ok(Command::Pause { deck: parse_deck(tokens.get(1), "pause")? }),
        "gain"  => parse_gain(&tokens),
        "eq"    => parse_eq(&tokens),
        "jog"   => Ok(Command::Jog   { deck: parse_deck(tokens.get(1), "jog")? }),

        // Sugar — expand before returning
        "kill"    => parse_kill(&tokens),
        "fadein"  => parse_fadein(&tokens),
        "fadeout" => parse_fadeout(&tokens),

        "quit" | "q" | "exit" => Ok(Command::Quit),
        other => Err(ParseError::UnknownCommand(other.to_string())),
    }
}

// ── Composition parser ───────────────────────────────────────────────────────

/// Parse a full composition expression into a `Script`.
///
/// Grammar (informal):
///   script    = statement (';' statement)*
///   statement = step ('>' step)*
///   step      = [N '|'] fragment ('&' fragment)*
///   fragment  = single-command
///
/// Operators:
///   `;`  — run statements independently in parallel background tasks
///   `>`  — run steps sequentially, waiting for the previous step's ramps
///   `&`  — run commands within a step simultaneously
///   `N |`— pipe context: inject deck N into commands that take a single deck
pub fn parse_script(input: &str) -> Result<Script, ParseError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(ParseError::Empty);
    }

    let statements = trimmed
        .split(';')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(parse_statement)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(Script { statements })
}

/// Parse one `;`-delimited statement: a sequence of `>`-separated steps.
///
/// Pipe context propagates: if a step has no `N |` prefix, it inherits the context from
/// the previous step. An explicit `N |` overrides it for that step and all following steps.
fn parse_statement(s: &str) -> Result<Statement, ParseError> {
    let mut steps = Vec::new();
    let mut ctx: Option<usize> = None;
    for part in s.split('>').map(str::trim).filter(|s| !s.is_empty()) {
        let (step, new_ctx) = parse_step(part, ctx)?;
        ctx = new_ctx;
        steps.push(step);
    }
    Ok(Statement { steps })
}

/// Parse one `>`-delimited step: an optional pipe context followed by `&`-separated fragments.
///
/// `default_ctx` is the inherited deck context from the previous step. An explicit `N |` prefix
/// overrides it; no prefix means `default_ctx` is used (propagation). Returns the resolved
/// context so `parse_statement` can thread it forward.
fn parse_step(s: &str, default_ctx: Option<usize>) -> Result<(ParallelStep, Option<usize>), ParseError> {
    let tokens: Vec<&str> = s.split_whitespace().collect();
    if tokens.is_empty() {
        return Err(ParseError::Empty);
    }

    // Detect explicit pipe context: first token is a digit, second token is "|".
    let (deck_ctx, remainder): (Option<usize>, &str) = if tokens.len() >= 2 && tokens[1] == "|" {
        let first = tokens[0];
        match first.parse::<usize>() {
            Ok(n @ 1..=2) => {
                let after_pipe = s
                    .find('|')
                    .map(|i| s[i + 1..].trim_start())
                    .unwrap_or("");
                (Some(n - 1), after_pipe)
            }
            Ok(_) | Err(_) => return Err(ParseError::PipeContext(first.to_string())),
        }
    } else {
        // No explicit prefix — inherit from previous step.
        (default_ctx, s)
    };

    let commands = remainder
        .split('&')
        .map(|frag| frag.trim())
        .filter(|frag| !frag.is_empty())
        .map(|frag| {
            let injected = match deck_ctx {
                Some(deck) => inject_deck(frag, deck),
                None       => frag.to_string(),
            };
            parse(&injected)
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok((ParallelStep { commands }, deck_ctx))
}

/// Single-deck command verbs that accept injection when no explicit deck is present.
const SINGLE_DECK_VERBS: &[&str] = &[
    "load", "play", "pause", "gain", "eq", "jog", "kill", "fadein", "fadeout",
];

/// Inject the pipe-context deck number into `fragment` if the command is a single-deck
/// command and the token immediately following the verb is not already "1" or "2".
///
/// The deck number is inserted between the verb and its remaining arguments so that
/// `parse()` receives a fully-formed command string (e.g. "fadein 16bars" becomes
/// "fadein 2 16bars" when the pipe context is deck 2).
fn inject_deck(fragment: &str, deck: usize) -> String {
    let tokens: Vec<&str> = fragment.split_whitespace().collect();
    let verb = match tokens.first() {
        Some(v) => v.to_lowercase(),
        None    => return fragment.to_string(),
    };
    let next_is_deck = tokens.get(1).map_or(false, |t| *t == "1" || *t == "2");
    if !SINGLE_DECK_VERBS.contains(&verb.as_str()) || next_is_deck {
        return fragment.to_string();
    }
    // Insert deck number after the verb: "fadein 16bars" -> "fadein 2 16bars"
    let rest = tokens[1..].join(" ");
    if rest.is_empty() {
        format!("{} {}", tokens[0], deck + 1)
    } else {
        format!("{} {} {}", tokens[0], deck + 1, rest)
    }
}

// ── step_max_secs helper ──────────────────────────────────────────────────────

/// Return the longest ramp duration (in seconds) among all commands in a step.
/// Commands with no ramp contribute 0.0.
pub fn step_max_secs(step: &ParallelStep, bpm: f32) -> f32 {
    step.commands
        .iter()
        .map(|c| match c {
            Command::Gain { ramp: Some(r), .. } => r.to_secs(bpm),
            Command::Eq   { ramp: Some(r), .. } => r.to_secs(bpm),
            _ => 0.0,
        })
        .fold(0.0f32, f32::max)
}

// ── Command parsers ───────────────────────────────────────────────────────────

fn parse_load(t: &[&str]) -> Result<Command, ParseError> {
    let deck = parse_deck(t.get(1), "load")?;
    let path = t.get(2)
        .ok_or(ParseError::MissingArg { cmd: "load", arg: "<path>" })?
        .to_string();
    Ok(Command::Load { deck, path })
}

fn parse_gain(t: &[&str]) -> Result<Command, ParseError> {
    let deck   = parse_deck(t.get(1), "gain")?;
    let target = parse_gain_value(t.get(2).ok_or(
        ParseError::MissingArg { cmd: "gain", arg: "<value>" }
    )?)?;
    let ramp = parse_over(t, 3)?;
    Ok(Command::Gain { deck, target, ramp })
}

fn parse_eq(t: &[&str]) -> Result<Command, ParseError> {
    let deck = parse_deck(t.get(1), "eq")?;
    let band = parse_band(t.get(2).ok_or(
        ParseError::MissingArg { cmd: "eq", arg: "<lo|mid|hi> <value>" }
    )?)?;
    let target = parse_gain_value(t.get(3).ok_or(
        ParseError::MissingArg { cmd: "eq", arg: "<value>" }
    )?)?;
    let ramp = parse_over(t, 4)?;
    Ok(Command::Eq { deck, band, target, ramp })
}

/// kill <deck> <band>  ->  eq <deck> <band> 0.0
fn parse_kill(t: &[&str]) -> Result<Command, ParseError> {
    let deck = parse_deck(t.get(1), "kill")?;
    let band = parse_band(t.get(2).ok_or(
        ParseError::MissingArg { cmd: "kill", arg: "<lo|mid|hi>" }
    )?)?;
    Ok(Command::Eq { deck, band, target: 0.0, ramp: None })
}

/// fadein <deck> <dur>  ->  gain <deck> 1.0 over <dur>
fn parse_fadein(t: &[&str]) -> Result<Command, ParseError> {
    let deck = parse_deck(t.get(1), "fadein")?;
    let dur  = parse_ramp_duration(t.get(2).ok_or(
        ParseError::MissingArg { cmd: "fadein", arg: "<duration>" }
    )?)?;
    Ok(Command::Gain { deck, target: 1.0, ramp: Some(dur) })
}

/// fadeout <deck> <dur>  ->  gain <deck> 0.0 over <dur>
fn parse_fadeout(t: &[&str]) -> Result<Command, ParseError> {
    let deck = parse_deck(t.get(1), "fadeout")?;
    let dur  = parse_ramp_duration(t.get(2).ok_or(
        ParseError::MissingArg { cmd: "fadeout", arg: "<duration>" }
    )?)?;
    Ok(Command::Gain { deck, target: 0.0, ramp: Some(dur) })
}

// ── Value / duration helpers ──────────────────────────────────────────────────

/// Parse an optional `over <duration>` tail starting at token index `start`.
fn parse_over(t: &[&str], start: usize) -> Result<Option<RampDuration>, ParseError> {
    match t.get(start) {
        Some(&"over") => {
            let dur_str = t.get(start + 1).ok_or(
                ParseError::InvalidDuration("(missing duration after 'over')".into())
            )?;
            Ok(Some(parse_ramp_duration(dur_str)?))
        }
        Some(other) => Err(ParseError::UnknownCommand(other.to_string())),
        None => Ok(None),
    }
}

/// Parse a gain/EQ value.
///
/// Accepted formats:
///   "-6db" / "-6dB"  -> 10^(-6/20) ~= 0.501
///   "0db"            -> 1.0
///   "+6db"           -> 2.0  (max; ~+6 dB)
///   "kill" / "-inf"  -> 0.0  (min; -96 dB in display)
///   "reset"          -> 1.0
///   "0.5"            -> 0.5  (bare float, clamped 0..=2)
pub fn parse_gain_value(s: &str) -> Result<f32, ParseError> {
    let lower = s.to_lowercase();
    if lower == "kill" || lower == "-inf" {
        return Ok(0.0);
    }
    if lower == "reset" {
        return Ok(1.0);
    }
    let stripped = lower.trim_end_matches("db");
    if stripped.len() < lower.len() {
        // Had a "db" suffix
        let db: f32 = stripped
            .parse()
            .map_err(|_| ParseError::InvalidValue(s.to_string()))?;
        return Ok(db_to_linear(db).clamp(0.0, 2.0));
    }
    // Bare float
    let v: f32 = s.parse().map_err(|_| ParseError::InvalidValue(s.to_string()))?;
    Ok(v.clamp(0.0, 2.0))
}

/// Parse a ramp duration: "4bars", "8beats", "30s", "2.5s".
pub fn parse_ramp_duration(s: &str) -> Result<RampDuration, ParseError> {
    let lower = s.to_lowercase();
    if let Some(n) = lower.strip_suffix("bars") {
        let v: f32 = n.parse().map_err(|_| ParseError::InvalidDuration(s.to_string()))?;
        return Ok(RampDuration::Bars(v));
    }
    if let Some(n) = lower.strip_suffix("beats") {
        let v: f32 = n.parse().map_err(|_| ParseError::InvalidDuration(s.to_string()))?;
        return Ok(RampDuration::Beats(v));
    }
    if let Some(n) = lower.strip_suffix('s') {
        let v: f32 = n.parse().map_err(|_| ParseError::InvalidDuration(s.to_string()))?;
        return Ok(RampDuration::Seconds(v));
    }
    Err(ParseError::InvalidDuration(s.to_string()))
}

fn parse_deck(token: Option<&&str>, cmd: &'static str) -> Result<usize, ParseError> {
    let s = token.ok_or(ParseError::MissingArg { cmd, arg: "" })?;
    match s.parse::<usize>() {
        Ok(n @ 1..=2) => Ok(n - 1),
        _ => Err(ParseError::InvalidDeck(s.to_string())),
    }
}

fn parse_band(s: &str) -> Result<EqBand, ParseError> {
    match s.to_lowercase().as_str() {
        "lo" | "low" | "bass"    => Ok(EqBand::Lo),
        "mid" | "mids"           => Ok(EqBand::Mid),
        "hi" | "high" | "treble" => Ok(EqBand::Hi),
        _ => Err(ParseError::InvalidValue(format!("unknown band '{s}' — use lo, mid, hi"))),
    }
}

fn db_to_linear(db: f32) -> f32 {
    if db <= -96.0 { 0.0 } else { 10f32.powf(db / 20.0) }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Existing commands ─────────────────────────────────────────────────────

    #[test]
    fn parse_load() {
        assert_eq!(
            parse("load 1 /tmp/song.wav"),
            Ok(Command::Load { deck: 0, path: "/tmp/song.wav".into() })
        );
    }

    #[test]
    fn parse_play() { assert_eq!(parse("play 2"), Ok(Command::Play { deck: 1 })); }

    #[test]
    fn parse_pause() { assert_eq!(parse("pause 1"), Ok(Command::Pause { deck: 0 })); }

    #[test]
    fn parse_quit() {
        assert_eq!(parse("quit"), Ok(Command::Quit));
        assert_eq!(parse("q"), Ok(Command::Quit));
    }

    #[test]
    fn parse_empty() {
        assert_eq!(parse(""), Err(ParseError::Empty));
        assert_eq!(parse("   "), Err(ParseError::Empty));
    }

    #[test]
    fn parse_missing_path() {
        assert!(matches!(parse("load 1"), Err(ParseError::MissingArg { .. })));
    }

    #[test]
    fn parse_bad_deck() {
        assert_eq!(parse("play 3"), Err(ParseError::InvalidDeck("3".into())));
        assert_eq!(parse("play 0"), Err(ParseError::InvalidDeck("0".into())));
    }

    #[test]
    fn parse_unknown() {
        assert!(matches!(parse("sync 1"), Err(ParseError::UnknownCommand(_))));
    }

    // ── Jog ──────────────────────────────────────────────────────────────────

    #[test]
    fn parse_jog_deck1() {
        assert_eq!(parse("jog 1"), Ok(Command::Jog { deck: 0 }));
    }

    #[test]
    fn parse_jog_deck2() {
        assert_eq!(parse("jog 2"), Ok(Command::Jog { deck: 1 }));
    }

    #[test]
    fn parse_jog_missing_deck() {
        assert!(matches!(parse("jog"), Err(ParseError::MissingArg { cmd: "jog", .. })));
    }

    #[test]
    fn parse_jog_bad_deck() {
        assert_eq!(parse("jog 3"), Err(ParseError::InvalidDeck("3".into())));
    }

    #[test]
    fn script_jog_single_command() {
        let s = parse_script("jog 1").unwrap();
        assert_eq!(s.statements[0].steps[0].commands, vec![Command::Jog { deck: 0 }]);
    }

    #[test]
    fn script_jog_pipe_context_injects_deck() {
        let s = parse_script("2 | jog").unwrap();
        let cmd = &s.statements[0].steps[0].commands[0];
        assert_eq!(*cmd, Command::Jog { deck: 1 });
    }

    // ── Gain ─────────────────────────────────────────────────────────────────

    #[test]
    fn parse_gain_instant_db() {
        let cmd = parse("gain 1 -6db").unwrap();
        if let Command::Gain { deck: 0, target, ramp: None } = cmd {
            assert!((target - 0.5012).abs() < 0.001);
        } else { panic!("{cmd:?}"); }
    }

    #[test]
    fn parse_gain_zero_db() {
        let cmd = parse("gain 1 0db").unwrap();
        if let Command::Gain { target, .. } = cmd {
            assert!((target - 1.0).abs() < 0.001);
        } else { panic!(); }
    }

    #[test]
    fn parse_gain_linear() {
        assert_eq!(
            parse("gain 2 0.5"),
            Ok(Command::Gain { deck: 1, target: 0.5, ramp: None })
        );
    }

    #[test]
    fn parse_gain_with_ramp() {
        let cmd = parse("gain 1 0db over 4bars").unwrap();
        assert_eq!(cmd, Command::Gain {
            deck: 0, target: 1.0,
            ramp: Some(RampDuration::Bars(4.0))
        });
    }

    #[test]
    fn parse_gain_reset() {
        assert_eq!(
            parse("gain 1 reset"),
            Ok(Command::Gain { deck: 0, target: 1.0, ramp: None })
        );
    }

    // ── EQ ───────────────────────────────────────────────────────────────────

    #[test]
    fn parse_eq_instant() {
        assert_eq!(
            parse("eq 1 lo 0.5"),
            Ok(Command::Eq { deck: 0, band: EqBand::Lo, target: 0.5, ramp: None })
        );
    }

    #[test]
    fn parse_eq_kill_value() {
        let cmd = parse("eq 1 lo kill").unwrap();
        assert_eq!(cmd, Command::Eq { deck: 0, band: EqBand::Lo, target: 0.0, ramp: None });
    }

    #[test]
    fn parse_eq_with_ramp() {
        let cmd = parse("eq 2 hi 0.5 over 8beats").unwrap();
        assert_eq!(cmd, Command::Eq {
            deck: 1, band: EqBand::Hi, target: 0.5,
            ramp: Some(RampDuration::Beats(8.0))
        });
    }

    #[test]
    fn parse_eq_bands() {
        assert!(matches!(parse("eq 1 lo 1.0"),  Ok(Command::Eq { band: EqBand::Lo, .. })));
        assert!(matches!(parse("eq 1 mid 1.0"), Ok(Command::Eq { band: EqBand::Mid, .. })));
        assert!(matches!(parse("eq 1 hi 1.0"),  Ok(Command::Eq { band: EqBand::Hi, .. })));
        // aliases
        assert!(matches!(parse("eq 1 low 1.0"),  Ok(Command::Eq { band: EqBand::Lo, .. })));
        assert!(matches!(parse("eq 1 high 1.0"), Ok(Command::Eq { band: EqBand::Hi, .. })));
    }

    // ── Sugar ─────────────────────────────────────────────────────────────────

    #[test]
    fn parse_kill_sugar() {
        assert_eq!(
            parse("kill 1 lo"),
            Ok(Command::Eq { deck: 0, band: EqBand::Lo, target: 0.0, ramp: None })
        );
    }

    #[test]
    fn parse_fadein_sugar() {
        assert_eq!(
            parse("fadein 2 16bars"),
            Ok(Command::Gain { deck: 1, target: 1.0, ramp: Some(RampDuration::Bars(16.0)) })
        );
    }

    #[test]
    fn parse_fadeout_sugar() {
        assert_eq!(
            parse("fadeout 1 4bars"),
            Ok(Command::Gain { deck: 0, target: 0.0, ramp: Some(RampDuration::Bars(4.0)) })
        );
    }

    // ── Duration ─────────────────────────────────────────────────────────────

    #[test]
    fn duration_to_secs() {
        let bpm = 120.0;
        assert!((RampDuration::Bars(4.0).to_secs(bpm) - 8.0).abs() < 0.001);
        assert!((RampDuration::Beats(8.0).to_secs(bpm) - 4.0).abs() < 0.001);
        assert!((RampDuration::Seconds(30.0).to_secs(bpm) - 30.0).abs() < 0.001);
    }

    #[test]
    fn parse_duration_formats() {
        assert_eq!(parse_ramp_duration("4bars"),  Ok(RampDuration::Bars(4.0)));
        assert_eq!(parse_ramp_duration("8beats"), Ok(RampDuration::Beats(8.0)));
        assert_eq!(parse_ramp_duration("30s"),    Ok(RampDuration::Seconds(30.0)));
        assert_eq!(parse_ramp_duration("2.5s"),   Ok(RampDuration::Seconds(2.5)));
    }

    // ── Script / composition parser ───────────────────────────────────────────

    #[test]
    fn script_single_command() {
        let s = parse_script("play 1").unwrap();
        assert_eq!(s.statements.len(), 1);
        assert_eq!(s.statements[0].steps.len(), 1);
        assert_eq!(s.statements[0].steps[0].commands, vec![Command::Play { deck: 0 }]);
    }

    #[test]
    fn script_semicolon_two_statements() {
        let s = parse_script("play 1; pause 2").unwrap();
        assert_eq!(s.statements.len(), 2);
        assert_eq!(s.statements[0].steps[0].commands, vec![Command::Play { deck: 0 }]);
        assert_eq!(s.statements[1].steps[0].commands, vec![Command::Pause { deck: 1 }]);
    }

    #[test]
    fn script_parallel_ampersand() {
        let s = parse_script("play 1 & play 2").unwrap();
        assert_eq!(s.statements.len(), 1);
        assert_eq!(s.statements[0].steps.len(), 1);
        assert_eq!(s.statements[0].steps[0].commands.len(), 2);
    }

    #[test]
    fn script_sequential_arrow() {
        let s = parse_script("fadein 1 4bars > fadeout 2 4bars").unwrap();
        assert_eq!(s.statements[0].steps.len(), 2);
        assert_eq!(s.statements[0].steps[0].commands.len(), 1);
        assert_eq!(s.statements[0].steps[1].commands.len(), 1);
    }

    #[test]
    fn script_pipe_context_injects_deck() {
        let s = parse_script("2 | eq hi 0.25 & fadein 16bars").unwrap();
        let cmds = &s.statements[0].steps[0].commands;
        assert_eq!(cmds.len(), 2);
        assert!(matches!(&cmds[0], Command::Eq { deck: 1, band: EqBand::Hi, .. }));
        assert!(matches!(&cmds[1], Command::Gain { deck: 1, target, ramp: Some(_), .. } if (*target - 1.0).abs() < 0.01));
    }

    #[test]
    fn script_pipe_context_propagates_across_arrow() {
        // No explicit N | on step 2 — inherits deck 2 from step 1.
        let s = parse_script("2 | fadein 4bars > fadeout 4bars").unwrap();
        assert!(matches!(&s.statements[0].steps[0].commands[0], Command::Gain { deck: 1, .. }));
        assert!(matches!(&s.statements[0].steps[1].commands[0], Command::Gain { deck: 1, .. }));
    }

    #[test]
    fn script_pipe_context_explicit_override() {
        // Explicit 1 | on step 2 overrides the inherited deck 2 context.
        let s = parse_script("2 | fadein 16bars > 1 | eq lo kill").unwrap();
        let step0 = &s.statements[0].steps[0].commands[0];
        let step1 = &s.statements[0].steps[1].commands[0];
        assert!(matches!(step0, Command::Gain { deck: 1, .. })); // deck 2 = index 1
        assert!(matches!(step1, Command::Eq { deck: 0, .. }));   // deck 1 = index 0, overridden
    }

    #[test]
    fn script_explicit_deck_overrides_pipe() {
        let s = parse_script("2 | gain 1 -6db").unwrap();
        let cmd = &s.statements[0].steps[0].commands[0];
        assert!(matches!(cmd, Command::Gain { deck: 0, .. })); // explicit deck 1 = index 0
    }

    #[test]
    fn script_all_or_nothing_bad_command() {
        assert!(matches!(
            parse_script("play 1 & bad_cmd"),
            Err(ParseError::UnknownCommand(_))
        ));
    }

    #[test]
    fn script_bad_pipe_deck() {
        assert!(matches!(
            parse_script("5 | play"),
            Err(ParseError::PipeContext(_))
        ));
    }

    #[test]
    fn script_nested_semicolon_and_arrow() {
        let s = parse_script("fadein 1 8bars > eq 1 lo kill; play 2").unwrap();
        assert_eq!(s.statements.len(), 2);
        assert_eq!(s.statements[0].steps.len(), 2);
        assert_eq!(s.statements[1].steps.len(), 1);
    }

    #[test]
    fn step_max_secs_returns_longest_ramp() {
        let step = ParallelStep {
            commands: vec![
                Command::Gain { deck: 0, target: 0.0, ramp: Some(RampDuration::Bars(4.0)) },
                Command::Eq   { deck: 0, band: EqBand::Lo, target: 0.0, ramp: None },
            ],
        };
        assert!((step_max_secs(&step, 120.0) - 8.0).abs() < 0.001); // 4 bars @ 120 BPM = 8s
    }

    #[test]
    fn step_max_secs_instant_only() {
        let step = ParallelStep { commands: vec![Command::Play { deck: 0 }] };
        assert_eq!(step_max_secs(&step, 120.0), 0.0);
    }

    #[test]
    fn script_full_t4_acceptance() {
        // The full T4 acceptance test expression
        let s = parse_script("2 | eq hi 0.25 & fadein 16bars > 1 | eq lo kill over 4bars > fadeout 1 4bars").unwrap();
        assert_eq!(s.statements.len(), 1);
        assert_eq!(s.statements[0].steps.len(), 3);
        // Step 0: two commands targeting deck 2
        assert_eq!(s.statements[0].steps[0].commands.len(), 2);
        assert!(matches!(&s.statements[0].steps[0].commands[0], Command::Eq { deck: 1, .. }));
        assert!(matches!(&s.statements[0].steps[0].commands[1], Command::Gain { deck: 1, .. }));
        // Step 1: eq lo kill over 4bars targeting deck 1
        assert!(matches!(&s.statements[0].steps[1].commands[0], Command::Eq { deck: 0, band: EqBand::Lo, ramp: Some(_), .. }));
        // Step 2: fadeout targeting deck 1
        assert!(matches!(&s.statements[0].steps[2].commands[0], Command::Gain { deck: 0, target, .. } if *target < 0.01));
    }
}
