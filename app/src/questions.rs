//! The questions recognition may ask (DESIGN.md section 5.2), as a state machine with no window
//! and no device in it: [`decide`] turns what recognition found into a question, and [`answer`]
//! turns the user's reply into what happens next.
//!
//! Nothing is learned or rejected until the user has answered. A question leaves the device
//! unidentified; only an explicit reply binds it to a known device (and teaches that device the
//! new signal values), or turns it into a new one.

use std::collections::HashSet;

use pnk_core::recognise::{Verdict, recognise, shared_usb_serials};
use pnk_core::registry::{KnownDevice, Registry};
use pnk_core::signals::{DeviceKind, SignalKind, Signals, Strength, strength};

/// A known device, for the question's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Named {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Question {
    /// Several known devices fit: which of these is it, or a new one?
    Which { candidates: Vec<Named> },
    /// Looks like `candidate`, but strong signals of the same kind differ: "Reformatted, or
    /// another of the same kind?" `changed` is what differs. `copy_of_stick` is set when the
    /// stick carries the candidate's ID file, so it may be a clone.
    Changed {
        candidate: Named,
        changed: Vec<SignalKind>,
        copy_of_stick: bool,
    },
    /// Only weak signals agree: "Is this X?"
    Maybe { candidate: Named },
}

/// What recognition found, before anything is asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Exactly one device fits and nothing clashes.
    Known {
        id: String,
        by: SignalKind,
    },
    Ask(Question),
    /// Nothing known fits: set it up.
    New,
}

/// The user's reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// For [`Question::Which`]: it is this known device.
    Pick(String),
    /// For [`Question::Which`]: it is none of these.
    NewDevice,
    /// For [`Question::Changed`] and [`Question::Maybe`]: yes, it is the candidate.
    Yes,
    /// ...no, it is another device.
    No,
}

/// What an answer led to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// It is this known device. `weak`: the identity rests on an "is this X?" answer, so
    /// archiving must ask first. `learned`: the kinds of signal the device gained.
    Bound {
        id: String,
        weak: bool,
        learned: Vec<SignalKind>,
    },
    /// Another question follows (an earlier "no" left another candidate).
    Ask(Question),
    New,
}

fn named(known: &[KnownDevice], id: &str) -> Named {
    Named {
        id: id.to_string(),
        name: known
            .iter()
            .find(|d| d.id == id)
            .map_or_else(|| "a known device".to_string(), |d| d.name.clone()),
    }
}

/// Decide what to do about `signals`, among the known devices. Changes nothing.
pub fn decide(known: &[KnownDevice], signals: &Signals, kind: DeviceKind) -> Decision {
    match recognise(signals, kind, known) {
        Verdict::Recognised { id, by } => Decision::Known { id, by },
        Verdict::AskWhich { ids } => Decision::Ask(Question::Which {
            candidates: ids.iter().map(|id| named(known, id)).collect(),
        }),
        Verdict::AskChanged { candidates } => match candidates.first() {
            Some(c) => Decision::Ask(Question::Changed {
                candidate: named(known, &c.id),
                changed: c.clashed.clone(),
                copy_of_stick: c.matched.contains(&SignalKind::IdFile),
            }),
            None => Decision::New,
        },
        Verdict::AskMaybe { candidates } => match candidates.first() {
            Some(c) => Decision::Ask(Question::Maybe {
                candidate: named(known, &c.id),
            }),
            None => Decision::New,
        },
        Verdict::New => Decision::New,
    }
}

fn resolution_of(d: Decision) -> Resolution {
    match d {
        Decision::Known { id, .. } => Resolution::Bound {
            id,
            weak: false,
            learned: Vec::new(),
        },
        Decision::Ask(q) => Resolution::Ask(q),
        Decision::New => Resolution::New,
    }
}

/// Apply the user's reply to `question`. Only an explicit "yes" (or picking a device) learns the
/// signal values; a "no" rejects the device, so it is not proposed for this thing again, and
/// recognition runs again without it.
pub fn answer(
    registry: &mut Registry,
    question: &Question,
    reply: &Answer,
    signals: &Signals,
    kind: DeviceKind,
    now: &str,
) -> Result<Resolution, String> {
    let learn = |registry: &mut Registry, id: &str, weak: bool| -> Result<Resolution, String> {
        let d = registry
            .get_mut(id)
            .ok_or("That device is no longer in the list.")?;
        let learned = d.learn(signals, now);
        Ok(Resolution::Bound {
            id: id.to_string(),
            weak,
            learned,
        })
    };
    let reject = |registry: &mut Registry, id: &str| {
        if let Some(d) = registry.get_mut(id) {
            d.reject(signals);
        }
    };
    match (question, reply) {
        (Question::Which { candidates }, Answer::Pick(id)) => {
            if !candidates.iter().any(|c| &c.id == id) {
                return Err("That is not one of the devices asked about.".into());
            }
            learn(registry, id, false)
        }
        (Question::Which { candidates }, Answer::NewDevice) => {
            for c in candidates {
                reject(registry, &c.id);
            }
            Ok(resolution_of(decide(registry.devices(), signals, kind)))
        }
        (Question::Changed { candidate, .. }, Answer::Yes) => learn(registry, &candidate.id, false),
        (Question::Maybe { candidate }, Answer::Yes) => learn(registry, &candidate.id, true),
        (Question::Changed { candidate, .. } | Question::Maybe { candidate }, Answer::No) => {
            reject(registry, &candidate.id);
            Ok(resolution_of(decide(registry.devices(), signals, kind)))
        }
        _ => Err("That answer does not fit the question.".into()),
    }
}

/// "It's one I already back up…": the device is this known one, whatever the question was.
pub fn link_existing(
    registry: &mut Registry,
    id: &str,
    signals: &Signals,
    now: &str,
) -> Result<Resolution, String> {
    let d = registry
        .get_mut(id)
        .ok_or("That device is no longer in the list.")?;
    let learned = d.learn(signals, now);
    Ok(Resolution::Bound {
        id: id.to_string(),
        weak: false,
        learned,
    })
}

/// The devices whose identity this session rests on an "is this X?" answer. Archiving asks first
/// for them, also after the device is plugged in again, until the app is closed.
#[derive(Debug, Default)]
pub struct WeakSession(HashSet<String>);

impl WeakSession {
    pub fn note(&mut self, r: &Resolution) {
        if let Resolution::Bound { id, weak: true, .. } = r {
            self.0.insert(id.clone());
        }
    }

    pub fn is_weak(&self, id: &str) -> bool {
        self.0.contains(id)
    }

    /// The user took it back ("not this device"), or the device was forgotten.
    pub fn forget(&mut self, id: &str) {
        self.0.remove(id);
    }
}

/// Whether a stick has nothing strong and lasting to recognise it by (DESIGN.md section 5.4): no
/// ID file, and no USB serial that is real and not shared with another known device. Its format
/// ID alone would recognise it only until the next reformat, so an ID file is offered.
pub fn lacks_strong_id(signals: &Signals, kind: DeviceKind, known: &[KnownDevice]) -> bool {
    if signals.id_file_id.is_some() {
        return false;
    }
    let Some(serial) = signals.usb_serial.as_deref() else {
        return true;
    };
    let norm = serial.trim().to_lowercase();
    let mut shared = shared_usb_serials(known);
    // Another known device with the same serial makes it a batch serial.
    if known
        .iter()
        .any(|d| d.history.contains(SignalKind::UsbSerial, &norm))
    {
        shared.insert(norm);
    }
    !strength(SignalKind::UsbSerial, serial, kind, &shared).is_some_and(|s| s >= Strength::Strong)
}

/// A name for a new device from what it reports: its own name, else its model, else a generic
/// one for its kind.
pub fn suggested_name(signals: &Signals, kind: DeviceKind) -> String {
    [&signals.friendly_name, &signals.label, &signals.model]
        .into_iter()
        .flatten()
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| match kind {
            DeviceKind::Storage => "USB stick".to_string(),
            DeviceKind::Camera => "New camera".to_string(),
            _ => "New phone".to_string(),
        })
}

/// How a question reads: the text above its buttons.
pub fn question_text(q: &Question) -> String {
    match q {
        Question::Which { .. } => {
            "More than one device you back up fits this one. Which is it?".into()
        }
        Question::Changed {
            candidate,
            changed,
            copy_of_stick,
        } => {
            let what = changed
                .iter()
                .map(|k| k.describe().trim_start_matches("its ").to_string())
                .collect::<Vec<_>>();
            let what = if what.is_empty() {
                "something".to_string()
            } else {
                what.join(" and ")
            };
            if *copy_of_stick {
                format!(
                    "This has the ID file of \"{}\", but its {what} changed. It may be a copy of that stick. Reformatted, or another of the same kind?",
                    candidate.name
                )
            } else {
                format!(
                    "Looks like \"{}\", but its {what} changed. Reformatted, or another of the same kind?",
                    candidate.name
                )
            }
        }
        Question::Maybe { candidate } => format!("Is this \"{}\"?", candidate.name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: &str = "2026-11-17T18:00:00Z";

    fn stick(vol: &str, usb: Option<&str>, id_file: Option<&str>) -> Signals {
        Signals {
            id_file_id: id_file.map(Into::into),
            volume_serial: Some(vol.into()),
            usb_serial: usb.map(Into::into),
            usb_vendor: Some("0781".into()),
            usb_product: Some("5581".into()),
            maker: Some("SanDisk".into()),
            model: Some("Ultra".into()),
            capacity: Some(64_000_000_000),
            label: Some("STICK".into()),
            ..Signals::default()
        }
    }

    /// A different stick of the same make: only the weak signals agree.
    fn twin() -> Signals {
        Signals {
            volume_serial: Some("ZZZZ-9999".into()),
            usb_vendor: Some("0781".into()),
            usb_product: Some("5581".into()),
            maker: Some("SanDisk".into()),
            model: Some("Ultra".into()),
            capacity: Some(64_000_000_000),
            label: Some("STICK".into()),
            ..Signals::default()
        }
    }

    fn registry_with(sig: &Signals) -> (Registry, String) {
        let d = KnownDevice::new(DeviceKind::Storage, "Work stick", sig, NOW);
        let id = d.id.clone();
        (Registry::new(vec![d]), id)
    }

    fn history_len(r: &Registry, id: &str, k: SignalKind) -> usize {
        r.get(id).unwrap().history.values(k).len()
    }

    #[test]
    fn deciding_changes_nothing() {
        let orig = stick("AAAA-1111", None, None);
        let (reg, id) = registry_with(&orig);
        let before = reg.get(&id).unwrap().clone();
        // A weak-only twin asks "is this X?" and learns nothing by itself.
        let d = decide(reg.devices(), &twin(), DeviceKind::Storage);
        assert!(matches!(d, Decision::Ask(Question::Maybe { .. })), "{d:?}");
        assert_eq!(
            reg.get(&id).unwrap(),
            &before,
            "nothing is learned before an answer"
        );
    }

    #[test]
    fn yes_to_is_this_x_learns_and_marks_the_identity_weak() {
        let (mut reg, id) = registry_with(&stick("AAAA-1111", None, None));
        let q = match decide(reg.devices(), &twin(), DeviceKind::Storage) {
            Decision::Ask(q) => q,
            d => panic!("{d:?}"),
        };
        assert_eq!(history_len(&reg, &id, SignalKind::VolumeSerial), 1);
        let r = answer(
            &mut reg,
            &q,
            &Answer::Yes,
            &twin(),
            DeviceKind::Storage,
            NOW,
        )
        .unwrap();
        match &r {
            Resolution::Bound {
                id: got,
                weak,
                learned,
            } => {
                assert_eq!(got, &id);
                assert!(weak, "the caution passes on to archiving");
                assert!(learned.contains(&SignalKind::VolumeSerial));
            }
            r => panic!("{r:?}"),
        }
        assert_eq!(history_len(&reg, &id, SignalKind::VolumeSerial), 2);
        let mut session = WeakSession::default();
        session.note(&r);
        assert!(session.is_weak(&id));
        assert!(!session.is_weak("another"));
    }

    #[test]
    fn no_to_is_this_x_rejects_it_and_leads_to_a_new_device() {
        let (mut reg, id) = registry_with(&stick("AAAA-1111", None, None));
        let q = match decide(reg.devices(), &twin(), DeviceKind::Storage) {
            Decision::Ask(q) => q,
            d => panic!("{d:?}"),
        };
        let r = answer(&mut reg, &q, &Answer::No, &twin(), DeviceKind::Storage, NOW).unwrap();
        assert_eq!(r, Resolution::New);
        assert_eq!(
            history_len(&reg, &id, SignalKind::VolumeSerial),
            1,
            "nothing learned"
        );
        assert_eq!(reg.get(&id).unwrap().not_this.len(), 1);
        // The same stick is not proposed as that device again.
        assert_eq!(
            decide(reg.devices(), &twin(), DeviceKind::Storage),
            Decision::New
        );
    }

    #[test]
    fn a_reformatted_stick_asks_and_a_yes_learns_the_new_format_id() {
        let (mut reg, id) = registry_with(&stick("AAAA-1111", Some("SER123456"), None));
        let now_seen = stick("BBBB-2222", Some("SER123456"), None);
        let Decision::Ask(q) = decide(reg.devices(), &now_seen, DeviceKind::Storage) else {
            panic!("expected a question");
        };
        match &q {
            Question::Changed {
                candidate,
                changed,
                copy_of_stick,
            } => {
                assert_eq!(candidate.id, id);
                assert_eq!(changed, &[SignalKind::VolumeSerial]);
                assert!(!copy_of_stick);
            }
            q => panic!("{q:?}"),
        }
        assert!(question_text(&q).contains("Work stick"));
        assert!(question_text(&q).contains("Reformatted, or another of the same kind?"));
        assert_eq!(history_len(&reg, &id, SignalKind::VolumeSerial), 1);
        let r = answer(
            &mut reg,
            &q,
            &Answer::Yes,
            &now_seen,
            DeviceKind::Storage,
            NOW,
        )
        .unwrap();
        assert!(matches!(&r, Resolution::Bound { weak: false, .. }), "{r:?}");
        assert_eq!(history_len(&reg, &id, SignalKind::VolumeSerial), 2);
        // Next time it is simply recognised.
        assert!(matches!(
            decide(reg.devices(), &now_seen, DeviceKind::Storage),
            Decision::Known { .. }
        ));
    }

    #[test]
    fn a_different_stick_of_the_same_kind_is_rejected_then_set_up_as_new() {
        let (mut reg, id) = registry_with(&stick("AAAA-1111", Some("SER123456"), None));
        let other = stick("BBBB-2222", Some("SER123456"), None);
        let Decision::Ask(q) = decide(reg.devices(), &other, DeviceKind::Storage) else {
            panic!("expected a question");
        };
        let r = answer(&mut reg, &q, &Answer::No, &other, DeviceKind::Storage, NOW).unwrap();
        assert_eq!(r, Resolution::New);
        assert_eq!(history_len(&reg, &id, SignalKind::VolumeSerial), 1);
    }

    #[test]
    fn a_stick_with_the_same_id_file_but_another_format_id_may_be_a_copy() {
        let (mut reg, _id) = registry_with(&stick("AAAA-1111", Some("SER123456"), Some("id-x")));
        let clone = stick("BBBB-2222", Some("SER999999"), Some("id-x"));
        let Decision::Ask(q) = decide(reg.devices(), &clone, DeviceKind::Storage) else {
            panic!("expected a question");
        };
        assert!(
            matches!(
                &q,
                Question::Changed {
                    copy_of_stick: true,
                    ..
                }
            ),
            "{q:?}"
        );
        assert!(question_text(&q).contains("copy"));
        let r = answer(&mut reg, &q, &Answer::No, &clone, DeviceKind::Storage, NOW).unwrap();
        // Rejected: it is set up as a new device (and offered a new ID file by the window).
        assert_eq!(r, Resolution::New);
    }

    #[test]
    fn which_asks_among_candidates_and_a_pick_binds_only_that_one() {
        let shared = stick("AAAA-1111", None, None);
        let a = KnownDevice::new(DeviceKind::Storage, "A", &shared, NOW);
        let b = KnownDevice::new(DeviceKind::Storage, "B", &shared, NOW);
        let (ida, idb) = (a.id.clone(), b.id.clone());
        let mut reg = Registry::new(vec![a, b]);
        let probe = stick("AAAA-1111", None, None);
        let Decision::Ask(q) = decide(reg.devices(), &probe, DeviceKind::Storage) else {
            panic!("expected a question");
        };
        let Question::Which { candidates } = &q else {
            panic!("{q:?}")
        };
        assert_eq!(candidates.len(), 2);
        assert!(
            answer(
                &mut reg,
                &q,
                &Answer::Pick("nope".into()),
                &probe,
                DeviceKind::Storage,
                NOW
            )
            .is_err()
        );
        assert!(answer(&mut reg, &q, &Answer::Yes, &probe, DeviceKind::Storage, NOW).is_err());
        let r = answer(
            &mut reg,
            &q,
            &Answer::Pick(idb.clone()),
            &probe,
            DeviceKind::Storage,
            NOW,
        )
        .unwrap();
        assert!(matches!(&r, Resolution::Bound { id, .. } if id == &idb));
        assert!(reg.get(&ida).unwrap().not_this.is_empty());
    }

    #[test]
    fn a_new_device_answer_rejects_every_candidate() {
        let shared = stick("AAAA-1111", None, None);
        let a = KnownDevice::new(DeviceKind::Storage, "A", &shared, NOW);
        let b = KnownDevice::new(DeviceKind::Storage, "B", &shared, NOW);
        let mut reg = Registry::new(vec![a, b]);
        let probe = stick("AAAA-1111", None, None);
        let Decision::Ask(q) = decide(reg.devices(), &probe, DeviceKind::Storage) else {
            panic!("expected a question");
        };
        let r = answer(
            &mut reg,
            &q,
            &Answer::NewDevice,
            &probe,
            DeviceKind::Storage,
            NOW,
        )
        .unwrap();
        assert_eq!(r, Resolution::New);
        assert!(reg.devices().iter().all(|d| d.not_this.len() == 1));
    }

    #[test]
    fn linking_to_an_existing_device_learns_its_signals() {
        let (mut reg, id) = registry_with(&stick("AAAA-1111", None, None));
        let card = stick("CCCC-3333", None, None);
        let r = link_existing(&mut reg, &id, &card, NOW).unwrap();
        assert!(matches!(&r, Resolution::Bound { weak: false, .. }));
        assert_eq!(history_len(&reg, &id, SignalKind::VolumeSerial), 2);
        assert!(link_existing(&mut reg, "missing", &card, NOW).is_err());
    }

    #[test]
    fn names_are_suggested_from_what_the_device_reports() {
        let s = Signals {
            friendly_name: Some("  Test iPhone ".into()),
            model: Some("iPhone".into()),
            ..Signals::default()
        };
        assert_eq!(suggested_name(&s, DeviceKind::Ptp), "Test iPhone");
        let s = Signals {
            label: Some("HOLIDAY".into()),
            ..Signals::default()
        };
        assert_eq!(suggested_name(&s, DeviceKind::Storage), "HOLIDAY");
        assert_eq!(
            suggested_name(&Signals::default(), DeviceKind::Storage),
            "USB stick"
        );
        assert_eq!(
            suggested_name(&Signals::default(), DeviceKind::Mtp),
            "New phone"
        );
    }

    #[test]
    fn an_id_file_is_offered_only_to_sticks_with_nothing_strong() {
        let st = DeviceKind::Storage;
        let none: Vec<KnownDevice> = Vec::new();
        // No USB serial, a made-up one, a placeholder: offered.
        assert!(lacks_strong_id(&stick("AAAA-1111", None, None), st, &none));
        assert!(lacks_strong_id(
            &stick("AAAA-1111", Some("6&2f3a&0&1"), None),
            st,
            &none
        ));
        assert!(lacks_strong_id(
            &stick("AAAA-1111", Some("0123456789ABCDEF"), None),
            st,
            &none
        ));
        // A real serial, or an ID file already: not offered.
        assert!(!lacks_strong_id(
            &stick("AAAA-1111", Some("SER123456"), None),
            st,
            &none
        ));
        assert!(!lacks_strong_id(
            &stick("AAAA-1111", None, Some("id-1")),
            st,
            &none
        ));
        // A serial another known stick has too is a batch serial: offered.
        let other = KnownDevice::new(
            st,
            "Other",
            &stick("BBBB-2222", Some("SER123456"), None),
            NOW,
        );
        assert!(lacks_strong_id(
            &stick("AAAA-1111", Some("SER123456"), None),
            st,
            &[other]
        ));
    }
}
