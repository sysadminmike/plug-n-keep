//! Turning decoded pixels upright, and fixing the EXIF Orientation tag to match
//! (DESIGN.md section 7.6).

use super::container::{Flip, Transform};

/// Decoded 8-bit RGB pixels, three bytes per pixel, rows top to bottom.
pub struct Rgb {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl Rgb {
    /// Applies the steps in order.
    pub fn apply(self, steps: &[Transform]) -> Rgb {
        steps.iter().fold(self, |img, t| match *t {
            Transform::Rotate(n) => img.rotate_anticlockwise(n),
            Transform::Mirror(f) => img.mirror(f),
        })
    }

    fn at(&self, x: usize, y: usize) -> &[u8] {
        let i = (y * self.width as usize + x) * 3;
        &self.data[i..i + 3]
    }

    /// `quarters` quarter turns anticlockwise (taken modulo 4).
    fn rotate_anticlockwise(self, quarters: u8) -> Rgb {
        let (w, h) = (self.width as usize, self.height as usize);
        match quarters % 4 {
            0 => self,
            2 => {
                let mut data = Vec::with_capacity(self.data.len());
                for px in self.data.as_chunks::<3>().0.iter().rev() {
                    data.extend_from_slice(px);
                }
                Rgb { data, ..self }
            }
            q => {
                // New image is h wide and w high. Anticlockwise: the right edge becomes the top.
                let mut data = Vec::with_capacity(self.data.len());
                for ny in 0..w {
                    for nx in 0..h {
                        let (x, y) = if q == 1 {
                            (w - 1 - ny, nx)
                        } else {
                            (ny, h - 1 - nx)
                        };
                        data.extend_from_slice(self.at(x, y));
                    }
                }
                Rgb {
                    width: self.height,
                    height: self.width,
                    data,
                }
            }
        }
    }

    fn mirror(mut self, flip: Flip) -> Rgb {
        let (w, h) = (self.width as usize, self.height as usize);
        match flip {
            Flip::Horizontal => {
                for row in self.data.chunks_exact_mut(w * 3) {
                    for x in 0..w / 2 {
                        for c in 0..3 {
                            row.swap(x * 3 + c, (w - 1 - x) * 3 + c);
                        }
                    }
                }
            }
            Flip::Vertical => {
                for y in 0..h / 2 {
                    let (top, bottom) = self.data.split_at_mut((h - 1 - y) * w * 3);
                    top[y * w * 3..(y + 1) * w * 3].swap_with_slice(&mut bottom[..w * 3]);
                }
            }
        }
        self
    }
}

/// Where the Orientation value sits in an EXIF TIFF block, and the block's byte order.
fn orientation_slot(tiff: &[u8]) -> Option<(usize, bool)> {
    let le = match tiff.get(..4)? {
        b"II*\0" => true,
        b"MM\0*" => false,
        _ => return None,
    };
    let u16_at = |at: usize| -> Option<u16> {
        let b = tiff.get(at..at.checked_add(2)?)?;
        Some(if le {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            u16::from_be_bytes([b[0], b[1]])
        })
    };
    let b = tiff.get(4..8)?;
    let ifd = if le {
        u32::from_le_bytes([b[0], b[1], b[2], b[3]])
    } else {
        u32::from_be_bytes([b[0], b[1], b[2], b[3]])
    } as usize;
    let count = usize::from(u16_at(ifd)?);
    for i in 0..count {
        let entry = ifd.checked_add(2)?.checked_add(i.checked_mul(12)?)?;
        if u16_at(entry)? == 0x0112 {
            // Type SHORT, one value, stored in the entry itself.
            let b = tiff.get(entry.checked_add(4)?..entry.checked_add(8)?)?;
            // The value itself must lie inside the block too.
            tiff.get(entry.checked_add(8)?..entry.checked_add(10)?)?;
            let n = if le {
                u32::from_le_bytes([b[0], b[1], b[2], b[3]])
            } else {
                u32::from_be_bytes([b[0], b[1], b[2], b[3]])
            };
            return (u16_at(entry + 2)? == 3 && n == 1).then_some((entry + 8, le));
        }
    }
    None
}

/// The EXIF Orientation value (1 to 8), if the block has one.
#[cfg(test)]
pub fn orientation(tiff: &[u8]) -> Option<u16> {
    let (at, le) = orientation_slot(tiff)?;
    let b = tiff.get(at..at + 2)?;
    Some(if le {
        u16::from_le_bytes([b[0], b[1]])
    } else {
        u16::from_be_bytes([b[0], b[1]])
    })
}

/// Sets the Orientation tag to 1 ("upright") in place, in either byte order. Returns whether
/// the block had one. Nothing else in the block changes.
pub fn set_upright(tiff: &mut [u8]) -> bool {
    let Some((at, le)) = orientation_slot(tiff) else {
        return false;
    };
    let one = if le {
        1u16.to_le_bytes()
    } else {
        1u16.to_be_bytes()
    };
    tiff[at..at + 2].copy_from_slice(&one);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 3 wide, 2 high; each pixel's three bytes are all its number 0..5.
    fn sample() -> Rgb {
        Rgb {
            width: 3,
            height: 2,
            data: (0..6u8).flat_map(|n| [n; 3]).collect(),
        }
    }

    fn numbers(i: &Rgb) -> Vec<u8> {
        i.data.as_chunks::<3>().0.iter().map(|p| p[0]).collect()
    }

    // The picture:   0 1 2
    //                3 4 5
    #[test]
    fn one_quarter_turn_anticlockwise_puts_the_right_edge_on_top() {
        let r = sample().apply(&[Transform::Rotate(1)]);
        assert_eq!((r.width, r.height), (2, 3));
        assert_eq!(numbers(&r), vec![2, 5, 1, 4, 0, 3]);
    }

    #[test]
    fn three_quarter_turns_is_one_turn_clockwise() {
        let r = sample().apply(&[Transform::Rotate(3)]);
        assert_eq!((r.width, r.height), (2, 3));
        assert_eq!(numbers(&r), vec![3, 0, 4, 1, 5, 2]);
    }

    #[test]
    fn a_half_turn_reverses_everything() {
        let r = sample().apply(&[Transform::Rotate(2)]);
        assert_eq!((r.width, r.height), (3, 2));
        assert_eq!(numbers(&r), vec![5, 4, 3, 2, 1, 0]);
    }

    #[test]
    fn mirrors_flip_one_axis() {
        let h = sample().apply(&[Transform::Mirror(Flip::Horizontal)]);
        assert_eq!(numbers(&h), vec![2, 1, 0, 5, 4, 3]);
        let v = sample().apply(&[Transform::Mirror(Flip::Vertical)]);
        assert_eq!(numbers(&v), vec![3, 4, 5, 0, 1, 2]);
    }

    #[test]
    fn steps_apply_in_order() {
        let a = sample().apply(&[Transform::Rotate(1), Transform::Mirror(Flip::Horizontal)]);
        assert_eq!(numbers(&a), vec![5, 2, 4, 1, 3, 0]);
    }

    fn tiff(le: bool, orientation: u16, other_first: bool) -> Vec<u8> {
        let w16 = |v: u16| if le { v.to_le_bytes() } else { v.to_be_bytes() };
        let w32 = |v: u32| if le { v.to_le_bytes() } else { v.to_be_bytes() };
        let mut t = if le {
            b"II*\0".to_vec()
        } else {
            b"MM\0*".to_vec()
        };
        t.extend_from_slice(&w32(8));
        t.extend_from_slice(&w16(if other_first { 2 } else { 1 }));
        if other_first {
            // Make: ASCII, 4 bytes, stored in the entry.
            t.extend_from_slice(&w16(0x010F));
            t.extend_from_slice(&w16(2));
            t.extend_from_slice(&w32(4));
            t.extend_from_slice(b"abc\0");
        }
        t.extend_from_slice(&w16(0x0112));
        t.extend_from_slice(&w16(3));
        t.extend_from_slice(&w32(1));
        t.extend_from_slice(&w16(orientation));
        t.extend_from_slice(&[0, 0]);
        t.extend_from_slice(&w32(0));
        t
    }

    #[test]
    fn orientation_is_set_to_one_in_both_byte_orders() {
        for le in [true, false] {
            for other_first in [false, true] {
                let mut t = tiff(le, 6, other_first);
                assert_eq!(orientation(&t), Some(6));
                let before = t.clone();
                assert!(set_upright(&mut t));
                assert_eq!(orientation(&t), Some(1));
                let differing = t.iter().zip(&before).filter(|(a, b)| a != b).count();
                assert_eq!(differing, 1, "only the orientation byte changes");
            }
        }
    }

    #[test]
    fn every_prefix_of_a_block_with_orientation_is_safe() {
        for le in [true, false] {
            for other_first in [false, true] {
                let full = tiff(le, 6, other_first);
                for cut in 0..=full.len() {
                    let mut c = full[..cut].to_vec();
                    let had = set_upright(&mut c);
                    // Cut right after the count bytes: no value to set.
                    if cut < full.len() - 6 {
                        assert!(!had, "cut {cut}");
                    }
                    let _ = orientation(&c);
                }
            }
        }
    }

    #[test]
    fn a_block_without_orientation_or_cut_short_is_left_alone() {
        let mut t = tiff(true, 6, false);
        t[10] = 0x13; // another tag
        let copy = t.clone();
        assert!(!set_upright(&mut t));
        assert_eq!(t, copy);
        for cut in 0..copy.len() {
            let mut c = copy[..cut].to_vec();
            let _ = set_upright(&mut c);
        }
        let mut junk = b"II*\0\xff\xff\xff\xff".to_vec();
        assert!(!set_upright(&mut junk));
        assert!(!set_upright(&mut b"nonsense".to_vec()));
    }
}
