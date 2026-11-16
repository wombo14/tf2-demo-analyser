//! Demo integrity: whether a demo file looks edited or cut.
//!
//! TF2 demos carry no signature, so nothing can prove a demo is genuine. What can be
//! checked is that the parts that must agree with each other still do:
//! - the header's frame count, tick count and length against what's in the file,
//! - the demo's ticks and the network packet counters, which only ever count up,
//! - the server's own clock (`net_Tick`, inside each packet's data), which moves in
//!   step with the demo's ticks. Cutting a stretch out and renumbering the rest
//!   leaves the server's clock jumping ahead where the cut was.
//!
//! The limits come from 57 real demos: in all of them the header's frame count was
//! exactly the number of packets, nothing counted backwards, and after the first
//! second the server's clock never got more than 29 ticks ahead in one step. Lag
//! makes the demo's ticks jump (up to 10 s), so a jump in ticks alone proves nothing.

use tf_demo_parser::demo::header::Header;
use tf_demo_parser::demo::message::Message;
use tf_demo_parser::demo::packet::Packet;
use tf_demo_parser::demo::parser::{DemoHandler, NullHandler, RawPacketStream};
use tf_demo_parser::Demo;

/// The server's clock settles during the first ticks of a demo, so its jumps there are ignored.
const WARMUP_TICKS: u32 = 200;
/// The server's clock getting this many ticks further ahead of the demo than it's
/// been in the last `CLOCK_MEMORY_TICKS` means that much of the match is missing
/// (about a second; real demos stay under 30). After a lag freeze the server's clock
/// catches up in one jump too, but only back to where it was before the freeze.
const CLOCK_SKIP_TICKS: i64 = 66;
const CLOCK_MEMORY_TICKS: u32 = 2000;
/// The header's tick count may run ahead of the last packet a little (up to 269 in real demos).
const HEADER_TICK_SLACK: i64 = 1000;
/// The header's length in seconds may differ from ticks times the tick interval by this much.
const HEADER_SECONDS_SLACK: f32 = 1.0;
/// At most this many places are listed for each kind of finding.
const MAX_PLACES: usize = 5;

/// How strongly a finding points to editing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// A fact about the demo, not a problem.
    Info,
    /// Damaged or unusual, which editing can cause, and so can a crash.
    Warning,
    /// Something a game never writes: a strong sign the file was edited.
    Edited,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Finding {
    pub severity: Severity,
    pub title: String,
    pub detail: String,
}

/// The result of checking a demo.
#[derive(Clone, Debug, Default)]
pub struct Report {
    pub findings: Vec<Finding>,
    /// Facts about the demo, for the top of the report.
    pub facts: Vec<(String, String)>,
}

impl Report {
    /// The strongest finding's severity, if any.
    pub fn worst(&self) -> Option<Severity> {
        self.findings.iter().map(|finding| finding.severity).max()
    }

    /// A short verdict for the main window.
    pub fn verdict(&self) -> &'static str {
        match self.worst() {
            Some(Severity::Edited) => "Looks edited",
            Some(Severity::Warning) => "Damaged or unusual",
            _ => "No signs of editing",
        }
    }
}

/// What the counters in a demo look like, measured packet by packet.
#[derive(Clone, Debug, Default)]
pub struct Measurements {
    pub header_ticks: u32,
    pub header_frames: u32,
    pub header_seconds: f32,
    pub header_signon: u32,
    pub server: String,
    pub nick: String,
    pub map: String,
    /// Seconds per tick, from the server's info in the demo.
    pub interval: f32,
    pub signon_packets: usize,
    pub message_packets: usize,
    pub other_packets: usize,
    pub first_tick: u32,
    pub last_tick: u32,
    pub ended_with_stop: bool,
    pub incomplete: bool,
    pub parse_error: Option<String>,
    /// Steps between consecutive gameplay packets' ticks, as (step, how often).
    pub tick_steps: Vec<(i64, usize)>,
    /// Steps between consecutive gameplay packets' incoming and outgoing sequence numbers.
    pub in_steps: Vec<(i64, usize)>,
    pub out_steps: Vec<(i64, usize)>,
    /// Server tick (net_Tick) minus demo tick, as (offset, how often).
    pub server_offsets: Vec<(i64, usize)>,
    /// The biggest change of that offset from one net_Tick to the next, and its demo tick.
    pub biggest_offset_jump: (i64, u32),
    /// Where a counter went backwards: which one, the demo tick, and by how much.
    pub backwards: Vec<(&'static str, u32, i64)>,
    /// Where the server's clock jumped ahead of the demo after the warmup: demo tick and ticks.
    pub clock_skips: Vec<(u32, i64)>,
}

fn count(list: &mut Vec<(i64, usize)>, value: i64) {
    match list.iter_mut().find(|(known, _)| *known == value) {
        Some((_, times)) => *times += 1,
        None => list.push((value, 1)),
    }
}

/// Checks a demo file.
pub fn check(bytes: &[u8]) -> Report {
    judge(&measure(bytes))
}

/// Measures a demo's counters. Never fails: what couldn't be read is noted in it.
pub fn measure(bytes: &[u8]) -> Measurements {
    let mut found = Measurements::default();
    let demo = Demo::new(bytes);
    let mut stream = demo.get_stream();
    let header = match stream.read::<Header>() {
        Ok(header) => header,
        Err(error) => {
            found.parse_error = Some(format!("header: {error}"));
            return found;
        }
    };
    found.header_ticks = header.ticks;
    found.header_frames = header.frames;
    found.header_seconds = header.duration;
    found.header_signon = header.signon;
    found.server = header.server.clone();
    found.nick = header.nick.clone();
    found.map = header.map.clone();

    let mut handler = DemoHandler::with_analyser(NullHandler);
    handler.handle_header(&header);
    let mut packets = RawPacketStream::new(stream);
    let mut previous: Option<(u32, u32, u32)> = None;
    let mut last_offset: Option<i64> = None;
    let mut recent: std::collections::VecDeque<(u32, i64)> = std::collections::VecDeque::new();
    let mut first = None;
    loop {
        let packet = match packets.next(&handler.state_handler) {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(error) => {
                found.parse_error = Some(error.to_string());
                break;
            }
        };
        match &packet {
            Packet::Signon(_) => found.signon_packets += 1,
            Packet::Message(message) => {
                found.message_packets += 1;
                let tick = u32::from(message.tick);
                let first_tick = *first.get_or_insert(tick);
                found.last_tick = tick;
                let (sequence_in, sequence_out) = (message.meta.sequence_in, message.meta.sequence_out);
                if let Some((tick_before, in_before, out_before)) = previous {
                    let steps = [
                        ("tick", i64::from(tick) - i64::from(tick_before)),
                        ("incoming packet number", i64::from(sequence_in) - i64::from(in_before)),
                        ("outgoing packet number", i64::from(sequence_out) - i64::from(out_before)),
                    ];
                    count(&mut found.tick_steps, steps[0].1);
                    count(&mut found.in_steps, steps[1].1);
                    count(&mut found.out_steps, steps[2].1);
                    for (what, step) in steps {
                        if step < 0 {
                            found.backwards.push((what, tick, step));
                        }
                    }
                }
                previous = Some((tick, sequence_in, sequence_out));
                for inner in &message.messages {
                    if let Message::NetTick(net_tick) = inner {
                        let offset = i64::from(u32::from(net_tick.tick)) - i64::from(tick);
                        count(&mut found.server_offsets, offset);
                        if let Some(before) = last_offset {
                            let jump = offset - before;
                            if jump.abs() > found.biggest_offset_jump.0.abs() {
                                found.biggest_offset_jump = (jump, tick);
                            }
                            let highest = recent.iter().map(|(_, offset)| *offset).max().unwrap_or(before);
                            let beyond = offset - highest;
                            if jump > CLOCK_SKIP_TICKS && beyond > CLOCK_SKIP_TICKS && tick.saturating_sub(first_tick) > WARMUP_TICKS {
                                found.clock_skips.push((tick, beyond));
                            }
                        }
                        while recent.front().is_some_and(|(at, _)| tick.saturating_sub(*at) > CLOCK_MEMORY_TICKS) {
                            recent.pop_front();
                        }
                        recent.push_back((tick, offset));
                        last_offset = Some(offset);
                    }
                }
            }
            Packet::Stop(_) => found.ended_with_stop = true,
            _ => found.other_packets += 1,
        }
        if let Err(error) = handler.handle_packet(packet) {
            found.parse_error = Some(error.to_string());
            break;
        }
    }
    found.interval = handler.state_handler.demo_meta.interval_per_tick;
    found.first_tick = first.unwrap_or(0);
    found.incomplete = packets.incomplete;
    for list in [&mut found.tick_steps, &mut found.in_steps, &mut found.out_steps, &mut found.server_offsets] {
        list.sort_by(|a, b| b.1.cmp(&a.1));
    }
    found
}

/// Turns measurements into findings.
pub fn judge(m: &Measurements) -> Report {
    let mut report = Report::default();
    let interval = if m.interval > 0.0 { m.interval } else { 0.015 };
    let clock = |tick: u32| {
        let seconds = (f64::from(tick.saturating_sub(m.first_tick)) * f64::from(interval)).round().max(0.0) as u64;
        format!("{}:{:02}", seconds / 60, seconds % 60)
    };
    let mut add = |severity, title: &str, detail: String| {
        report.findings.push(Finding { severity, title: title.to_string(), detail });
    };

    if let Some(error) = &m.parse_error {
        add(
            Severity::Warning,
            "Part of it can't be read",
            format!("The demo stops making sense partway through ({error}). It's damaged, or was edited carelessly."),
        );
    }

    let finished = m.header_frames > 0 || m.header_ticks > 0;
    if !finished {
        add(
            Severity::Warning,
            "The header was never filled in",
            "TF2 fills in the demo's length when recording stops. It's empty here: the game closed or crashed while \
             recording, or the demo was still being recorded. The rest can't be compared with it."
                .to_string(),
        );
    } else {
        let packets = m.message_packets as i64;
        // A file whose end is missing is short of packets and ticks for that reason alone.
        let cut_short = !m.ended_with_stop && packets < i64::from(m.header_frames);
        if cut_short {
            add(
                Severity::Warning,
                "The end of the file is missing",
                format!(
                    "The header says the demo has {} packets, but the file stops after {packets}, without the marker                      demos end with. It was cut short after it was saved: a failed download or copy, or on purpose.",
                    m.header_frames
                ),
            );
        } else if i64::from(m.header_frames) != packets {
            add(
                Severity::Edited,
                "Packets were added or removed",
                format!(
                    "The header says the demo has {} packets, but it has {packets}. TF2 writes the count when recording \
                     stops; in real demos it always matches exactly.",
                    m.header_frames
                ),
            );
        }
        let extra = i64::from(m.header_ticks) - i64::from(m.last_tick);
        if m.message_packets > 0 && extra < -5 {
            add(
                Severity::Edited,
                "It goes on longer than its header says",
                format!(
                    "The header says the demo ends at tick {}, but it goes on to tick {}: something was added after it was saved.",
                    m.header_ticks, m.last_tick
                ),
            );
        } else if m.message_packets > 0 && extra > HEADER_TICK_SLACK && !cut_short {
            add(
                Severity::Warning,
                "It ends earlier than its header says",
                format!(
                    "The header says the demo ends at tick {}, but the last packet is at tick {} ({:.0} s earlier): its end may have been cut off.",
                    m.header_ticks,
                    m.last_tick,
                    extra as f32 * interval
                ),
            );
        }
        let expected = m.header_ticks as f32 * interval;
        if m.interval > 0.0 && (m.header_seconds - expected).abs() > HEADER_SECONDS_SLACK {
            add(
                Severity::Edited,
                "The header's length doesn't add up",
                format!(
                    "The header says {:.0} seconds, but its {} ticks make {expected:.0} seconds. TF2 writes both together.",
                    m.header_seconds, m.header_ticks
                ),
            );
        }
    }

    if finished && !m.ended_with_stop && m.message_packets as i64 >= i64::from(m.header_frames) {
        add(
            Severity::Warning,
            "The end of the file is missing",
            "Demos end with a stop marker, and this one has none, although its header was filled in. The file was cut \
             short after it was saved."
                .to_string(),
        );
    }

    if !m.backwards.is_empty() {
        let places: Vec<String> = m
            .backwards
            .iter()
            .take(MAX_PLACES)
            .map(|(what, tick, step)| format!("{what} by {} at {}", -step, clock(*tick)))
            .collect();
        add(
            Severity::Edited,
            "Time goes backwards",
            format!(
                "A counter that only ever counts up went back {} time{}: {}. Parts of different recordings were joined, \
                 or packets were moved.",
                m.backwards.len(),
                if m.backwards.len() == 1 { "" } else { "s" },
                places.join(", ")
            ),
        );
    }

    if !m.clock_skips.is_empty() {
        let places: Vec<String> = m
            .clock_skips
            .iter()
            .take(MAX_PLACES)
            .map(|(tick, ticks)| format!("{:.1} s at {}", *ticks as f32 * interval, clock(*tick)))
            .collect();
        add(
            Severity::Edited,
            "Part of the match is missing",
            format!(
                "The server's own clock, which is inside every packet, jumps ahead of the demo: {}. The demo's ticks \
                 carry on as if nothing happened, so that stretch was cut out and the rest renumbered. Lag never makes \
                 the server's clock jump ahead like this.",
                places.join(", ")
            ),
        );
    }

    let length = (f64::from(m.last_tick.saturating_sub(m.first_tick)) * f64::from(interval)).round() as u64;
    report.facts = vec![
        ("Recorded by".to_string(), m.nick.clone()),
        ("Map".to_string(), m.map.clone()),
        ("Length".to_string(), format!("{}:{:02} ({} ticks)", length / 60, length % 60, m.last_tick.saturating_sub(m.first_tick))),
        ("Packets".to_string(), m.message_packets.to_string()),
    ];
    if report.findings.is_empty() {
        report.findings.push(Finding {
            severity: Severity::Info,
            title: "Everything adds up".to_string(),
            detail: "The header matches the content, nothing counts backwards, and the server's clock keeps in step with \
                     the demo. This can't prove the demo is genuine, since a careful editor can keep all of this right, \
                     but nothing here was broken."
                .to_string(),
        });
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn genuine() -> Measurements {
        Measurements {
            header_ticks: 40_200,
            header_frames: 39_562,
            header_seconds: 603.0,
            interval: 0.015,
            message_packets: 39_562,
            first_tick: 0,
            last_tick: 40_200,
            ended_with_stop: true,
            nick: "someone".into(),
            map: "koth_brazil".into(),
            ..Measurements::default()
        }
    }

    #[test]
    fn a_genuine_demo_has_no_findings() {
        let report = judge(&genuine());
        assert_eq!(report.worst(), Some(Severity::Info));
        assert_eq!(report.verdict(), "No signs of editing");
    }

    #[test]
    fn removed_packets_are_found() {
        let report = judge(&Measurements { message_packets: 39_000, ..genuine() });
        assert_eq!(report.worst(), Some(Severity::Edited));
        assert!(report.findings.iter().any(|f| f.title.contains("added or removed")));
    }

    #[test]
    fn joined_recordings_go_backwards() {
        let report = judge(&Measurements { backwards: vec![("tick", 20_000, -5_000)], ..genuine() });
        assert_eq!(report.verdict(), "Looks edited");
        assert!(report.findings[0].detail.contains("5000"));
    }

    #[test]
    fn a_cut_shows_in_the_servers_clock() {
        let report = judge(&Measurements { clock_skips: vec![(30_000, 1_000)], ..genuine() });
        assert!(report.findings.iter().any(|f| f.title == "Part of the match is missing" && f.detail.contains("15.0 s at 7:30")));
    }

    #[test]
    fn a_crash_is_damage_not_editing() {
        let crashed = Measurements { header_ticks: 0, header_frames: 0, header_seconds: 0.0, ended_with_stop: false, ..genuine() };
        let report = judge(&crashed);
        assert_eq!(report.worst(), Some(Severity::Warning));
        assert_eq!(report.findings.len(), 1, "an empty header isn't also a mismatch: {:?}", report.findings);
    }

    #[test]
    fn a_wrong_length_in_the_header_is_found() {
        let report = judge(&Measurements { header_seconds: 300.0, ..genuine() });
        assert!(report.findings.iter().any(|f| f.title.contains("length doesn't add up")));
        let extended = judge(&Measurements { last_tick: 45_000, ..genuine() });
        assert!(extended.findings.iter().any(|f| f.title.contains("longer than")));
    }
}

/// Edits real demos the ways a forger would, to check each edit is caught.
#[cfg(test)]
mod forgeries {
    use super::*;

    /// Byte offset where each packet starts, and its kind (the first byte).
    fn frames(bytes: &[u8]) -> Vec<(usize, u8)> {
        let demo = Demo::new(bytes);
        let mut stream = demo.get_stream();
        let header: Header = stream.read().unwrap();
        let mut handler = DemoHandler::with_analyser(NullHandler);
        handler.handle_header(&header);
        let mut packets = RawPacketStream::new(stream);
        let mut found = Vec::new();
        loop {
            let at = packets.pos();
            assert_eq!(at % 8, 0, "packets start on whole bytes");
            let Ok(Some(packet)) = packets.next(&handler.state_handler) else { break };
            found.push((at / 8, bytes[at / 8]));
            handler.handle_packet(packet).unwrap();
        }
        found
    }

    const PACKET: u8 = 2;
    const HEADER_SECONDS: usize = 1056;
    const HEADER_TICKS: usize = 1060;
    const HEADER_FRAMES: usize = 1064;

    fn read_u32(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
    }

    fn write_u32(bytes: &mut [u8], at: usize, value: u32) {
        bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    /// Cuts out the packets from `from` to `to` (indexes into `frames`). With `cover_up`,
    /// also renumbers the ticks and packet counters after the cut and fixes the header,
    /// as a careful forger would.
    fn cut(bytes: &[u8], frames: &[(usize, u8)], from: usize, to: usize, cover_up: bool) -> Vec<u8> {
        let tick_at = |frame: usize| read_u32(bytes, frames[frame].0 + 1);
        let removed_ticks = tick_at(to) - tick_at(from);
        let removed_packets = frames[from..to].iter().filter(|(_, kind)| *kind == PACKET).count() as u32;
        let mut out = bytes[..frames[from].0].to_vec();
        let rest_start = out.len();
        out.extend_from_slice(&bytes[frames[to].0..]);
        if cover_up {
            let shift = frames[to].0 - frames[from].0;
            for &(start, kind) in &frames[to..] {
                let at = start - shift;
                if at + 5 > out.len() {
                    continue;
                }
                let tick = read_u32(&out, at + 1);
                write_u32(&mut out, at + 1, tick.saturating_sub(removed_ticks));
                if kind == PACKET {
                    for field in [at + 81, at + 85] {
                        let value = read_u32(&out, field);
                        write_u32(&mut out, field, value.saturating_sub(removed_packets));
                    }
                }
            }
            let ticks = read_u32(&out, HEADER_TICKS) - removed_ticks;
            write_u32(&mut out, HEADER_TICKS, ticks);
            let frames_left = read_u32(&out, HEADER_FRAMES) - removed_packets;
            write_u32(&mut out, HEADER_FRAMES, frames_left);
            let seconds = f32::from_le_bytes(out[HEADER_SECONDS..HEADER_SECONDS + 4].try_into().unwrap());
            let shorter = seconds - removed_ticks as f32 * 0.015;
            out[HEADER_SECONDS..HEADER_SECONDS + 4].copy_from_slice(&shorter.to_le_bytes());
        }
        let _ = rest_start;
        out
    }

    fn titles(report: &Report) -> Vec<String> {
        report.findings.iter().map(|f| format!("{:?}: {}", f.severity, f.title)).collect()
    }

    /// `DEMO=... cargo test --release --lib integrity::forgeries -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn forged_demos_are_caught() {
        let Ok(path) = std::env::var("DEMO") else { return };
        let bytes = std::fs::read(path).unwrap();
        let original = check(&bytes);
        println!("original: {} {:?}", original.verdict(), titles(&original));
        assert_eq!(original.verdict(), "No signs of editing");

        let frames = frames(&bytes);
        let packets: Vec<usize> = (0..frames.len()).filter(|&i| frames[i].1 == PACKET).collect();
        // Thirty seconds or so, from the middle of the match.
        let from = packets[packets.len() / 2];
        let length: usize = std::env::var("CUT_PACKETS").ok().and_then(|v| v.parse().ok()).unwrap_or(1500);
        let to = packets[packets.len() / 2 + length];

        let careless = check(&cut(&bytes, &frames, from, to, false));
        println!("cut out: {} {:?}", careless.verdict(), titles(&careless));
        assert_eq!(careless.verdict(), "Looks edited");

        let careful_bytes = cut(&bytes, &frames, from, to, true);
        if let Ok(out) = std::env::var("FORGED_OUT") {
            std::fs::write(out, &careful_bytes).unwrap();
        }
        let careful = check(&careful_bytes);
        println!("cut out, renumbered, header fixed: {} {:?}", careful.verdict(), titles(&careful));
        for finding in &careful.findings {
            println!("    {}", finding.detail);
        }
        assert_eq!(careful.verdict(), "Looks edited");
        assert!(careful.findings.iter().any(|f| f.title == "Part of the match is missing"));

        // Two halves of the match joined the wrong way round.
        let gameplay = frames[packets[0]].0;
        let mut swapped = bytes[..gameplay].to_vec();
        swapped.extend_from_slice(&bytes[frames[from].0..frames[to].0]);
        swapped.extend_from_slice(&bytes[gameplay..frames[from].0]);
        swapped.extend_from_slice(&bytes[frames[to].0..]);
        let joined = check(&swapped);
        println!("reordered: {} {:?}", joined.verdict(), titles(&joined));
        assert_eq!(joined.verdict(), "Looks edited");

        // Only the header's length changed.
        let mut header = bytes.clone();
        write_u32(&mut header, HEADER_FRAMES, read_u32(&bytes, HEADER_FRAMES) + 10);
        let changed = check(&header);
        println!("header changed: {} {:?}", changed.verdict(), titles(&changed));
        assert_eq!(changed.verdict(), "Looks edited");

        // The end chopped off, as a crash or a failed download would.
        let chopped = check(&bytes[..bytes.len() * 3 / 4]);
        println!("end chopped off: {} {:?}", chopped.verdict(), titles(&chopped));
        assert_eq!(chopped.verdict(), "Damaged or unusual");
    }
}

#[cfg(test)]
mod survey {
    use super::*;

    fn summary(list: &[(i64, usize)]) -> String {
        let shown: Vec<String> = list.iter().take(6).map(|(value, times)| format!("{value}x{times}")).collect();
        format!("{} [{} kinds]", shown.join(" "), list.len())
    }

    /// `DEMO_DIR=dir1;dir2 cargo test --release --lib integrity::survey -- --ignored --nocapture`,
    /// with `BRIEF=1` for one line per demo.
    #[test]
    #[ignore]
    fn measure_demos() {
        let Ok(dirs) = std::env::var("DEMO_DIR") else { return };
        let mut paths: Vec<std::path::PathBuf> = Vec::new();
        for dir in dirs.split(';') {
            let Ok(entries) = std::fs::read_dir(dir) else { continue };
            paths.extend(entries.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "dem")));
        }
        paths.sort();
        for path in paths {
            let bytes = std::fs::read(&path).unwrap();
            let m = measure(&bytes);
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            let report = judge(&m);
            let titles: Vec<&str> = report.findings.iter().filter(|f| f.severity > Severity::Info).map(|f| f.title.as_str()).collect();
            if std::env::var("BRIEF").is_ok() {
                let biggest_step = m.tick_steps.iter().map(|(s, _)| *s).max().unwrap_or(0);
                println!(
                    "{name:32} {:20} frames {} ticks {}-{} step {biggest_step} offjump {:?} {titles:?}",
                    report.verdict(),
                    i64::from(m.header_frames) - m.message_packets as i64,
                    m.header_ticks,
                    m.last_tick,
                    m.biggest_offset_jump,
                );
                continue;
            }
            println!(
                "{name}: hdr ticks {} frames {} secs {:.0} | msg {} | ticks {}..{} stop {} err {:?} | {}",
                m.header_ticks, m.header_frames, m.header_seconds, m.message_packets, m.first_tick, m.last_tick,
                m.ended_with_stop, m.parse_error, report.verdict()
            );
            println!("    tick steps {}", summary(&m.tick_steps));
            println!("    in steps   {}", summary(&m.in_steps));
            println!("    out steps  {}", summary(&m.out_steps));
            println!("    server-demo {}", summary(&m.server_offsets));
        }
    }
}
