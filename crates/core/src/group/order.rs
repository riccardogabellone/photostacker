//! The layer order of a stack (spec §5.6): first shot on top.

use super::FileFacts;

/// Sorts the ids of `group` (indices into `facts`) into layer order. Index 0
/// is the top layer.
///
/// The key is ascending: bracket shot number (unnumbered shots last), then the
/// Stage-1 time (capture instant, or else modification time), then file name,
/// then full path so the order is total.
///
/// # Panics
///
/// If an id in `group` is not a valid index into `facts`.
pub fn order_layers(group: &mut [usize], facts: &[FileFacts]) {
    group.sort_by_key(|&id| {
        let file = &facts[id];
        (
            file.bracket_shot().unwrap_or(u16::MAX),
            file.time_ms(),
            file.path.file_name(),
            &file.path,
        )
    });
}

#[cfg(test)]
mod tests {
    use chrono::{NaiveDate, NaiveDateTime, TimeDelta};

    use super::*;
    use crate::exif::{BracketInfo, BracketMode, ExifData};

    fn at(seconds: i64) -> NaiveDateTime {
        let start = NaiveDate::from_ymd_opt(2026, 11, 7)
            .and_then(|d| d.and_hms_opt(10, 0, 0))
            .unwrap();
        start + TimeDelta::seconds(seconds)
    }

    /// A photo taken `seconds` after a fixed instant.
    fn photo(path: &str, seconds: i64) -> FileFacts {
        FileFacts {
            path: path.into(),
            exif: Some(ExifData {
                capture_time: Some(at(seconds)),
                ..ExifData::default()
            }),
            mtime_ms: 0,
        }
    }

    fn bracketed(path: &str, seconds: i64, mode: BracketMode, shot: u16) -> FileFacts {
        let mut file = photo(path, seconds);
        file.exif.as_mut().unwrap().bracket = Some(BracketInfo {
            mode,
            shot_number: Some(shot),
            value_ev: None,
        });
        file
    }

    #[test]
    fn layers_ordered_by_shot_then_time_then_name() {
        let mut mtime_only = photo("a/m.jpg", 0);
        mtime_only.exif.as_mut().unwrap().capture_time = None;
        mtime_only.mtime_ms = at(3).and_utc().timestamp_millis();
        let facts = [
            // A numbered shot comes before any unnumbered one, whatever its time.
            bracketed("a/c.jpg", 0, BracketMode::Exposure, 2),
            bracketed("a/b.jpg", 5, BracketMode::Exposure, 1),
            // Unnumbered shots go by time, then by file name (not by path).
            photo("a/x.jpg", 1),
            photo("z/a.jpg", 1),
            photo("a/z.jpg", 0),
            // A shot number with the bracket mode off does not count.
            bracketed("a/off.jpg", 2, BracketMode::Off, 0),
            // Without a capture time, the modification time places the shot.
            mtime_only,
            // Same time and file name as `a/m.jpg`: the full path breaks the tie.
            photo("b/m.jpg", 3),
        ];
        let mut group = [7, 5, 3, 1, 6, 4, 2, 0];
        order_layers(&mut group, &facts);
        assert_eq!(group, [1, 0, 4, 3, 2, 5, 6, 7]);
    }

    #[test]
    fn capture_time_uses_the_utc_offset() {
        // 10:00:05 at UTC+1 was taken before 10:00:00 at UTC.
        let mut east = photo("a/east.jpg", 5);
        east.exif.as_mut().unwrap().utc_offset_minutes = Some(60);
        let facts = [photo("a/utc.jpg", 0), east];
        let mut group = [0, 1];
        order_layers(&mut group, &facts);
        assert_eq!(group, [1, 0]);
    }
}
