//! One device at a time: devices that arrive while another is being looked at wait their turn.
//! This is only the order; opening a device is the window's job.

use std::collections::VecDeque;

/// The devices in `now` that were not in `before`, in the order of `now`.
pub fn arrivals(before: &[String], now: &[String]) -> Vec<String> {
    now.iter()
        .filter(|id| !before.contains(id))
        .cloned()
        .collect()
}

/// What decides whether the next waiting device may be opened.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Gate {
    /// Nothing is scanning or copying.
    pub idle: bool,
    /// A device is being opened.
    pub opening: bool,
    /// A question or the new-device card is waiting for an answer.
    pub needs_answer: bool,
    /// A scan is loaded: its results are being looked at, and opening another device would
    /// drop them.
    pub scan_loaded: bool,
    /// A dialog or a confirmation is open (or the archive rename is still saving the rest of a
    /// form), or the Settings form has changes not saved yet: another device taking over would
    /// swap the page from under it.
    pub editing: bool,
}

impl Gate {
    pub fn open(self) -> bool {
        self.idle && !self.opening && !self.needs_answer && !self.scan_loaded && !self.editing
    }
}

#[derive(Debug, Default)]
pub struct Queue {
    waiting: VecDeque<String>,
}

impl Queue {
    /// Add devices that just arrived. One that is already waiting, or is the device being looked
    /// at now, is not added again.
    pub fn add(&mut self, ids: impl IntoIterator<Item = String>, current: Option<&str>) {
        for id in ids {
            if Some(id.as_str()) != current && !self.waiting.contains(&id) {
                self.waiting.push_back(id);
            }
        }
    }

    /// Drop the devices that are no longer plugged in.
    pub fn keep_only(&mut self, listed: &[String]) {
        self.waiting.retain(|id| listed.contains(id));
    }

    pub fn remove(&mut self, id: &str) {
        self.waiting.retain(|w| w != id);
    }

    /// The next device to look at, if the gate is open.
    pub fn next(&mut self, gate: Gate) -> Option<String> {
        if gate.open() {
            self.waiting.pop_front()
        } else {
            None
        }
    }

    #[cfg(test)]
    pub fn contains(&self, id: &str) -> bool {
        self.waiting.iter().any(|w| w == id)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.waiting.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    const OPEN: Gate = Gate {
        idle: true,
        opening: false,
        needs_answer: false,
        scan_loaded: false,
        editing: false,
    };

    #[test]
    fn arrivals_are_the_new_ones_in_list_order() {
        assert_eq!(arrivals(&v(&["a"]), &v(&["b", "a", "c"])), ["b", "c"]);
        assert!(arrivals(&v(&["a", "b"]), &v(&["b"])).is_empty());
        assert_eq!(arrivals(&[], &v(&["x"])), ["x"]);
    }

    #[test]
    fn devices_are_taken_first_come_first_served_and_only_when_the_gate_is_open() {
        let mut q = Queue::default();
        q.add(v(&["a", "b", "c"]), None);
        assert_eq!(q.len(), 3);
        let shut = [
            Gate {
                idle: false,
                ..OPEN
            },
            Gate {
                opening: true,
                ..OPEN
            },
            Gate {
                needs_answer: true,
                ..OPEN
            },
            Gate {
                scan_loaded: true,
                ..OPEN
            },
            Gate {
                editing: true,
                ..OPEN
            },
        ];
        for g in shut {
            assert_eq!(q.next(g), None, "{g:?}");
        }
        assert_eq!(q.next(OPEN).as_deref(), Some("a"));
        assert_eq!(q.next(OPEN).as_deref(), Some("b"));
        assert_eq!(q.next(OPEN).as_deref(), Some("c"));
        assert_eq!(q.next(OPEN), None);
    }

    #[test]
    fn nothing_is_queued_twice_and_the_current_device_is_not_queued() {
        let mut q = Queue::default();
        q.add(v(&["a", "b"]), Some("a"));
        q.add(v(&["b", "c"]), Some("a"));
        assert_eq!(q.len(), 2);
        assert!(q.contains("b") && q.contains("c") && !q.contains("a"));
    }

    #[test]
    fn a_device_that_is_unplugged_or_chosen_by_hand_leaves_the_queue() {
        let mut q = Queue::default();
        q.add(v(&["a", "b", "c"]), None);
        q.keep_only(&v(&["a", "c"]));
        assert!(!q.contains("b"));
        q.remove("a");
        assert_eq!(q.next(OPEN).as_deref(), Some("c"));
    }
}
