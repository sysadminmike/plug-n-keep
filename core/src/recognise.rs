//! Recognising a plugged-in device among the known ones (DESIGN.md section 5.2). Pure logic: it
//! takes the device's [`Signals`] and the known devices and gives a [`Verdict`]. It never starts
//! anything; asking the user and remembering the answer are for the caller
//! ([`KnownDevice::learn`] and [`KnownDevice::reject`] record answers).
//!
//! For each known device only the signals both sides have are compared:
//! - a *strong match*: a strong signal equals one the device has been seen with;
//! - a *strong clash*: the device has strong values of that kind and none equals the new one;
//! - a *weak score*: how many weak signals equal one the device has been seen with.
#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};

use crate::registry::KnownDevice;
use crate::signals::{DeviceKind, SignalKind, Signals, Strength, can_clash, strength};

/// How many weak signals must agree, for a device of the same kind, before the app asks "is
/// this X?". Two (a maker and a model) describe every stick of one make.
pub const WEAK_ASK_MIN: usize = 3;

/// What recognition decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Exactly one known device matches strongly and nothing clashes. `by` is the strongest
    /// signal that decided it.
    Recognised { id: String, by: SignalKind },
    /// Several known devices match strongly: which of these is it?
    AskWhich { ids: Vec<String> },
    /// Something matches strongly, but a strong signal of the same kind differs as well: "looks
    /// like X, but its format ID changed. Reformatted, or another of the same kind?" Best
    /// candidate first.
    AskChanged { candidates: Vec<Changed> },
    /// Only weak signals agree, for a device of the same kind: "is this X?" Best first.
    AskMaybe { candidates: Vec<Maybe> },
    /// Nothing known fits: set it up as a new device.
    New,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changed {
    pub id: String,
    /// The strong signals that matched.
    pub matched: Vec<SignalKind>,
    /// The strong signals that differ.
    pub clashed: Vec<SignalKind>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Maybe {
    pub id: String,
    /// How many weak signals agree.
    pub score: usize,
}

/// The USB serials (normalised) that appear in the history of two or more known devices. Some
/// cheap sticks share one serial across a whole batch, so from then on such a serial is weak.
pub fn shared_usb_serials(known: &[KnownDevice]) -> HashSet<String> {
    let mut count: HashMap<&str, usize> = HashMap::new();
    for d in known {
        for v in d.history.values(SignalKind::UsbSerial) {
            *count.entry(v.as_str()).or_default() += 1;
        }
    }
    count
        .into_iter()
        .filter(|&(_, n)| n >= 2)
        .map(|(v, _)| v.to_string())
        .collect()
}

struct Evaluation<'a> {
    device: &'a KnownDevice,
    matched: Vec<(SignalKind, Strength)>,
    clashed: Vec<SignalKind>,
    weak_score: usize,
}

impl Evaluation<'_> {
    fn id(&self) -> String {
        self.device.id.clone()
    }

    /// The signal that decided: the strongest match; among equals, the first kind.
    fn deciding(&self) -> Option<SignalKind> {
        self.matched
            .iter()
            .max_by_key(|&&(kind, s)| (s, std::cmp::Reverse(kind)))
            .map(|&(kind, _)| kind)
    }

    fn matched_kinds(&self) -> Vec<SignalKind> {
        self.matched.iter().map(|&(k, _)| k).collect()
    }

    /// The device has a different serial or ID-file ID: a different unit, not a reformat.
    fn clashes_on_a_unit_id(&self) -> bool {
        self.clashed.iter().any(|k| {
            matches!(
                k,
                SignalKind::IdFile | SignalKind::DeviceSerial | SignalKind::UsbSerial
            )
        })
    }
}

fn evaluate<'a>(
    device: &'a KnownDevice,
    values: &[(SignalKind, String)],
    kind: DeviceKind,
    shared: &HashSet<String>,
) -> Evaluation<'a> {
    let mut e = Evaluation {
        device,
        matched: Vec::new(),
        clashed: Vec::new(),
        weak_score: 0,
    };
    for (k, v) in values {
        let Some(s) = strength(*k, v, kind, shared) else {
            continue;
        };
        let known = device.history.values(*k);
        if known.contains(v) {
            if s >= Strength::Strong {
                e.matched.push((*k, s));
            } else {
                e.weak_score += 1;
            }
        } else if s >= Strength::Strong
            && can_clash(*k)
            && known
                .iter()
                .any(|hv| strength(*k, hv, kind, shared).is_some_and(|hs| hs >= Strength::Strong))
        {
            e.clashed.push(*k);
        }
    }
    e
}

/// Whether the user already said that what is plugged in is not `device`: it looks exactly like
/// something they rejected, meaning the same strong values (or, if neither had a strong value,
/// the same values altogether). A device that merely shares one value with a rejected one (the
/// real device shares its USB serial with a copy, say) is not rejected.
fn is_rejected(
    device: &KnownDevice,
    values: &[(SignalKind, String)],
    kind: DeviceKind,
    shared: &HashSet<String>,
) -> bool {
    let strong_of = |vals: &[(SignalKind, String)]| -> Vec<(SignalKind, String)> {
        vals.iter()
            .filter(|(k, v)| strength(*k, v, kind, shared).is_some_and(|s| s >= Strength::Strong))
            .cloned()
            .collect()
    };
    let mine = strong_of(values);
    device.not_this.iter().any(|no| {
        let theirs_all = no.values();
        let theirs = strong_of(&theirs_all);
        if mine.is_empty() && theirs.is_empty() {
            !values.is_empty() && values == theirs_all.as_slice()
        } else {
            mine == theirs
        }
    })
}

/// Decide what `signals` of a device of `kind` is, among `known` (DESIGN.md section 5.2).
pub fn recognise(signals: &Signals, kind: DeviceKind, known: &[KnownDevice]) -> Verdict {
    let shared = shared_usb_serials(known);
    let values = signals.values();
    let evals: Vec<Evaluation> = known
        .iter()
        .filter(|d| !is_rejected(d, &values, kind, &shared))
        .map(|d| evaluate(d, &values, kind, &shared))
        .collect();

    // An ID file is the strongest thing there is: if it matches a device, that decides, whatever
    // else matches. If something else differs, the stick may be a clone: ask.
    let by_id_file: Vec<&Evaluation> = evals
        .iter()
        .filter(|e| e.matched.iter().any(|&(_, s)| s == Strength::Strongest))
        .collect();
    if !by_id_file.is_empty() {
        return decide_among(by_id_file);
    }

    let matching: Vec<&Evaluation> = evals.iter().filter(|e| !e.matched.is_empty()).collect();
    if !matching.is_empty() {
        let clean: Vec<&Evaluation> = matching
            .iter()
            .copied()
            .filter(|e| e.clashed.is_empty())
            .collect();
        if !clean.is_empty() {
            return decide_among(clean);
        }
        return asked_changed(matching);
    }

    let mut maybe: Vec<Maybe> = evals
        .iter()
        .filter(|e| {
            e.device.kind == kind && e.weak_score >= WEAK_ASK_MIN && !e.clashes_on_a_unit_id()
        })
        .map(|e| Maybe {
            id: e.id(),
            score: e.weak_score,
        })
        .collect();
    if maybe.is_empty() {
        return Verdict::New;
    }
    maybe.sort_by_key(|m| std::cmp::Reverse(m.score));
    Verdict::AskMaybe { candidates: maybe }
}

/// One candidate recognises it; several ask which. A candidate that also clashes asks instead.
fn decide_among(candidates: Vec<&Evaluation>) -> Verdict {
    if let [one] = candidates.as_slice() {
        if one.clashed.is_empty() {
            let by = one.deciding().expect("a candidate has a match");
            return Verdict::Recognised { id: one.id(), by };
        }
        return asked_changed(candidates);
    }
    Verdict::AskWhich {
        ids: candidates.iter().map(|e| e.id()).collect(),
    }
}

fn asked_changed(mut candidates: Vec<&Evaluation>) -> Verdict {
    candidates.sort_by_key(|e| std::cmp::Reverse(e.matched.len()));
    Verdict::AskChanged {
        candidates: candidates
            .iter()
            .map(|e| Changed {
                id: e.id(),
                matched: e.matched_kinds(),
                clashed: e.clashed.clone(),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: &str = "2026-11-17T18:00:00Z";

    fn known(kind: DeviceKind, name: &str, signals: &Signals) -> KnownDevice {
        KnownDevice::new(kind, name, signals, NOW)
    }

    fn stick(vol: &str, usb: Option<&str>) -> Signals {
        Signals {
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

    fn phone(serial: &str, name: &str) -> Signals {
        Signals {
            device_serial: Some(serial.into()),
            maker: Some("Apple Inc.".into()),
            model: Some("iPhone".into()),
            friendly_name: Some(name.into()),
            ..Signals::default()
        }
    }

    /// The identical sticks: no serial of their own, same model, size and label.
    fn bare_stick() -> Signals {
        Signals {
            usb_vendor: Some("0781".into()),
            usb_product: Some("5581".into()),
            maker: Some("Generic".into()),
            model: Some("Flash Disk".into()),
            capacity: Some(8_000_000_000),
            label: Some("USB DISK".into()),
            ..Signals::default()
        }
    }

    #[test]
    fn an_empty_list_means_new() {
        assert_eq!(
            recognise(&phone("ABC123456", "P"), DeviceKind::Ptp, &[]),
            Verdict::New
        );
    }

    #[test]
    fn a_known_phone_is_recognised_by_its_serial() {
        let a = known(DeviceKind::Ptp, "A", &phone("AAAA1111", "Mine"));
        let b = known(DeviceKind::Ptp, "B", &phone("BBBB2222", "Mine"));
        let v = recognise(&phone("AAAA1111", "Mine"), DeviceKind::Ptp, &[b, a.clone()]);
        assert_eq!(
            v,
            Verdict::Recognised {
                id: a.id,
                by: SignalKind::DeviceSerial
            }
        );
    }

    #[test]
    fn a_renamed_phone_is_still_recognised() {
        let a = known(DeviceKind::Ptp, "Mine", &phone("AAAA1111", "Mine"));
        let v = recognise(
            &phone("AAAA1111", "Dad's iPhone"),
            DeviceKind::Ptp,
            std::slice::from_ref(&a),
        );
        assert_eq!(
            v,
            Verdict::Recognised {
                id: a.id,
                by: SignalKind::DeviceSerial
            }
        );
    }

    #[test]
    fn a_phone_with_the_same_name_but_another_serial_is_not_recognised_by_the_name() {
        let a = known(DeviceKind::Ptp, "Mine", &phone("AAAA1111", "iPhone"));
        // Same maker, model and name: two weak points, below the asking threshold.
        let v = recognise(&phone("BBBB2222", "iPhone"), DeviceKind::Ptp, &[a]);
        assert_eq!(v, Verdict::New);
    }

    #[test]
    fn a_stick_is_recognised_by_its_volume_serial_and_a_real_usb_serial() {
        let a = known(
            DeviceKind::Storage,
            "A",
            &stick("1111-AAAA", Some("AA0400000001")),
        );
        let b = known(
            DeviceKind::Storage,
            "B",
            &stick("2222-BBBB", Some("BB0400000002")),
        );
        let v = recognise(
            &stick("1111-aaaa", Some("AA0400000001")),
            DeviceKind::Storage,
            &[a.clone(), b],
        );
        assert!(
            matches!(&v, Verdict::Recognised { id, .. } if *id == a.id),
            "{v:?}"
        );
    }

    #[test]
    fn two_identical_sticks_are_never_silently_taken_for_each_other() {
        let a = known(DeviceKind::Storage, "Stick 1", &bare_stick());
        let b = known(DeviceKind::Storage, "Stick 2", &bare_stick());
        let v = recognise(&bare_stick(), DeviceKind::Storage, &[a.clone(), b.clone()]);
        match v {
            Verdict::AskMaybe { candidates } => {
                let mut ids: Vec<_> = candidates.iter().map(|c| c.id.clone()).collect();
                ids.sort();
                let mut want = vec![a.id, b.id];
                want.sort();
                assert_eq!(ids, want);
                assert!(candidates.iter().all(|c| c.score >= WEAK_ASK_MIN));
            }
            other => panic!("expected a question, got {other:?}"),
        }
    }

    #[test]
    fn a_new_stick_of_the_same_make_as_one_known_one_is_only_a_maybe() {
        let a = known(DeviceKind::Storage, "Stick 1", &bare_stick());
        let v = recognise(&bare_stick(), DeviceKind::Storage, std::slice::from_ref(&a));
        assert!(
            matches!(&v, Verdict::AskMaybe { candidates } if candidates.len() == 1 && candidates[0].id == a.id),
            "{v:?}"
        );
        // Of another kind, even with the same signals, it is never proposed.
        assert_eq!(
            recognise(&bare_stick(), DeviceKind::Mtp, &[a]),
            Verdict::New
        );
    }

    #[test]
    fn identical_sticks_with_different_volume_serials_tell_themselves_apart() {
        let mut s1 = bare_stick();
        s1.volume_serial = Some("1111-1111".into());
        let mut s2 = bare_stick();
        s2.volume_serial = Some("2222-2222".into());
        let a = known(DeviceKind::Storage, "1", &s1);
        let b = known(DeviceKind::Storage, "2", &s2);
        let v = recognise(&s2, DeviceKind::Storage, &[a, b.clone()]);
        assert!(
            matches!(&v, Verdict::Recognised { id, by: SignalKind::VolumeSerial } if *id == b.id),
            "{v:?}"
        );
    }

    #[test]
    fn a_reformatted_stick_asks_instead_of_being_recognised() {
        let before = stick("1111-AAAA", Some("AA0400000001"));
        let a = known(DeviceKind::Storage, "A", &before);
        let after = stick("9999-FFFF", Some("AA0400000001"));
        let v = recognise(&after, DeviceKind::Storage, std::slice::from_ref(&a));
        match v {
            Verdict::AskChanged { candidates } => {
                assert_eq!(candidates.len(), 1);
                assert_eq!(candidates[0].id, a.id);
                assert_eq!(candidates[0].matched, [SignalKind::UsbSerial]);
                assert_eq!(candidates[0].clashed, [SignalKind::VolumeSerial]);
            }
            other => panic!("expected AskChanged, got {other:?}"),
        }
    }

    #[test]
    fn after_the_user_confirms_a_reformat_the_new_format_id_is_known() {
        let mut a = known(
            DeviceKind::Storage,
            "A",
            &stick("1111-AAAA", Some("AA0400000001")),
        );
        let after = stick("9999-FFFF", Some("AA0400000001"));
        let gained = a.learn(&after, NOW);
        assert_eq!(gained, [SignalKind::VolumeSerial]);
        let v = recognise(&after, DeviceKind::Storage, std::slice::from_ref(&a));
        assert!(matches!(v, Verdict::Recognised { .. }), "{v:?}");
        // The old format ID is still known too.
        let old = stick("1111-AAAA", Some("AA0400000001"));
        assert!(matches!(
            recognise(&old, DeviceKind::Storage, &[a]),
            Verdict::Recognised { .. }
        ));
    }

    #[test]
    fn a_reformatted_stick_with_no_usb_serial_is_only_a_maybe() {
        let mut before = bare_stick();
        before.volume_serial = Some("1111-AAAA".into());
        let a = known(DeviceKind::Storage, "A", &before);
        let mut after = bare_stick();
        after.volume_serial = Some("9999-FFFF".into());
        // The old and new volume serials differ, nothing else is strong: weak agreement only.
        let v = recognise(&after, DeviceKind::Storage, std::slice::from_ref(&a));
        assert!(
            matches!(&v, Verdict::AskMaybe { candidates } if candidates[0].id == a.id),
            "{v:?}"
        );
    }

    #[test]
    fn a_placeholder_serial_is_not_trusted() {
        let mut one = bare_stick();
        one.usb_serial = Some("0123456789ABCDEF".into());
        one.volume_serial = Some("1111-1111".into());
        let mut two = bare_stick();
        two.usb_serial = Some("0123456789ABCDEF".into());
        two.volume_serial = Some("2222-2222".into());
        let a = known(DeviceKind::Storage, "1", &one);
        // Another stick with the same placeholder and another format ID: the serial neither
        // matches nor clashes, so nothing strong says it is stick 1.
        let v = recognise(&two, DeviceKind::Storage, std::slice::from_ref(&a));
        assert!(
            !matches!(v, Verdict::Recognised { .. } | Verdict::AskChanged { .. }),
            "{v:?}"
        );
        // And a stick with only the placeholder is not recognised by it either.
        let mut only = Signals {
            usb_serial: Some("0123456789ABCDEF".into()),
            ..Signals::default()
        };
        assert_eq!(
            recognise(&only, DeviceKind::Storage, std::slice::from_ref(&a)),
            Verdict::New
        );
        // Placeholder device serials are ignored the same way.
        only = Signals {
            device_serial: Some("000000000000".into()),
            ..Signals::default()
        };
        let p = known(DeviceKind::Mtp, "P", &only);
        assert_eq!(recognise(&only, DeviceKind::Mtp, &[p]), Verdict::New);
    }

    #[test]
    fn a_windows_made_usb_serial_is_not_trusted() {
        let mut s = bare_stick();
        s.usb_serial = Some("6&2a1b3c4d&0&2".into());
        let a = known(DeviceKind::Storage, "1", &s);
        let t = Signals {
            usb_serial: Some("6&2a1b3c4d&0&2".into()),
            ..Signals::default()
        };
        assert_eq!(recognise(&t, DeviceKind::Storage, &[a]), Verdict::New);
    }

    #[test]
    fn a_serial_shared_by_a_batch_stops_being_strong() {
        // Two different sticks, one batch serial.
        let batch = "BATCH0000AAAA";
        let a = known(DeviceKind::Storage, "A", &stick("1111-AAAA", Some(batch)));
        let b = known(DeviceKind::Storage, "B", &stick("2222-BBBB", Some(batch)));
        let known_list = [a.clone(), b.clone()];
        assert!(shared_usb_serials(&known_list).contains("batch0000aaaa"));

        // Plugging A: the volume serial decides, the shared serial does not drag B in.
        let v = recognise(
            &stick("1111-AAAA", Some(batch)),
            DeviceKind::Storage,
            &known_list,
        );
        assert!(
            matches!(&v, Verdict::Recognised { id, by: SignalKind::VolumeSerial } if *id == a.id),
            "{v:?}"
        );

        // A third stick of the batch: the shared serial alone proves nothing strong, and the
        // new stick is not mistaken for A or B.
        let third = stick("3333-CCCC", Some(batch));
        let v = recognise(&third, DeviceKind::Storage, &known_list);
        assert!(
            !matches!(v, Verdict::Recognised { .. } | Verdict::AskChanged { .. }),
            "{v:?}"
        );
    }

    #[test]
    fn before_the_batch_serial_is_known_to_be_shared_a_clash_still_protects() {
        // Only one stick of the batch is known so far, so its serial is still strong. A second
        // stick with the same serial but another format ID is a question, not a recognition.
        let batch = "BATCH0000AAAA";
        let a = known(DeviceKind::Storage, "A", &stick("1111-AAAA", Some(batch)));
        let v = recognise(
            &stick("2222-BBBB", Some(batch)),
            DeviceKind::Storage,
            std::slice::from_ref(&a),
        );
        assert!(
            matches!(&v, Verdict::AskChanged { candidates } if candidates[0].id == a.id),
            "{v:?}"
        );
    }

    #[test]
    fn a_card_seen_as_a_camera_and_as_a_reader() {
        // In a card reader: a drive.
        let as_reader = Signals {
            volume_serial: Some("5A5A-1234".into()),
            label: Some("EOS_DIGITAL".into()),
            capacity: Some(32_000_000_000),
            ..Signals::default()
        };
        let mut card = known(DeviceKind::Storage, "Camera card", &as_reader);
        // In the camera: a portable device with no volume serial.
        let as_camera = Signals {
            device_serial: Some("CAM0099887766".into()),
            maker: Some("Canon".into()),
            model: Some("EOS 250D".into()),
            capacity: Some(32_000_000_000),
            ..Signals::default()
        };
        // Nothing strong in common: new, which is where the card's "It's one I already
        // back up" answer comes in.
        assert_eq!(
            recognise(&as_camera, DeviceKind::Ptp, std::slice::from_ref(&card)),
            Verdict::New
        );
        // The user says it is the card: the camera's signals are added to it.
        card.learn(&as_camera, NOW);
        let v = recognise(&as_camera, DeviceKind::Ptp, std::slice::from_ref(&card));
        assert!(
            matches!(&v, Verdict::Recognised { id, by: SignalKind::DeviceSerial } if *id == card.id),
            "{v:?}"
        );
        // And the card in the reader is still recognised.
        let v = recognise(&as_reader, DeviceKind::Storage, std::slice::from_ref(&card));
        assert!(
            matches!(&v, Verdict::Recognised { id, by: SignalKind::VolumeSerial } if *id == card.id),
            "{v:?}"
        );
    }

    #[test]
    fn a_not_this_device_answer_is_respected() {
        let mut a = known(
            DeviceKind::Storage,
            "A",
            &stick("1111-AAAA", Some("AA0400000001")),
        );
        let other = stick("9999-FFFF", Some("AA0400000001"));
        assert!(matches!(
            recognise(&other, DeviceKind::Storage, std::slice::from_ref(&a)),
            Verdict::AskChanged { .. }
        ));
        a.reject(&other);
        assert_eq!(
            recognise(&other, DeviceKind::Storage, std::slice::from_ref(&a)),
            Verdict::New
        );
        // The real A is still recognised.
        let real = stick("1111-AAAA", Some("AA0400000001"));
        assert!(matches!(
            recognise(&real, DeviceKind::Storage, &[a]),
            Verdict::Recognised { .. }
        ));
    }

    #[test]
    fn a_not_this_answer_holds_for_a_stick_with_only_weak_signals() {
        let mut a = known(DeviceKind::Storage, "A", &bare_stick());
        assert!(matches!(
            recognise(&bare_stick(), DeviceKind::Storage, std::slice::from_ref(&a)),
            Verdict::AskMaybe { .. }
        ));
        a.reject(&bare_stick());
        assert_eq!(
            recognise(&bare_stick(), DeviceKind::Storage, &[a]),
            Verdict::New
        );
    }

    #[test]
    fn rejecting_one_of_two_candidates_leaves_the_other() {
        let mut a = known(DeviceKind::Storage, "A", &bare_stick());
        let b = known(DeviceKind::Storage, "B", &bare_stick());
        a.reject(&bare_stick());
        let v = recognise(&bare_stick(), DeviceKind::Storage, &[a, b.clone()]);
        assert!(
            matches!(&v, Verdict::AskMaybe { candidates } if candidates.len() == 1 && candidates[0].id == b.id),
            "{v:?}"
        );
    }

    #[test]
    fn two_devices_matching_strongly_ask_which() {
        // Two entries that each have the same strong value (the user once said "another of the
        // same kind" with a shared GPT ID, say).
        let s = Signals {
            gpt_partition_id: Some("{GPT-1}".into()),
            ..Signals::default()
        };
        let a = known(DeviceKind::Storage, "A", &s);
        let b = known(DeviceKind::Storage, "B", &s);
        let v = recognise(&s, DeviceKind::Storage, &[a.clone(), b.clone()]);
        assert_eq!(
            v,
            Verdict::AskWhich {
                ids: vec![a.id, b.id]
            }
        );
    }

    #[test]
    fn an_id_file_match_beats_everything_else() {
        // B matches by serial only; A matches by the ID file. The ID file decides.
        let mut a_sig = stick("1111-AAAA", None);
        a_sig.id_file_id = Some("5f1c-id".into());
        let a = known(DeviceKind::Storage, "A", &a_sig);
        let b = known(
            DeviceKind::Storage,
            "B",
            &stick("2222-BBBB", Some("BB0400000002")),
        );
        let now = Signals {
            id_file_id: Some("5F1C-ID".into()),
            usb_serial: Some("BB0400000002".into()),
            ..Signals::default()
        };
        let v = recognise(&now, DeviceKind::Storage, &[b.clone(), a.clone()]);
        assert_eq!(
            v,
            Verdict::Recognised {
                id: a.id.clone(),
                by: SignalKind::IdFile
            }
        );
        // Even where the format ID has changed: with a clash the ID file match asks (a cloned
        // stick would look like this) rather than being silently accepted or dropped.
        let reformatted = Signals {
            id_file_id: Some("5f1c-id".into()),
            volume_serial: Some("7777-7777".into()),
            ..Signals::default()
        };
        let v = recognise(&reformatted, DeviceKind::Storage, &[b.clone(), a.clone()]);
        assert!(
            matches!(&v, Verdict::AskChanged { candidates } if candidates.len() == 1 && candidates[0].id == a.id
                && candidates[0].matched == [SignalKind::IdFile]),
            "{v:?}"
        );
        // And with no other signals at all, the ID file alone recognises it.
        let only = Signals {
            id_file_id: Some("5f1c-id".into()),
            ..Signals::default()
        };
        assert_eq!(
            recognise(&only, DeviceKind::Storage, &[b, a.clone()]),
            Verdict::Recognised {
                id: a.id,
                by: SignalKind::IdFile
            }
        );
    }

    #[test]
    fn the_deciding_signal_is_the_strongest_one() {
        let s = phone("AAAA1111", "Mine");
        let mut with_usb = s.clone();
        with_usb.usb_serial = Some("USB0000AAAA".into());
        with_usb.gpt_partition_id = Some("{G}".into());
        let a = known(DeviceKind::Ptp, "A", &with_usb);
        let v = recognise(&with_usb, DeviceKind::Ptp, std::slice::from_ref(&a));
        // Several strong matches at the same level: the first kind in a fixed order.
        assert_eq!(
            v,
            Verdict::Recognised {
                id: a.id,
                by: SignalKind::DeviceSerial
            }
        );
    }

    #[test]
    fn a_volume_id_from_another_pc_is_not_a_clash() {
        let mut s = stick("1111-AAAA", Some("AA0400000001"));
        s.volume_guid = Some("{PC-ONE}".into());
        let a = known(DeviceKind::Storage, "A", &s);
        let mut here = s.clone();
        here.volume_guid = Some("{PC-TWO}".into());
        let v = recognise(&here, DeviceKind::Storage, &[a]);
        assert!(matches!(v, Verdict::Recognised { .. }), "{v:?}");
    }

    #[test]
    fn a_different_real_serial_rules_out_a_weak_proposal() {
        let mut a_sig = bare_stick();
        a_sig.usb_serial = Some("AA0400000001".into());
        let a = known(DeviceKind::Storage, "A", &a_sig);
        let mut other = bare_stick();
        other.usb_serial = Some("ZZ0400000002".into());
        assert_eq!(recognise(&other, DeviceKind::Storage, &[a]), Verdict::New);
    }
}
