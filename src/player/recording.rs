//! Reads a whole demo once and keeps a snapshot of every sampled tick, so the
//! player can jump to any moment instantly. A demo on its own is a stream of
//! changes, which would otherwise have to be replayed from the start on every seek.

use std::collections::{HashMap, HashSet};

use glam::{Quat, Vec3};

use tf_demo_parser::demo::sendprop::SendPropValue;
use tf_demo_parser::demo::data::DemoTick;
use tf_demo_parser::demo::gamevent::GameEvent;
use tf_demo_parser::demo::header::Header;
use tf_demo_parser::demo::message::packetentities::{EntityId, UpdateType};
use tf_demo_parser::demo::message::usermessage::{ChatMessageKind, UserMessage};
use tf_demo_parser::demo::message::Message;
use tf_demo_parser::demo::packet::datatable::{ParseSendTable, ServerClass};
use tf_demo_parser::demo::packet::message::MessagePacketMeta;
use tf_demo_parser::demo::packet::stringtable::StringTableEntry;
use tf_demo_parser::demo::parser::analyser::ChatMessage;
use tf_demo_parser::demo::parser::gamestateanalyser::{
    Building, GameState, GameStateAnalyser, PlayerState,
};
use tf_demo_parser::demo::parser::handler::BorrowMessageHandler;
use tf_demo_parser::demo::parser::MessageHandler;
use tf_demo_parser::demo::sendprop::SendPropIdentifier;
use tf_demo_parser::demo::vector::Vector;
use tf_demo_parser::{Demo, DemoParser, MessageType, ParserState};

pub const TEAM_RED: u8 = 2;
pub const TEAM_BLUE: u8 = 3;

pub const CLASS_MEDIC: u8 = 5;
pub const CLASS_HEAVY: u8 = 6;

pub const PLAYER_ALIVE: u8 = 1 << 0;
/// The player dropped out of what the recording player's game could see.
pub const PLAYER_OUT_OF_VIEW: u8 = 1 << 1;
pub const PLAYER_DUCKING: u8 = 1 << 2;
pub const PLAYER_ON_GROUND: u8 = 1 << 3;
pub const PLAYER_IN_WATER: u8 = 1 << 4;
/// A Sniper looking through their scope.
pub const PLAYER_ZOOMED: u8 = 1 << 5;
/// A revved minigun, a charging rifle or a drawn bow.
pub const PLAYER_AIMING: u8 = 1 << 6;
/// A Spy who's cloaked (TF2's "stealthed" condition), Dead Ringer included.
pub const PLAYER_CLOAKED: u8 = 1 << 7;
const COND_AIMING: i64 = 1 << 0;
const COND_ZOOMED: i64 = 1 << 1;
const COND_STEALTHED: i64 = 1 << 4;
/// Moves bigger than this between frames are respawns or teleports, not walking.
const MAX_STRIDE: f32 = 300.0;

/// Which of a player's three weapons is out, when that's known.
pub const WEAPON_UNKNOWN: u8 = 255;

const FL_ONGROUND: i64 = 1 << 0;
const FL_DUCKING: i64 = 1 << 1;
const FL_INWATER: i64 = 1 << 9;
/// Entity handles keep the entity's index in their low bits.
const HANDLE_INDEX_MASK: i64 = 0x7FF;
const EF_NODRAW: i64 = 0x20;
const DEMO_USE_ORIGIN2: u32 = 1;
const DEMO_USE_ANGLES2: u32 = 2;
/// Changes smaller than these don't start a new keyframe for a map object.
const PROP_MOVE_EPSILON: f32 = 0.5;
const PROP_TURN_EPSILON: f32 = 0.002;

/// Entity classes that are map objects worth drawing: carts, doors, pickups and so on.
const PROP_CLASSES: [&str; 14] = [
    "CDynamicProp",
    "CPhysicsProp",
    "CPhysicsPropMultiplayer",
    "CBaseAnimating",
    "CBreakableProp",
    "CCaptureFlag",
    "CTFAmmoPack",
    "CTFDroppedWeapon",
    "CCurrencyPack",
    "CHalloweenPickup",
    "CTFPumpkinBomb",
    "CTFGenericBomb",
    "CTeamControlPoint",
    "CItem",
];

/// Entity classes for the map's moving brushes: doors (sliding and swinging),
/// trains and spinning parts. TF2 sends func_movelinear, func_brush and the
/// like as plain CBaseEntity, which covers pieces riding on a door too. Only
/// the ones whose model is one of the map's brushes (`*N`) are followed.
const MOVER_CLASSES: [&str; 4] = ["CBaseDoor", "CFuncTrackTrain", "CFuncRotating", "CBaseEntity"];

/// Long demos are sampled more sparsely so one file can't exhaust memory.
const MAX_FRAMES: u32 = 150_000;
const DEFAULT_TICK_INTERVAL: f32 = 0.015;
const STANDING_HULL_HEIGHT: f32 = 70.0;

const CUSTOM_KILL_HEADSHOT: u16 = 1;
const CUSTOM_KILL_BACKSTAB: u16 = 2;
const DEATH_FLAG_FEIGNED: u16 = 32;
const STEAM_ID64_BASE: u64 = 76561197960265728;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PlayerFrame {
    pub position: [f32; 3],
    pub yaw: f32,
    pub pitch: f32,
    pub health: i16,
    pub max_health: i16,
    pub team: u8,
    pub class: u8,
    pub flags: u8,
    /// Medic uber percentage.
    pub charge: u8,
    /// 0 for primary, 1 for secondary, 2 for melee.
    pub weapon_slot: u8,
    /// The weapon in hand, as an index into `Recording::models`. Zero means unknown.
    pub weapon_model: u16,
    /// How far this player has walked since the demo started, which keeps their
    /// steps in time with their movement.
    pub travelled: f32,
    /// Ping in milliseconds as the server measured it, the number TF2's scoreboard
    /// shows. The server updates it about once a second.
    pub ping: u16,
    /// The player's own model, as an index into `Recording::models`, when it isn't
    /// their class's usual one: Mann vs. Machine robots. Zero for the usual one.
    pub body_model: u16,
    /// How big they're drawn: 1.75 for a Mann vs. Machine giant. Zero means 1.
    pub scale: f32,
}

impl PlayerFrame {
    pub fn is_alive(&self) -> bool {
        self.flags & PLAYER_ALIVE != 0
    }

    pub fn is_on_a_team(&self) -> bool {
        self.team == TEAM_RED || self.team == TEAM_BLUE
    }

    pub fn is_ducking(&self) -> bool {
        self.flags & PLAYER_DUCKING != 0
    }

    pub fn is_on_ground(&self) -> bool {
        self.flags & PLAYER_ON_GROUND != 0
    }

    pub fn is_in_water(&self) -> bool {
        self.flags & PLAYER_IN_WATER != 0
    }

    pub fn is_cloaked(&self) -> bool {
        self.flags & PLAYER_CLOAKED != 0
    }

    pub fn is_zoomed(&self) -> bool {
        self.flags & PLAYER_ZOOMED != 0
    }

    pub fn is_aiming(&self) -> bool {
        self.flags & PLAYER_AIMING != 0
    }

    pub fn is_out_of_view(&self) -> bool {
        self.flags & PLAYER_OUT_OF_VIEW != 0
    }

    /// How far above their feet this player's eyes are.
    /// How big they're drawn, 1 for normal size.
    pub fn size(&self) -> f32 {
        if self.scale > 0.0 { self.scale } else { 1.0 }
    }

    pub fn eye_height(&self) -> f32 {
        let height = if self.is_ducking() {
            45.0
        } else {
            match self.class {
                1 => 65.0,
                2 | 5 | 6 | 8 => 75.0,
                _ => 68.0,
            }
        };
        height * self.size()
    }
}

/// Exactly what the recording player's camera was doing, straight from the demo.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ViewFrame {
    pub origin: [f32; 3],
    pub pitch: f32,
    pub yaw: f32,
    pub roll: f32,
    /// The first-person arms, as an index into `Recording::models`. Zero means none.
    pub arms: u16,
    /// The animation the arms are playing, and the tick it started.
    pub sequence: u16,
    pub sequence_start: u32,
}

/// One keyframe of a map object. A new one is only stored when something changes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PropKey {
    pub tick: u32,
    pub position: [f32; 3],
    pub rotation: [f32; 4],
    pub sequence: u16,
    pub sequence_start: u32,
    pub skin: u8,
    pub body: u16,
}

/// A map object such as a cart or door, from when it appears until it's gone.
#[derive(Clone, Debug)]
pub struct PropTrack {
    pub model: u16,
    pub keys: Vec<PropKey>,
    /// The tick it stopped being shown. `u32::MAX` while it's still around.
    pub until: u32,
}

impl PropTrack {
    pub fn first_tick(&self) -> u32 {
        self.keys.first().map_or(u32::MAX, |key| key.tick)
    }

    /// The keyframe in effect at a tick and the one after it.
    pub fn keys_around(&self, tick: f64) -> Option<(&PropKey, Option<&PropKey>)> {
        if tick < f64::from(self.first_tick()) || tick >= f64::from(self.until) {
            return None;
        }
        let after = self.keys.partition_point(|key| f64::from(key.tick) <= tick);
        let current = self.keys.get(after.checked_sub(1)?)?;
        Some((current, self.keys.get(after)))
    }
}

/// Where a moving part of the map was at one tick.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MoverKey {
    pub tick: u32,
    pub position: [f32; 3],
    pub rotation: [f32; 4],
}

/// A part of the map built from its own brushes that moves, like a door or a
/// lift, and where it was over the demo. The map draws it; this says where.
#[derive(Clone, Debug)]
pub struct MoverTrack {
    /// The map's brush model it is, the `N` of `*N`.
    pub brush: u16,
    pub keys: Vec<MoverKey>,
}

impl MoverTrack {
    /// Where it is at a tick, gliding between keys, and staying put before the
    /// first and after the last. None before the demo first shows it, when it
    /// stays where the map put it.
    pub fn at(&self, tick: f64) -> Option<(Vec3, Quat)> {
        let after = self.keys.partition_point(|key| f64::from(key.tick) <= tick);
        let current = self.keys.get(after.checked_sub(1)?)?;
        let (position, rotation) = (Vec3::from(current.position), Quat::from_array(current.rotation));
        let Some(next) = self.keys.get(after) else { return Some((position, rotation)) };
        let span = f64::from(next.tick) - f64::from(current.tick);
        let t = if span > 0.0 { ((tick - f64::from(current.tick)) / span).clamp(0.0, 1.0) as f32 } else { 1.0 };
        Some((position.lerp(Vec3::from(next.position), t), rotation.slerp(Quat::from_array(next.rotation), t)))
    }

    /// Adds where it is now. It only gets a key when it has moved, so before a
    /// move it first gets one where it sat until `last_seen`; without it, it
    /// would seem to creep all the way from its previous move.
    fn push(&mut self, key: MoverKey, last_seen: u32) {
        let Some(last) = self.keys.last().copied() else {
            self.keys.push(key);
            return;
        };
        let moved = Vec3::from(last.position).distance(Vec3::from(key.position)) > PROP_MOVE_EPSILON;
        let turned = Quat::from_array(last.rotation).dot(Quat::from_array(key.rotation)).abs() < 1.0 - PROP_TURN_EPSILON;
        if !moved && !turned {
            return;
        }
        if last.tick < last_seen && last_seen < key.tick {
            self.keys.push(MoverKey { tick: last_seen, ..last });
        }
        self.keys.push(key);
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ObjectKind {
    Rocket,
    Arrow,
    Sticky,
    Pipe,
    Flare,
    LooseCannon,
    /// The Righteous Bison's and the Pomson's ring of energy.
    EnergyRing,
    /// The Cow Mangler's shot.
    EnergyBall,
    /// The Short Circuit's slow orb.
    Orb,
    /// The Dragon's Fury's fireball, and spell fireballs.
    Fireball,
    /// Thrown things: Jarate, Mad Milk, Gas Passer, cleavers, the Sandman's ball and
    /// the Wrap Assassin's ornament.
    Thrown,
    #[default]
    OtherProjectile,
    Sentry,
    Dispenser,
    TeleEntrance,
    TeleExit,
    Cart,
}

impl ObjectKind {
    /// What kind of projectile an entity of this class is, if it's one.
    pub fn of_projectile_class(class: &str) -> Option<Self> {
        Some(match class {
            "CTFProjectile_Rocket" | "CTFProjectile_SentryRocket" => Self::Rocket,
            "CTFProjectile_Arrow" | "CTFProjectile_HealingBolt" | "CTFProjectile_GrapplingHook" => Self::Arrow,
            "CTFProjectile_Flare" => Self::Flare,
            // Which of the three it is comes later, from its type.
            "CTFGrenadePipebombProjectile" => Self::Pipe,
            "CTFProjectile_EnergyRing" => Self::EnergyRing,
            "CTFProjectile_EnergyBall" => Self::EnergyBall,
            "CTFProjectile_MechanicalArmOrb" => Self::Orb,
            "CTFStunBall" | "CTFBall_Ornament" | "CTFProjectile_Cleaver" => Self::Thrown,
            _ if class.starts_with("CTFProjectile_Jar") || class.starts_with("CTFProjectile_Throwable") => Self::Thrown,
            _ if class.contains("BallOfFire") || class.contains("Fireball") => Self::Fireball,
            _ if class.starts_with("CTFProjectile_") => Self::OtherProjectile,
            _ => return None,
        })
    }

    /// Projectiles TF2 draws as a glow, with no model of their own.
    pub fn is_energy(self) -> bool {
        matches!(self, Self::EnergyRing | Self::EnergyBall | Self::Orb | Self::Fireball)
    }

    pub fn is_building(self) -> bool {
        matches!(
            self,
            Self::Sentry | Self::Dispenser | Self::TeleEntrance | Self::TeleExit
        )
    }
}

/// How a building's model is animating: the sequence, how far through it, and its
/// pose parameters (a sentry's aim, a teleporter's arrow) as 0 to 1 of their range.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ObjectAnimation {
    pub sequence: u16,
    /// Sent by the server for sentries. Teleporters animate on their own, by time.
    pub cycle: Option<f32>,
    pub pose: [f32; 2],
    /// How fast it plays, like a teleporter spinning up.
    pub rate: f32,
    /// A sentry's bullets left, or -1 when unknown. It drops as the sentry fires.
    pub shells: i16,
    /// The player slot a sentry is shooting at.
    pub target: Option<u8>,
}

impl Default for ObjectAnimation {
    fn default() -> Self {
        Self { sequence: 0, cycle: None, pose: [0.5, 0.5], rate: 1.0, shells: -1, target: None }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ObjectFrame {
    pub id: u16,
    pub kind: ObjectKind,
    pub team: u8,
    pub level: u8,
    pub crit: bool,
    pub sapped: bool,
    pub mini: bool,
    pub under_construction: bool,
    pub position: [f32; 3],
    pub yaw: f32,
    pub pitch: f32,
    pub health: i16,
    pub max_health: i16,
    /// Index into `Recording::models`. Zero means unknown.
    pub model: u16,
    pub animation: ObjectAnimation,
    /// The player slot that fired a projectile, `NO_OWNER` when unknown or not a projectile.
    pub owner: u8,
}

/// `ObjectFrame::owner` when no player owns it.
pub const NO_OWNER: u8 = u8::MAX;

#[derive(Clone, Debug, Default)]
pub struct SlotName {
    pub from_tick: u32,
    pub name: String,
    pub steam_id: u64,
}

/// Slots are reused when people leave, so one can hold several names over a demo.
#[derive(Clone, Debug, Default)]
pub struct Slot {
    pub names: Vec<SlotName>,
}

#[derive(Clone, Debug, Default)]
pub struct Kill {
    pub tick: u32,
    pub victim: Option<usize>,
    pub attacker: Option<usize>,
    pub victim_name: String,
    pub attacker_name: String,
    pub assister_name: String,
    pub victim_team: u8,
    pub attacker_team: u8,
    pub weapon: String,
    pub headshot: bool,
    pub backstab: bool,
    /// Dead Ringer deaths appear in the feed but nobody actually died.
    pub feigned: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Round {
    pub start_tick: u32,
    pub end_tick: Option<u32>,
    pub winner: u8,
}

/// One hitscan shot, as the server announced it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Shot {
    pub tick: u32,
    pub slot: usize,
    pub team: u8,
    /// Where the bullet left from, which is the shooter's eyes.
    pub origin: [f32; 3],
    pub pitch: f32,
    pub yaw: f32,
    pub crit: bool,
}

/// A sound the server sent the recorder.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DemoSound {
    pub tick: u32,
    /// Index into `Recording::sound_names`.
    pub sound: u16,
    /// The entity it came from, and the player slot when that's a player.
    pub entity: u16,
    pub slot: Option<usize>,
    pub flags: u16,
    pub channel: u8,
    pub volume: f32,
    /// How far it carries, in decibels (0: everywhere).
    pub level: u16,
    /// 100 is its own pitch.
    pub pitch: u8,
    pub origin: [f32; 3],
}

/// An explosion. TF2 plays its sound itself, so the demo only has where it was.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Explosion {
    pub tick: u32,
    pub origin: [f32; 3],
}

/// TF2's player animation events (PlayerAnimEvent_t) the player uses.
pub const ANIM_ATTACK_PRIMARY: u8 = 0;
pub const ANIM_ATTACK_SECONDARY: u8 = 1;
pub const ANIM_RELOAD: u8 = 3;
pub const ANIM_RELOAD_LOOP: u8 = 4;
pub const ANIM_RELOAD_END: u8 = 5;
pub const ANIM_JUMP: u8 = 6;
pub const ANIM_DOUBLE_JUMP: u8 = 15;

/// A player animation event: shooting, reloading, jumping.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AnimEvent {
    pub tick: u32,
    pub slot: usize,
    pub event: u8,
}

/// Damage one player did to another (player_hurt).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Hurt {
    pub tick: u32,
    pub victim: Option<usize>,
    pub attacker: Option<usize>,
    pub damage: u16,
    /// Health left after the hit.
    pub health: u16,
    pub crit: bool,
    pub mini_crit: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Chat {
    pub tick: u32,
    pub from: String,
    pub text: String,
    pub team_only: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Recording {
    pub map: String,
    pub server: String,
    pub recorded_by: String,
    pub sky_name: String,
    pub tick_interval: f32,
    /// Set when the file ended early but the part before that was usable.
    pub warning: Option<String>,
    /// Slot of the player who recorded this demo, if it wasn't SourceTV.
    pub recorder_slot: Option<usize>,
    pub slots: Vec<Slot>,
    pub kills: Vec<Kill>,
    pub rounds: Vec<Round>,
    pub chat: Vec<Chat>,
    /// Hitscan shots, in order.
    pub shots: Vec<Shot>,
    /// Every time someone got hurt, in order.
    pub hurts: Vec<Hurt>,
    /// Shooting, reloading and jumping, in order.
    pub anims: Vec<AnimEvent>,
    /// The demo's sound files (its sound precache), as paths under `sound/`.
    pub sound_names: Vec<String>,
    /// Every sound the server sent, in order.
    pub sounds: Vec<DemoSound>,
    pub explosions: Vec<Explosion>,
    /// Demo tick of each sampled frame, ascending.
    pub ticks: Vec<u32>,
    /// Frame-major: `players[frame * slot_count + slot]`.
    pub players: Vec<PlayerFrame>,
    pub slot_count: usize,
    pub views: Vec<ViewFrame>,
    /// Frame `f` owns `objects[object_offsets[f]..object_offsets[f + 1]]`.
    pub object_offsets: Vec<u32>,
    pub objects: Vec<ObjectFrame>,
    /// Frame `f` owns `dots[dot_offsets[f]..dot_offsets[f + 1]]`.
    pub dot_offsets: Vec<u32>,
    /// Scoped Snipers' laser dots.
    pub dots: Vec<SniperDot>,
    /// Model files the demo uses, starting with an empty entry for "no model".
    pub models: Vec<String>,
    pub props: Vec<PropTrack>,
    /// Doors and other moving parts of the map, one track per brush model.
    pub movers: Vec<MoverTrack>,
}

impl Recording {
    pub fn first_tick(&self) -> u32 {
        self.ticks.first().copied().unwrap_or(0)
    }

    pub fn last_tick(&self) -> u32 {
        self.ticks.last().copied().unwrap_or(0)
    }

    pub fn seconds_between(&self, from: f64, to: f64) -> f64 {
        (to - from) * f64::from(self.tick_interval)
    }

    /// The last frame at or before a tick.
    pub fn frame_at(&self, tick: f64) -> usize {
        let after = self.ticks.partition_point(|&t| f64::from(t) <= tick);
        after.saturating_sub(1)
    }

    pub fn player(&self, frame: usize, slot: usize) -> &PlayerFrame {
        &self.players[frame * self.slot_count + slot]
    }

    /// The sniper dots in a frame. Older recordings without them have none.
    pub fn dots_at(&self, frame: usize) -> &[SniperDot] {
        match (self.dot_offsets.get(frame), self.dot_offsets.get(frame + 1)) {
            (Some(&start), Some(&end)) => &self.dots[start as usize..end as usize],
            _ => &[],
        }
    }

    /// A player's sniper dot in a frame.
    pub fn dot_of(&self, frame: usize, slot: usize) -> Option<&SniperDot> {
        self.dots_at(frame).iter().find(|dot| usize::from(dot.slot) == slot)
    }

    pub fn objects_at(&self, frame: usize) -> &[ObjectFrame] {
        let start = self.object_offsets[frame] as usize;
        let end = self.object_offsets[frame + 1] as usize;
        &self.objects[start..end]
    }

    pub fn name_at(&self, slot: usize, tick: f64) -> &str {
        let names = &self.slots[slot].names;
        names
            .iter()
            .rev()
            .find(|entry| f64::from(entry.from_tick) <= tick)
            .or(names.first())
            .map_or("Unknown player", |entry| entry.name.as_str())
    }

    /// The SteamID64 of whoever held a slot at a tick. Bots have none (zero).
    pub fn steam_id_at(&self, slot: usize, tick: f64) -> u64 {
        let names = &self.slots[slot].names;
        names
            .iter()
            .rev()
            .find(|entry| f64::from(entry.from_tick) <= tick)
            .or(names.first())
            .map_or(0, |entry| entry.steam_id)
    }

    /// The slot holding a Steam account at a tick.
    pub fn slot_of_steam_id(&self, steam_id: u64, tick: f64) -> Option<usize> {
        let held_at = |slot: &Slot| {
            slot.names
                .iter()
                .rev()
                .find(|entry| f64::from(entry.from_tick) <= tick)
                .is_some_and(|entry| entry.steam_id == steam_id)
        };
        self.slots.iter().position(held_at).or_else(|| {
            self.slots
                .iter()
                .position(|slot| slot.names.iter().any(|entry| entry.steam_id == steam_id))
        })
    }
}

/// A scoped Sniper's laser dot: where the server's aim trace for them ends. Its place
/// is sent to 1/32 of a unit, so the direction from their eyes to it is their aim far
/// more exactly than the eye angles the demo stores for other players (0.35° of yaw,
/// 0.7° of pitch), which hide the small flicks of a low FOV aimbot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SniperDot {
    pub slot: u8,
    pub team: u8,
    pub position: [f32; 3],
    /// The aim was taken from this dot: it moved recently and agrees with the eye angles.
    pub precise: bool,
}

/// Each class's usual model, `models/player/<name>.mdl`, by class number.
const CLASS_MODEL_NAMES: [&str; 10] = ["", "scout", "sniper", "soldier", "demo", "medic", "heavy", "pyro", "spy", "engineer"];

/// A dot that's further than this from the eye angles' aim is stale or wrong.
const DOT_AGREE_DEGREES: f32 = 4.0;
/// A dot that hasn't moved for this long while its owner aims elsewhere is stale.
const DOT_STALE_TICKS: u32 = 33;

/// The pitch and yaw (TF2's way: pitch positive looking down) from an eye to a point.
pub fn angles_towards(eye: Vec3, point: Vec3) -> Option<(f32, f32)> {
    let to = point - eye;
    let flat = to.truncate().length();
    if to.length() < 1.0 {
        return None;
    }
    Some(((-to.z).atan2(flat).to_degrees(), to.y.atan2(to.x).to_degrees()))
}

/// How far apart two aims are, in degrees.
fn aim_difference((pitch_a, yaw_a): (f32, f32), (pitch_b, yaw_b): (f32, f32)) -> f32 {
    let wrap = |angle: f32| (angle + 540.0).rem_euclid(360.0) - 180.0;
    let normal = |pitch: f32| if pitch > 180.0 { pitch - 360.0 } else { pitch };
    let pitch = normal(pitch_a) - normal(pitch_b);
    let yaw = wrap(yaw_a - yaw_b) * normal(pitch_a).to_radians().cos().abs();
    pitch.hypot(yaw)
}

/// `GameStateAnalyser` plus what it doesn't track: chat, the recording player's
/// camera, and which entities dropped out of that player's view.
#[derive(Default)]
struct Analyser {
    game: GameStateAnalyser,
    chat: Vec<ChatMessage>,
    out_of_view: HashSet<EntityId>,
    view: ViewFrame,
    recorder_entity: Option<EntityId>,
    sky_name: String,
    /// The server's numbered list of model files.
    precache: Vec<String>,
    entities: HashMap<EntityId, Entity>,
    view_models: HashSet<EntityId>,
    props: HashSet<EntityId>,
    movers: HashSet<EntityId>,
    projectiles: HashSet<EntityId>,
    sniper_dots: HashSet<EntityId>,
    class_kinds: HashMap<usize, EntityKind>,
    fields: HashMap<SendPropIdentifier, Field>,
    /// Shots seen so far: tick, shooter entity, origin, pitch, yaw, crit.
    shots: Vec<(u32, EntityId, [f32; 3], f32, f32, bool)>,
    /// Player animation events seen so far: tick, player entity, event.
    anims: Vec<(u32, EntityId, u8)>,
    /// The sound precache, as the demo names its files.
    sound_names: Vec<String>,
    sounds: Vec<(u32, super::demo_sounds::SoundInfo)>,
    explosions: Vec<(u32, [f32; 3])>,
}

const BULLETS_PLAYER: SendPropIdentifier = SendPropIdentifier::new("DT_TEFireBullets", "m_iPlayer");
const BULLETS_ORIGIN: SendPropIdentifier = SendPropIdentifier::new("DT_TEFireBullets", "m_vecOrigin");
const BULLETS_PITCH: SendPropIdentifier = SendPropIdentifier::new("DT_TEFireBullets", "m_vecAngles[0]");
const BULLETS_YAW: SendPropIdentifier = SendPropIdentifier::new("DT_TEFireBullets", "m_vecAngles[1]");
const BULLETS_CRIT: SendPropIdentifier = SendPropIdentifier::new("DT_TEFireBullets", "m_bCritical");
const ANIM_PLAYER: SendPropIdentifier = SendPropIdentifier::new("DT_TEPlayerAnimEvent", "m_hPlayer");
const ANIM_EVENT: SendPropIdentifier = SendPropIdentifier::new("DT_TEPlayerAnimEvent", "m_iEvent");

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum EntityKind {
    #[default]
    Other,
    ViewModel,
    Prop,
    /// A moving part of the map made of its brushes, like a door.
    Mover,
    Projectile(ObjectKind),
    SniperDot,
}

fn kind_of_class(name: &str) -> EntityKind {
    if name.contains("ViewModel") {
        EntityKind::ViewModel
    } else if name == "CSniperDot" {
        EntityKind::SniperDot
    } else if PROP_CLASSES.contains(&name) {
        EntityKind::Prop
    } else if MOVER_CLASSES.contains(&name) {
        EntityKind::Mover
    } else if let Some(kind) = ObjectKind::of_projectile_class(name) {
        EntityKind::Projectile(kind)
    } else {
        EntityKind::Other
    }
}

/// What's known about one networked entity.
#[derive(Clone, Debug)]
struct Entity {
    kind: EntityKind,
    model: u32,
    /// Weapons are networked with their first-person model, so this is the one others see.
    world_model: u32,
    origin: Option<Vec3>,
    angles: Vec3,
    parent: i64,
    owner: i64,
    body: i64,
    skin: i64,
    sequence: i64,
    parity: i64,
    effects: i64,
    conditions: i64,
    active_weapon: Option<i64>,
    flags: Option<i64>,
    /// How far through its sequence the server says it is, when it says.
    cycle: Option<f32>,
    pose_parameters: [f32; 2],
    playback_rate: f32,
    /// How far a building is built, 0 to 1.
    constructed: Option<f32>,
    /// A sentry's bullets left and the handle of what it's shooting at.
    shells: Option<i64>,
    enemy: i64,
    team: u8,
    crit: bool,
    /// A pipebomb's type: 0 a pipe, 1 and 2 stickies, 3 a Loose Cannon ball.
    pipe_type: i64,
    /// The tick its origin last changed.
    moved: u32,
    /// How big it's drawn (`m_flModelScale`).
    scale: f32,
    /// A player's own model instead of their class's (`m_iszCustomModel`, Mann vs.
    /// Machine robots), as an index into the model precache. Zero for none.
    custom_model: u32,
    /// Who threw a grenade, pipe or sticky (`m_hThrower`), and what launched it.
    thrower: i64,
    launcher: i64,
}

impl Default for Entity {
    fn default() -> Self {
        Self {
            kind: EntityKind::Other,
            model: 0,
            world_model: 0,
            origin: None,
            angles: Vec3::ZERO,
            parent: -1,
            owner: -1,
            body: 0,
            skin: 0,
            sequence: 0,
            parity: 0,
            effects: 0,
            conditions: 0,
            active_weapon: None,
            flags: None,
            cycle: None,
            pose_parameters: [0.5, 0.5],
            playback_rate: 1.0,
            constructed: None,
            shells: None,
            enemy: -1,
            team: 0,
            crit: false,
            pipe_type: 0,
            moved: 0,
            scale: 1.0,
            custom_model: 0,
            thrower: -1,
            launcher: -1,
        }
    }
}

fn handle_target(handle: i64) -> Option<EntityId> {
    let index = handle & HANDLE_INDEX_MASK;
    (handle >= 0 && index != HANDLE_INDEX_MASK).then(|| EntityId::from(index as u32))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Ignored,
    Cycle,
    PoseParameter(u8),
    PlaybackRate,
    Constructed,
    Shells,
    Enemy,
    ModelIndex,
    WorldModelIndex,
    ActiveWeapon,
    MovementFlags,
    Origin,
    Angles,
    Parent,
    Owner,
    Body,
    Skin,
    Sequence,
    Parity,
    Effects,
    Conditions,
    Team,
    Critical,
    PipeType,
    ModelScale,
    CustomModel,
    Thrower,
    Launcher,
}

fn field_of(cache: &mut HashMap<SendPropIdentifier, Field>, identifier: SendPropIdentifier) -> Field {
    *cache.entry(identifier).or_insert_with(|| {
        let Some((table, prop)) = identifier.names() else {
            return Field::Ignored;
        };
        match (table.as_str(), prop.as_str()) {
            (_, "m_nModelIndex") => Field::ModelIndex,
            (_, "m_iWorldModelIndex") => Field::WorldModelIndex,
            (_, "m_hActiveWeapon") => Field::ActiveWeapon,
            (_, "m_fFlags") => Field::MovementFlags,
            // Rockets and grenades send their place in their own tables, for precision.
            ("DT_BaseEntity" | "DT_TFBaseRocket" | "DT_TFWeaponBaseGrenadeProj", "m_vecOrigin") => Field::Origin,
            ("DT_BaseEntity" | "DT_TFBaseRocket" | "DT_TFWeaponBaseGrenadeProj", "m_angRotation") => Field::Angles,
            ("DT_BaseEntity", "m_iTeamNum") => Field::Team,
            (table, "m_bCritical") if table.starts_with("DT_TFProjectile_") || table == "DT_TFWeaponBaseGrenadeProj" => {
                Field::Critical
            }
            ("DT_TFProjectile_Pipebomb", "m_iType") => Field::PipeType,
            (_, "moveparent") => Field::Parent,
            (_, "m_hOwnerEntity") | ("DT_BaseViewModel", "m_hOwner") => Field::Owner,
            (_, "m_hThrower") => Field::Thrower,
            (_, "m_hLauncher") => Field::Launcher,
            (_, "m_nBody") => Field::Body,
            (_, "m_nSkin") => Field::Skin,
            (_, "m_nSequence") => Field::Sequence,
            (_, "m_nAnimationParity") => Field::Parity,
            (_, "m_fEffects") => Field::Effects,
            (_, "m_nPlayerCond" | "_condition_bits") => Field::Conditions,
            ("DT_ServerAnimationData", "m_flCycle") => Field::Cycle,
            ("m_flPoseParameter", index) => match index.parse::<u8>() {
                Ok(index) if index < 2 => Field::PoseParameter(index),
                _ => Field::Ignored,
            },
            ("DT_BaseAnimating", "m_flPlaybackRate") => Field::PlaybackRate,
            ("DT_BaseAnimating", "m_flModelScale") => Field::ModelScale,
            (_, "m_iszCustomModel") => Field::CustomModel,
            ("DT_BaseObject", "m_flPercentageConstructed") => Field::Constructed,
            ("DT_ObjectSentrygun", "m_iAmmoShells") => Field::Shells,
            ("DT_ObjectSentrygun", "m_hEnemy") => Field::Enemy,
            _ => Field::Ignored,
        }
    })
}

fn quat_from_angles(angles: Vec3) -> Quat {
    Quat::from_rotation_z(angles.y.to_radians())
        * Quat::from_rotation_y(angles.x.to_radians())
        * Quat::from_rotation_x(angles.z.to_radians())
}

impl Analyser {
    fn state(&self) -> &GameState {
        &self.game.state
    }

    fn forget(&mut self, id: EntityId) {
        self.entities.remove(&id);
        self.view_models.remove(&id);
        self.props.remove(&id);
        self.movers.remove(&id);
        self.projectiles.remove(&id);
        self.sniper_dots.remove(&id);
    }

    /// Where an entity is in the world, following whatever it's attached to.
    fn world_transform(&self, id: EntityId, depth: u32) -> Option<(Vec3, Quat)> {
        if let Some(player) = self.state().players.iter().find(|player| player.entity == id) {
            let position = Vec3::new(player.position.x, player.position.y, player.position.z);
            return Some((position, Quat::from_rotation_z(player.view_angle.to_radians())));
        }
        let entity = self.entities.get(&id)?;
        let origin = entity.origin?;
        let rotation = quat_from_angles(entity.angles);
        if depth < 8 {
            if let Some(parent) = handle_target(entity.parent) {
                if let Some((position, turn)) = self.world_transform(parent, depth + 1) {
                    return Some((position + turn * origin, turn * rotation));
                }
            }
        }
        Some((origin, rotation))
    }
}

impl MessageHandler for Analyser {
    type Output = Self;

    fn does_handle(message_type: MessageType) -> bool {
        GameStateAnalyser::does_handle(message_type)
            || message_type == MessageType::UserMessage
            || message_type == MessageType::TempEntities
            || message_type == MessageType::ParseSounds
    }

    fn handle_header(&mut self, header: &Header) {
        self.game.handle_header(header);
    }

    fn handle_message(&mut self, message: &Message, tick: DemoTick, parser_state: &ParserState) {
        match message {
            Message::UserMessage(UserMessage::SayText2(text)) => {
                self.chat.push(ChatMessage::from_message(text, tick));
            }
            Message::UserMessage(_) => {}
            _ => self.game.handle_message(message, tick, parser_state),
        }

        match message {
            Message::ParseSounds(sounds) => {
                for sound in super::demo_sounds::read_sounds(sounds).unwrap_or_default() {
                    if !sound.sentence {
                        self.sounds.push((u32::from(tick), sound));
                    }
                }
            }
            Message::TempEntities(effects) => {
                // Each animation event only sends what changed since the previous one of its kind.
                let (mut anim_class, mut anim_player, mut anim_event) = (None, None, 0i64);
                for effect in &effects.events {
                    let class = parser_state.server_classes.get(usize::from(effect.class_id));
                    if class.is_some_and(|class| class.name.as_str() == "CTEPlayerAnimEvent") {
                        if anim_class != Some(effect.class_id) {
                            anim_class = Some(effect.class_id);
                            anim_player = None;
                            anim_event = 0;
                        }
                        for prop in &effect.props {
                            match prop.identifier {
                                ANIM_PLAYER => anim_player = i64::try_from(&prop.value).ok().and_then(handle_target),
                                ANIM_EVENT => anim_event = i64::try_from(&prop.value).unwrap_or(0),
                                _ => {}
                            }
                        }
                        if let (Some(player), Ok(event)) = (anim_player, u8::try_from(anim_event)) {
                            self.anims.push((u32::from(tick), player, event));
                        }
                        continue;
                    }
                    if class.is_some_and(|class| class.name.as_str() == "CTETFExplosion") {
                        let mut origin = [None; 3];
                        for prop in &effect.props {
                            let Some((_, name)) = prop.identifier.names() else { continue };
                            let axis = match name.as_str() {
                                "m_vecOrigin[0]" => 0,
                                "m_vecOrigin[1]" => 1,
                                "m_vecOrigin[2]" => 2,
                                _ => continue,
                            };
                            origin[axis] = f32::try_from(&prop.value).ok();
                        }
                        if let [Some(x), Some(y), Some(z)] = origin {
                            self.explosions.push((u32::from(tick), [x, y, z]));
                        }
                        continue;
                    }
                    if class.is_none_or(|class| class.name.as_str() != "CTEFireBullets") {
                        continue;
                    }
                    let (mut shooter, mut origin, mut pitch, mut yaw, mut crit) = (None, None, 0.0, 0.0, false);
                    for prop in &effect.props {
                        match prop.identifier {
                            // The server counts players from zero here, one below their entity number.
                            BULLETS_PLAYER => shooter = i64::try_from(&prop.value).ok().map(|index| index + 1),
                            BULLETS_ORIGIN => origin = Vector::try_from(&prop.value).ok(),
                            BULLETS_PITCH => pitch = f32::try_from(&prop.value).unwrap_or(0.0),
                            BULLETS_YAW => yaw = f32::try_from(&prop.value).unwrap_or(0.0),
                            BULLETS_CRIT => crit = i64::try_from(&prop.value).is_ok_and(|value| value != 0),
                            _ => {}
                        }
                    }
                    if let (Some(shooter), Some(origin)) = (shooter, origin) {
                        self.shots.push((
                            u32::from(tick),
                            EntityId::from(shooter.max(0) as u32),
                            [origin.x, origin.y, origin.z],
                            pitch,
                            yaw,
                            crit,
                        ));
                    }
                }
            }
            Message::ServerInfo(info) => {
                self.sky_name = info.skybox.clone();
                if !info.stv {
                    self.recorder_entity = Some(EntityId::from(u32::from(info.player_slot) + 1));
                }
            }
            Message::PacketEntities(packet) => {
                for entity in &packet.entities {
                    let id = entity.entity_index;
                    match entity.update_type {
                        UpdateType::Leave => {
                            self.out_of_view.insert(id);
                            continue;
                        }
                        UpdateType::Delete => {
                            self.forget(id);
                            continue;
                        }
                        UpdateType::Enter => {
                            self.out_of_view.remove(&id);
                            let class = usize::from(entity.server_class);
                            let kind = *self.class_kinds.entry(class).or_insert_with(|| {
                                parser_state
                                    .server_classes
                                    .get(class)
                                    .map_or(EntityKind::Other, |server| kind_of_class(server.name.as_str()))
                            });
                            let known = self.entities.get(&id).is_some_and(|known| known.kind == kind);
                            if !known {
                                self.forget(id);
                                self.entities.insert(id, Entity { kind, ..Entity::default() });
                                match kind {
                                    EntityKind::ViewModel => self.view_models.insert(id),
                                    EntityKind::Prop => self.props.insert(id),
                                    EntityKind::Mover => self.movers.insert(id),
                                    EntityKind::Projectile(_) => self.projectiles.insert(id),
                                    EntityKind::SniperDot => self.sniper_dots.insert(id),
                                    EntityKind::Other => false,
                                };
                            }
                        }
                        _ => {}
                    }
                    for prop in entity.props(parser_state) {
                        let field = field_of(&mut self.fields, prop.identifier);
                        if field == Field::Ignored {
                            continue;
                        }
                        let state = self.entities.entry(id).or_default();
                        if field == Field::CustomModel {
                            if let SendPropValue::String(path) = &prop.value {
                                let wanted = path.replace('\\', "/").to_ascii_lowercase();
                                state.custom_model = if wanted.is_empty() {
                                    0
                                } else {
                                    self.precache
                                        .iter()
                                        .position(|known| known.replace('\\', "/").eq_ignore_ascii_case(&wanted))
                                        .map_or(0, |index| index as u32)
                                };
                            }
                            continue;
                        }
                        if matches!(field, Field::Origin | Field::Angles) {
                            if let Ok(vector) = Vector::try_from(&prop.value) {
                                let value = Vec3::new(vector.x, vector.y, vector.z);
                                if field == Field::Origin {
                                    if state.origin != Some(value) {
                                        state.moved = u32::from(tick);
                                    }
                                    state.origin = Some(value);
                                } else {
                                    state.angles = value;
                                }
                            }
                            continue;
                        }
                        if matches!(field, Field::Cycle | Field::PoseParameter(_) | Field::PlaybackRate | Field::Constructed | Field::ModelScale) {
                            let Ok(value) = f32::try_from(&prop.value) else { continue };
                            match field {
                                Field::Cycle => state.cycle = Some(value),
                                Field::PoseParameter(index) => state.pose_parameters[usize::from(index)] = value,
                                Field::PlaybackRate => state.playback_rate = value,
                                Field::ModelScale => state.scale = value,
                                _ => state.constructed = Some(value),
                            }
                            continue;
                        }
                        let Ok(value) = i64::try_from(&prop.value) else { continue };
                        match field {
                            Field::ModelIndex => state.model = value.max(0) as u32,
                            Field::WorldModelIndex => state.world_model = value.max(0) as u32,
                            Field::ActiveWeapon => state.active_weapon = Some(value),
                            Field::MovementFlags => state.flags = Some(value),
                            Field::Parent => state.parent = value,
                            Field::Owner => state.owner = value,
                            Field::Thrower => state.thrower = value,
                            Field::Launcher => state.launcher = value,
                            Field::Body => state.body = value,
                            Field::Skin => state.skin = value,
                            Field::Sequence => state.sequence = value,
                            Field::Parity => state.parity = value,
                            Field::Effects => state.effects = value,
                            Field::Conditions => state.conditions = value,
                            Field::Shells => state.shells = Some(value),
                            Field::Enemy => state.enemy = value,
                            Field::Team => state.team = value.clamp(0, 255) as u8,
                            Field::Critical => state.crit = value != 0,
                            Field::PipeType => state.pipe_type = value,
                            Field::Ignored
                            | Field::Origin
                            | Field::Angles
                            | Field::Cycle
                            | Field::PoseParameter(_)
                            | Field::PlaybackRate
                            | Field::Constructed
                            | Field::ModelScale
                            | Field::CustomModel => {}
                        }
                    }
                }
                for id in &packet.removed_entities {
                    self.forget(*id);
                }
            }
            _ => {}
        }
    }

    fn handle_string_entry(
        &mut self,
        table: &str,
        index: usize,
        entry: &StringTableEntry,
        parser_state: &ParserState,
    ) {
        if table == "modelprecache" {
            if self.precache.len() <= index {
                self.precache.resize(index + 1, String::new());
            }
            self.precache[index] = entry.text().replace('\\', "/").to_ascii_lowercase();
        }
        if table == "soundprecache" {
            if self.sound_names.len() <= index {
                self.sound_names.resize(index + 1, String::new());
            }
            self.sound_names[index] = entry.text().to_string();
        }
        self.game
            .handle_string_entry(table, index, entry, parser_state);
    }

    fn handle_data_tables(
        &mut self,
        tables: &[ParseSendTable],
        server_classes: &[ServerClass],
        parser_state: &ParserState,
    ) {
        self.game
            .handle_data_tables(tables, server_classes, parser_state);
    }

    fn handle_packet_meta(
        &mut self,
        tick: DemoTick,
        meta: &MessagePacketMeta,
        parser_state: &ParserState,
    ) {
        // The demo stores the camera twice and flags which copy is in use,
        // for instance while the recording player is dead and spectating.
        let [first, second] = &meta.view_angles;
        let origin = if meta.flags & DEMO_USE_ORIGIN2 != 0 { second.origin } else { first.origin };
        let angles = if meta.flags & DEMO_USE_ANGLES2 != 0 { second.angles } else { first.angles };
        self.view = ViewFrame {
            origin: [origin.x, origin.y, origin.z],
            pitch: angles.x,
            yaw: angles.y,
            roll: angles.z,
            ..ViewFrame::default()
        };
        self.game.handle_packet_meta(tick, meta, parser_state);
    }

    fn into_output(self, _state: &ParserState) -> Self::Output {
        self
    }
}

impl BorrowMessageHandler for Analyser {
    fn borrow_output(&self, _state: &ParserState) -> &Self::Output {
        self
    }
}

fn clamp_health(value: u16) -> i16 {
    value.min(i16::MAX as u16) as i16
}

fn check_signature(bytes: &[u8]) -> Result<(), String> {
    match bytes.get(..8) {
        Some(b"HL2DEMO\0") => Ok(()),
        Some(b"PBDEMS2\0") => {
            Err("This is a Source 2 demo. Only Team Fortress 2 demos can be played.".into())
        }
        _ => Err("This file isn't a Source demo.".into()),
    }
}

fn tick_gap(total_ticks: u32) -> u32 {
    total_ticks.div_ceil(MAX_FRAMES).max(1)
}

/// `[U:1:123]` to a 64 bit Steam ID, the form detections use.
fn steam_id_to_u64(text: &str) -> u64 {
    text.trim_matches(|c| c == '[' || c == ']')
        .rsplit(':')
        .next()
        .and_then(|account| account.parse::<u64>().ok())
        .map_or(0, |account| STEAM_ID64_BASE + account)
}

#[derive(Clone, Default)]
struct Identity {
    slot: usize,
    name: String,
    team: u8,
}

#[derive(Default)]
struct Recorder {
    recording: Recording,
    /// Slots only ever grow, so each frame keeps the length it had at the time.
    rows: Vec<PlayerFrame>,
    row_starts: Vec<usize>,
    people: HashMap<u16, Identity>,
    events_seen: usize,
    chat_seen: usize,
    shots_seen: usize,
    sounds_seen: usize,
    explosions_seen: usize,
    anims_seen: usize,
    /// Server model numbers to positions in `Recording::models`.
    model_ids: HashMap<u32, u16>,
    /// Map objects being followed, and the track each one is writing to.
    open_props: HashMap<EntityId, usize>,
    /// Each moving brush's track in `Recording::movers`, and the tick it was last seen.
    open_movers: HashMap<u16, (usize, u32)>,
    /// The arms animation last seen: sequence, restart counter, and starting tick.
    view_sequence: (i64, i64, u32),
}

/// Remembers a model file the first time it's seen, returning its position in the list.
fn intern_index(recording: &mut Recording, ids: &mut HashMap<u32, u16>, analyser: &Analyser, index: u32) -> u16 {
    if index == 0 {
        return 0;
    }
    if let Some(&id) = ids.get(&index) {
        return id;
    }
    let Some(path) = analyser.precache.get(index as usize).filter(|path| path.ends_with(".mdl")) else {
        return 0;
    };
    if recording.models.is_empty() {
        recording.models.push(String::new());
    }
    let id = match recording.models.iter().position(|known| known == path) {
        Some(existing) => existing as u16,
        None => {
            recording.models.push(path.clone());
            (recording.models.len() - 1) as u16
        }
    };
    ids.insert(index, id);
    id
}

/// The brush model number of a map part's model, `*8` being 8. None for model files.
fn brush_number(path: &str) -> Option<u16> {
    path.strip_prefix('*')?.parse().ok()
}

/// The model others see an entity with.
fn intern_model(recording: &mut Recording, ids: &mut HashMap<u32, u16>, analyser: &Analyser, entity: EntityId) -> u16 {
    let Some(state) = analyser.entities.get(&entity) else { return 0 };
    let index = if state.world_model > 0 { state.world_model } else { state.model };
    intern_index(recording, ids, analyser, index)
}

impl Recorder {
    /// Throws away the frames recorded so far. Demos start with a few setup packets
    /// stamped with the server's tick before gameplay counts up again from zero.
    fn restart(&mut self) {
        let recording = &mut self.recording;
        recording.ticks.clear();
        recording.views.clear();
        recording.object_offsets.clear();
        recording.objects.clear();
        recording.dot_offsets.clear();
        recording.dots.clear();
        recording.props.clear();
        recording.movers.clear();
        self.rows.clear();
        self.row_starts.clear();
        self.open_props.clear();
        self.open_movers.clear();
        self.view_sequence = (0, 0, 0);
    }

    fn identify_players(&mut self, tick: u32, state: &GameState) {
        for (slot, player) in state.players.iter().enumerate() {
            if self.recording.slots.len() <= slot {
                self.recording.slots.resize_with(slot + 1, Slot::default);
            }
            let Some(info) = &player.info else { continue };

            let steam_id = steam_id_to_u64(&info.steam_id);
            let names = &mut self.recording.slots[slot].names;
            let changed = names
                .last()
                .is_none_or(|last| last.name != info.name || last.steam_id != steam_id);
            if changed {
                names.push(SlotName {
                    from_tick: tick,
                    name: info.name.clone(),
                    steam_id,
                });
            }

            self.people.insert(
                u16::from(info.user_id),
                Identity {
                    slot,
                    name: info.name.clone(),
                    team: player.team as u8,
                },
            );
        }
    }

    fn record_frame(&mut self, tick: u32, analyser: &Analyser) {
        let state = analyser.state();
        let Recorder {
            recording,
            rows,
            row_starts,
            model_ids,
            open_props,
            open_movers,
            view_sequence,
            ..
        } = self;
        recording.ticks.push(tick);

        // The recording player's first-person arms and what they're doing.
        let mut view = analyser.view;
        let arms = analyser
            .view_models
            .iter()
            .filter_map(|id| analyser.entities.get(id))
            .filter(|state| handle_target(state.owner) == analyser.recorder_entity)
            .find(|state| state.model > 0 && state.effects & EF_NODRAW == 0);
        if let Some(arms) = arms {
            view.arms = intern_index(recording, model_ids, analyser, arms.model);
            if (arms.sequence, arms.parity) != (view_sequence.0, view_sequence.1) {
                *view_sequence = (arms.sequence, arms.parity, tick);
            }
            view.sequence = arms.sequence.clamp(0, i64::from(u16::MAX)) as u16;
            view.sequence_start = view_sequence.2;
        }
        recording.views.push(view);
        row_starts.push(rows.len());
        recording.object_offsets.push(recording.objects.len() as u32);
        recording.dot_offsets.push(recording.dots.len() as u32);

        // Each owner's dot: the one that moved last, as a stale one can linger.
        let mut dots: HashMap<EntityId, (u32, Vec3)> = HashMap::new();
        for id in &analyser.sniper_dots {
            if analyser.out_of_view.contains(id) {
                continue;
            }
            let Some(dot) = analyser.entities.get(id) else { continue };
            let (Some(owner), Some(origin)) = (handle_target(dot.owner), dot.origin) else { continue };
            if dots.get(&owner).is_none_or(|(moved, _)| dot.moved > *moved) {
                dots.insert(owner, (dot.moved, origin));
            }
        }

        for player in &state.players {
            let mut flags = 0;
            if player.state == PlayerState::Alive && player.health > 0 {
                flags |= PLAYER_ALIVE;
            }
            if analyser.out_of_view.contains(&player.entity) {
                flags |= PLAYER_OUT_OF_VIEW;
            }
            let state = analyser.entities.get(&player.entity);
            match state.and_then(|state| state.flags) {
                Some(movement) => {
                    if movement & FL_DUCKING != 0 {
                        flags |= PLAYER_DUCKING;
                    }
                    if movement & FL_ONGROUND != 0 {
                        flags |= PLAYER_ON_GROUND;
                    }
                    if movement & FL_INWATER != 0 {
                        flags |= PLAYER_IN_WATER;
                    }
                }
                None => {
                    flags |= PLAYER_ON_GROUND;
                    if player.bounds.max.z < STANDING_HULL_HEIGHT {
                        flags |= PLAYER_DUCKING;
                    }
                }
            }

            if state.is_some_and(|state| state.conditions & COND_ZOOMED != 0) {
                flags |= PLAYER_ZOOMED;
            }
            if state.is_some_and(|state| state.conditions & COND_AIMING != 0) {
                flags |= PLAYER_AIMING;
            }
            if state.is_some_and(|state| state.conditions & COND_STEALTHED != 0) {
                flags |= PLAYER_CLOAKED;
            }

            let (mut weapon_slot, mut weapon_model) = (WEAPON_UNKNOWN, 0);
            if let Some(active) = state.and_then(|state| state.active_weapon) {
                let index = active & HANDLE_INDEX_MASK;
                if let Some(slot) = player.weapons.iter().position(|weapon| weapon.0 & HANDLE_INDEX_MASK == index) {
                    weapon_slot = slot as u8;
                }
                weapon_model = intern_model(recording, model_ids, analyser, EntityId::from(index as u32));
            }

            let mut aim = (player.pitch_angle, player.view_angle);
            let dot = dots.get(&player.entity).filter(|_| flags & PLAYER_ALIVE != 0);
            if let Some(&(moved, position)) = dot {
                let probe = PlayerFrame { class: player.class as u8, flags, ..PlayerFrame::default() };
                let eye = Vec3::new(player.position.x, player.position.y, player.position.z) + Vec3::Z * probe.eye_height();
                let exact = angles_towards(eye, position)
                    .filter(|exact| aim_difference(*exact, aim) < DOT_AGREE_DEGREES)
                    .filter(|_| tick.saturating_sub(moved) < DOT_STALE_TICKS);
                if let Some(exact) = exact {
                    aim = exact;
                }
                recording.dots.push(SniperDot {
                    slot: u8::try_from(rows.len() - row_starts.last().copied().unwrap_or(0)).unwrap_or(u8::MAX),
                    team: player.team as u8,
                    position: position.to_array(),
                    precise: exact.is_some(),
                });
            }

            // A model of their own instead of the class's (Mann vs. Machine robots).
            let custom = state.map_or(0, |state| state.custom_model);
            let body_model = if custom > 0 {
                intern_index(recording, model_ids, analyser, custom)
            } else {
                0
            };
            let body_model = if body_model > 0 { body_model } else { state.filter(|state| state.model > 0).map_or(0, |_| {
                let model = intern_model(recording, model_ids, analyser, player.entity);
                let usual = CLASS_MODEL_NAMES.get(player.class as usize).filter(|name| !name.is_empty());
                let path = &recording.models[usize::from(model)];
                match usual {
                    Some(name) if *path != format!("models/player/{name}.mdl") => model,
                    _ => 0,
                }
            }) };
            let scale = state.map_or(1.0, |state| state.scale);

            rows.push(PlayerFrame {
                body_model,
                scale,
                position: [player.position.x, player.position.y, player.position.z],
                yaw: aim.1,
                pitch: aim.0,
                health: clamp_health(player.health),
                max_health: clamp_health(player.max_health),
                team: player.team as u8,
                class: player.class as u8,
                flags,
                charge: player.charge,
                weapon_slot,
                weapon_model,
                travelled: 0.0,
                ping: player.ping,
            });
        }

        // Map objects: start a track when one appears, add a key when it changes.
        let mut seen = HashSet::new();
        for id in &analyser.props {
            let Some(prop) = analyser.entities.get(id) else { continue };
            if prop.effects & EF_NODRAW != 0 {
                continue;
            }
            let model = intern_model(recording, model_ids, analyser, *id);
            let Some((position, rotation)) = analyser.world_transform(*id, 0) else { continue };
            if model == 0 {
                continue;
            }
            seen.insert(*id);
            let sequence = prop.sequence.clamp(0, i64::from(u16::MAX)) as u16;
            let mut key = PropKey {
                tick,
                position: position.to_array(),
                rotation: rotation.to_array(),
                sequence,
                sequence_start: tick,
                skin: prop.skin.clamp(0, 255) as u8,
                body: prop.body.clamp(0, i64::from(u16::MAX)) as u16,
            };
            let open = open_props
                .get(id)
                .copied()
                .filter(|&track| recording.props[track].model == model);
            let Some(track) = open else {
                open_props.insert(*id, recording.props.len());
                recording.props.push(PropTrack {
                    model,
                    keys: vec![key],
                    until: u32::MAX,
                });
                continue;
            };
            let last = *recording.props[track].keys.last().expect("tracks start with a key");
            if last.sequence == sequence {
                key.sequence_start = last.sequence_start;
            }
            let moved = Vec3::from(last.position).distance(position) > PROP_MOVE_EPSILON;
            let turned = Quat::from_array(last.rotation).dot(rotation).abs() < 1.0 - PROP_TURN_EPSILON;
            let changed = last.sequence != sequence || last.skin != key.skin || last.body != key.body;
            if moved || turned || changed {
                recording.props[track].keys.push(key);
            }
        }
        open_props.retain(|id, track| {
            let still_here = seen.contains(id);
            if !still_here {
                recording.props[*track].until = tick;
            }
            still_here
        });

        // Doors and other moving brushes: one track per brush model, kept for the
        // whole demo, since they never leave the map. One out of view keeps the
        // place it was last seen.
        for id in &analyser.movers {
            if analyser.out_of_view.contains(id) {
                continue;
            }
            let Some(mover) = analyser.entities.get(id) else { continue };
            let Some(brush) = analyser.precache.get(mover.model as usize).and_then(|path| brush_number(path)) else {
                continue;
            };
            let Some((position, rotation)) = analyser.world_transform(*id, 0) else { continue };
            let key = MoverKey { tick, position: position.to_array(), rotation: rotation.to_array() };
            match open_movers.get_mut(&brush) {
                Some((track, last_seen)) => {
                    recording.movers[*track].push(key, *last_seen);
                    *last_seen = tick;
                }
                None => {
                    open_movers.insert(brush, (recording.movers.len(), tick));
                    recording.movers.push(MoverTrack { brush, keys: vec![key] });
                }
            }
        }

        // Our own tracking, not the parser's: it misses projectiles placed by the
        // generic position (the Bison's and Pomson's rings) and some classes entirely
        // (the Sandman's ball, the Wrap Assassin's ornament), and it doesn't follow
        // stickies stuck to something that moves.
        let mut projectiles: Vec<EntityId> = analyser.projectiles.iter().copied().collect();
        projectiles.sort_unstable();
        for id in projectiles {
            if analyser.out_of_view.contains(&id) {
                continue;
            }
            let Some(entity) = analyser.entities.get(&id) else { continue };
            let EntityKind::Projectile(kind) = entity.kind else { continue };
            let Some((position, _)) = analyser.world_transform(id, 0) else { continue };
            let kind = match (kind, entity.pipe_type) {
                (ObjectKind::Pipe, 1 | 2) => ObjectKind::Sticky,
                (ObjectKind::Pipe, 3) => ObjectKind::LooseCannon,
                (kind, _) => kind,
            };
            let model = intern_model(recording, model_ids, analyser, id);
            // Grenades name their thrower, or at least the launcher, which its holder owns.
            let launcher_owner = handle_target(entity.launcher)
                .and_then(|launcher| analyser.entities.get(&launcher))
                .and_then(|launcher| handle_target(launcher.owner));
            let shooter = |id: EntityId| state.players.iter().position(|player| player.entity == id);
            let owner = handle_target(entity.thrower)
                .and_then(shooter)
                .or_else(|| handle_target(entity.owner).and_then(shooter))
                .or_else(|| launcher_owner.and_then(shooter))
                .and_then(|slot| u8::try_from(slot).ok())
                .unwrap_or(NO_OWNER);
            recording.objects.push(ObjectFrame {
                owner,
                id: u32::from(id) as u16,
                kind,
                team: entity.team,
                level: 0,
                crit: entity.crit,
                sapped: false,
                mini: false,
                under_construction: false,
                position: position.to_array(),
                yaw: entity.angles.y,
                pitch: entity.angles.x,
                health: 0,
                max_health: 0,
                model,
                animation: ObjectAnimation::default(),
            });
        }

        for (id, building) in &state.buildings {
            if building.health() == 0 {
                continue; // Picked up, or not networked yet.
            }
            let (kind, under_construction, mini) = match building {
                Building::Sentry(sentry) => (ObjectKind::Sentry, sentry.building, sentry.is_mini),
                Building::Dispenser(dispenser) => (ObjectKind::Dispenser, dispenser.building, false),
                Building::Teleporter(tele) if tele.is_entrance => {
                    (ObjectKind::TeleEntrance, tele.building, false)
                }
                Building::Teleporter(tele) => (ObjectKind::TeleExit, tele.building, false),
                _ => continue,
            };
            let position = building.position();
            let model = intern_model(recording, model_ids, analyser, *id);
            let animation = analyser.entities.get(id).map_or_else(ObjectAnimation::default, |entity| {
                // Without a cycle from the server, a building being built is as far
                // through its build animation as it is built.
                let cycle = entity.cycle.or(if under_construction { entity.constructed } else { None });
                let target = handle_target(entity.enemy)
                    .and_then(|enemy| state.players.iter().position(|player| player.entity == enemy))
                    .and_then(|slot| u8::try_from(slot).ok());
                ObjectAnimation {
                    sequence: entity.sequence.clamp(0, i64::from(u16::MAX)) as u16,
                    cycle,
                    pose: entity.pose_parameters,
                    rate: entity.playback_rate,
                    shells: entity.shells.map_or(-1, |shells| shells.clamp(0, i64::from(i16::MAX)) as i16),
                    target,
                }
            });
            recording.objects.push(ObjectFrame {
                id: u32::from(*id) as u16,
                kind,
                team: building.team() as u8,
                level: building.level(),
                crit: false,
                sapped: building.sapped(),
                mini,
                under_construction,
                position: [position.x, position.y, position.z],
                yaw: building.angle(),
                pitch: 0.0,
                health: clamp_health(building.health()),
                max_health: clamp_health(building.max_health()),
                model,
                animation,
                owner: NO_OWNER,
            });
        }

        for (id, objective) in &state.objectives {
            if let Some(cart) = objective.as_cart() {
                recording.objects.push(ObjectFrame {
                    id: u32::from(*id) as u16,
                    kind: ObjectKind::Cart,
                    team: 0,
                    level: 0,
                    crit: false,
                    sapped: false,
                    mini: false,
                    under_construction: false,
                    position: [cart.position.x, cart.position.y, cart.position.z],
                    yaw: 0.0,
                    pitch: 0.0,
                    health: 0,
                    max_health: 0,
                    model: 0,
                    animation: ObjectAnimation::default(),
                    owner: NO_OWNER,
                });
            }
        }
    }

    fn who(&self, user_id: u16) -> Identity {
        self.people.get(&user_id).cloned().unwrap_or_default()
    }

    fn slot_of(&self, user_id: u16) -> Option<usize> {
        self.people.get(&user_id).map(|person| person.slot)
    }

    fn collect_events(&mut self, analyser: &Analyser) {
        let events = &analyser.state().events;
        for (tick, event) in events.iter().skip(self.events_seen) {
            let tick = u32::from(*tick);
            match event {
                GameEvent::PlayerDeath(death) => {
                    let victim = self.who(death.user_id);
                    let attacker = self.who(death.attacker);
                    let assister = self.who(death.assister);
                    let kill = Kill {
                        tick,
                        victim: self.slot_of(death.user_id),
                        attacker: self.slot_of(death.attacker),
                        victim_name: victim.name,
                        attacker_name: attacker.name,
                        assister_name: assister.name,
                        victim_team: victim.team,
                        attacker_team: attacker.team,
                        weapon: death.weapon.to_string(),
                        headshot: death.custom_kill == CUSTOM_KILL_HEADSHOT,
                        backstab: death.custom_kill == CUSTOM_KILL_BACKSTAB,
                        feigned: death.death_flags & DEATH_FLAG_FEIGNED != 0,
                    };
                    self.recording.kills.push(kill);
                }
                GameEvent::PlayerHurt(hurt) => {
                    self.recording.hurts.push(Hurt {
                        tick,
                        victim: self.slot_of(hurt.user_id),
                        attacker: self.slot_of(hurt.attacker).filter(|_| hurt.attacker != hurt.user_id),
                        damage: hurt.damage_amount,
                        health: hurt.health,
                        crit: hurt.crit,
                        mini_crit: hurt.mini_crit,
                    });
                }
                GameEvent::TeamPlayRoundStart(_) => {
                    if let Some(round) = self.recording.rounds.last_mut() {
                        round.end_tick.get_or_insert(tick);
                    }
                    self.recording.rounds.push(Round {
                        start_tick: tick,
                        ..Round::default()
                    });
                }
                GameEvent::TeamPlayRoundWin(win) => {
                    if self.recording.rounds.is_empty() {
                        let start_tick = self.recording.ticks.first().copied().unwrap_or(tick);
                        self.recording.rounds.push(Round {
                            start_tick,
                            ..Round::default()
                        });
                    }
                    if let Some(round) = self.recording.rounds.last_mut() {
                        round.end_tick = Some(tick);
                        round.winner = win.team;
                    }
                }
                _ => {}
            }
        }
        self.events_seen = events.len();

        for message in analyser.chat.iter().skip(self.chat_seen) {
            if matches!(message.kind, ChatMessageKind::NameChange) || message.text.is_empty() {
                continue;
            }
            self.recording.chat.push(Chat {
                tick: u32::from(message.tick),
                from: message.from.clone(),
                text: message.text.clone(),
                team_only: matches!(
                    message.kind,
                    ChatMessageKind::ChatTeam | ChatMessageKind::ChatTeamDead
                ),
            });
        }
        self.chat_seen = analyser.chat.len();

        let players = &analyser.state().players;
        for &(tick, shooter, origin, pitch, yaw, crit) in analyser.shots.iter().skip(self.shots_seen) {
            let Some(slot) = players.iter().position(|player| player.entity == shooter) else { continue };
            self.recording.shots.push(Shot {
                tick,
                slot,
                team: players[slot].team as u8,
                origin,
                pitch,
                yaw,
                crit,
            });
        }
        self.shots_seen = analyser.shots.len();

        for &(tick, player, event) in analyser.anims.iter().skip(self.anims_seen) {
            let Some(slot) = players.iter().position(|p| p.entity == player) else { continue };
            self.recording.anims.push(AnimEvent { tick, slot, event });
        }
        self.anims_seen = analyser.anims.len();

        for (tick, sound) in analyser.sounds.iter().skip(self.sounds_seen) {
            let entity = EntityId::from(sound.entity);
            self.recording.sounds.push(DemoSound {
                tick: *tick,
                sound: u16::try_from(sound.sound).unwrap_or(u16::MAX),
                entity: u16::try_from(sound.entity).unwrap_or(u16::MAX),
                slot: players.iter().position(|player| player.entity == entity),
                flags: u16::try_from(sound.flags).unwrap_or(0),
                channel: u8::try_from(sound.channel).unwrap_or(0),
                volume: sound.volume,
                level: u16::try_from(sound.level).unwrap_or(75),
                pitch: u8::try_from(sound.pitch).unwrap_or(100),
                origin: sound.origin,
            });
        }
        self.sounds_seen = analyser.sounds.len();

        for &(tick, origin) in analyser.explosions.iter().skip(self.explosions_seen) {
            self.recording.explosions.push(Explosion { tick, origin });
        }
        self.explosions_seen = analyser.explosions.len();
    }

    fn finish(mut self) -> Recording {
        let recording = &mut self.recording;
        let slot_count = recording.slots.len();
        let frame_count = recording.ticks.len();
        let mut players = vec![PlayerFrame::default(); frame_count * slot_count];
        for (frame, start) in self.row_starts.iter().enumerate() {
            let end = self.row_starts.get(frame + 1).copied().unwrap_or(self.rows.len());
            let row = &self.rows[*start..end];
            players[frame * slot_count..frame * slot_count + row.len()].copy_from_slice(row);
        }
        // Add up how far each player walked, ignoring respawns and teleports.
        for slot in 0..slot_count {
            let mut total = 0.0;
            let mut previous: Option<PlayerFrame> = None;
            for frame in 0..frame_count {
                let current = &mut players[frame * slot_count + slot];
                if let Some(before) = previous.filter(|before| before.is_alive() && current.is_alive()) {
                    let dx = current.position[0] - before.position[0];
                    let dy = current.position[1] - before.position[1];
                    let step = dx.hypot(dy);
                    if step < MAX_STRIDE {
                        total += step;
                    }
                }
                current.travelled = total;
                previous = Some(*current);
            }
        }
        recording.players = players;
        recording.slot_count = slot_count;
        recording.object_offsets.push(recording.objects.len() as u32);
        recording.dot_offsets.push(recording.dots.len() as u32);
        if recording.models.is_empty() {
            recording.models.push(String::new());
        }
        self.recording
    }
}

/// Reads a demo, reporting progress between 0 and 1 as it goes.
///
/// A file that ends early is still returned, with `Recording::warning` set,
/// since crashes and disconnects routinely leave demos cut off.
pub fn read_demo(bytes: &[u8], mut on_progress: impl FnMut(f32)) -> Result<Recording, String> {
    check_signature(bytes)?;

    let demo = Demo::new(bytes);
    let parser = DemoParser::new_with_analyser(demo.get_stream(), Analyser::default());
    let (header, mut ticker) = parser
        .ticker()
        .map_err(|error| format!("This demo couldn't be read: {error}"))?;

    if header.game != "tf" {
        return Err(format!(
            "This demo was recorded in \"{}\". Only Team Fortress 2 demos can be played.",
            header.game
        ));
    }

    let mut recorder = Recorder::default();
    recorder.recording.map = header.map.clone();
    recorder.recording.server = header.server.clone();
    recorder.recording.recorded_by = header.nick.clone();

    let gap = tick_gap(header.ticks);
    let mut last_frame_tick = 0;
    let mut packets = 0u32;
    let mut tick_interval = 0.0;

    loop {
        match ticker.next() {
            Ok(Some(step)) => {
                let tick = u32::from(step.tick);
                let analyser = step.state;
                tick_interval = analyser.state().interval_per_tick;

                if tick > 0 && tick + gap < last_frame_tick {
                    recorder.restart();
                    last_frame_tick = 0;
                }
                let due = recorder.recording.ticks.is_empty() || tick >= last_frame_tick + gap;
                if tick > 0 && due {
                    recorder.identify_players(tick, analyser.state());
                    recorder.collect_events(analyser);
                    recorder.record_frame(tick, analyser);
                    last_frame_tick = tick;
                }

                packets += 1;
                if packets % 4096 == 0 && header.ticks > 0 {
                    on_progress((tick as f32 / header.ticks as f32).min(1.0));
                }
            }
            Ok(None) => break,
            Err(error) => {
                if recorder.recording.ticks.is_empty() {
                    return Err(format!("This demo couldn't be read: {error}"));
                }
                recorder.recording.warning =
                    Some("This demo ends early, so the last part may be missing.".into());
                break;
            }
        }
    }

    // Events that arrived after the last sampled frame.
    let analyser = ticker.state();
    recorder.identify_players(last_frame_tick, analyser.state());
    recorder.collect_events(analyser);

    if recorder.recording.ticks.is_empty() {
        return Err("This demo doesn't contain any gameplay.".into());
    }

    recorder.recording.sky_name = analyser.sky_name.clone();
    recorder.recording.sound_names = analyser.sound_names.iter().map(|name| super::demo_sounds::sound_path(name)).collect();
    // Sounds from the setup packets at the start carry the server's own tick, so put
    // them in order with the rest.
    recorder.recording.sounds.sort_by_key(|sound| sound.tick);
    recorder.recording.explosions.sort_by_key(|explosion| explosion.tick);
    recorder.recording.recorder_slot = analyser
        .recorder_entity
        .and_then(|entity| analyser.state().players.iter().position(|p| p.entity == entity));
    recorder.recording.tick_interval = if tick_interval > 0.0 {
        tick_interval
    } else if header.ticks > 0 && header.duration > 0.0 {
        header.duration / header.ticks as f32
    } else {
        DEFAULT_TICK_INTERVAL
    };

    on_progress(1.0);
    Ok(recorder.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_projectile_class_has_a_kind() {
        let kind = ObjectKind::of_projectile_class;
        assert_eq!(kind("CTFProjectile_Rocket"), Some(ObjectKind::Rocket));
        assert_eq!(kind("CTFProjectile_SentryRocket"), Some(ObjectKind::Rocket));
        assert_eq!(kind("CTFProjectile_Arrow"), Some(ObjectKind::Arrow));
        assert_eq!(kind("CTFProjectile_HealingBolt"), Some(ObjectKind::Arrow));
        assert_eq!(kind("CTFGrenadePipebombProjectile"), Some(ObjectKind::Pipe));
        assert_eq!(kind("CTFProjectile_EnergyRing"), Some(ObjectKind::EnergyRing));
        assert_eq!(kind("CTFProjectile_EnergyBall"), Some(ObjectKind::EnergyBall));
        assert_eq!(kind("CTFProjectile_MechanicalArmOrb"), Some(ObjectKind::Orb));
        assert_eq!(kind("CTFProjectile_BallOfFire"), Some(ObjectKind::Fireball));
        // Classes the parser didn't follow at all.
        assert_eq!(kind("CTFStunBall"), Some(ObjectKind::Thrown));
        assert_eq!(kind("CTFBall_Ornament"), Some(ObjectKind::Thrown));
        for thrown in ["CTFProjectile_Jar", "CTFProjectile_JarMilk", "CTFProjectile_JarGas", "CTFProjectile_Cleaver"] {
            assert_eq!(kind(thrown), Some(ObjectKind::Thrown), "{thrown}");
        }
        assert_eq!(kind("CTFProjectile_SomethingNew"), Some(ObjectKind::OtherProjectile));
        // Weapons and players are not projectiles.
        assert_eq!(kind("CTFGrenadeLauncher"), None);
        assert_eq!(kind("CTFPlayer"), None);
        assert!(ObjectKind::EnergyRing.is_energy() && ObjectKind::Fireball.is_energy());
        assert!(!ObjectKind::Thrown.is_energy() && !ObjectKind::Rocket.is_energy());
    }

    #[test]
    fn rejects_files_that_are_not_demos() {
        assert!(read_demo(b"definitely not a demo", |_| {}).is_err());
        assert!(read_demo(b"HL2", |_| {}).is_err());
    }

    #[test]
    fn names_source_2_demos_specifically() {
        let error = read_demo(b"PBDEMS2\0rest of the file", |_| {}).unwrap_err();
        assert!(error.contains("Source 2"));
    }

    #[test]
    fn a_truncated_header_is_reported_not_a_crash() {
        assert!(read_demo(b"HL2DEMO\0cut off here", |_| {}).is_err());
    }

    #[test]
    fn steam_ids_convert_to_their_64_bit_form() {
        assert_eq!(steam_id_to_u64("[U:1:123]"), STEAM_ID64_BASE + 123);
        assert_eq!(steam_id_to_u64("BOT"), 0);
    }

    #[test]
    fn long_demos_are_sampled_more_sparsely() {
        assert_eq!(tick_gap(0), 1);
        assert_eq!(tick_gap(100_000), 1);
        assert_eq!(tick_gap(450_000), 3);
    }

    fn slot(names: &[(u32, &str, u64)]) -> Slot {
        Slot {
            names: names
                .iter()
                .map(|&(from_tick, name, steam_id)| SlotName {
                    from_tick,
                    name: name.to_string(),
                    steam_id,
                })
                .collect(),
        }
    }

    #[test]
    fn detections_find_the_slot_their_player_held_at_the_time() {
        let recording = Recording {
            slots: vec![
                slot(&[(0, "first", 11), (500, "second", 22)]),
                slot(&[(0, "other", 33)]),
            ],
            ..Recording::default()
        };
        assert_eq!(recording.slot_of_steam_id(11, 100.0), Some(0));
        assert_eq!(recording.slot_of_steam_id(22, 600.0), Some(0));
        assert_eq!(recording.slot_of_steam_id(33, 600.0), Some(1));
        // Before they joined, fall back to whichever slot they ever held.
        assert_eq!(recording.slot_of_steam_id(22, 100.0), Some(0));
        assert_eq!(recording.slot_of_steam_id(99, 100.0), None);
        assert_eq!(recording.name_at(0, 100.0), "first");
        assert_eq!(recording.name_at(0, 700.0), "second");
    }

    #[test]
    fn map_objects_only_show_while_they_exist() {
        let key = |tick| PropKey {
            tick,
            position: [0.0; 3],
            rotation: [0.0, 0.0, 0.0, 1.0],
            sequence: 0,
            sequence_start: tick,
            skin: 0,
            body: 0,
        };
        let track = PropTrack {
            model: 1,
            keys: vec![key(10), key(20)],
            until: 30,
        };
        assert!(track.keys_around(5.0).is_none());
        assert_eq!(track.keys_around(15.0).map(|(k, n)| (k.tick, n.map(|n| n.tick))), Some((10, Some(20))));
        assert_eq!(track.keys_around(25.0).map(|(k, n)| (k.tick, n.is_none())), Some((20, true)));
        assert!(track.keys_around(30.0).is_none());
    }

    #[test]
    fn aim_towards_a_point() {
        let eye = Vec3::new(0.0, 0.0, 100.0);
        let (pitch, yaw) = angles_towards(eye, Vec3::new(100.0, 100.0, 100.0)).unwrap();
        assert!(pitch.abs() < 1e-4 && (yaw - 45.0).abs() < 1e-4);
        let (pitch, _) = angles_towards(eye, Vec3::new(100.0, 0.0, 0.0)).unwrap();
        assert!((pitch - 45.0).abs() < 1e-4, "looking down is positive pitch");
        assert!(aim_difference((0.0, 179.5), (0.0, -179.5)) < 1.01);
        assert!(aim_difference((350.0, 10.0), (-10.0, 10.0)) < 1e-3);
        assert!(aim_difference((0.0, 0.0), (0.0, 30.0)) > 29.0);
    }

    #[test]
    fn doors_glide_between_keys_and_wait_before_moving() {
        let at = |tick: u32, z: f32| MoverKey { tick, position: [0.0, 0.0, z], rotation: Quat::IDENTITY.to_array() };
        let mut door = MoverTrack { brush: 8, keys: vec![at(100, 0.0)] };
        // Seen standing still for a while: nothing new to keep.
        door.push(at(200, 0.0), 199);
        assert_eq!(door.keys.len(), 1);
        // Then it opens. It sat still until tick 299, so it doesn't creep from 100.
        door.push(at(300, 4.5), 299);
        door.push(at(301, 9.0), 300);
        assert_eq!(door.keys.iter().map(|key| key.tick).collect::<Vec<_>>(), [100, 299, 300, 301]);

        assert!(door.at(50.0).is_none(), "where the map put it, before the demo shows it");
        assert_eq!(door.at(250.0).unwrap().0.z, 0.0);
        assert!((door.at(300.5).unwrap().0.z - 6.75).abs() < 1e-4);
        assert_eq!(door.at(5000.0).unwrap().0.z, 9.0, "stays open after the last key");
        assert_eq!(brush_number("*8"), Some(8));
        assert_eq!(brush_number("models/props/door.mdl"), None);
    }

    #[test]
    fn eye_height_depends_on_class_and_ducking() {
        let heavy = PlayerFrame { class: 6, ..PlayerFrame::default() };
        let scout = PlayerFrame { class: 1, ..PlayerFrame::default() };
        let ducking = PlayerFrame { flags: PLAYER_DUCKING, ..heavy };
        assert_eq!(heavy.eye_height(), 75.0);
        assert_eq!(scout.eye_height(), 65.0);
        assert_eq!(ducking.eye_height(), 45.0);
    }
}


#[cfg(test)]
mod projectile_survey {
    use super::*;
    use std::collections::BTreeMap;
    use std::collections::BTreeSet;

    /// Which projectile classes each demo has, which props place them, and kills by weapon.
    #[derive(Default)]
    struct Scan {
        classes: BTreeMap<String, usize>,
        placed_by: BTreeMap<String, BTreeSet<String>>,
        weapons: BTreeMap<String, usize>,
    }

    fn is_projectile(name: &str) -> bool {
        name.starts_with("CTFProjectile_")
            || name.starts_with("CTFGrenade")
            || name.contains("StunBall")
            || name.contains("Ball_")
            || name.contains("Throwable")
    }

    impl MessageHandler for Scan {
        type Output = Self;

        fn does_handle(message_type: MessageType) -> bool {
            matches!(message_type, MessageType::PacketEntities | MessageType::GameEvent)
        }

        fn handle_message(&mut self, message: &Message, _tick: DemoTick, state: &ParserState) {
            match message {
                Message::PacketEntities(packet) => {
                    for entity in &packet.entities {
                        let Some(class) = state.server_classes.get(usize::from(entity.server_class)) else { continue };
                        let name = class.name.as_str();
                        if !is_projectile(name) {
                            continue;
                        }
                        if entity.update_type == UpdateType::Enter {
                            *self.classes.entry(name.to_string()).or_default() += 1;
                        }
                        for prop in entity.props(state) {
                            if let Some((table, prop_name)) = prop.identifier.names() {
                                if prop_name.contains("Origin") || prop_name.contains("Rotation") || prop_name.contains("Velocity") {
                                    self.placed_by.entry(name.to_string()).or_default().insert(format!("{table}.{prop_name}"));
                                }
                            }
                        }
                    }
                }
                Message::GameEvent(event) => {
                    if let GameEvent::PlayerDeath(death) = &event.event {
                        *self.weapons.entry(death.weapon.to_string()).or_default() += 1;
                    }
                }
                _ => {}
            }
        }

        fn into_output(self, _state: &ParserState) -> Self::Output {
            self
        }
    }

    /// `DEMO_DIR=... cargo test --release --lib projectile_survey -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn list_projectiles_in_demos() {
        let Ok(folder) = std::env::var("DEMO_DIR") else { return };
        let mut demos: Vec<_> = std::fs::read_dir(&folder).unwrap().flatten().map(|entry| entry.path()).filter(|path| path.extension().is_some_and(|ext| ext == "dem")).collect();
        demos.sort();
        let mut all_classes: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut all_placed: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for path in demos {
            let bytes = std::fs::read(&path).unwrap();
            let demo = Demo::new(&bytes);
            let Ok((_, scan)) = DemoParser::new_with_analyser(demo.get_stream(), Scan::default()).parse() else { continue };
            let file = path.file_name().unwrap().to_string_lossy().to_string();
            for (class, count) in &scan.classes {
                all_classes.entry(class.clone()).or_default().push(format!("{file}:{count}"));
            }
            for (class, props) in scan.placed_by {
                all_placed.entry(class).or_default().extend(props);
            }
            let weapons: Vec<String> = scan.weapons.iter().map(|(weapon, count)| format!("{weapon}={count}")).collect();
            println!("WEAPONS {file}: {}", weapons.join(" "));
        }
        for (class, demos) in &all_classes {
            println!("CLASS {class} in {} demos: {}", demos.len(), demos.iter().take(6).cloned().collect::<Vec<_>>().join(" "));
            println!("  placed by {:?}", all_placed.get(class));
        }
    }
}
