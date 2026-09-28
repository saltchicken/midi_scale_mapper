use clap::Parser;
use crossterm::event::{read, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use midir::os::unix::{VirtualInput, VirtualOutput};
use midir::{MidiInput, MidiOutput, MidiOutputConnection};
use std::error::Error;
use std::io::{self, Write};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

const ROOT_NOTES: &[&str] = &["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];

const SCALES: &[(&str, [u8; 7])] = &[
    ("Major (Ionian)", [0, 2, 4, 5, 7, 9, 11]),
    ("Minor (Aeolian)", [0, 2, 3, 5, 7, 8, 10]),
    ("Harmonic Minor", [0, 2, 3, 5, 7, 8, 11]),
    ("Dorian", [0, 2, 3, 5, 7, 9, 10]),
    ("Phrygian", [0, 1, 3, 5, 7, 8, 10]),
    ("Lydian", [0, 2, 4, 6, 7, 9, 11]),
    ("Mixolydian", [0, 2, 4, 5, 7, 9, 10]),
    ("Locrian", [0, 1, 3, 5, 6, 8, 10]),
];

#[derive(Parser, Debug)]
#[command(author, version, about = "White-key to arbitrary scale MIDI mapper")]
struct Args {
    /// MIDI channel to listen for (1-16)
    #[arg(short, long, value_parser = clap::value_parser!(u8).range(1..=16))]
    in_channel: u8,

    /// MIDI channel to translate to (1-16)
    #[arg(short, long, value_parser = clap::value_parser!(u8).range(1..=16))]
    out_channel: u8,
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();

    let in_chan_idx = args.in_channel - 1;
    let out_chan_idx = args.out_channel - 1;

    // Shared state for the active scale and root note
    let current_root = Arc::new(AtomicU8::new(0)); // Default to C (0)
    let current_scale = Arc::new(AtomicU8::new(0)); // Default to Major (0)

    let cb_root = Arc::clone(&current_root);
    let cb_scale = Arc::clone(&current_scale);

    let midi_in = MidiInput::new("MIDI Scale Mapper")?;
    let midi_out = MidiOutput::new("MIDI Scale Mapper")?;

    let port_name = format!("Scale Mapper Ch {}", args.in_channel);
    let mut conn_out = midi_out.create_virtual(&port_name)?;

    // We must track which notes are currently active so that if the user changes 
    // the scale while holding a chord, we send the Note Off to the *correct* mapped note.
    let mut active_notes: [Option<u8>; 128] = [None; 128];

    let _conn_in = midi_in.create_virtual(
        &port_name,
        move |_stamp, message, _| {
            if message.len() < 3 {
                return;
            }

            let status = message[0];
            let message_type = status & 0xF0;
            let channel = status & 0x0F;

            if channel == in_chan_idx && (message_type == 0x90 || message_type == 0x80) {
                let original_note = message[1];
                let velocity = message[2];

                let is_note_on = message_type == 0x90 && velocity > 0;
                let is_note_off = message_type == 0x80 || (message_type == 0x90 && velocity == 0);

                if is_note_on {
                    let octave = original_note / 12;
                    let pitch_class = original_note % 12;

                    // Map the white keys (C Major) to scale indices (0-6)
                    let scale_index = match pitch_class {
                        0 => Some(0),  // C -> 1st degree
                        2 => Some(1),  // D -> 2nd degree
                        4 => Some(2),  // E -> 3rd degree
                        5 => Some(3),  // F -> 4th degree
                        7 => Some(4),  // G -> 5th degree
                        9 => Some(5),  // A -> 6th degree
                        11 => Some(6), // B -> 7th degree
                        _ => None,     // Ignore black keys entirely
                    };

                    if let Some(idx) = scale_index {
                        let root = cb_root.load(Ordering::Relaxed);
                        let scale = cb_scale.load(Ordering::Relaxed) as usize;
                        let intervals = SCALES[scale].1;

                        // Calculate new note based on root and intervals
                        let mut new_note = (octave * 12) as u16 + root as u16 + intervals[idx] as u16;
                        new_note = new_note.min(127); // Ensure it's valid MIDI

                        // Track the note so we can release it properly later
                        active_notes[original_note as usize] = Some(new_note as u8);

                        let translated = [message_type | out_chan_idx, new_note as u8, velocity];
                        let _ = conn_out.send(&translated);
                    }
                } else if is_note_off {
                    // Check if we are tracking a mapped Note On for this key
                    if let Some(mapped_note) = active_notes[original_note as usize] {
                        let translated = [message_type | out_chan_idx, mapped_note, velocity];
                        let _ = conn_out.send(&translated);
                        
                        // Clear the active note tracking
                        active_notes[original_note as usize] = None;
                    }
                }
            } else if channel == in_chan_idx {
                // Pass through other messages (Pitch bend, Mod wheel, CCs) directly
                let mut translated = [0u8; 3];
                let len = message.len().min(3);
                translated[..len].copy_from_slice(&message[..len]);
                translated[0] = (status & 0xF0) | out_chan_idx;
                let _ = conn_out.send(&translated[..len]);
            }
        },
        (),
    )?;

    enable_raw_mode()?;

    let print_ui = |root_idx: u8, scale_idx: u8| {
        // \x1B[2K clears the current line in terminal so UI doesn't spam downwards
        print!("\r\x1B[2K> Current Target: {} {} (Use UP/DOWN for Root, LEFT/RIGHT for Scale)\r", 
            ROOT_NOTES[root_idx as usize], 
            SCALES[scale_idx as usize].0
        );
        let _ = io::stdout().flush();
    };

    println!("White-Key Scale Mapper initialized!\r");
    println!("Listening for Ch {} -> Outputting on Ch {}\r", args.in_channel, args.out_channel);
    println!("Press 'q' or Esc to quit.\r");
    println!("----------------------------------------------------\r");
    print_ui(0, 0);

    loop {
        if let Event::Key(key_event) = read()? {
            if key_event.kind != KeyEventKind::Press {
                continue;
            }

            let mut r_idx = current_root.load(Ordering::Relaxed);
            let mut s_idx = current_scale.load(Ordering::Relaxed);
            let mut changed = false;

            match key_event.code {
                KeyCode::Up => {
                    r_idx = (r_idx + 1) % 12;
                    current_root.store(r_idx, Ordering::Relaxed);
                    changed = true;
                }
                KeyCode::Down => {
                    r_idx = if r_idx == 0 { 11 } else { r_idx - 1 };
                    current_root.store(r_idx, Ordering::Relaxed);
                    changed = true;
                }
                KeyCode::Right => {
                    s_idx = (s_idx + 1) % SCALES.len() as u8;
                    current_scale.store(s_idx, Ordering::Relaxed);
                    changed = true;
                }
                KeyCode::Left => {
                    s_idx = if s_idx == 0 { SCALES.len() as u8 - 1 } else { s_idx - 1 };
                    current_scale.store(s_idx, Ordering::Relaxed);
                    changed = true;
                }
                KeyCode::Char('q') | KeyCode::Esc => break,
                KeyCode::Char('c') if key_event.modifiers.contains(KeyModifiers::CONTROL) => break,
                _ => {}
            }

            if changed {
                print_ui(r_idx, s_idx);
            }
        }
    }

    disable_raw_mode()?;
    println!("\r\nExiting...");
    Ok(())
}
