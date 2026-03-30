/// Typed builders for all SuperCollider OSC messages used in Performative.
use rosc::{OscMessage, OscType};

// ── Server ────────────────────────────────────────────────────────────────────

pub fn status() -> OscMessage {
    OscMessage { addr: "/status".into(), args: vec![] }
}

pub fn quit() -> OscMessage {
    OscMessage { addr: "/quit".into(), args: vec![] }
}

pub fn notify(flag: bool) -> OscMessage {
    OscMessage {
        addr: "/notify".into(),
        args: vec![OscType::Int(flag as i32)],
    }
}

// ── SynthDef loading ─────────────────────────────────────────────────────────

/// Load a pre-compiled .scsyndef file from disk.
pub fn d_load(path: &str) -> OscMessage {
    OscMessage {
        addr: "/d_load".into(),
        args: vec![OscType::String(path.to_string())],
    }
}

/// Remove a SynthDef from the server.
pub fn d_free(name: &str) -> OscMessage {
    OscMessage {
        addr: "/d_free".into(),
        args: vec![OscType::String(name.to_string())],
    }
}

// ── Buffer management ─────────────────────────────────────────────────────────

/// Allocate a buffer and read an audio file into it.
/// buf_num: buffer number (0-1023)
/// path: path to audio file
pub fn b_alloc_read(buf_num: i32, path: &str) -> OscMessage {
    OscMessage {
        addr: "/b_allocRead".into(),
        args: vec![
            OscType::Int(buf_num),
            OscType::String(path.to_string()),
        ],
    }
}

/// Free a buffer.
pub fn b_free(buf_num: i32) -> OscMessage {
    OscMessage {
        addr: "/b_free".into(),
        args: vec![OscType::Int(buf_num)],
    }
}

/// Query buffer info (returns /b_info with num_frames, num_channels, sample_rate).
pub fn b_query(buf_num: i32) -> OscMessage {
    OscMessage {
        addr: "/b_query".into(),
        args: vec![OscType::Int(buf_num)],
    }
}

// ── Group management ─────────────────────────────────────────────────────────

/// Create a group node at the head of another group.
pub fn g_new_head(group_id: i32, target_id: i32) -> OscMessage {
    OscMessage {
        addr: "/g_new".into(),
        args: vec![
            OscType::Int(group_id),
            OscType::Int(0), // add action: ADD_TO_HEAD
            OscType::Int(target_id),
        ],
    }
}

// ── Synth management ─────────────────────────────────────────────────────────

/// Instantiate a synth.
/// add_action: 0=addToHead, 1=addToTail, 2=addBefore, 3=addAfter, 4=addReplace
pub fn s_new(def_name: &str, node_id: i32, add_action: i32, target: i32, controls: &[(&str, f32)]) -> OscMessage {
    let mut args = vec![
        OscType::String(def_name.to_string()),
        OscType::Int(node_id),
        OscType::Int(add_action),
        OscType::Int(target),
    ];
    for (name, val) in controls {
        args.push(OscType::String(name.to_string()));
        args.push(OscType::Float(*val));
    }
    OscMessage { addr: "/s_new".into(), args }
}

/// Set one or more controls on an existing node.
pub fn n_set(node_id: i32, controls: &[(&str, f32)]) -> OscMessage {
    let mut args = vec![OscType::Int(node_id)];
    for (name, val) in controls {
        args.push(OscType::String(name.to_string()));
        args.push(OscType::Float(*val));
    }
    OscMessage { addr: "/n_set".into(), args }
}

/// Set a control by integer index.
pub fn n_set_int(node_id: i32, index: i32, val: f32) -> OscMessage {
    OscMessage {
        addr: "/n_set".into(),
        args: vec![
            OscType::Int(node_id),
            OscType::Int(index),
            OscType::Float(val),
        ],
    }
}

/// Free (remove) a node.
pub fn n_free(node_id: i32) -> OscMessage {
    OscMessage {
        addr: "/n_free".into(),
        args: vec![OscType::Int(node_id)],
    }
}

/// Run (unpause) or pause a node.
pub fn n_run(node_id: i32, run: bool) -> OscMessage {
    OscMessage {
        addr: "/n_run".into(),
        args: vec![OscType::Int(node_id), OscType::Int(run as i32)],
    }
}

// ── Node ID constants (reserved ranges) ──────────────────────────────────────
// 1         = root group (always exists)
// 100–109   = deck player synths (100 = deck 1, 101 = deck 2)
// 110–119   = deck EQ synths    (110 = deck 1, 111 = deck 2)
// 120       = master mix synth
// 121       = cue mix synth
// 1000–1999 = buffers (1000 = deck 1, 1001 = deck 2)

pub const ROOT_GROUP: i32 = 1;
pub const DECK_PLAYER_BASE: i32 = 100;   // +deck_index (0-based)
pub const DECK_EQ_BASE: i32 = 110;
pub const MASTER_MIX_NODE: i32 = 120;
pub const CUE_MIX_NODE: i32 = 121;
pub const BUFFER_BASE: i32 = 1000;       // +deck_index

pub const DECK_BUS_BASE: i32 = 10;       // +deck_index (private audio buses)
// Deck 1 → bus 10–11 (stereo)
// Deck 2 → bus 12–13 (stereo)
pub const MASTER_BUS: i32 = 0;           // hardware output 0–1
pub const CUE_BUS: i32 = 2;             // hardware output 2–3
